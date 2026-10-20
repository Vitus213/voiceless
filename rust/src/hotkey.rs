//! Push-to-talk key read straight from evdev (`/dev/input`, needs the
//! `input` group), so it works on compositors without key-release bindings
//! (niri, KDE). The listener reports presses and releases; the daemon turns
//! them into hold-to-talk or tap-to-toggle (see `daemon::HotkeyMachine`).
//! Events are only read, never grabbed: the key still reaches applications,
//! so pick one nothing else uses (ScrollLock, Pause, a spare F-key).

use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use evdev::{EventSummary, KeyCode};
use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyEvent {
    /// The push-to-talk key went down; `ctrl` if a Ctrl key was held.
    Press {
        ctrl: bool,
    },
    Release,
}

/// Parse an evdev key name such as `KEY_SCROLLLOCK`.
pub fn parse_key(name: &str) -> Result<KeyCode, String> {
    KeyCode::from_str(name).map_err(|_| format!("unknown evdev key name: {name}"))
}

/// Watch every keyboard that has `key` and forward its presses and
/// releases. Returns how many devices are watched; 0 usually means no read
/// access to /dev/input (join the `input` group).
pub fn listen(key: KeyCode, events: mpsc::Sender<KeyEvent>) -> usize {
    let ctrl_down = Arc::new(AtomicUsize::new(0));
    let mut watched = 0;
    for (_path, device) in evdev::enumerate() {
        let has_key = device
            .supported_keys()
            .is_some_and(|keys| keys.contains(key));
        if !has_key {
            continue;
        }
        let Ok(mut stream) = device.into_event_stream() else {
            continue;
        };
        watched += 1;
        let events = events.clone();
        let ctrl_down = ctrl_down.clone();
        tokio::spawn(async move {
            while let Ok(event) = stream.next_event().await {
                let EventSummary::Key(_, code, value) = event.destructure() else {
                    continue;
                };
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
                if events.send(forwarded).await.is_err() {
                    break;
                }
            }
        });
    }
    watched
}
