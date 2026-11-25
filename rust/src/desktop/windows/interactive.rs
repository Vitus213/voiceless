//! Run explicitly on an unlocked Windows test desktop with a disposable clipboard.
use super::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    SetForegroundWindow, ShowWindow, TranslateMessage, ES_MULTILINE, WS_OVERLAPPEDWINDOW,
};

#[test]
#[ignore = "needs foreground activation and a disposable Windows clipboard"]
fn windows_delivers_unicode_to_the_original_editor_only() {
    let create = || unsafe {
        let hwnd = CreateWindowExW(
            0,
            w!("EDIT"),
            w!(""),
            WS_OVERLAPPEDWINDOW | ES_MULTILINE as u32,
            80,
            80,
            360,
            160,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null(),
        );
        assert!(!hwnd.is_null());
        ClipboardOwner(hwnd)
    };
    let focus = |window: HWND| unsafe {
        ShowWindow(window, SW_SHOWNORMAL);
        assert_ne!(
            SetForegroundWindow(window),
            0,
            "test process needs foreground activation"
        );
        SetFocus(window);
    };
    let original = create();
    let other = create();
    focus(original.0);
    let key = focused_window().unwrap().key;
    focus(other.0);
    assert!(paste_text("must never arrive", &key).is_err());
    assert!(unsafe { window_title(other.0) }.is_none());
    focus(original.0);
    let text = "Voiceless 中文 Ω";
    let delivery = std::thread::spawn(move || paste_text(text, &key));
    let deadline = Instant::now() + Duration::from_secs(8);
    while !delivery.is_finished() {
        assert!(
            Instant::now() < deadline,
            "timed out delivering to the editor"
        );
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    delivery.join().unwrap().unwrap();
    assert_eq!(unsafe { window_title(original.0) }.as_deref(), Some(text));
    assert!(unsafe { window_title(other.0) }.is_none());
}
