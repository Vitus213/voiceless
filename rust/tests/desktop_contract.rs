//! Native desktop contracts run on the platform providing the corresponding APIs.

#[cfg(target_os = "linux")]
#[path = "desktop_contract/linux.rs"]
mod linux;

#[cfg(target_os = "linux")]
#[path = "common/session_bus.rs"]
mod session_bus;

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[path = "desktop_contract/macos.rs"]
mod macos;

#[cfg(target_os = "windows")]
#[path = "desktop_contract/windows.rs"]
mod windows;
