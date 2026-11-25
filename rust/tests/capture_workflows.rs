//! Recording and review workflows across capture, persistent jobs, and saved results.

#[cfg(target_os = "linux")]
#[path = "capture_workflows/annotation.rs"]
mod annotation;

#[cfg(target_os = "linux")]
#[path = "capture_workflows/meeting_capture.rs"]
mod meeting_capture;

#[cfg(any(target_os = "macos", windows))]
#[path = "capture_workflows/native_meeting.rs"]
mod native_meeting;

#[path = "capture_workflows/meeting_review.rs"]
mod meeting_review;

#[cfg(target_os = "linux")]
#[path = "capture_workflows/recorder.rs"]
mod recorder;

#[cfg(target_os = "linux")]
#[path = "common/session_bus.rs"]
mod session_bus;

#[cfg(target_os = "linux")]
fn script(path: std::path::PathBuf, text: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;

    // Forks in concurrent tests can inherit a writer and make exec fail with
    // ETXTBSY. Only this short-lived child opens the executable for writing.
    let status = std::process::Command::new("/bin/sh")
        .args(["-c", r#"printf '%s' "$1" > "$2""#, "write-fixture", text])
        .arg(&path)
        .status()
        .unwrap();
    assert!(status.success(), "could not write {}", path.display());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    path
}
