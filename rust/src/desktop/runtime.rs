//! Connect a desktop host to its resident runtime and start it when needed.
//! The host opens the runtime's console and never holds a session or model.
//!
//! A runtime that does not run is started detached (`voiceless daemon`,
//! beside the host or on `PATH`) and then awaited through the local control
//! transport. Failures are reported to the desktop host.

use std::path::{Path, PathBuf};
use std::time::Duration;

#[cfg(target_os = "linux")]
use crate::daemon::{BUS_NAME, INTERFACE, OBJECT_PATH};

#[cfg(target_os = "linux")]
pub type RuntimeClient = zbus::Connection;
#[cfg(not(target_os = "linux"))]
pub use super::native_runtime::{
    console_url, paused, repaste, runtime_state, set_paused, RuntimeClient,
};

/// Connect to the platform's local runtime transport.
pub async fn connect() -> Result<RuntimeClient, RuntimeError> {
    #[cfg(target_os = "linux")]
    return RuntimeClient::session()
        .await
        .map_err(|error| RuntimeError::Bus(error.to_string()));
    #[cfg(not(target_os = "linux"))]
    Ok(RuntimeClient::local())
}

/// The names a tray host registers on the session bus.
#[cfg(target_os = "linux")]
pub const TRAY_WATCHERS: &[&str] = &[
    "org.kde.StatusNotifierWatcher",
    "org.freedesktop.StatusNotifierWatcher",
];

/// How long a runtime that may be starting (the service and the host are
/// started together at login) is given to take its data directory before
/// the host starts one.
pub const RUNTIME_GRACE: Duration = Duration::from_secs(1);
/// How long a runtime that was just started may take to appear: a first
/// start loads the local model before it takes its name.
pub const RUNTIME_START_TIMEOUT: Duration = Duration::from_secs(60);

/// Why the host could not reach a running runtime.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RuntimeError {
    /// The daemon was started, or already starting, but took no bus name in time.
    #[error("the runtime did not start within {0:?}")]
    Timeout(Duration),
    /// The session bus refused the question.
    #[error("the session bus: {0}")]
    Bus(String),
    /// A native control request failed.
    #[error("the local runtime: {0}")]
    Local(String),
    /// `voiceless daemon` could not be started.
    #[error("cannot start the runtime: {0}")]
    Spawn(String),
    /// The runtime predates the desktop app: it answers the bus but has
    /// none of the methods the host asks for.
    #[error("the running voiceless daemon predates the desktop app; update voiceless and restart the daemon")]
    TooOld,
}

#[cfg(target_os = "linux")]
fn not_running(error: &zbus::Error) -> bool {
    matches!(error, zbus::Error::MethodError(name, _, _) if name.as_str() == "org.freedesktop.DBus.Error.ServiceUnknown")
}

#[cfg(target_os = "linux")]
fn unknown_method(error: &zbus::Error) -> bool {
    matches!(error, zbus::Error::MethodError(name, _, _) if name.as_str() == "org.freedesktop.DBus.Error.UnknownMethod")
}

/// Call `method` on the daemon and keep its string reply; `None` when no
/// runtime owns the bus name.
#[cfg(target_os = "linux")]
async fn ask(connection: &zbus::Connection, method: &str) -> Result<Option<String>, RuntimeError> {
    match connection
        .call_method(Some(BUS_NAME), OBJECT_PATH, Some(INTERFACE), method, &())
        .await
    {
        Ok(reply) => Ok(Some(
            reply
                .body()
                .deserialize::<String>()
                .map_err(|error| RuntimeError::Bus(error.to_string()))?,
        )),
        Err(error) if not_running(&error) => Ok(None),
        Err(error) if unknown_method(&error) => Err(RuntimeError::TooOld),
        Err(error) => Err(RuntimeError::Bus(error.to_string())),
    }
}

/// The console's loopback URL (`http://127.0.0.1:PORT/`); `None` when no
/// runtime is running, `Some("")` when it serves no console.
#[cfg(target_os = "linux")]
pub async fn console_url(connection: &zbus::Connection) -> Result<Option<String>, RuntimeError> {
    ask(connection, "ConsoleUrl").await
}

/// The runtime's last published state (`idle`, `recording` or `stopping`);
/// `None` when no runtime is running.
#[cfg(target_os = "linux")]
pub async fn runtime_state(connection: &zbus::Connection) -> Result<Option<String>, RuntimeError> {
    ask(connection, "Status").await
}

/// Whether dictation is suspended; `None` when no runtime is running.
#[cfg(target_os = "linux")]
pub async fn paused(connection: &zbus::Connection) -> Result<Option<bool>, RuntimeError> {
    match connection
        .call_method(Some(BUS_NAME), OBJECT_PATH, Some(INTERFACE), "Paused", &())
        .await
    {
        Ok(reply) => Ok(Some(
            reply
                .body()
                .deserialize::<bool>()
                .map_err(|error| RuntimeError::Bus(error.to_string()))?,
        )),
        Err(error) if not_running(&error) => Ok(None),
        Err(error) if unknown_method(&error) => Err(RuntimeError::TooOld),
        Err(error) => Err(RuntimeError::Bus(error.to_string())),
    }
}

/// Whether a tray host is registered on the session bus.
#[cfg(target_os = "linux")]
pub async fn tray_available(connection: &zbus::Connection) -> bool {
    let Ok(bus) = zbus::fdo::DBusProxy::new(connection).await else {
        return false;
    };
    for watcher in TRAY_WATCHERS {
        let Ok(name) = zbus::names::BusName::try_from(*watcher) else {
            continue;
        };
        if bus.name_has_owner(name).await.unwrap_or(false) {
            return true;
        }
    }
    false
}

/// Native desktop applications can use the OS menu bar or notification area.
#[cfg(not(target_os = "linux"))]
pub async fn tray_available(_connection: &RuntimeClient) -> bool {
    true
}

/// The `voiceless` binary the host starts: `VOICELESS_DAEMON` when set
/// (tests), else the one beside the host, else `voiceless` on `PATH`.
pub fn daemon_executable() -> PathBuf {
    if let Some(path) = std::env::var_os("VOICELESS_DAEMON") {
        return PathBuf::from(path);
    }
    let name = if cfg!(windows) {
        "voiceless.exe"
    } else {
        "voiceless"
    };
    std::env::current_exe()
        .ok()
        .and_then(|host| host.parent().map(|directory| directory.join(name)))
        .filter(|beside| beside.is_file())
        .unwrap_or_else(|| PathBuf::from(name))
}

/// Start the runtime detached, in its own process group with no streams:
/// it outlives the host and logs to the journal of whoever owns it. The
/// process id is returned for tests; the host never reaps it.
pub fn start_runtime(daemon: Option<&Path>) -> Result<u32, RuntimeError> {
    let daemon = daemon
        .map(Path::to_path_buf)
        .unwrap_or_else(daemon_executable);
    let mut command = std::process::Command::new(&daemon);
    command
        .arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS};
        command.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
    }
    let child = command
        .spawn()
        .map_err(|error| RuntimeError::Spawn(format!("{}: {error}", daemon.display())))?;
    let pid = child.id();
    // Not waited on: the runtime is not the host's to reap.
    std::mem::forget(child);
    Ok(pid)
}

/// Whether a runtime process holds `data_dir`. It does from its first
/// moments, before its engine is loaded and it takes its bus name.
fn runtime_process_exists(data_dir: &Path) -> bool {
    crate::runtime::lock_runtime(data_dir).is_err()
}

/// The console URL of a running runtime, starting one when none runs and
/// waiting for it. A runtime that is still starting (it holds `data_dir`
/// but has no bus name yet) is waited for, not duplicated. `Ok(None)`
/// when it serves no console (`[console] enabled = false`); the host then
/// shows how to turn it on.
pub async fn ensure_console(
    connection: &RuntimeClient,
    wait: Duration,
    daemon: Option<&Path>,
    data_dir: &Path,
) -> Result<Option<String>, RuntimeError> {
    let deadline = tokio::time::Instant::now() + wait;
    let start_not_before = tokio::time::Instant::now() + RUNTIME_GRACE;
    let mut started = false;
    loop {
        #[cfg(target_os = "linux")]
        let url = console_url(connection).await?;
        #[cfg(not(target_os = "linux"))]
        let url = connection.console_url_in(data_dir).await?;
        match url {
            Some(url) => return Ok((!url.is_empty()).then_some(url)),
            None if !started
                && tokio::time::Instant::now() >= start_not_before
                && !runtime_process_exists(data_dir) =>
            {
                start_runtime(daemon)?;
                started = true;
            }
            None => {}
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(RuntimeError::Timeout(wait));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Suspend or resume dictation; a session in progress is left alone.
#[cfg(target_os = "linux")]
pub async fn set_paused(connection: &zbus::Connection, paused: bool) -> Result<(), RuntimeError> {
    match connection
        .call_method(
            Some(BUS_NAME),
            OBJECT_PATH,
            Some(INTERFACE),
            "SetPaused",
            &(paused,),
        )
        .await
    {
        Ok(_) => Ok(()),
        Err(error) if not_running(&error) => {
            Err(RuntimeError::Bus("the runtime is not running".to_owned()))
        }
        Err(error) if unknown_method(&error) => Err(RuntimeError::TooOld),
        Err(error) => Err(RuntimeError::Bus(error.to_string())),
    }
}

/// Deliver the last completed result again to the focused window.
#[cfg(target_os = "linux")]
pub async fn repaste(connection: &zbus::Connection) -> Result<(), RuntimeError> {
    ask(connection, "Repaste").await.map(|_| ())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A daemon-shaped object on a private bus.
    #[derive(Default)]
    struct FakeDaemon {
        console: Option<&'static str>,
        paused: Vec<bool>,
        repastes: usize,
    }

    #[zbus::interface(name = "dev.voiceless.Daemon.Controller")]
    impl FakeDaemon {
        fn console_url(&self) -> String {
            self.console.unwrap_or_default().to_owned()
        }

        fn status(&self) -> String {
            "idle".to_owned()
        }

        async fn set_paused(&mut self, paused: bool) -> String {
            self.paused.push(paused);
            if paused { "paused" } else { "resumed" }.to_owned()
        }

        async fn repaste(&mut self) -> String {
            self.repastes += 1;
            "repasted".to_owned()
        }
    }

    /// A private session bus, a connection that owns it and a client's.
    async fn private_bus() -> (tokio::process::Child, zbus::Connection, zbus::Connection) {
        let (bus, address) = crate::session_bus::private_bus().await;
        let connect = || async {
            zbus::connection::Builder::address(address.as_str())
                .unwrap()
                .build()
                .await
                .unwrap()
        };
        (bus, connect().await, connect().await)
    }

    /// Serve `daemon` under the runtime's bus name and path.
    async fn serve(connection: &zbus::Connection, daemon: FakeDaemon) {
        connection
            .object_server()
            .at(OBJECT_PATH, daemon)
            .await
            .unwrap();
        connection.request_name(BUS_NAME).await.unwrap();
    }

    async fn recorded(connection: &zbus::Connection) -> (Vec<bool>, usize) {
        let daemon = connection
            .object_server()
            .interface::<_, FakeDaemon>(OBJECT_PATH)
            .await
            .unwrap();
        let daemon = daemon.get().await;
        (daemon.paused.clone(), daemon.repastes)
    }

    #[tokio::test]
    async fn a_missing_runtime_is_reported_and_a_started_one_is_awaited() {
        let (_bus, _connection, client) = private_bus().await;
        assert_eq!(console_url(&client).await, Ok(None));
        assert_eq!(runtime_state(&client).await, Ok(None));
        assert!(!tray_available(&client).await);

        // A runtime that never takes its name: the host waits, then says so, and the
        // process it started is detached and keeps running until stopped by hand.
        let script = tempfile::tempdir().unwrap();
        let script = script.path().join("voiceless");
        std::fs::write(&script, "#!/bin/sh\nsleep 30\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let started = start_runtime(Some(&script)).expect("the fake runtime starts");
        let error = tokio::time::timeout(
            Duration::from_secs(2),
            ensure_console(
                &client,
                Duration::from_millis(300),
                Some(&script),
                script.parent().unwrap(),
            ),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert_eq!(error, RuntimeError::Timeout(Duration::from_millis(300)));
        assert!(
            Path::new(&format!("/proc/{started}")).exists(),
            "the started runtime survives the host"
        );
        std::process::Command::new("kill")
            .arg(started.to_string())
            .status()
            .unwrap();
        assert!(matches!(
            start_runtime(Some(Path::new("/nonexistent/voiceless"))),
            Err(RuntimeError::Spawn(_))
        ));
    }

    #[tokio::test]
    async fn a_running_runtime_answers_where_its_console_listens_and_acts() {
        let (_bus, connection, client) = private_bus().await;
        let data = tempfile::tempdir().unwrap();
        serve(
            &connection,
            FakeDaemon {
                console: Some("http://127.0.0.1:8790/"),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(
            console_url(&client).await,
            Ok(Some("http://127.0.0.1:8790/".to_owned()))
        );
        assert_eq!(runtime_state(&client).await, Ok(Some("idle".to_owned())));
        assert_eq!(
            ensure_console(&client, Duration::from_secs(1), None, data.path()).await,
            Ok(Some("http://127.0.0.1:8790/".to_owned()))
        );
        set_paused(&client, true).await.unwrap();
        set_paused(&client, false).await.unwrap();
        repaste(&client).await.unwrap();
        assert_eq!(recorded(&connection).await, (vec![true, false], 1));
    }

    #[tokio::test]
    async fn a_console_turned_off_is_told_apart_from_a_missing_runtime() {
        let (_bus, connection, client) = private_bus().await;
        let data = tempfile::tempdir().unwrap();
        serve(&connection, FakeDaemon::default()).await;
        assert_eq!(console_url(&client).await, Ok(Some(String::new())));
        assert_eq!(
            ensure_console(&client, Duration::from_secs(1), None, data.path()).await,
            Ok(None)
        );
    }

    #[tokio::test]
    async fn a_tray_is_a_registered_watcher() {
        let (_bus, connection, client) = private_bus().await;
        assert!(!tray_available(&client).await);
        connection
            .request_name("org.kde.StatusNotifierWatcher")
            .await
            .unwrap();
        assert!(tray_available(&client).await);
    }

    /// A daemon from before the desktop app: it answers the bus with its
    /// state, and has none of the other methods.
    struct OldDaemon;

    #[zbus::interface(name = "dev.voiceless.Daemon.Controller")]
    impl OldDaemon {
        fn status(&self) -> String {
            "idle".to_owned()
        }
    }

    #[tokio::test]
    async fn a_runtime_older_than_the_host_is_told_apart_from_a_broken_bus() {
        let (_bus, connection, client) = private_bus().await;
        connection
            .object_server()
            .at(OBJECT_PATH, OldDaemon)
            .await
            .unwrap();
        connection.request_name(BUS_NAME).await.unwrap();
        assert_eq!(runtime_state(&client).await, Ok(Some("idle".to_owned())));
        assert_eq!(console_url(&client).await, Err(RuntimeError::TooOld));
        assert_eq!(paused(&client).await, Err(RuntimeError::TooOld));
        assert_eq!(set_paused(&client, true).await, Err(RuntimeError::TooOld));
        assert_eq!(repaste(&client).await, Err(RuntimeError::TooOld));
    }
}
