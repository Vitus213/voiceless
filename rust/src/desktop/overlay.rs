//! Overlay capability of the desktop session: whether the compositor can
//! host the eww pill (`contrib/eww-overlay`).
//!
//! The pill is a `zwlr_layer_shell_v1` surface, so it shows only where the
//! compositor advertises that protocol. [`overlay_support`] asks the
//! compositor through the standard `wayland-info` CLI, found on `PATH`
//! like the tools `doctor` checks, instead of linking a Wayland client:
//! the probe lists globals and creates no surface, so it cannot take
//! focus. Nothing here opens a window; that stays with the watcher, which
//! opens the pill only for a session whose `state` event does not say
//! `overlay: false`.

#[cfg(target_os = "linux")]
use std::io;
#[cfg(target_os = "linux")]
use std::process::Output;
#[cfg(target_os = "linux")]
use std::time::Duration;

#[cfg(target_os = "linux")]
use tokio::process::Command;

/// Wayland global the overlay window binds (`defwindow` in
/// `contrib/eww-overlay/eww.yuck`).
#[cfg(target_os = "linux")]
const LAYER_SHELL: &str = "zwlr_layer_shell_v1";
/// CLI that lists the compositor's globals (`wayland-utils` in Nixpkgs).
#[cfg(target_os = "linux")]
const WAYLAND_INFO: &str = "wayland-info";
/// A healthy compositor answers the registry round trip in milliseconds.
#[cfg(target_os = "linux")]
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Whether the session compositor can show the overlay.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct OverlaySupport {
    /// The compositor advertises `zwlr_layer_shell_v1`.
    pub available: bool,
    /// Why the overlay is unavailable, `None` when it is available. The
    /// causes read differently: `wayland-info` is **absent** from `PATH`,
    /// the compositor is **unsupported** (no layer shell), or the answer is
    /// **unavailable** (no Wayland display, the probe failed or timed out).
    pub reason: Option<String>,
}

/// Probe the compositor for the layer-shell protocol.
///
/// Runs `wayland-info` from `PATH` under [`PROBE_TIMEOUT`] and reads the
/// globals it lists. The probe never fails: one that cannot be completed
/// reports `available: false` with the cause. It spawns a process, and the
/// answer holds for the whole login session, so a hot path should cache it.
#[cfg(target_os = "linux")]
pub(crate) async fn overlay_support() -> OverlaySupport {
    let probe = Command::new(WAYLAND_INFO).kill_on_drop(true).output();
    match tokio::time::timeout(PROBE_TIMEOUT, probe).await {
        Ok(Ok(output)) if !output.status.success() => unavailable(format!(
            "overlay support is unavailable: {WAYLAND_INFO} failed: {}",
            failure(&output)
        )),
        Ok(Ok(output)) if advertises_layer_shell(&output.stdout) => OverlaySupport {
            available: true,
            reason: None,
        },
        Ok(Ok(_)) => unavailable(format!(
            "this Wayland compositor is unsupported: it does not advertise {LAYER_SHELL}"
        )),
        Ok(Err(error)) if error.kind() == io::ErrorKind::NotFound => unavailable(format!(
            "{WAYLAND_INFO} is absent from PATH: install wayland-utils to detect overlay support"
        )),
        Ok(Err(error)) => unavailable(format!(
            "overlay support is unavailable: cannot run {WAYLAND_INFO}: {error}"
        )),
        Err(_) => unavailable(format!(
            "overlay support is unavailable: {WAYLAND_INFO} timed out after {PROBE_TIMEOUT:?}"
        )),
    }
}

/// The recording status is available in the desktop window.
#[cfg(not(target_os = "linux"))]
pub(crate) async fn overlay_support() -> OverlaySupport {
    unavailable("The Wayland recording overlay is unavailable on this desktop.".to_owned())
}

fn unavailable(reason: String) -> OverlaySupport {
    OverlaySupport {
        available: false,
        reason: Some(reason),
    }
}

/// `wayland-info`'s own complaint, e.g. `failed to create display: No such
/// file or directory`; its exit status when stderr is empty.
#[cfg(target_os = "linux")]
fn failure(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    match stderr.trim().lines().next() {
        Some(line) => line.to_owned(),
        None => output.status.to_string(),
    }
}

/// Whether `wayland-info` output lists the layer-shell global.
#[cfg(target_os = "linux")]
fn advertises_layer_shell(output: &[u8]) -> bool {
    String::from_utf8_lossy(output)
        .lines()
        .filter_map(interface_name)
        .any(|name| name == LAYER_SHELL)
}

/// Name on one `interface: 'zwlr_layer_shell_v1', version: 5, name: 6`
/// line; `None` for the indented detail lines.
#[cfg(target_os = "linux")]
fn interface_name(line: &str) -> Option<&str> {
    let rest = line.trim_start().strip_prefix("interface:")?.trim_start();
    let rest = rest.trim_start_matches(['\'', '"']);
    let end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    Some(&rest[..end])
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    /// Globals as wayland-utils 1.3.0 prints them on Niri: padded columns,
    /// `version:`/`name:` with two spaces, then indented detail lines.
    const NIRI: &str = "interface: 'wl_compositor',        version:  6, name:  1\n\
                        interface: 'zwlr_layer_shell_v1',  version:  5, name:  6\n\
                        \tformats (fourcc):\n\
                        \t         1 = 'XR24'\n";

    /// The same listing on a compositor without the layer-shell global.
    const NO_LAYER_SHELL: &str = "interface: 'wl_compositor',        version:  6, name:  1\n\
                                  \tformats (fourcc):\n";

    #[test]
    fn layer_shell_global_means_supported() {
        assert!(advertises_layer_shell(NIRI.as_bytes()));
        assert_eq!(
            interface_name(NIRI.lines().nth(1).unwrap()),
            Some("zwlr_layer_shell_v1")
        );
    }

    #[test]
    fn anything_else_is_not_supported() {
        let other_revision = "interface: 'zwlr_layer_shell_v2', version: 1, name: 4";
        assert!(!advertises_layer_shell(NO_LAYER_SHELL.as_bytes()));
        assert!(!advertises_layer_shell(b""));
        assert!(!advertises_layer_shell(other_revision.as_bytes()));
        // Named outside an interface line only.
        assert!(!advertises_layer_shell(b"\tpath: zwlr_layer_shell_v1\n"));
    }
}
