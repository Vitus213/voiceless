//! What uses the data directory while `voiceless update` looks at the
//! installation. The updater only watches: it never pauses, restarts or
//! stops a daemon, because a dictation in progress has to finish on the
//! program it started with.
//!
//! `observe` checks the runtime lock for a stream or a starting daemon and
//! queries the session bus even when that lock is free: older daemons only
//! own the bus name. The version is the daemon's own answer (`Version`; a daemon
//! that predates the method answers `UnknownMethod`) or else what the
//! program file it was started from reports.

use std::ffi::OsString;
use std::fs;
use std::io::ErrorKind;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use semver::Version;
use zbus::zvariant::Type;

use crate::daemon::{BUS_NAME, INTERFACE, OBJECT_PATH};

use super::effective_uid;

/// The program file a process was started from, as `/proc/<pid>/exe` names it.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Image {
    pub path: PathBuf,
    pub device: u64,
    pub inode: u64,
    /// The file at `path` is no longer the one running: it was replaced or
    /// removed after the process started (Bun reinstalls in place).
    pub replaced: bool,
}

/// What uses the data directory right now.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Runtime {
    /// No process holds it: the next daemon starts the installed program.
    None,
    /// A process holds it, but which one and what it runs could not be
    /// learned; the string says why. It keeps the version it started with.
    Unknown(String),
    /// The daemon serving this data directory.
    Daemon(Daemon),
}

/// The daemon on the session bus that serves this data directory.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Daemon {
    /// Its last published session state: `idle`, `recording` or `stopping`.
    pub state: String,
    /// Whether dictation is suspended; `None` for a daemon without `Paused`.
    pub paused: Option<bool>,
    /// The version it runs, when it could be learned.
    pub version: Option<Version>,
    pub pid: u32,
    /// The systemd user service that runs it, when it is one's.
    pub unit: Option<String>,
}

/// What uses `data_dir` right now. The runtime is watched through its lock
/// and the session bus, and only ever asked.
pub(super) async fn observe(
    data_dir: &Path,
    version_of: &dyn Fn(&Image) -> Option<Version>,
) -> Runtime {
    let connection = zbus::Connection::session().await.ok();
    observe_on(data_dir, version_of, connection.as_ref()).await
}

async fn observe_on(
    data_dir: &Path,
    version_of: &dyn Fn(&Image) -> Option<Version>,
    connection: Option<&zbus::Connection>,
) -> Runtime {
    let locked = match crate::runtime::lock_runtime(data_dir) {
        Ok(_free) => false,
        Err(error) if error.kind() == ErrorKind::WouldBlock => true,
        Err(error) => {
            return Runtime::Unknown(format!("its runtime lock cannot be checked ({error})"))
        }
    };
    // v0.1.3 owns the bus name but predates runtime.lock.
    let Some(connection) = connection else {
        return if locked {
            Runtime::Unknown("the session bus is not reachable".to_owned())
        } else {
            Runtime::None
        };
    };
    match find_daemon(connection, data_dir, version_of).await {
        Ok(Some(daemon)) => Runtime::Daemon(daemon),
        Ok(None) if !locked => Runtime::None,
        Ok(None) => Runtime::Unknown(
            "no daemon of this data directory answers on the session bus: it is one still loading its model, or a `voiceless stream` that is recording".to_owned(),
        ),
        Err(error) => Runtime::Unknown(format!("it does not answer on the session bus: {error}")),
    }
}

/// The daemon of `data_dir` on the session bus: `None` when nobody owns the
/// bus name, or when the daemon that does serves another data directory.
async fn find_daemon(
    connection: &zbus::Connection,
    data_dir: &Path,
    version_of: &dyn Fn(&Image) -> Option<Version>,
) -> Result<Option<Daemon>, String> {
    let Some(state) = ask::<String>(connection, "Status").await? else {
        return Ok(None);
    };
    // A daemon without `DataDirectory` predates several data directories and
    // is taken to serve this one.
    if let Some(served) = ask::<Vec<u8>>(connection, "DataDirectory").await? {
        if !same_directory(PathBuf::from(OsString::from_vec(served)), data_dir) {
            return Ok(None);
        }
    }
    let pid = owner_pid(connection).await?;
    let paused = ask::<bool>(connection, "Paused").await?;
    let version = match ask::<String>(connection, "Version").await? {
        Some(reported) => Version::parse(reported.trim()).ok(),
        None => image(pid).and_then(|image| version_of(&image)),
    };
    Ok(Some(Daemon {
        state,
        paused,
        version,
        pid,
        unit: user_unit(connection, pid).await,
    }))
}

/// The daemon's reply to `method`: `None` when nobody owns the bus name, or
/// when the daemon that does predates `method`.
async fn ask<T>(connection: &zbus::Connection, method: &'static str) -> Result<Option<T>, String>
where
    T: serde::de::DeserializeOwned + Type,
{
    match connection
        .call_method(Some(BUS_NAME), OBJECT_PATH, Some(INTERFACE), method, &())
        .await
    {
        Ok(reply) => reply
            .body()
            .deserialize::<T>()
            .map(Some)
            .map_err(|error| format!("{method} did not answer something readable ({error})")),
        Err(error) if absent(&error) => Ok(None),
        Err(error) => Err(format!("{method} failed: {error}")),
    }
}

/// Nobody owns the bus name (no daemon), or the daemon does not know `method`
/// (it predates it).
fn absent(error: &zbus::Error) -> bool {
    matches!(
        error,
        zbus::Error::MethodError(name, _, _)
            if matches!(
                name.as_str(),
                "org.freedesktop.DBus.Error.ServiceUnknown"
                    | "org.freedesktop.DBus.Error.UnknownMethod"
            )
    )
}

/// The process that owns the daemon's bus name.
async fn owner_pid(connection: &zbus::Connection) -> Result<u32, String> {
    let name = zbus::names::BusName::try_from(BUS_NAME).map_err(|error| error.to_string())?;
    let dbus = zbus::fdo::DBusProxy::new(connection)
        .await
        .map_err(|error| error.to_string())?;
    dbus.get_connection_unix_process_id(name)
        .await
        .map_err(|error| error.to_string())
}

/// Whether two paths name the same directory, by what they lead to.
fn same_directory(served: PathBuf, here: &Path) -> bool {
    match (served.canonicalize(), here.canonicalize()) {
        (Ok(served), Ok(here)) => served == here,
        _ => served.as_path() == here,
    }
}

/// The program file of `pid`; `None` once the process is gone.
fn image(pid: u32) -> Option<Image> {
    let executable = format!("/proc/{pid}/exe");
    let metadata = fs::metadata(&executable).ok()?;
    let link = fs::read_link(executable).ok()?;
    let name = link.to_string_lossy();
    let (path, deleted) = match name.strip_suffix(" (deleted)") {
        Some(path) => (path, true),
        None => (name.as_ref(), false),
    };
    let path = PathBuf::from(path);
    Some(Image {
        device: metadata.dev(),
        inode: metadata.ino(),
        replaced: deleted
            || !fs::metadata(&path)
                .is_ok_and(|file| file.dev() == metadata.dev() && file.ino() == metadata.ino()),
        path,
    })
}

/// A terminal-launched daemon can inherit an unrelated service's cgroup.
/// Recommend restarting a service only when systemd tracks this exact main PID.
async fn user_unit(connection: &zbus::Connection, pid: u32) -> Option<String> {
    let cgroups = fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    let unit = user_unit_in(&cgroups, effective_uid())?;
    let dbus = zbus::fdo::DBusProxy::new(connection).await.ok()?;
    let name = zbus::names::BusName::try_from("org.freedesktop.systemd1").ok()?;
    let owner = dbus.get_name_owner(name).await.ok()?;
    let manager = zbus::Proxy::new(
        connection,
        owner.as_str(),
        "/org/freedesktop/systemd1",
        "org.freedesktop.systemd1.Manager",
    )
    .await
    .ok()?;
    let path: zbus::zvariant::OwnedObjectPath =
        manager.call("GetUnit", &(unit.as_str(),)).await.ok()?;
    let service = zbus::Proxy::new(
        connection,
        owner.clone(),
        path,
        "org.freedesktop.systemd1.Service",
    )
    .await
    .ok()?;
    let main_pid: u32 = service.get_property("MainPID").await.ok()?;
    (main_pid == pid).then_some(unit)
}

/// The `.service` that owns the cgroup under the user's systemd manager. A
/// process started by a terminal or an application sits in a scope, not a
/// service, and is nobody's but the user's to restart.
fn user_unit_in(cgroups: &str, uid: u32) -> Option<String> {
    let manager = format!("/user.slice/user-{uid}.slice/user@{uid}.service/");
    cgroups
        .lines()
        .filter_map(|line| line.rsplit_once(':'))
        .map(|(_, path)| path)
        .filter_map(|path| path.strip_prefix(&manager))
        .filter_map(|under| under.rsplit_once('/'))
        .map(|(_, leaf)| leaf)
        .find(|leaf| leaf.ends_with(".service"))
        .map(str::to_owned)
}

impl Runtime {
    /// The report: what runs, and what its owner does to switch it when it
    /// does not run the installed version.
    pub(super) fn report(&self, installed: &Version) -> Vec<String> {
        match self {
            Runtime::None => vec![format!(
                "No daemon is running for this data directory; the next `voiceless daemon` starts Voiceless {installed}."
            )],
            Runtime::Unknown(why) => vec![format!(
                "A Voiceless runtime may still use this data directory, but {why}. Nothing was switched: it keeps the version it started with."
            )],
            Runtime::Daemon(daemon) => daemon.report(installed),
        }
    }
}

impl Daemon {
    fn report(&self, installed: &Version) -> Vec<String> {
        let mut state = format!("{}; pid {}", self.state, self.pid);
        if self.paused.unwrap_or(false) {
            state.push_str("; paused");
        }
        if let Some(unit) = &self.unit {
            state.push_str(&format!("; systemd user unit {unit}"));
        }
        // A daemon whose version could not be learned is not known to run the installed one.
        let outdated = self
            .version
            .as_ref()
            .is_none_or(|running| running != installed);
        let named = match &self.version {
            Some(running) => format!("Voiceless {running}"),
            None => "an unknown version".to_owned(),
        };
        let mut lines = vec![format!(
            "Running daemon: {named} ({state}); {}.",
            if outdated {
                "it was not switched"
            } else {
                "it already runs the installed version"
            }
        )];
        if outdated {
            let keeps = match &self.version {
                Some(running) => format!("It keeps Voiceless {running}"),
                None => "It keeps the version it started with".to_owned(),
            };
            let switch = match &self.unit {
                Some(unit) => format!("restart it with `systemctl --user restart {unit}`"),
                None => format!("stop pid {} and run `voiceless daemon` again", self.pid),
            };
            lines.push(format!(
                "{keeps} until its owner restarts it. Once dictation is idle, {switch}."
            ));
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A daemon-shaped object on the bus: what it is asked and what it answers.
    struct FakeDaemon {
        directory: PathBuf,
        status: &'static str,
    }

    #[zbus::interface(name = "dev.voiceless.Daemon.Controller")]
    impl FakeDaemon {
        fn status(&self) -> String {
            self.status.to_owned()
        }

        fn data_directory(&self) -> Vec<u8> {
            self.directory.as_os_str().as_encoded_bytes().to_vec()
        }

        fn paused(&self) -> bool {
            true
        }

        fn version(&self) -> String {
            "0.1.3".to_owned()
        }
    }

    /// v0.1.3 only answers Status and does not hold runtime.lock.
    struct OldDaemon;

    #[zbus::interface(name = "dev.voiceless.Daemon.Controller")]
    impl OldDaemon {
        fn status(&self) -> String {
            "idle".to_owned()
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

    #[tokio::test]
    async fn the_daemon_is_asked_and_never_told() {
        let (_bus, owner, client) = private_bus().await;
        let scratch = tempfile::tempdir().unwrap();
        let data_dir = scratch.path().join("data");
        fs::create_dir_all(&data_dir).unwrap();
        let nothing = |_: &Image| -> Option<Version> {
            panic!("the daemon's own answer comes before its program file")
        };
        assert!(find_daemon(&client, &data_dir, &nothing)
            .await
            .unwrap()
            .is_none());

        // A daemon of another data directory is not this one's runtime.
        owner
            .object_server()
            .at(
                OBJECT_PATH,
                FakeDaemon {
                    directory: scratch.path().join("elsewhere"),
                    status: "idle",
                },
            )
            .await
            .unwrap();
        owner.request_name(BUS_NAME).await.unwrap();
        assert!(find_daemon(&client, &data_dir, &nothing)
            .await
            .unwrap()
            .is_none());

        owner
            .object_server()
            .remove::<FakeDaemon, _>(OBJECT_PATH)
            .await
            .unwrap();
        owner
            .object_server()
            .at(
                OBJECT_PATH,
                FakeDaemon {
                    directory: data_dir.clone(),
                    status: "recording",
                },
            )
            .await
            .unwrap();
        let found = find_daemon(&client, &data_dir, &nothing)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.state, "recording");
        assert_eq!(found.paused, Some(true));
        assert_eq!(found.version, Some(Version::new(0, 1, 3)));
        assert_eq!(found.pid, std::process::id());
    }

    #[tokio::test]
    async fn an_older_daemon_is_versioned_by_its_program_file() {
        let (_bus, owner, client) = private_bus().await;
        let scratch = tempfile::tempdir().unwrap();
        let data_dir = scratch.path().join("data");
        fs::create_dir_all(&data_dir).unwrap();
        owner
            .object_server()
            .at(OBJECT_PATH, OldDaemon)
            .await
            .unwrap();
        owner.request_name(BUS_NAME).await.unwrap();

        // The fake daemon is this test process; its program file is the test binary.
        let by_file = |image: &Image| {
            (!image.replaced && image.path == std::env::current_exe().unwrap())
                .then_some(Version::new(9, 9, 9))
        };
        let Runtime::Daemon(found) = observe_on(&data_dir, &by_file, Some(&client)).await else {
            panic!("a v0.1.3 daemon must be visible without runtime.lock");
        };
        assert_eq!(found.state, "idle");
        assert_eq!(found.paused, None);
        assert_eq!(found.version, Some(Version::new(9, 9, 9)));

        // Whatever the program file is read as is the answer.
        let unreplaced = |image: &Image| (!image.replaced).then_some(Version::new(1, 0, 0));
        assert_eq!(
            find_daemon(&client, &data_dir, &unreplaced)
                .await
                .unwrap()
                .unwrap()
                .version,
            Some(Version::new(1, 0, 0))
        );
    }

    #[tokio::test]
    async fn a_free_data_directory_has_no_runtime_to_report() {
        let scratch = tempfile::tempdir().unwrap();
        let data_dir = scratch.path().join("data");
        let (_bus, _owner, client) = private_bus().await;
        let observed = observe_on(
            &data_dir,
            &|_| panic!("no daemon owns the name"),
            Some(&client),
        )
        .await;
        assert_eq!(observed, Runtime::None);
    }

    #[test]
    fn the_systemd_user_service_is_read_from_the_cgroup() {
        let scope = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/app.slice/vte-spawn-1234.scope\n";
        assert_eq!(user_unit_in(scope, 1000), None);
        let service =
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/voiceless.service\n";
        assert_eq!(
            user_unit_in(service, 1000).as_deref(),
            Some("voiceless.service")
        );
        // Another user's manager, and a system service, are not this user's to restart.
        assert_eq!(user_unit_in(service, 1001), None);
        assert_eq!(user_unit_in("0::/system.slice/sshd.service\n", 1000), None);
        let first =
            "12:pids:/user.slice/user-1000.slice/user@1000.service/app.slice/voiceless.service\n\
                     11:memory:/user.slice/user-1000.slice/user@1000.service/app.slice\n";
        assert_eq!(
            user_unit_in(first, 1000).as_deref(),
            Some("voiceless.service")
        );
    }

    #[test]
    fn this_process_is_an_image_that_was_not_replaced() {
        let own = image(std::process::id()).unwrap();
        assert!(!own.replaced);
        assert_eq!(own.path, std::env::current_exe().unwrap());
        assert_eq!(image(i32::MAX as u32), None);
    }

    #[test]
    fn the_report_names_who_switches_the_daemon_and_when() {
        let installed = Version::new(0, 2, 0);
        let daemon = |version: Option<Version>, unit: Option<&str>| Daemon {
            state: "recording".to_owned(),
            paused: Some(false),
            version,
            pid: 42,
            unit: unit.map(str::to_owned),
        };

        let older = Version::new(0, 1, 3);
        let lines =
            Runtime::Daemon(daemon(Some(older), Some("voiceless.service"))).report(&installed);
        assert!(lines[0].contains("0.1.3 (recording; pid 42"));
        assert!(lines[1].contains("Once dictation is idle"));
        assert!(lines[1].contains("systemctl --user restart voiceless.service"));

        let lines = Runtime::Daemon(daemon(None, None)).report(&installed);
        assert!(lines[0].contains("unknown version (recording; pid 42"));
        assert!(lines[1].contains("stop pid 42 and run `voiceless daemon` again"));

        let lines = Runtime::Daemon(daemon(Some(installed.clone()), None)).report(&installed);
        assert_eq!(
            lines,
            ["Running daemon: Voiceless 0.2.0 (recording; pid 42); it already runs the installed version."]
        );

        let mut current = daemon(Some(installed.clone()), None);
        current.paused = Some(true);
        assert!(current.report(&installed)[0].contains("recording; pid 42; paused)"));

        assert!(Runtime::None.report(&installed)[0]
            .contains("No daemon is running for this data directory"));
        let unknown = Runtime::Unknown("the session bus is not reachable".to_owned());
        assert!(unknown.report(&installed)[0].contains("the session bus is not reachable"));
    }
}
