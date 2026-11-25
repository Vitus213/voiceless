//! Audio sources, focus, overlays and lifecycle at the desktop boundary.

#[cfg(target_os = "linux")]
pub(crate) mod audio;
#[cfg(target_os = "macos")]
#[path = "desktop/macos/audio.rs"]
pub(crate) mod audio;
#[cfg(target_os = "windows")]
#[path = "desktop/windows/audio.rs"]
pub(crate) mod audio;
pub mod focus;
pub mod lifecycle;
#[cfg(target_os = "macos")]
#[path = "desktop/macos.rs"]
pub(crate) mod native;
#[cfg(target_os = "windows")]
#[path = "desktop/windows.rs"]
pub(crate) mod native;
#[cfg(not(target_os = "linux"))]
pub(crate) mod native_keys;
#[cfg(not(target_os = "linux"))]
pub(crate) mod native_runtime;
pub(crate) mod overlay;
pub mod runtime;
