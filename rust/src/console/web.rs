//! Open a running loopback console or serve one for the life of the command.

use std::io::{self, Read};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
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
    let mut address = super::resolve_address(host, port).await?;
    address.set_ip(match address.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => Ipv4Addr::LOCALHOST.into(),
        IpAddr::V6(ip) if ip.is_unspecified() => Ipv6Addr::LOCALHOST.into(),
        ip => ip,
    });
    if !matches!(
        address.ip(),
        IpAddr::V4(Ipv4Addr::LOCALHOST) | IpAddr::V6(Ipv6Addr::LOCALHOST)
    ) {
        return Err(format!(
            "voiceless web requires a loopback address, got {address}"
        ));
    }

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
    show_url(
        listener.local_addr().map_err(|error| error.to_string())?,
        open_browser,
    );
    super::serve_listener(
        listener,
        None,
        crate::models::default_data_dir().join("history.sqlite3"),
        config_path,
        super::random_token(),
    )
    .await
}

/// Recognize older daemon builds through the same page marker the console
/// uses for its API token. Probe only the local address, without redirects.
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
    prefix[..length as usize]
        .windows(META.len())
        .any(|part| part == META)
}

fn show_url(address: SocketAddr, open_browser: bool) {
    let url = format!("http://{address}/");
    println!("Voiceless console: {url}");
    if open_browser {
        // xdg-open may stay alive with the browser. Detach its standard
        // streams so a caller reading the URL can exit independently.
        if let Err(error) = tokio::process::Command::new("xdg-open")
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
