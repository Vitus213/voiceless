use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

/// Starts a private bus without loading system policy or activating desktop services.
/// Keep the child alive until all clients have disconnected.
pub(crate) async fn private_bus() -> (Child, String) {
    let mut bus = Command::new("dbus-daemon")
        .arg(concat!(
            "--config-file=",
            env!("CARGO_MANIFEST_DIR"),
            "/../frontend/e2e/fixtures/session.conf"
        ))
        .args(["--nofork", "--print-address=1"])
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("start private test bus");
    let mut output = BufReader::new(bus.stdout.take().unwrap()).lines();
    let address = tokio::time::timeout(Duration::from_secs(3), output.next_line())
        .await
        .expect("timed out waiting for private test bus")
        .expect("read private test bus address")
        .expect("private test bus exited without an address");
    (bus, address)
}
