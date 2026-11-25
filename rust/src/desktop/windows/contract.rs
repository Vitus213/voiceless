use super::*;
use crate::hotkey::{Action, Transition};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, MOD_CONTROL, MOD_SHIFT, VK_F22, VK_F23,
};

fn target(key: &str, process: u32) -> Target {
    Target {
        thread: 1,
        process,
        key: key.into(),
        content: None,
        app_id: None,
        title: None,
    }
}

#[test]
fn windows_focus_change_inside_one_frame_refuses_delivery() {
    let original = "17:24/18:24";
    assert!(confirm_observed_focus(original, Some(target(original, 24))).is_ok());
    assert!(confirm_observed_focus(original, Some(target("17:24/19:24", 24))).is_err());
    assert!(confirm_observed_focus(original, None).is_err());
}

#[test]
fn windows_denies_input_when_the_focused_child_token_cannot_be_read() {
    let mut frame = target("frame", std::process::id());
    assert!(refusal_for(&frame).is_none());
    frame.content = Some(0); // OpenProcess(0) always fails, including when elevated.
    assert!(refusal_for(&frame).unwrap().contains("integrity level"));
}

#[test]
fn windows_keyboard_removal_cancels_hold_before_reconnect() {
    let mut tracker = keys::ChordTracker::new(
        vec![Chord {
            action: Action::Hold,
            key: VK_F22,
            modifiers: Some(CTRL),
        }]
        .into(),
    );
    let at = Instant::now();
    tracker.on_key(VK_F22, true, CTRL, at, |_| true);
    tracker.disconnect(at);
    assert_eq!(
        tracker.events,
        [KeyEvent {
            action: Action::Hold,
            transition: Transition::Disconnected,
            at
        }]
    );
    tracker.on_key(VK_F22, false, 0, at, |_| false);
    assert!(tracker.events.is_empty());
    tracker.on_key(VK_F22, true, CTRL, at, |_| true);
    assert_eq!(
        tracker.events[0].transition,
        Transition::Press { ctrl: true }
    );
}

#[test]
fn windows_conflicting_replacement_preserves_the_live_registration() {
    let chord = |key| Chord {
        action: Action::Hold,
        key,
        modifiers: Some(CTRL | SHIFT),
    };
    let live = shortcuts::reserve(&[chord(VK_F22)]).unwrap();
    let window = std::ptr::null_mut();
    unsafe {
        assert_ne!(
            RegisterHotKey(window, 42, MOD_CONTROL | MOD_SHIFT, VK_F23.into()),
            0
        );
        let replacement = shortcuts::reserve(&[chord(VK_F22), chord(VK_F23)]);
        UnregisterHotKey(window, 42);
        assert!(replacement.err().unwrap().contains("hotkey.hold_key"));
        assert_eq!(
            RegisterHotKey(window, 43, MOD_CONTROL | MOD_SHIFT, VK_F22.into()),
            0
        );
    }
    drop(live);
}

#[tokio::test]
async fn windows_removed_wasapi_endpoint_never_falls_back_to_default() {
    let missing =
        cpal::DeviceId(cpal::HostId::Wasapi, "{voiceless-removed-endpoint}".into()).to_string();
    let error = crate::desktop::audio::require_source(&missing)
        .await
        .unwrap_err();
    assert!(
        error.contains(&missing) && error.contains("reconnect"),
        "{error}"
    );
}
