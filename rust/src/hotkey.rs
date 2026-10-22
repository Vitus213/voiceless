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
//! daemon reports "no readable keyboard" when nothing could be watched.

use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use evdev::{EventSummary, KeyCode};
use tokio::sync::mpsc;

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
/// releases. Returns one task per watched device (abort them to stop); none
/// usually means no read access to /dev/input (join the `input` group).
///
/// Devices are enumerated once: keyboards plugged in later are not seen
/// until the daemon reloads, and a device whose event stream fails (unplug)
/// ends its task quietly. A task also ends when `events` is closed. The Ctrl
/// state is one counter shared by all devices, so Ctrl on one keyboard
/// qualifies a press on another; it can never go below zero even if Ctrl
/// was already down when the listener started.
pub fn listen(key: KeyCode, events: mpsc::Sender<KeyEvent>) -> Vec<tokio::task::JoinHandle<()>> {
    let ctrl_down = Arc::new(AtomicUsize::new(0));
    let mut watched = Vec::new();
    for (_path, device) in evdev::enumerate() {
        // Only devices that report the key at all; this also skips mice,
        // touchpads and other non-keyboard nodes with no extra probing.
        let has_key = device
            .supported_keys()
            .is_some_and(|keys| keys.contains(key));
        if !has_key {
            continue;
        }
        let Ok(mut stream) = device.into_event_stream() else {
            continue;
        };
        let events = events.clone();
        let ctrl_down = ctrl_down.clone();
        watched.push(tokio::spawn(async move {
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
                            let _ = ctrl_down.fetch_update(
                                Ordering::Relaxed,
                                Ordering::Relaxed,
                                |held| held.checked_sub(1),
                            );
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
                // The daemon dropped its receiver (reload or shutdown).
                if events.send(forwarded).await.is_err() {
                    break;
                }
            }
        }));
    }
    watched
}
