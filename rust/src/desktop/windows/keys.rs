//! Key names and chord rules of the Windows desktop adapter. Nothing here
//! calls the system: the hook thread reads the keyboard and hands
//! [`ChordTracker`] what it saw.

use std::sync::Arc;
use std::time::Instant;

use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    VK_0, VK_1, VK_A, VK_ADD, VK_APPS, VK_BACK, VK_BROWSER_BACK, VK_BROWSER_FAVORITES,
    VK_BROWSER_FORWARD, VK_BROWSER_HOME, VK_BROWSER_REFRESH, VK_BROWSER_SEARCH, VK_BROWSER_STOP,
    VK_CANCEL, VK_CAPITAL, VK_CLEAR, VK_DECIMAL, VK_DELETE, VK_DIVIDE, VK_DOWN, VK_END, VK_ESCAPE,
    VK_F1, VK_HELP, VK_HOME, VK_INSERT, VK_LAUNCH_MAIL, VK_LAUNCH_MEDIA_SELECT, VK_LCONTROL,
    VK_LEFT, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_MEDIA_PLAY_PAUSE, VK_MEDIA_PREV_TRACK, VK_MEDIA_STOP,
    VK_MULTIPLY, VK_NEXT, VK_NUMLOCK, VK_NUMPAD0, VK_OEM_1, VK_OEM_102, VK_OEM_2, VK_OEM_3,
    VK_OEM_4, VK_OEM_6, VK_OEM_7, VK_OEM_COMMA, VK_OEM_MINUS, VK_OEM_PERIOD, VK_OEM_PLUS, VK_PAUSE,
    VK_PRINT, VK_PRIOR, VK_RCONTROL, VK_RETURN, VK_RIGHT, VK_RMENU, VK_RSHIFT, VK_RWIN, VK_SCROLL,
    VK_SELECT, VK_SLEEP, VK_SNAPSHOT, VK_SPACE, VK_SUBTRACT, VK_TAB, VK_UP, VK_VOLUME_DOWN,
    VK_VOLUME_MUTE, VK_VOLUME_UP,
};

use crate::desktop::native_keys::Chord;
use crate::hotkey::{KeyEvent, Transition};

/// The Ctrl bit of `Chord::modifiers`; the other three follow.
pub(super) const CTRL: u8 = 1;
pub(super) const ALT: u8 = 2;
pub(super) const SHIFT: u8 = 4;
pub(super) const SUPER: u8 = 8;

/// Keys with a name of their own, by evdev name without `KEY_`, in the
/// order Windows users meet them on a United States layout. Letters,
/// digits, `F1` to `F24` and `KP0` to `KP9` follow from their names.
const NAMED: &[(&str, u16)] = &[
    ("102ND", VK_OEM_102),
    ("APOSTROPHE", VK_OEM_7),
    ("BACK", VK_BROWSER_BACK),
    ("BACKSPACE", VK_BACK),
    ("BOOKMARKS", VK_BROWSER_FAVORITES),
    ("CANCEL", VK_CANCEL),
    ("CAPSLOCK", VK_CAPITAL),
    ("CLEAR", VK_CLEAR),
    ("COMMA", VK_OEM_COMMA),
    ("COMPOSE", VK_APPS),
    ("DELETE", VK_DELETE),
    ("DOT", VK_OEM_PERIOD),
    ("DOWN", VK_DOWN),
    ("END", VK_END),
    ("ENTER", VK_RETURN),
    ("EQUAL", VK_OEM_PLUS),
    ("ESC", VK_ESCAPE),
    ("FORWARD", VK_BROWSER_FORWARD),
    ("GRAVE", VK_OEM_3),
    ("HELP", VK_HELP),
    ("HOME", VK_HOME),
    ("HOMEPAGE", VK_BROWSER_HOME),
    ("INSERT", VK_INSERT),
    ("KPASTERISK", VK_MULTIPLY),
    ("KPDOT", VK_DECIMAL),
    ("KPENTER", VK_RETURN),
    ("KPMINUS", VK_SUBTRACT),
    ("KPPLUS", VK_ADD),
    ("KPSLASH", VK_DIVIDE),
    ("LEFT", VK_LEFT),
    ("LEFTALT", VK_LMENU),
    ("LEFTBRACE", VK_OEM_4),
    ("LEFTCTRL", VK_LCONTROL),
    ("LEFTMETA", VK_LWIN),
    ("LEFTSHIFT", VK_LSHIFT),
    ("MAIL", VK_LAUNCH_MAIL),
    ("MEDIA", VK_LAUNCH_MEDIA_SELECT),
    ("MENU", VK_APPS),
    ("MINUS", VK_OEM_MINUS),
    ("MUTE", VK_VOLUME_MUTE),
    ("NUMLOCK", VK_NUMLOCK),
    ("PAGEDOWN", VK_NEXT),
    ("PAGEUP", VK_PRIOR),
    ("PAUSE", VK_PAUSE),
    ("PREVIOUSSONG", VK_MEDIA_PREV_TRACK),
    ("PLAYPAUSE", VK_MEDIA_PLAY_PAUSE),
    ("PRINT", VK_PRINT),
    ("REFRESH", VK_BROWSER_REFRESH),
    ("RIGHT", VK_RIGHT),
    ("RIGHTALT", VK_RMENU),
    ("RIGHTBRACE", VK_OEM_6),
    ("RIGHTCTRL", VK_RCONTROL),
    ("RIGHTMETA", VK_RWIN),
    ("RIGHTSHIFT", VK_RSHIFT),
    ("SCROLLLOCK", VK_SCROLL),
    ("SEARCH", VK_BROWSER_SEARCH),
    ("SELECT", VK_SELECT),
    ("SEMICOLON", VK_OEM_1),
    ("SLEEP", VK_SLEEP),
    ("SLASH", VK_OEM_2),
    ("SPACE", VK_SPACE),
    ("STOP", VK_BROWSER_STOP),
    ("STOPCD", VK_MEDIA_STOP),
    ("SYSRQ", VK_SNAPSHOT),
    ("TAB", VK_TAB),
    ("UP", VK_UP),
    ("VOLUMEDOWN", VK_VOLUME_DOWN),
    ("VOLUMEUP", VK_VOLUME_UP),
];

/// The Windows virtual-key code of a key named the way the configuration
/// names it on every desktop: an evdev key name such as `KEY_SCROLLLOCK`,
/// `KEY_F6` or `KEY_A`. `None` for a key Windows has no code for.
pub(crate) fn key_code(name: &str) -> Option<u16> {
    let key = name.strip_prefix("KEY_")?;
    match key.as_bytes() {
        [letter @ b'A'..=b'Z'] => Some(VK_A + u16::from(letter - b'A')),
        [digit @ b'1'..=b'9'] => Some(VK_1 + u16::from(digit - b'1')),
        [b'0'] => Some(VK_0),
        [b'F', digit @ b'1'..=b'9'] => Some(VK_F1 + u16::from(digit - b'1')),
        [b'F', tens @ b'1'..=b'2', ones @ b'0'..=b'9'] => {
            let number = u16::from(tens - b'0') * 10 + u16::from(ones - b'0');
            (number <= 24).then(|| VK_F1 + number - 1)
        }
        [b'K', b'P', digit @ b'0'..=b'9'] => Some(VK_NUMPAD0 + u16::from(digit - b'0')),
        _ => NAMED
            .iter()
            .find(|(known, _)| *known == key)
            .map(|(_, code)| *code),
    }
}

/// The modifier bit a key *is*: Ctrl, Alt, Shift or Win, left or right.
pub(super) fn modifier_bit(key: u16) -> Option<u8> {
    Some(match key {
        VK_LCONTROL | VK_RCONTROL => CTRL,
        VK_LMENU | VK_RMENU => ALT,
        VK_LSHIFT | VK_RSHIFT => SHIFT,
        VK_LWIN | VK_RWIN => SUPER,
        _ => return None,
    })
}

/// Whether the keys that are down make `required`.
///
/// A chord with a mask needs exactly those modifiers. The legacy key carries
/// no mask and takes its key alone or with Ctrl, which the press reports;
/// with Alt, Shift or Win held it belongs to the system and to the focused
/// application, as it does for a chord of its own.
pub(super) fn matches(required: Option<u8>, held: u8) -> bool {
    match required {
        Some(mask) => mask == held,
        None => held == 0 || held == CTRL,
    }
}

/// Which chords are pressed, from what the hook saw.
pub(super) struct ChordTracker {
    chords: Arc<[Chord]>,
    /// Per chord: its key went down and has not come up since.
    pressed: Vec<bool>,
    /// What the last [`ChordTracker::on_key`] produced, drained by the hook.
    pub(super) events: Vec<KeyEvent>,
}

impl ChordTracker {
    pub(super) fn new(chords: Arc<[Chord]>) -> Self {
        Self {
            pressed: vec![false; chords.len()],
            chords,
            events: Vec::new(),
        }
    }

    /// Whether a chord sits on `key`; every other key is none of ours.
    pub(super) fn watches(&self, key: u16) -> bool {
        self.chords.iter().any(|chord| chord.key == key)
    }

    /// Folds one transition of `key` into the state. `held` are the
    /// modifiers down at that moment and `is_down` answers for every other
    /// key: the hook sees each key before Windows updates its own state, so
    /// the key of the event itself is never second-guessed.
    pub(super) fn on_key(
        &mut self,
        key: u16,
        down: bool,
        held: u8,
        at: Instant,
        is_down: impl Fn(u16) -> bool,
    ) {
        self.events.clear();
        self.forget_released(key, at, &is_down);
        if down {
            self.press(key, held, at);
        } else {
            self.release(key, at);
        }
    }

    pub(super) fn disconnect(&mut self, at: Instant) {
        self.events.clear();
        self.forget_released(0, at, &|_| false);
    }

    /// Ends chords whose key is up although no release was seen: the lock
    /// screen or a UAC prompt can take the desktop between a press and its
    /// release. The daemon cancels what a held key recorded, as it does when
    /// a keyboard is unplugged.
    fn forget_released(&mut self, key: u16, at: Instant, is_down: &impl Fn(u16) -> bool) {
        for (index, chord) in self.chords.iter().enumerate() {
            if self.pressed[index] && chord.key != key && !is_down(chord.key) {
                self.pressed[index] = false;
                self.events.push(KeyEvent {
                    action: chord.action,
                    transition: Transition::Disconnected,
                    at,
                });
            }
        }
    }

    fn press(&mut self, key: u16, held: u8, at: Instant) {
        // Windows repeats a held key as fresh presses; one press is one gesture.
        if self
            .chords
            .iter()
            .zip(&self.pressed)
            .any(|(chord, pressed)| chord.key == key && *pressed)
        {
            return;
        }
        let Some(index) = self
            .chords
            .iter()
            .position(|chord| chord.key == key && matches(chord.modifiers, held))
        else {
            return;
        };
        self.pressed[index] = true;
        self.events.push(KeyEvent {
            action: self.chords[index].action,
            transition: Transition::Press {
                ctrl: held & CTRL != 0,
            },
            at,
        });
    }

    fn release(&mut self, key: u16, at: Instant) {
        let Some(index) = self
            .chords
            .iter()
            .zip(&self.pressed)
            .position(|(chord, pressed)| chord.key == key && *pressed)
        else {
            return;
        };
        self.pressed[index] = false;
        self.events.push(KeyEvent {
            action: self.chords[index].action,
            transition: Transition::Release,
            at,
        });
    }
}

/// `text` as `CF_UNICODETEXT`: UTF-16, every line ended by CRLF, and a NUL
/// only at the end, since one inside the text would end it there.
pub(super) fn clipboard_text(text: &str) -> Vec<u16> {
    const CRLF: [u16; 2] = [0x0D, 0x0A];
    let mut units = Vec::with_capacity(text.len() + 1);
    // Set after a CR: the LF that may follow ends the same line.
    let mut after_cr = false;
    for character in text.chars() {
        match character {
            '\0' => {}
            '\r' => {
                units.extend_from_slice(&CRLF);
                after_cr = true;
            }
            '\n' => {
                if after_cr {
                    after_cr = false;
                } else {
                    units.extend_from_slice(&CRLF);
                }
            }
            _ => {
                after_cr = false;
                let mut pair = [0u16; 2];
                units.extend_from_slice(character.encode_utf16(&mut pair));
            }
        }
    }
    units.push(0);
    units
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hotkey::Action;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{VK_9, VK_F24, VK_F6, VK_F7};

    fn chord(action: Action, key: u16, modifiers: Option<u8>) -> Chord {
        Chord {
            action,
            key,
            modifiers,
        }
    }

    fn tracker(chords: Vec<Chord>) -> ChordTracker {
        ChordTracker::new(chords.into())
    }

    /// Every other key is still down: nothing is second-guessed.
    fn held(_: u16) -> bool {
        true
    }

    #[test]
    fn key_names_are_evdev_names_answered_with_virtual_keys() {
        assert_eq!(key_code("KEY_SCROLLLOCK"), Some(VK_SCROLL));
        assert_eq!(key_code("KEY_CAPSLOCK"), Some(VK_CAPITAL));
        assert_eq!(key_code("KEY_F6"), Some(VK_F6));
        assert_eq!(key_code("KEY_F24"), Some(VK_F24));
        assert_eq!(key_code("KEY_A"), Some(VK_A));
        assert_eq!(key_code("KEY_0"), Some(VK_0));
        assert_eq!(key_code("KEY_9"), Some(VK_9));
        assert_eq!(key_code("KEY_KP1"), Some(VK_NUMPAD0 + 1));
        assert_eq!(key_code("KEY_LEFTMETA"), Some(VK_LWIN));
        assert_eq!(key_code("KEY_SYSRQ"), Some(VK_SNAPSHOT));
        // Linux-only keys, other spellings and other prefixes have no Windows code.
        for name in [
            "KEY_FOOBAR",
            "KEY_F25",
            "KEY_F06",
            "KEY_MICMUTE",
            "KEY_HANGEUL",
            "key_scrolllock",
            "SCROLLLOCK",
            "",
        ] {
            assert_eq!(key_code(name), None, "{name}");
        }
    }

    #[test]
    fn a_held_chord_presses_once_and_releases_with_its_key() {
        let mut tracker = tracker(vec![chord(Action::Hold, VK_F6, Some(CTRL))]);
        let at = Instant::now();
        let press = KeyEvent {
            action: Action::Hold,
            transition: Transition::Press { ctrl: true },
            at,
        };
        tracker.on_key(VK_F6, true, CTRL, at, held);
        assert_eq!(tracker.events, vec![press]);
        for _ in 0..3 {
            tracker.on_key(VK_F6, true, CTRL, at, held);
            assert!(tracker.events.is_empty(), "a repeated press");
        }
        // Modifiers let go before the key: still the same press, so the same release.
        tracker.on_key(VK_F6, true, 0, at, held);
        assert!(tracker.events.is_empty());
        tracker.on_key(VK_F6, false, 0, at, held);
        assert_eq!(
            tracker.events,
            vec![KeyEvent {
                action: Action::Hold,
                transition: Transition::Release,
                at,
            }]
        );
        // A release without a press, as after the listener started under a held key.
        tracker.on_key(VK_F6, false, 0, at, held);
        assert!(tracker.events.is_empty());
    }

    #[test]
    fn chords_need_their_modifiers_and_the_legacy_key_takes_ctrl_or_nothing() {
        let mut tracker = tracker(vec![
            chord(Action::Legacy, VK_SCROLL, None),
            chord(Action::Toggle, VK_F7, Some(CTRL | SHIFT)),
            chord(Action::Cancel, VK_F7, Some(CTRL)),
        ]);
        let at = Instant::now();
        for mask in [0, CTRL] {
            tracker.on_key(VK_SCROLL, true, mask, at, held);
            assert_eq!(
                tracker.events[0].transition,
                Transition::Press { ctrl: mask == CTRL }
            );
            tracker.on_key(VK_SCROLL, true, mask, at, held);
            assert!(tracker.events.is_empty());
            tracker.on_key(VK_SCROLL, false, mask, at, held);
            assert_eq!(tracker.events[0].transition, Transition::Release);
        }
        for modifiers in [ALT, SHIFT, SUPER, CTRL | SHIFT] {
            tracker.on_key(VK_SCROLL, true, modifiers, at, held);
            assert!(tracker.events.is_empty(), "with {modifiers} held");
        }

        // Two chords on one key: the modifiers choose, and the release finds the one pressed.
        let mut gestures = Vec::new();
        for (down, mask) in [
            (true, CTRL | SHIFT),
            (false, CTRL),
            (true, CTRL),
            (false, 0),
            (true, CTRL | ALT),
        ] {
            tracker.on_key(VK_F7, down, mask, at, held);
            gestures.extend(
                tracker
                    .events
                    .iter()
                    .map(|event| (event.action, event.transition)),
            );
        }
        assert_eq!(
            gestures,
            vec![
                (Action::Toggle, Transition::Press { ctrl: true }),
                (Action::Toggle, Transition::Release),
                (Action::Cancel, Transition::Press { ctrl: true }),
                (Action::Cancel, Transition::Release),
            ]
        );
    }

    #[test]
    fn a_release_the_hook_never_saw_ends_the_chord_as_disconnected() {
        let mut tracker = tracker(vec![
            chord(Action::Hold, VK_F6, Some(CTRL)),
            chord(Action::Toggle, VK_F7, Some(CTRL)),
        ]);
        let at = Instant::now();
        tracker.on_key(VK_F6, true, CTRL, at, held);
        // Ctrl went down, F6 came up on another desktop, Ctrl stayed: the next chord key tells.
        tracker.on_key(VK_F7, true, CTRL, at, |key| key != VK_F6);
        let gestures: Vec<(Action, Transition)> = tracker
            .events
            .iter()
            .map(|event| (event.action, event.transition))
            .collect();
        assert_eq!(
            gestures,
            vec![
                (Action::Hold, Transition::Disconnected),
                (Action::Toggle, Transition::Press { ctrl: true }),
            ]
        );
        // The key of the event is read from the event alone, even if reported up.
        tracker.on_key(VK_F7, true, CTRL, at, |_| false);
        assert!(tracker.events.is_empty(), "a repeated press");
        // Nothing is left pressed, so later releases are quiet.
        tracker.on_key(VK_F7, false, 0, at, held);
        assert_eq!(tracker.events.len(), 1);
    }

    #[test]
    fn keys_without_a_chord_change_nothing() {
        let mut tracker = tracker(vec![chord(Action::Hold, VK_F6, Some(CTRL))]);
        assert!(tracker.watches(VK_F6));
        assert!(!tracker.watches(VK_F7));
        let at = Instant::now();
        tracker.on_key(VK_F7, true, CTRL, at, held);
        tracker.on_key(VK_F7, false, 0, at, held);
        assert!(tracker.events.is_empty());
    }

    #[test]
    fn modifier_keys_are_their_own_bit_and_nothing_else_is() {
        assert_eq!(modifier_bit(VK_LCONTROL), Some(CTRL));
        assert_eq!(modifier_bit(VK_RCONTROL), Some(CTRL));
        assert_eq!(modifier_bit(VK_LMENU), Some(ALT));
        assert_eq!(modifier_bit(VK_RMENU), Some(ALT));
        assert_eq!(modifier_bit(VK_LSHIFT), Some(SHIFT));
        assert_eq!(modifier_bit(VK_RSHIFT), Some(SHIFT));
        assert_eq!(modifier_bit(VK_LWIN), Some(SUPER));
        assert_eq!(modifier_bit(VK_RWIN), Some(SUPER));
        assert_eq!(modifier_bit(VK_F6), None);
    }

    #[test]
    fn clipboard_text_ends_every_line_with_crlf_and_ends_with_one_nul() {
        let crlf = [0x0D, 0x0A];
        let mut expected = String::from("a").encode_utf16().collect::<Vec<_>>();
        expected.extend_from_slice(&crlf);
        expected.extend_from_slice(&String::from("b").encode_utf16().collect::<Vec<_>>());
        expected.extend_from_slice(&crlf);
        expected.extend_from_slice(&String::from("c").encode_utf16().collect::<Vec<_>>());
        expected.extend_from_slice(&crlf);
        expected.extend_from_slice(&String::from("d").encode_utf16().collect::<Vec<_>>());
        expected.extend_from_slice(&crlf);
        expected.extend_from_slice(&crlf);
        expected.extend_from_slice(&String::from("e").encode_utf16().collect::<Vec<_>>());
        expected.push(0);
        // LF, CRLF, CR and a CR before CRLF each end one line; a NUL inside would end the text.
        assert_eq!(clipboard_text("a\nb\r\nc\rd\r\r\n\0e"), expected);
        // Text outside the Basic Multilingual Plane keeps its surrogate pair.
        assert_eq!(clipboard_text("é 😀"), [0xE9, 0x20, 0xD83D, 0xDE00, 0]);
        assert_eq!(clipboard_text(""), vec![0]);
    }
}
