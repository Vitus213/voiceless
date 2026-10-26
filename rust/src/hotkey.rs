//! Push-to-talk key read straight from evdev (`/dev/input`, needs the
//! `input` group), so it works on compositors without key-release bindings
//! (niri, KDE). The listener reports presses and releases; the daemon turns
//! them into hold-to-talk or tap-to-toggle (see `daemon::HotkeyMachine`).
//! Events are only read, never grabbed: the key still reaches applications,
//! so pick one nothing else uses (ScrollLock, Pause, a spare F-key).
//!
//! Only `voiceless daemon` uses this module (`daemon::start_hotkey`), for
//! the `[hotkey] key` config value; `config` validates that value with
//! `parse_key`. No key is configured by default. The listener is
//! best-effort: a device that cannot be opened is skipped silently, and the
//! daemon reports "no readable keyboard" while nothing is watched.
//!
//! Keyboards are followed across hotplug: an inotify watch on `/dev/input`
//! picks up nodes that appear (a keyboard plugged in, or re-enumerated after
//! a USB reset or KVM switch) or whose permissions change (udev applies the
//! `input` group after creating the node), and drops nodes that disappear.

use std::collections::HashMap;
use std::ffi::{CString, OsStr};
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use evdev::{EventStream, EventSummary, KeyCode};
use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc;
use tokio::task::{AbortHandle, JoinSet};

const INPUT_DIR: &str = "/dev/input";

/// A transition of the configured key, with the Ctrl modifier sampled at
/// press time (autorepeat is filtered out).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyEvent {
    /// The push-to-talk key went down; `ctrl` if a Ctrl key was held.
    Press { ctrl: bool },
    /// The push-to-talk key went up.
    Release,
}

/// Parse an evdev key name such as `KEY_SCROLLLOCK` (the `KEY_*` constants
/// of `linux/input-event-codes.h`, as `evdev` spells them). Used both by
/// config validation and by the daemon when starting the listener.
pub fn parse_key(name: &str) -> Result<KeyCode, String> {
    KeyCode::from_str(name).map_err(|_| format!("unknown evdev key name: {name}"))
}

/// Watch every keyboard that has `key` and forward its presses and
/// releases. Returns one task; aborting it stops every device watch. It
/// also ends when `events` is closed.
///
/// `on_count` receives the number of watched devices: once synchronously
/// before this returns, then whenever hotplug changes it. 0 usually means
/// no read access to /dev/input (join the `input` group) or no keyboard
/// with the key plugged in. Without inotify the initial set is kept and a
/// device whose event stream fails (unplug) is only dropped.
///
/// The Ctrl state is one counter shared by all devices, so Ctrl on one
/// keyboard qualifies a press on another; it can never go below zero even
/// if Ctrl was already down when the listener started.
pub fn listen(
    key: KeyCode,
    events: mpsc::Sender<KeyEvent>,
    mut on_count: impl FnMut(usize) + Send + 'static,
) -> tokio::task::JoinHandle<()> {
    // Watch before scanning so a node created in between is not missed;
    // `add` ignores one already watched.
    let mut notify = watch_input_dir().ok();
    let mut watcher = Watcher {
        key,
        events,
        ctrl_down: Arc::new(AtomicUsize::new(0)),
        devices: JoinSet::new(),
        paths: HashMap::new(),
    };
    watcher.scan();
    let mut reported = watcher.paths.len();
    on_count(reported);
    tokio::spawn(async move {
        let closed = watcher.events.clone();
        let mut buf = [0u8; 4096];
        loop {
            tokio::select! {
                // The daemon dropped its receiver (reload or shutdown);
                // dropping `watcher` aborts the device tasks.
                () = closed.closed() => return,
                // Reap device tasks that ended (unplug); `None` when there
                // are none disables this branch for this round.
                Some(_) = watcher.devices.join_next() => {}
                len = read_events(&mut notify, &mut buf) => watcher.apply(&buf[..len]),
            }
            watcher.paths.retain(|_, task| !task.is_finished());
            let count = watcher.paths.len();
            if count != reported {
                reported = count;
                on_count(count);
            }
        }
    })
}

struct Watcher {
    key: KeyCode,
    events: mpsc::Sender<KeyEvent>,
    ctrl_down: Arc<AtomicUsize>,
    devices: JoinSet<()>,
    /// Watched device node → its task; the task may have finished (stream
    /// failed) until the next prune.
    paths: HashMap<PathBuf, AbortHandle>,
}

impl Watcher {
    /// Try every `event*` node under /dev/input.
    fn scan(&mut self) {
        let Ok(entries) = std::fs::read_dir(INPUT_DIR) else {
            return;
        };
        for entry in entries.flatten() {
            if entry.file_name().as_bytes().starts_with(b"event") {
                self.add(entry.path());
            }
        }
    }

    /// Apply one inotify read: re-scan on queue overflow, otherwise add
    /// created or permission-changed nodes and drop deleted ones.
    fn apply(&mut self, batch: &[u8]) {
        for (mask, name) in parse_inotify(batch) {
            if mask & libc::IN_Q_OVERFLOW != 0 {
                self.scan();
                continue;
            }
            if !name.as_bytes().starts_with(b"event") {
                continue;
            }
            let path = Path::new(INPUT_DIR).join(name);
            if mask & libc::IN_DELETE != 0 {
                if let Some(task) = self.paths.remove(&path) {
                    task.abort();
                }
            } else {
                self.add(path);
            }
        }
    }

    /// Watch `path` unless a live task already does. Devices without the key
    /// and nodes that cannot be opened (yet) are skipped.
    fn add(&mut self, path: PathBuf) {
        if self
            .paths
            .get(&path)
            .is_some_and(|task| !task.is_finished())
        {
            return;
        }
        let Some(stream) = open_keyboard(&path, self.key) else {
            return;
        };
        let task = self.devices.spawn(forward(
            stream,
            self.key,
            self.events.clone(),
            self.ctrl_down.clone(),
        ));
        self.paths.insert(path, task);
    }
}

/// Open `path` as an event stream if the device reports `key` at all; this
/// skips mice, touchpads and other non-keyboard nodes with no extra probing.
fn open_keyboard(path: &Path, key: KeyCode) -> Option<EventStream> {
    let device = evdev::Device::open(path).ok()?;
    let has_key = device
        .supported_keys()
        .is_some_and(|keys| keys.contains(key));
    if !has_key {
        return None;
    }
    device.into_event_stream().ok()
}

/// Forward one device's transitions of `key` until its stream fails
/// (unplug) or `events` is closed.
async fn forward(
    mut stream: EventStream,
    key: KeyCode,
    events: mpsc::Sender<KeyEvent>,
    ctrl_down: Arc<AtomicUsize>,
) {
    while let Ok(event) = stream.next_event().await {
        let EventSummary::Key(_, code, value) = event.destructure() else {
            continue;
        };
        // Track Ctrl ourselves (value 1 = down, 0 = up, 2 = repeat):
        // there is no modifier state in raw evdev events.
        if matches!(code, KeyCode::KEY_LEFTCTRL | KeyCode::KEY_RIGHTCTRL) {
            match value {
                1 => {
                    ctrl_down.fetch_add(1, Ordering::Relaxed);
                }
                0 => {
                    let _ = ctrl_down.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |held| {
                        held.checked_sub(1)
                    });
                }
                _ => {}
            }
            continue;
        }
        if code != key {
            continue;
        }
        // value 2 is autorepeat: neither a new press nor a release.
        let forwarded = match value {
            1 => KeyEvent::Press {
                ctrl: ctrl_down.load(Ordering::Relaxed) > 0,
            },
            0 => KeyEvent::Release,
            _ => continue,
        };
        if events.send(forwarded).await.is_err() {
            return;
        }
    }
}

/// Non-blocking inotify fd watching /dev/input for created, deleted and
/// permission-changed nodes.
fn watch_input_dir() -> io::Result<AsyncFd<File>> {
    // SAFETY: plain syscall; the returned fd is checked and owned below.
    let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a fresh descriptor nothing else owns.
    let file = File::from(unsafe { OwnedFd::from_raw_fd(fd) });
    let dir = CString::new(INPUT_DIR).expect("no NUL in INPUT_DIR");
    let mask = libc::IN_CREATE | libc::IN_ATTRIB | libc::IN_DELETE;
    // SAFETY: `file` keeps the fd open and `dir` is NUL-terminated.
    if unsafe { libc::inotify_add_watch(fd, dir.as_ptr(), mask) } < 0 {
        return Err(io::Error::last_os_error());
    }
    AsyncFd::new(file)
}

/// Wait for the next inotify read into `buf`; pending forever without a
/// watch. A read error drops the watch (no more hotplug) rather than spin.
async fn read_events(notify: &mut Option<AsyncFd<File>>, buf: &mut [u8]) -> usize {
    loop {
        let Some(fd) = notify.as_ref() else {
            return std::future::pending().await;
        };
        let result = match fd.readable().await {
            Ok(mut guard) => match guard.try_io(|inner| inner.get_ref().read(buf)) {
                Ok(result) => result,
                // Readiness was stale; wait again.
                Err(_would_block) => continue,
            },
            Err(error) => Err(error),
        };
        match result {
            Ok(len) => return len,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => *notify = None,
        }
    }
}

/// Split one inotify read into `(mask, name)` records. Names are NUL-padded
/// to `len`; a truncated trailing record ends the iteration.
fn parse_inotify<'a>(batch: &'a [u8]) -> impl Iterator<Item = (u32, &'a OsStr)> + 'a {
    const HEADER: usize = std::mem::size_of::<libc::inotify_event>();
    let mut rest: &'a [u8] = batch;
    std::iter::from_fn(move || {
        let current = rest;
        let header = current.get(..HEADER)?;
        // struct inotify_event { int wd; uint32_t mask, cookie, len; char name[]; }
        let mask = u32::from_ne_bytes(header[4..8].try_into().ok()?);
        let len = u32::from_ne_bytes(header[12..16].try_into().ok()?) as usize;
        let name = current.get(HEADER..HEADER + len)?;
        rest = &current[HEADER + len..];
        let end = name
            .iter()
            .position(|&byte| byte == 0)
            .unwrap_or(name.len());
        Some((mask, OsStr::from_bytes(&name[..end])))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(mask: u32, name: &[u8], padded: usize) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1i32.to_ne_bytes());
        bytes.extend_from_slice(&mask.to_ne_bytes());
        bytes.extend_from_slice(&0u32.to_ne_bytes());
        bytes.extend_from_slice(&(padded as u32).to_ne_bytes());
        bytes.extend_from_slice(name);
        bytes.resize(bytes.len() + padded - name.len(), 0);
        bytes
    }

    #[test]
    fn inotify_records_split_on_padded_names_and_stop_at_truncation() {
        let mut batch = record(libc::IN_CREATE, b"event12", 16);
        batch.extend(record(libc::IN_Q_OVERFLOW, b"", 0));
        batch.extend(record(libc::IN_DELETE, b"event3", 16));
        let full = batch.len();
        batch.extend(record(libc::IN_ATTRIB, b"event4", 16));
        batch.truncate(full + 20);

        let parsed: Vec<_> = parse_inotify(&batch)
            .map(|(mask, name)| (mask, name.to_owned()))
            .collect();
        assert_eq!(
            parsed,
            vec![
                (libc::IN_CREATE, "event12".into()),
                (libc::IN_Q_OVERFLOW, "".into()),
                (libc::IN_DELETE, "event3".into()),
            ]
        );
    }
}
