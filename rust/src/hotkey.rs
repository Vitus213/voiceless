//! Linux shortcuts read from evdev. Input devices remain available to the
//! compositor and other apps, so action bindings should use spare chords.
//! The daemon owns gestures and session identity. This module discovers
//! keyboards and emits transitions of configured chords only.
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
use std::sync::Arc;
use std::time::Instant;

use evdev::{EventStream, EventSummary, KeyCode};
use tokio::io::unix::AsyncFd;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::task::{AbortHandle, JoinSet};

const INPUT_DIR: &str = "/dev/input";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    Legacy,
    Hold,
    Toggle,
    Cancel,
    Raw,
    Repaste,
}

const ACTION_COUNT: usize = 6;
const ACTIONS: [Action; ACTION_COUNT] = [
    Action::Legacy,
    Action::Hold,
    Action::Toggle,
    Action::Cancel,
    Action::Raw,
    Action::Repaste,
];

impl Action {
    pub(crate) fn field(self) -> &'static str {
        match self {
            Self::Legacy => "key",
            Self::Hold => "hold_key",
            Self::Toggle => "toggle_key",
            Self::Cancel => "cancel_key",
            Self::Raw => "raw_key",
            Self::Repaste => "repaste_key",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Transition {
    Press { ctrl: bool },
    Release,
    Disconnected,
}

/// A configured chord transition timed at the input device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyEvent {
    pub(crate) action: Action,
    pub(crate) transition: Transition,
    pub(crate) at: Instant,
}

const CTRL: u8 = 1;
const ALT: u8 = 2;
const SHIFT: u8 = 4;
const SUPER: u8 = 8;

#[derive(Debug)]
pub(crate) struct Binding {
    pub(crate) action: Action,
    key: KeyCode,
    // The legacy key reacts with or without Ctrl for the two old modes.
    modifiers: Option<u8>,
}

pub(crate) fn configured_keys(
    config: &crate::config::HotkeyConfig,
) -> [Option<&str>; ACTION_COUNT] {
    [
        config.key.as_deref(),
        config.hold_key.as_deref(),
        config.toggle_key.as_deref(),
        config.cancel_key.as_deref(),
        config.raw_key.as_deref(),
        config.repaste_key.as_deref(),
    ]
}

pub(crate) fn bindings(config: &crate::config::HotkeyConfig) -> Result<Vec<Binding>, String> {
    let mut bindings: Vec<Binding> = Vec::new();
    let names = configured_keys(config);
    for (action, name) in ACTIONS.into_iter().zip(names) {
        let Some(name) = name else { continue };
        let field = action.field();
        let error = |error| format!("hotkey.{field}: {error}");
        let (key, modifiers) = if action == Action::Legacy && !crate::config::is_chord(name) {
            (parse_key(name).map_err(error)?, None)
        } else {
            parse_chord(name)
                .map_err(error)
                .map(|(key, modifiers)| (key, Some(modifiers)))?
        };
        if let Some(other) = bindings.iter().find(|other| {
            other.key == key && (other.modifiers == modifiers || other.modifiers.is_none())
        }) {
            return Err(format!(
                "hotkey.{} conflicts with hotkey.{} on {name}",
                action.field(),
                other.action.field()
            ));
        }
        bindings.push(Binding {
            action,
            key,
            modifiers,
        });
    }
    Ok(bindings)
}

fn parse_chord(name: &str) -> Result<(KeyCode, u8), String> {
    let mut parts = name.rsplit('+');
    let key = parse_key(parts.next().unwrap_or_default())?;
    let mut modifiers = 0;
    for part in parts {
        let bit = match part {
            "Ctrl" => CTRL,
            "Alt" => ALT,
            "Shift" => SHIFT,
            "Super" => SUPER,
            _ => return Err(format!("unknown modifier {part:?}")),
        };
        if modifiers & bit != 0 {
            return Err(format!("duplicate modifier {part}"));
        }
        modifiers |= bit;
    }
    // A chord of modifiers alone (`Ctrl+Super`) names modifiers, not one key
    // of them: either Win key is the key, so every spelling of the chord is
    // one binding.
    if modifiers == 0 {
        return Ok((key, 0));
    }
    match modifier_bit(key) {
        Some(own) => {
            let set = modifiers | own;
            let main = 1 << (7 - set.leading_zeros());
            Ok((modifier_key(main), set & !main))
        }
        None => Ok((key, modifiers)),
    }
}

/// The modifier a key itself is: its bit in the modifier mask.
fn modifier_bit(key: KeyCode) -> Option<u8> {
    modifier_side(key).map(|side| 1 << (side / 2))
}

/// The left key of the modifier a bit of the mask stands for.
fn modifier_key(bit: u8) -> KeyCode {
    match bit {
        CTRL => KeyCode::KEY_LEFTCTRL,
        ALT => KeyCode::KEY_LEFTALT,
        SHIFT => KeyCode::KEY_LEFTSHIFT,
        _ => KeyCode::KEY_LEFTMETA,
    }
}

impl Binding {
    /// Whether a press of `key` is this chord. `before` and `after` are the
    /// modifiers down before and after the key changed. A chord of modifiers
    /// alone has no key to press last: it is also pressed by the modifier
    /// that completes it, whichever of them went down last.
    fn pressed_by(&self, key: KeyCode, before: u8, after: u8) -> bool {
        if self.key == key {
            return self.modifiers.is_none_or(|required| required == before);
        }
        match (modifier_bit(self.key), modifier_bit(key), self.modifiers) {
            (Some(own), Some(completing), Some(required @ 1..)) => {
                before & completing == 0 && after == (required | own)
            }
            _ => false,
        }
    }

    /// Whether `key` going up releases this chord: its own key, or either
    /// Win key of a chord of modifiers that the left one names.
    fn released_by(&self, key: KeyCode) -> bool {
        self.key == key || (self.is_modifiers_only() && modifier_bit(key) == modifier_bit(self.key))
    }

    /// Whether this chord is made of modifiers alone, such as `Ctrl+Super`.
    fn is_modifiers_only(&self) -> bool {
        matches!(
            (modifier_bit(self.key), self.modifiers),
            (Some(_), Some(1..))
        )
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct KeyboardSupport {
    pub(crate) total: usize,
    counts: [usize; ACTION_COUNT],
}

impl KeyboardSupport {
    pub(crate) fn for_action(self, action: Action) -> usize {
        self.counts[action as usize]
    }
}

/// Parse an evdev key name such as `KEY_SCROLLLOCK` (the `KEY_*` constants
/// of `linux/input-event-codes.h`, as `evdev` spells them). Used both by
/// config validation and by the daemon when starting the listener.
pub fn parse_key(name: &str) -> Result<KeyCode, String> {
    KeyCode::from_str(name).map_err(|_| format!("unknown evdev key name: {name}"))
}

/// Owns the actual keyboard streams discovered during a scan. A config
/// change can reject an empty set before stopping the current listener.
pub(crate) struct Listener {
    watcher: Watcher,
    notify: Option<AsyncFd<File>>,
}

impl Listener {
    pub fn open(bindings: Arc<[Binding]>, events: mpsc::Sender<KeyEvent>) -> Self {
        // Watch before scanning so a node created in between is not missed.
        let notify = watch_input_dir().ok();
        let mut watcher = Watcher {
            bindings,
            events,
            keys: Arc::new(Mutex::new(PressedKeys::default())),
            devices: JoinSet::new(),
            paths: HashMap::new(),
        };
        watcher.scan();
        Self { watcher, notify }
    }

    pub fn support(&self) -> KeyboardSupport {
        self.watcher.support()
    }

    /// Aborting the returned task or closing the channel stops every watch.
    pub fn start(
        self,
        mut on_count: impl FnMut(KeyboardSupport) + Send + 'static,
    ) -> tokio::task::JoinHandle<()> {
        let mut reported = self.support();
        on_count(reported);
        let Self {
            mut watcher,
            mut notify,
        } = self;
        tokio::spawn(async move {
            let closed = watcher.events.clone();
            let mut buf = [0u8; 4096];
            loop {
                tokio::select! {
                    () = closed.closed() => return,
                    Some(_) = watcher.devices.join_next() => {}
                    len = read_events(&mut notify, &mut buf) => watcher.apply(&buf[..len]),
                }
                watcher.paths.retain(|_, device| !device.task.is_finished());
                let count = watcher.support();
                if count != reported {
                    reported = count;
                    on_count(count);
                }
            }
        })
    }
}

struct Watcher {
    bindings: Arc<[Binding]>,
    events: mpsc::Sender<KeyEvent>,
    keys: Arc<Mutex<PressedKeys>>,
    devices: JoinSet<()>,
    paths: HashMap<PathBuf, WatchedDevice>,
}

struct WatchedDevice {
    task: AbortHandle,
    supports: u8,
    // Dropping this stops the reader and releases the keys that it contributed.
    _disconnect: oneshot::Sender<()>,
}

impl Watcher {
    fn support(&self) -> KeyboardSupport {
        let mut support = KeyboardSupport {
            total: self.paths.len(),
            ..Default::default()
        };
        for device in self.paths.values() {
            for (index, count) in support.counts.iter_mut().enumerate() {
                *count += usize::from(device.supports & (1 << index) != 0);
            }
        }
        support
    }

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
                self.paths.remove(&path);
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
            .is_some_and(|device| !device.task.is_finished())
        {
            return;
        }
        let Some((stream, supports)) = open_keyboard(&path, &self.bindings) else {
            return;
        };
        let (disconnect, disconnected) = oneshot::channel();
        let task = self.devices.spawn(forward(
            stream,
            self.bindings.clone(),
            self.events.clone(),
            self.keys.clone(),
            disconnected,
        ));
        self.paths.insert(
            path,
            WatchedDevice {
                task,
                supports,
                _disconnect: disconnect,
            },
        );
    }
}

fn open_keyboard(path: &Path, bindings: &[Binding]) -> Option<(EventStream, u8)> {
    let device = evdev::Device::open(path).ok()?;
    let keys = device.supported_keys()?;
    let supports = bindings
        .iter()
        .filter(|binding| keys.contains(binding.key))
        .fold(0, |mask, binding| mask | (1 << binding.action as usize));
    if supports == 0 {
        return None;
    }
    Some((device.into_event_stream().ok()?, supports))
}

#[derive(Default)]
struct PressedKeys {
    modifiers: [usize; 4],
    actions: [usize; ACTION_COUNT],
}

impl PressedKeys {
    fn modifier_mask(&self) -> u8 {
        self.modifiers
            .iter()
            .enumerate()
            .fold(0, |mask, (index, count)| {
                mask | (u8::from(*count > 0) << index)
            })
    }
}

#[derive(Default)]
struct DeviceKeys {
    modifiers: u8,
    actions: [bool; ACTION_COUNT],
}

impl DeviceKeys {
    fn update_modifier(&mut self, side: usize, down: bool, pressed: &mut PressedKeys) {
        let bit = 1 << side;
        if down != (self.modifiers & bit != 0) {
            self.modifiers ^= bit;
            if down {
                pressed.modifiers[side / 2] += 1;
            } else {
                pressed.modifiers[side / 2] -= 1;
            }
        }
    }

    fn release(&mut self, action: Action, pressed: &mut PressedKeys) -> bool {
        let index = action as usize;
        if !std::mem::take(&mut self.actions[index]) {
            return false;
        }
        pressed.actions[index] -= 1;
        pressed.actions[index] == 0
    }

    fn transition(
        &mut self,
        bindings: &[Binding],
        pressed: &mut PressedKeys,
        key: KeyCode,
        value: i32,
        at: Instant,
    ) -> Option<KeyEvent> {
        if value != 0 && value != 1 {
            return None;
        }
        // A chord is matched without counting the modifier its own key is.
        let before = pressed.modifier_mask();
        if let Some(side) = modifier_side(key) {
            self.update_modifier(side, value == 1, pressed);
        }
        if value == 0 {
            let action = bindings
                .iter()
                .find(|binding| self.actions[binding.action as usize] && binding.released_by(key))?
                .action;
            return self.release(action, pressed).then_some(KeyEvent {
                action,
                at,
                transition: Transition::Release,
            });
        }
        let binding = bindings.iter().find(|binding| {
            !self.actions[binding.action as usize]
                && binding.pressed_by(key, before, pressed.modifier_mask())
        })?;
        let index = binding.action as usize;
        self.actions[index] = true;
        pressed.actions[index] += 1;
        (pressed.actions[index] == 1).then_some(KeyEvent {
            action: binding.action,
            at,
            transition: Transition::Press {
                ctrl: before & CTRL != 0,
            },
        })
    }
}

fn modifier_side(key: KeyCode) -> Option<usize> {
    Some(match key {
        KeyCode::KEY_LEFTCTRL => 0,
        KeyCode::KEY_RIGHTCTRL => 1,
        KeyCode::KEY_LEFTALT => 2,
        KeyCode::KEY_RIGHTALT => 3,
        KeyCode::KEY_LEFTSHIFT => 4,
        KeyCode::KEY_RIGHTSHIFT => 5,
        KeyCode::KEY_LEFTMETA => 6,
        KeyCode::KEY_RIGHTMETA => 7,
        _ => return None,
    })
}

async fn forward(
    mut stream: EventStream,
    bindings: Arc<[Binding]>,
    events: mpsc::Sender<KeyEvent>,
    keys: Arc<Mutex<PressedKeys>>,
    mut disconnected: oneshot::Receiver<()>,
) {
    let mut device = DeviceKeys::default();
    loop {
        let next_event = tokio::select! {
            event = stream.next_event() => event,
            _ = &mut disconnected => break,
        };
        let Ok(event) = next_event else { break };
        let EventSummary::Key(_, code, value) = event.destructure() else {
            continue;
        };
        if modifier_side(code).is_none() && !bindings.iter().any(|binding| binding.key == code) {
            continue;
        }
        let at = Instant::now();
        // Send under the same lock so presses and releases from different
        // devices reach the gateway in the order they changed the aggregate.
        let mut pressed = keys.lock().await;
        if let Some(event) = device.transition(&bindings, &mut pressed, code, value, at) {
            if events.send(event).await.is_err() {
                return;
            }
        }
    }
    let mut pressed = keys.lock().await;
    for side in 0..8 {
        device.update_modifier(side, false, &mut pressed);
    }
    let at = Instant::now();
    for action in ACTIONS {
        if device.release(action, &mut pressed)
            && events
                .send(KeyEvent {
                    action,
                    at,
                    transition: Transition::Disconnected,
                })
                .await
                .is_err()
        {
            break;
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

    #[test]
    fn a_held_shortcut_suppresses_repeat_and_releases_after_its_modifiers() {
        let config = crate::config::HotkeyConfig {
            hold_key: Some("Ctrl+KEY_F6".to_owned()),
            ..Default::default()
        };
        let bindings = bindings(&config).unwrap();
        let mut pressed = PressedKeys::default();
        let mut first = DeviceKeys::default();
        let mut second = DeviceKeys::default();
        let now = Instant::now();
        first.transition(&bindings, &mut pressed, KeyCode::KEY_LEFTCTRL, 1, now);
        assert_eq!(
            first.transition(&bindings, &mut pressed, KeyCode::KEY_F6, 1, now),
            Some(KeyEvent {
                action: Action::Hold,
                at: now,
                transition: Transition::Press { ctrl: true }
            }),
        );
        for repeat in [1, 2] {
            assert!(first
                .transition(&bindings, &mut pressed, KeyCode::KEY_F6, repeat, now)
                .is_none());
        }
        assert!(second
            .transition(&bindings, &mut pressed, KeyCode::KEY_F6, 1, now)
            .is_none());
        first.transition(&bindings, &mut pressed, KeyCode::KEY_LEFTCTRL, 0, now);
        assert!(first
            .transition(&bindings, &mut pressed, KeyCode::KEY_F6, 0, now)
            .is_none());
        assert_eq!(
            second.transition(&bindings, &mut pressed, KeyCode::KEY_F6, 0, now),
            Some(KeyEvent {
                action: Action::Hold,
                at: now,
                transition: Transition::Release
            }),
        );
    }

    #[test]
    fn a_chord_of_modifiers_is_pressed_in_either_order_and_released_with_its_key() {
        let config = crate::config::HotkeyConfig {
            toggle_key: Some("Ctrl+KEY_RIGHTMETA".to_owned()),
            ..Default::default()
        };
        let bindings = bindings(&config).unwrap();
        let mut pressed = PressedKeys::default();
        let mut keys = DeviceKeys::default();
        let now = Instant::now();
        let toggle = |transition| {
            Some(KeyEvent {
                action: Action::Toggle,
                at: now,
                transition,
            })
        };
        let step = |keys: &mut DeviceKeys, pressed: &mut PressedKeys, key, value| {
            keys.transition(&bindings, pressed, key, value, now)
        };

        // The left Win key completes the chord that names the right one.
        step(&mut keys, &mut pressed, KeyCode::KEY_LEFTCTRL, 1);
        assert_eq!(
            step(&mut keys, &mut pressed, KeyCode::KEY_LEFTMETA, 1),
            toggle(Transition::Press { ctrl: true })
        );
        // Ctrl going up first keeps the chord pressed until its key goes up.
        assert_eq!(
            step(&mut keys, &mut pressed, KeyCode::KEY_LEFTCTRL, 0),
            None
        );
        assert_eq!(
            step(&mut keys, &mut pressed, KeyCode::KEY_LEFTMETA, 0),
            toggle(Transition::Release)
        );

        // Ctrl pressed after Win is the same chord, and pressing it again
        // while Win is held is not a second press.
        step(&mut keys, &mut pressed, KeyCode::KEY_RIGHTMETA, 1);
        assert_eq!(
            step(&mut keys, &mut pressed, KeyCode::KEY_RIGHTCTRL, 1),
            toggle(Transition::Press { ctrl: false })
        );
        step(&mut keys, &mut pressed, KeyCode::KEY_RIGHTCTRL, 0);
        assert_eq!(
            step(&mut keys, &mut pressed, KeyCode::KEY_RIGHTCTRL, 1),
            None
        );
        assert_eq!(
            step(&mut keys, &mut pressed, KeyCode::KEY_RIGHTMETA, 0),
            toggle(Transition::Release)
        );
        step(&mut keys, &mut pressed, KeyCode::KEY_RIGHTCTRL, 0);

        // Other modifiers are not the chord: Win alone, then Win with Shift.
        assert_eq!(
            step(&mut keys, &mut pressed, KeyCode::KEY_LEFTMETA, 1),
            None
        );
        assert_eq!(
            step(&mut keys, &mut pressed, KeyCode::KEY_LEFTSHIFT, 1),
            None
        );
        assert_eq!(
            step(&mut keys, &mut pressed, KeyCode::KEY_LEFTCTRL, 1),
            None
        );
    }

    #[test]
    fn spellings_of_one_chord_of_modifiers_are_one_binding() {
        let config = crate::config::HotkeyConfig {
            key: Some("Ctrl+KEY_RIGHTMETA".to_owned()),
            hold_key: Some("Super+KEY_LEFTCTRL".to_owned()),
            ..Default::default()
        };
        assert_eq!(
            bindings(&config).unwrap_err(),
            "hotkey.hold_key conflicts with hotkey.key on Super+KEY_LEFTCTRL"
        );
    }

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
