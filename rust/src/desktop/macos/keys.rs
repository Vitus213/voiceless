use crate::desktop::native_keys::Chord;
use crate::hotkey::{KeyEvent, Transition};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc as sync, Arc,
};
use std::time::Instant;
use tokio::sync::mpsc::{self, error::TrySendError};

extern "C" {
    fn vl_run_keys(ctx: *mut c_void);
}

// Carbon Events.h virtual key codes. Letter bindings refer to physical keys.
pub(crate) fn key_code(name: &str) -> Option<u16> {
    let key = name.strip_prefix("KEY_")?;
    const LETTERS: [u16; 26] = [
        0, 11, 8, 2, 14, 3, 5, 4, 34, 38, 40, 37, 46, 45, 31, 35, 12, 15, 1, 17, 32, 9, 13, 7, 16,
        6,
    ];
    const DIGITS: [u16; 10] = [29, 18, 19, 20, 21, 23, 22, 26, 28, 25];
    const FUNCTION: [u16; 20] = [
        122, 120, 99, 118, 96, 97, 98, 100, 101, 109, 103, 111, 105, 107, 113, 106, 64, 79, 80, 90,
    ];
    match key.as_bytes() {
        [letter @ b'A'..=b'Z'] => return Some(LETTERS[(letter - b'A') as usize]),
        [digit @ b'0'..=b'9'] => return Some(DIGITS[(digit - b'0') as usize]),
        [b'F', b'1'..=b'9', ..] => {
            return FUNCTION
                .get(key[1..].parse::<usize>().ok()?.checked_sub(1)?)
                .copied()
        }
        _ => {}
    }
    Some(match key {
        "LEFTMETA" => 55,
        "RIGHTMETA" => 54,
        "LEFTSHIFT" => 56,
        "RIGHTSHIFT" => 60,
        "LEFTALT" => 58,
        "RIGHTALT" => 61,
        "LEFTCTRL" => 59,
        "RIGHTCTRL" => 62,
        "FN" => 63,
        "CAPSLOCK" => 57,
        "NUMLOCK" => 71,
        "ESC" => 53,
        "SPACE" => 49,
        "ENTER" => 36,
        "TAB" => 48,
        "BACKSPACE" => 51,
        "DELETE" => 117,
        "LEFT" => 123,
        "RIGHT" => 124,
        "DOWN" => 125,
        "UP" => 126,
        "HOME" => 115,
        "END" => 119,
        "PAGEUP" => 116,
        "PAGEDOWN" => 121,
        "INSERT" => 114,
        // Extended PC keyboards label the Mac F13/F14/F15 positions this way.
        "SYSRQ" => 105,
        "SCROLLLOCK" => 107,
        "PAUSE" => 113,
        "COMMA" => 43,
        "DOT" => 47,
        "SLASH" => 44,
        "SEMICOLON" => 41,
        "APOSTROPHE" => 39,
        "BACKSLASH" => 42,
        "MINUS" => 27,
        "EQUAL" => 24,
        "LEFTBRACE" => 33,
        "RIGHTBRACE" => 30,
        "GRAVE" => 50,
        _ => return None,
    })
}

pub(crate) struct KeyListener {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for KeyListener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Keys {
    chords: Arc<[Chord]>,
    pressed: Vec<bool>,
    pending: VecDeque<KeyEvent>,
    send: mpsc::Sender<KeyEvent>,
    ready: sync::Sender<bool>,
    stop: Arc<AtomicBool>,
}

pub(crate) fn start_keys(
    chords: Arc<[Chord]>,
    send: mpsc::Sender<KeyEvent>,
) -> Result<KeyListener, String> {
    let stop = Arc::new(AtomicBool::new(false));
    if chords.is_empty() {
        return Ok(KeyListener { stop, thread: None });
    }
    if !unsafe { super::vl_listen_permission(false) } {
        return Err(
            "Allow Input Monitoring in System Settings > Privacy & Security, then reopen Voiceless"
                .into(),
        );
    }
    let (ready, installed) = sync::channel();
    let mut keys = Keys {
        pressed: vec![false; chords.len()],
        chords,
        pending: VecDeque::new(),
        send,
        ready,
        stop: stop.clone(),
    };
    let thread = std::thread::Builder::new()
        .name("voiceless-shortcuts".into())
        .spawn(move || {
            // The bridge calls the four callbacks synchronously on this thread only.
            unsafe { vl_run_keys((&mut keys as *mut Keys).cast()) };
        })
        .map_err(|e| e.to_string())?;
    let listener = KeyListener {
        stop,
        thread: Some(thread),
    };
    if installed.recv().unwrap_or(false) {
        Ok(listener)
    } else {
        Err("macOS could not install a keyboard event tap in the current login session".into())
    }
}

#[no_mangle]
unsafe extern "C" fn vl_keys_ready(keys: *mut Keys, installed: bool) {
    let _ = (*keys).ready.send(installed);
}

#[no_mangle]
unsafe extern "C" fn vl_key(keys: *mut Keys, key: u16, down: bool, modifiers: u8) {
    let keys = &mut *keys;
    let own = match key {
        59 | 62 => 1,
        58 | 61 => 2,
        56 | 60 => 4,
        54 | 55 => 8,
        _ => 0,
    };
    for (chord, pressed) in keys.chords.iter().zip(&mut keys.pressed) {
        if chord.key != key {
            continue;
        }
        if let Some(transition) = transition(pressed, down, modifiers & !own, chord.modifiers) {
            keys.pending.push_back(KeyEvent {
                action: chord.action,
                transition,
                at: Instant::now(),
            });
        }
    }
}

fn transition(
    pressed: &mut bool,
    down: bool,
    held: u8,
    required: Option<u8>,
) -> Option<Transition> {
    if *pressed == down {
        return None;
    }
    if down && !required.map_or(held <= 1, |mask| mask == held) {
        return None;
    }
    *pressed = down;
    Some(if down {
        Transition::Press {
            ctrl: held & 1 != 0,
        }
    } else {
        Transition::Release
    })
}

#[no_mangle]
unsafe extern "C" fn vl_key_reset(keys: *mut Keys) {
    let keys = &mut *keys;
    for (chord, pressed) in keys.chords.iter().zip(&mut keys.pressed) {
        if std::mem::take(pressed) {
            keys.pending.push_back(KeyEvent {
                action: chord.action,
                transition: Transition::Disconnected,
                at: Instant::now(),
            });
        }
    }
}

#[no_mangle]
unsafe extern "C" fn vl_keys_tick(keys: *mut Keys) -> bool {
    let keys = &mut *keys;
    while let Some(event) = keys.pending.pop_front() {
        match keys.send.try_send(event) {
            Ok(()) => {}
            Err(TrySendError::Full(event)) => {
                keys.pending.push_front(event);
                break;
            }
            Err(TrySendError::Closed(_)) => return false,
        }
    }
    !keys.stop.load(Ordering::Acquire) && !keys.send.is_closed()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_ctrl_gesture_ignores_repeat_and_releases_after_ctrl() {
        let mut pressed = false;
        assert_eq!(transition(&mut pressed, true, 2, None), None);
        assert_eq!(
            transition(&mut pressed, true, 1, None),
            Some(Transition::Press { ctrl: true })
        );
        assert_eq!(transition(&mut pressed, true, 0, None), None);
        assert_eq!(
            transition(&mut pressed, false, 0, None),
            Some(Transition::Release)
        );
    }

    #[test]
    fn extended_keyboard_aliases_collide_during_binding_validation() {
        assert_eq!(key_code("KEY_SCROLLLOCK"), key_code("KEY_F14"));
        assert_eq!(key_code("KEY_RIGHTALT"), Some(61));
        assert_eq!(key_code("KEY_F21"), None);
    }
}
