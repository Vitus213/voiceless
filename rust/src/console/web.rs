//! Open a running loopback console or serve one for the life of the command.

use std::io::{self, Read};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::net::TcpListener;

/// Open the configured console. An unused port starts a standalone console
/// over the same history and configuration as the daemon.
pub async fn open_web(
    config_path: PathBuf,
    host: &str,
    port: u16,
    open_browser: bool,
) -> Result<(), String> {
    let address = super::resolve_address(host, port).await?;

    let listener = match TcpListener::bind(address).await {
        Ok(listener) => listener,
        Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
            if tokio::task::spawn_blocking(move || is_console(address))
                .await
                .map_err(|error| error.to_string())?
            {
                show_url(address, open_browser);
                return Ok(());
            }
            return Err(format!(
                "{address} is in use and did not serve a Voiceless console. Try voiceless web --port 0"
            ));
        }
        Err(error) => return Err(format!("cannot bind {address}: {error}")),
    };
    super::serve_when_ready(
        listener,
        None,
        crate::models::default_data_dir().join("history.sqlite3"),
        config_path,
        super::random_token(),
        |address| show_url(address, open_browser),
    )
    .await
}

/// Recognize an existing console by its page token and history API. Both
/// exist in older releases. Probe the local address without redirects.
fn is_console(address: SocketAddr) -> bool {
    let agent = ureq::AgentBuilder::new()
        .try_proxy_from_env(false)
        .redirects(0)
        .timeout(Duration::from_secs(1))
        .build();
    let Ok(response) = agent.get(&format!("http://{address}/")).call() else {
        return false;
    };
    if response.status() != 200 {
        return false;
    }
    const META: &[u8] = b"<meta name=\"voiceless-token\" content=\"";
    let mut prefix = [0_u8; 2048];
    let mut reader = response.into_reader().take(prefix.len() as u64);
    let Ok(length) = io::copy(&mut reader, &mut prefix.as_mut_slice()) else {
        return false;
    };
    let prefix = &prefix[..length as usize];
    let Some(start) = prefix.windows(META.len()).position(|part| part == META) else {
        return false;
    };
    let token = &prefix[start + META.len()..];
    let Some(end) = token.iter().position(|&byte| byte == b'"') else {
        return false;
    };
    let token = &token[..end];
    if token.is_empty()
        || !token
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return false;
    }
    let token = std::str::from_utf8(token).expect("ASCII token");
    let Ok(response) = agent
        .get(&format!("http://{address}/api/totals"))
        .set("X-Voiceless-Token", token)
        .call()
    else {
        return false;
    };
    let Ok(totals) =
        serde_json::from_reader::<_, serde_json::Value>(response.into_reader().take(1024))
    else {
        return false;
    };
    totals["records"].is_i64()
        && totals["characters"].is_i64()
        && totals["duration_seconds"].is_number()
}

fn show_url(address: SocketAddr, open_browser: bool) {
    let url = format!("http://{address}/");
    println!("Voiceless console: {url}");
    if open_browser {
        // xdg-open may stay alive with the browser. Detach its standard
        // streams so a caller reading the URL can exit independently.
        #[cfg(target_os = "linux")]
        let mut launcher = tokio::process::Command::new("xdg-open");
        #[cfg(target_os = "macos")]
        let mut launcher = tokio::process::Command::new("open");
        #[cfg(windows)]
        let mut launcher = {
            let mut command = tokio::process::Command::new("rundll32.exe");
            command.arg("url.dll,FileProtocolHandler");
            command
        };
        if let Err(error) = launcher
            .arg(&url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            eprintln!("Voiceless: cannot open the browser: {error}. Open {url} manually.");
        }
    }
}
