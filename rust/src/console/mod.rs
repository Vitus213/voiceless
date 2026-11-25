//! Web console (`docs/console-api.md`): live control, settings, models,
//! modes, history and diagnostics over a loopback HTTP API, served by the
//! daemon, `voiceless web`, or `voiceless history serve`.
//!
//! The entry points share one axum router (`router`):
//!
//! - `serve_daemon`, spawned by `voiceless daemon` when `[console] enabled`
//!   is true, binding `[console] host`/`port` (default `127.0.0.1:8790`).
//!   The `DaemonShared` handle supplies the command channel, the event
//!   broadcast, the warm engine, the status snapshot and the log ring that
//!   the live endpoints need.
//! - `serve_standalone`, `voiceless history serve [--host] [--port]`
//!   (defaults `127.0.0.1`, `8790`), with no daemon: history, config, text
//!   preview, prompt, modes, the model catalog and installs, hotkey keys,
//!   doctor and the injection test all work; `/api/status`, `/api/events`,
//!   `/api/session/*`, `/api/engine/reload`, `/api/logs`, re-transcription
//!   and the benchmark answer 503 `{"error":"daemon not running"}`
//!   (`ConsoleState::daemon`).
//! - `open_web`, `voiceless web`, opens an existing console or serves the
//!   standalone console on the configured loopback address.
//!
//! This file holds the router, the security middleware, the page and the
//! live/diagnostic handlers; `settings` holds config, text preview, prompt
//! and modes; `library` holds models, records, audio, corrections and the
//! benchmark. Handlers return `ApiResult`, so a handler failure is always
//! JSON `{"error": message}` with a status code (the Host check and axum's
//! own extractor rejections are plain text).
//!
//! Every request must name a loopback `Host` (DNS rebinding), every `/api`
//! request must carry the per-start token embedded in the page (cross-site
//! requests cannot read it), and mutating requests from a browser must come
//! from the console's own origin. Secrets in the config are never returned
//! (`settings::masked`). The middleware below documents the threat each
//! check blocks.
//!
//! Settings requests read `config.toml` from disk and replace it through
//! the shared settings writer. `history.sqlite3` in the data dir is opened once
//! per server; `modes.json`, the model store under the data/cache dirs and
//! the retained WAV recordings are reached through `settings`/`library`.
//! `build.rs` locates the Vite output. These files are embedded with
//! `include_str!` so the console works offline with just the binary.

mod jobs;
mod library;
mod meeting_review;
mod meetings;
mod retention;
mod revisions;
pub(crate) mod settings;
mod web;

pub use web::open_web;

use std::convert::Infallible;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::daemon::{DaemonCommand, DaemonShared};
use crate::history::HistoryStore;

const INDEX: &str = include_str!(concat!(env!("VOICELESS_CONSOLE_DIST"), "/index.html"));
const APP_JS: &str = include_str!(concat!(env!("VOICELESS_CONSOLE_DIST"), "/app.js"));
const STYLE_CSS: &str = include_str!(concat!(env!("VOICELESS_CONSOLE_DIST"), "/style.css"));

/// The system this binary runs on, as `std::env::consts::OS` names it
/// (`linux`, `macos`, `windows`). It is the runtime that delivers the text,
/// so the page offers its typing controls, not those of whatever system the
/// browser runs on.
const PLATFORM: &str = std::env::consts::OS;

/// Everything the handlers share; one instance per server behind an `Arc`
/// in axum's state.
pub struct ConsoleState {
    /// `None` under `voiceless history serve`: live endpoints answer 503.
    daemon: Option<Arc<DaemonShared>>,
    /// History database, opened once in `serve_listener`.
    store: Arc<HistoryStore>,
    /// `config.toml`: read on demand, patched in place by `settings`.
    config_path: PathBuf,
    /// Per-start session token, substituted into the page by `index`.
    token: String,
    /// The bound port; the `Host` and `Origin` checks compare against it.
    port: u16,
    /// Background model installs in flight (`library::Installs`).
    installs: library::Installs,
}

impl ConsoleState {
    /// The daemon handle, or the 503 `daemon not running` error that every
    /// live endpoint returns under `history serve`.
    fn daemon(&self) -> Result<&Arc<DaemonShared>, ApiError> {
        self.daemon.as_ref().ok_or_else(|| {
            ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "daemon not running".to_owned(),
            )
        })
    }

    /// Load `config.toml` fresh from disk, or the defaults when the file
    /// does not exist. Load failures become 500: invalid TOML, an unknown
    /// key, or a file holding an API key while readable by other users
    /// (`AppConfig::load` refuses such files, as the CLI does).
    fn config(&self) -> Result<crate::config::AppConfig, ApiError> {
        if self.config_path.exists() {
            crate::config::AppConfig::load(&self.config_path).map_err(ApiError::internal)
        } else {
            Ok(crate::config::AppConfig::default())
        }
    }
}

/// `{"error": message}` with a status code.
///
/// Built with the tuple constructor for a specific status (404, 409, 503,
/// …) or through `bad_request` / `internal`, which double as `map_err`
/// adapters for anything `ToString`.
#[derive(Debug)]
pub struct ApiError(StatusCode, String);

impl ApiError {
    /// 400: the request itself is wrong (missing field, invalid config
    /// value, regex that does not compile).
    fn bad_request(message: impl ToString) -> Self {
        Self(StatusCode::BAD_REQUEST, message.to_string())
    }

    /// 500: something on the server side failed (database, config file,
    /// a closed channel, a panicked blocking task).
    fn internal(message: impl ToString) -> Self {
        Self(StatusCode::INTERNAL_SERVER_ERROR, message.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

/// JSON body on success, `ApiError` otherwise: the type of every JSON
/// handler.
type ApiResult = Result<Json<Value>, ApiError>;

/// Console served by the daemon: full API, including live control.
///
/// Opens `history.sqlite3` from the default data dir, reuses the config
/// path the daemon was started with, binds `host:port` (`[console]`) and
/// serves until the listener fails. A fresh random token is minted per
/// start, so a page left open from a previous run gets 403 until it is
/// refreshed. The URL is announced through `daemon.log`, so it appears in
/// the journal and in `/api/logs`. Errors are bind/open failures as text;
/// the daemon logs them as "console unavailable" and keeps running without
/// a console.
pub async fn serve_daemon(daemon: Arc<DaemonShared>, host: &str, port: u16) -> Result<(), String> {
    let store_path = daemon.data_dir.join("history.sqlite3");
    let config_path = daemon.config_path.clone();
    let listener = bind(host, port).await?;
    serve_when_ready(
        listener,
        Some(daemon.clone()),
        store_path,
        config_path,
        random_token(),
        |address| {
            daemon.set_console_url(format!("http://{address}/"));
            daemon.log(format!("Voiceless console: http://{address}/"));
        },
    )
    .await
}

/// Start the native control transport. A hidden console exposes only
/// authenticated routes and keeps CLI controls available.
#[cfg(not(target_os = "linux"))]
pub(crate) async fn start_native(
    daemon: Arc<DaemonShared>,
    config: &crate::config::ConsoleConfig,
) -> Result<tokio::task::JoinHandle<()>, String> {
    let listener = if config.enabled {
        bind(&config.host, config.port).await?
    } else {
        bind("127.0.0.1", 0).await?
    };
    let address = listener.local_addr().map_err(|error| error.to_string())?;
    let state = server_state(
        address,
        Some(daemon.clone()),
        daemon.data_dir.join("history.sqlite3"),
        daemon.config_path.clone(),
        random_token(),
    )?;
    crate::desktop::native_runtime::publish(
        &daemon.data_dir,
        address,
        daemon.status().runtime_id,
        &state.token,
        config.enabled,
    )?;
    if config.enabled {
        daemon.set_console_url(format!("http://{address}/"));
        daemon.log(format!("Voiceless console: http://{address}/"));
    }
    let app = router(state, config.enabled);
    Ok(tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, app).await {
            daemon.log(format!("Voiceless control server stopped: {error}"));
        }
    }))
}

/// `voiceless history serve`: the console without live control.
///
/// `store_path` is the history database to browse (the CLI passes the
/// default one), the config path is `default_config_path()` and the daemon
/// handle is `None`, so every live endpoint answers 503. Prints the URL to
/// stdout. Returns the process exit code (0 if the server ever returns) or
/// a bind/open error for `main` to print.
pub async fn serve_standalone(store_path: PathBuf, host: &str, port: u16) -> Result<i32, String> {
    let listener = bind(host, port).await?;
    serve_when_ready(
        listener,
        None,
        store_path,
        crate::config::default_config_path(),
        random_token(),
        |address| println!("Voiceless console: http://{address}/"),
    )
    .await
    .map(|()| 0)
}

/// Bind a console listener to the configured loopback address.
async fn bind(host: &str, port: u16) -> Result<tokio::net::TcpListener, String> {
    let address = resolve_address(host, port).await?;
    tokio::net::TcpListener::bind(address)
        .await
        .map_err(|error| format!("cannot bind {address}: {error}"))
}

async fn resolve_address(host: &str, port: u16) -> Result<SocketAddr, String> {
    let mut address = tokio::net::lookup_host((host, port))
        .await
        .map_err(|error| format!("invalid listen address {host}:{port}: {error}"))?
        .next()
        .ok_or_else(|| format!("listen address {host}:{port} resolves to nothing"))?;
    let ip = match address.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => Ipv4Addr::LOCALHOST.into(),
        IpAddr::V6(ip) if ip.is_unspecified() => Ipv6Addr::LOCALHOST.into(),
        ip @ (IpAddr::V4(Ipv4Addr::LOCALHOST) | IpAddr::V6(Ipv6Addr::LOCALHOST)) => ip,
        _ => {
            return Err(format!(
                "Voiceless console requires a loopback address, got {address}"
            ))
        }
    };
    address.set_ip(ip);
    Ok(address)
}

/// Serve on a bound listener; tests bind an ephemeral port and pass their
/// own token.
///
/// Opens (and if needed migrates) the history store, builds the shared
/// state with the listener's actual port — the `Host`/`Origin` checks need
/// it, and it is only known here when port 0 was bound — and runs
/// `axum::serve`. No shutdown signal is wired, so this returns only when
/// the store cannot be opened or the accept loop fails.
pub async fn serve_listener(
    listener: tokio::net::TcpListener,
    daemon: Option<Arc<DaemonShared>>,
    store_path: PathBuf,
    config_path: PathBuf,
    token: String,
) -> Result<(), String> {
    serve_when_ready(listener, daemon, store_path, config_path, token, |_| {}).await
}

async fn serve_when_ready(
    listener: tokio::net::TcpListener,
    daemon: Option<Arc<DaemonShared>>,
    store_path: PathBuf,
    config_path: PathBuf,
    token: String,
    ready: impl FnOnce(SocketAddr),
) -> Result<(), String> {
    let address = listener.local_addr().map_err(|error| error.to_string())?;
    let state = server_state(address, daemon, store_path, config_path, token)?;
    ready(address);
    axum::serve(listener, router(state, true))
        .await
        .map_err(|error| error.to_string())
}

fn server_state(
    address: SocketAddr,
    daemon: Option<Arc<DaemonShared>>,
    store_path: PathBuf,
    config_path: PathBuf,
    token: String,
) -> Result<Arc<ConsoleState>, String> {
    if !address.ip().is_loopback() {
        return Err(format!(
            "Voiceless console requires a loopback address, got {address}"
        ));
    }
    let port = address.port();
    let store = Arc::new(HistoryStore::open(store_path).map_err(|error| error.to_string())?);
    Ok(Arc::new(ConsoleState {
        daemon,
        store,
        config_path,
        token,
        port,
        installs: library::Installs::default(),
    }))
}

/// The route table, matching the sections of `docs/console-api.md`.
/// `/api/*` sits behind `require_token`; the whole tree, static assets
/// included, sits behind `require_loopback_host`, which therefore runs
/// first. Unknown paths get axum's empty 404 and malformed JSON bodies its
/// plain-text rejections (415 without a JSON content type), not the
/// `{"error"}` shape.
fn router(state: Arc<ConsoleState>, show_console: bool) -> Router {
    let api = Router::new()
        .route("/status", get(status))
        .route("/events", get(events))
        .route("/session/{action}", post(session))
        .route("/engine/reload", post(reload))
        .route(
            "/config",
            get(settings::get_config).put(settings::put_config),
        )
        .route("/config/template", get(settings::export_template))
        .route("/config/restore", post(restore_config))
        .route("/text/preview", post(settings::preview))
        .route("/text/vocabulary/merge", post(settings::merge_vocabulary))
        .route(
            "/prompt",
            get(settings::get_prompt).post(settings::save_prompt),
        )
        .route("/modes", get(settings::list_modes).post(settings::add_mode))
        .route("/modes/preview", post(settings::preview_output))
        .route(
            "/modes/{id}",
            put(settings::update_mode).delete(settings::remove_mode),
        )
        .route("/models", get(library::list_models))
        .route("/models/import", post(library::import_model))
        .route("/models/{id}/install", post(library::install_model))
        .route("/models/{id}", delete(library::remove_model))
        .route("/records", get(library::records))
        .route("/records/export", get(library::export_records))
        .route("/records/facets", get(library::record_facets))
        .route("/records/{id}", delete(library::delete_record))
        .route("/records/{id}/retranscribe", post(library::retranscribe))
        .merge(revisions::routes())
        .route("/jobs", get(jobs::list).post(jobs::submit))
        .route("/jobs/{id}", get(jobs::get).delete(jobs::remove))
        .route("/jobs/{id}/reselect", post(jobs::reselect))
        .route("/jobs/{id}/{action}", post(jobs::action))
        .route("/jobs/{id}/export/{format}", get(jobs::export))
        .route("/totals", get(library::totals))
        .route("/audio/{id}", get(library::audio))
        .route("/corrections", post(library::corrections))
        .route("/benchmark", post(library::benchmark))
        .route("/hotkey/keys", get(hotkey_keys))
        .route("/desktop/audio", get(desktop_audio))
        .route("/desktop/overlay", get(desktop_overlay))
        .route("/inject/test", post(inject_test))
        .route("/doctor", get(doctor))
        .route("/logs", get(logs));
    let api = Router::new()
        .nest("/api", api)
        .merge(retention::routes())
        .merge(meetings::routes())
        .merge(meeting_review::routes())
        // Token (and, for writes, Origin) check on every /api route.
        .layer(middleware::from_fn_with_state(state.clone(), require_token));
    let mut app = Router::new();
    if show_console {
        app = app
            .route("/", get(index))
            .route("/index.html", get(index))
            // The assets hold no secret, so default caching is fine; the page
            // itself is served `no-store` because it embeds the token.
            .route(
                "/app.js",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                        APP_JS,
                    )
                }),
            )
            .route(
                "/style.css",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
                        STYLE_CSS,
                    )
                }),
            );
    }
    app.merge(api)
        // Outermost layer: the Host check runs before anything else.
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_loopback_host,
        ))
        .with_state(state)
}

/// 24 bytes from the OS RNG as 48 lowercase hex characters, minted once
/// per server start. Panics if the OS random source is unavailable: better
/// no console than a guessable token.
fn random_token() -> String {
    let mut bytes = [0_u8; 24];
    getrandom::getrandom(&mut bytes).expect("OS random source available");
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Constant-time comparison, so the token cannot be guessed byte by byte.
/// The length check leaks only the length (fixed at 48 hex chars); the
/// bytes are OR-folded through XOR so the time taken does not depend on
/// where the first mismatch is.
fn same_token(given: &str, expected: &str) -> bool {
    given.len() == expected.len()
        && given
            .bytes()
            .zip(expected.bytes())
            .fold(0_u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

/// Middleware on every route: `Host` must be exactly `127.0.0.1:<port>`,
/// `localhost:<port>` or `[::1]:<port>` with the bound port, else 421
/// Misdirected Request (plain text `unexpected Host`).
///
/// Threat: DNS rebinding. A hostile page can make one of its own names
/// resolve to 127.0.0.1 after the browser loaded it, and then talk to the
/// console through that name from an origin it controls. Such requests
/// carry the attacker's name in `Host`, so they never reach a handler, not
/// even the page that embeds the token. A missing `Host` is rejected too.
async fn require_loopback_host(
    State(state): State<Arc<ConsoleState>>,
    request: Request,
    next: Next,
) -> Response {
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    // Exact matches, port included: a different port or a bare name is not us.
    let allowed = [
        format!("127.0.0.1:{}", state.port),
        format!("localhost:{}", state.port),
        format!("[::1]:{}", state.port),
    ];
    if !allowed.iter().any(|candidate| candidate == host) {
        // 421 rather than 403: this server does not serve the host named.
        return (StatusCode::MISDIRECTED_REQUEST, "unexpected Host").into_response();
    }
    next.run(request).await
}

/// Middleware on `/api/*`: the per-start token must arrive as the
/// `X-Voiceless-Token` header (the app's `fetch` calls) or as `?token=` in
/// the query (`EventSource` and `<audio src>` cannot set headers); missing
/// or wrong → 403 `{"error":"missing or wrong console token"}`. Non-GET
/// requests must additionally pass `same_origin`, else 403
/// `{"error":"cross-origin request"}`.
///
/// Threats. (1) Any other website open in the same browser can send
/// requests to 127.0.0.1:8790, but it cannot read the token out of the
/// console page, so without it nothing is readable (history, logs, config)
/// or triggerable (sessions, injection): the token is the capability.
/// (2) A `?token=` URL can still leak (copied link, browser history); the
/// `Origin` check on writes stops a leaked token from being replayed in a
/// cross-site form post or `no-cors` fetch, which browsers send without a
/// preflight. GETs skip that check: with the token they are read-only, and
/// the page's own `EventSource` and audio loads are same-origin anyway.
async fn require_token(
    State(state): State<Arc<ConsoleState>>,
    request: Request,
    next: Next,
) -> Response {
    let header_token = request
        .headers()
        .get("x-voiceless-token")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    // Naive query split: the token is hex, so no percent-decoding is needed.
    let query_token = request.uri().query().and_then(|query| {
        query
            .split('&')
            .find_map(|pair| pair.strip_prefix("token="))
            .map(str::to_owned)
    });
    // The header wins when both are present.
    let token_ok = header_token
        .or(query_token)
        .is_some_and(|given| same_token(&given, &state.token));
    if !token_ok {
        return ApiError(
            StatusCode::FORBIDDEN,
            "missing or wrong console token".to_owned(),
        )
        .into_response();
    }
    // Writes only: a cross-site form post needs no preflight, so the token
    // alone would not stop a replayed one; its Origin header does.
    if request.method() != Method::GET && !same_origin(request.headers(), state.port) {
        return ApiError(StatusCode::FORBIDDEN, "cross-origin request".to_owned()).into_response();
    }
    next.run(request).await
}

/// Browsers send `Origin` on mutating requests; it must be the console.
/// Non-browser clients (curl, tests) send none and pass on the token alone.
///
/// Accepted origins are exactly `http://127.0.0.1:<port>`,
/// `http://localhost:<port>` and `http://[::1]:<port>`; `Origin: null`
/// (sandboxed frames, `file://` pages) and every other site are refused.
/// Responses carry no CORS headers, which already keeps other origins from
/// reading them; this check covers the writes browsers send blind.
fn same_origin(headers: &HeaderMap, port: u16) -> bool {
    match headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
    {
        None => true,
        Some(origin) => [
            format!("http://127.0.0.1:{port}"),
            format!("http://localhost:{port}"),
            format!("http://[::1]:{port}"),
        ]
        .iter()
        .any(|allowed| allowed == origin),
    }
}

/// `GET /` and `GET /index.html`: the bundled console shell
/// with the token substituted for the `VOICELESS_TOKEN` placeholder in
/// `<meta name="voiceless-token">`, and [`PLATFORM`] for the
/// `VOICELESS_PLATFORM` placeholder in `<meta name="voiceless-platform">`.
/// Sent with `Cache-Control: no-store`, so the token never lands in a disk
/// cache, and `Referrer-Policy: no-referrer`, so no request made from the
/// console reveals where it came from. Always 200.
async fn index(State(state): State<Arc<ConsoleState>>) -> Response {
    let page = INDEX
        .replace("{{VOICELESS_TOKEN}}", &state.token)
        .replace("{{VOICELESS_PLATFORM}}", PLATFORM);
    (
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::REFERRER_POLICY, "no-referrer"),
        ],
        Html(page),
    )
        .into_response()
}

/// `GET /api/status` (console API "Live session", the `Status` object):
/// the daemon's `DaemonStatus` (`state`, `mode`, `backend`,
/// `fallback_backend`, `engine`, `hotkey`, `last_session`) plus
/// `daemon: true`, `default_mode`, `gpu` from `nvidia-smi`,
/// `uptime_seconds` and the crate `version`. 200, or 503 without a daemon.
/// Each call spawns `nvidia-smi` (up to 2 s); the page polls this every
/// 5 s on the live page and every 15 s elsewhere.
async fn status(State(state): State<Arc<ConsoleState>>) -> ApiResult {
    let daemon = state.daemon()?;
    let status = daemon.status();
    let mut body = serde_json::to_value(&status).map_err(ApiError::internal)?;
    body["daemon"] = json!(true);
    body["default_mode"] = json!(daemon.default_mode);
    body["gpu"] = json!(gpus().await);
    body["uptime_seconds"] = json!(daemon.started.elapsed().as_secs());
    body["version"] = json!(env!("CARGO_PKG_VERSION"));
    Ok(Json(body))
}

/// GPU memory from `nvidia-smi`, empty without it.
///
/// Runs `nvidia-smi --query-gpu=name,memory.used,memory.total` under a 2 s
/// timeout and parses the CSV into `{name, memory_used_mib,
/// memory_total_mib}`. A missing binary, a timeout or a non-zero exit
/// (empty output) yields `[]` and an unparsable line is skipped, so the
/// status call never fails because of the probe.
async fn gpus() -> Vec<Value> {
    let output = tokio::time::timeout(
        Duration::from_secs(2),
        tokio::process::Command::new("nvidia-smi")
            .args([
                "--query-gpu=name,memory.used,memory.total",
                "--format=csv,noheader,nounits",
            ])
            .output(),
    )
    .await;
    let Ok(Ok(output)) = output else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split(',').map(str::trim);
            let name = fields.next()?;
            let used: u64 = fields.next()?.parse().ok()?;
            let total: u64 = fields.next()?.parse().ok()?;
            Some(json!({"name": name, "memory_used_mib": used, "memory_total_mib": total}))
        })
        .collect()
}

/// `GET /api/events?token=` (console API "Live session"):
/// `text/event-stream` of the daemon's event lines, one `data: <json>`
/// message per line, same schema as `voiceless stream --json` and
/// `voiceless ctl watch`. 503 without a daemon; otherwise the response
/// stays open until the daemon drops its broadcast sender.
///
/// Subscribe before taking the snapshot. Clients discard events at or below
/// its sequence. A subscriber that falls behind receives a new snapshot.
async fn events(
    State(state): State<Arc<ConsoleState>>,
) -> Result<Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let daemon = state.daemon()?.clone();
    let receiver = daemon.events.subscribe();
    let initial = snapshot_event(&daemon);
    let updates =
        futures_util::stream::unfold((daemon, receiver), |(daemon, mut receiver)| async move {
            let event = match receiver.recv().await {
                Ok(line) => Event::default().data(line),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => snapshot_event(&daemon),
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
            };
            Some((Ok(event), (daemon, receiver)))
        });
    use futures_util::StreamExt;
    let stream = futures_util::stream::once(async move { Ok(initial) }).chain(updates);
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
}

fn snapshot_event(daemon: &DaemonShared) -> Event {
    Event::default().data(
        json!({
            "type": "snapshot",
            "status": daemon.status(),
            "default_mode": daemon.default_mode,
        })
        .to_string(),
    )
}

/// `POST /api/session/{action}` with `action` one of `toggle`, `start`,
/// `stop`, `cancel` (console API "Live session"). `toggle` and `start`
/// read an optional JSON body `{"mode": "..."}`; `""` or no body means the
/// daemon's default mode. `stop` and `cancel` can carry a `session_id`.
/// The command loop replies with the outcome and active session identity.
/// Unrecognized actions return 404. An unavailable daemon returns 503 and
/// a rejected action returns 409 with the gateway's reason.
async fn session(
    State(state): State<Arc<ConsoleState>>,
    axum::extract::Path(action): axum::extract::Path<String>,
    body: Option<Json<Value>>,
) -> ApiResult {
    let daemon = state.daemon()?;
    let params = body.as_ref().map(|Json(params)| params);
    if let Some(instance) = params.and_then(|params| params.get("runtime_id")) {
        if !instance.as_str().is_some_and(|id| daemon.has_instance(id)) {
            return Err(ApiError(
                StatusCode::CONFLICT,
                "the runtime has restarted".to_owned(),
            ));
        }
    }
    let session_id = params
        .and_then(|params| params.get("session_id"))
        .map(|id| {
            id.as_u64()
                .ok_or_else(|| ApiError::bad_request("invalid session_id"))
        })
        .transpose()?;
    let mode = params
        .and_then(|body| body["mode"].as_str())
        .unwrap_or_default()
        .to_owned();
    let (reply, verdict) = tokio::sync::oneshot::channel();
    let command = match (action.as_str(), session_id) {
        ("toggle", _) => DaemonCommand::Toggle {
            mode,
            reply: Some(reply),
        },
        ("start", _) => match params.and_then(|body| body.get("inject")) {
            Some(value) => DaemonCommand::StartSession {
                mode,
                inject: value
                    .as_bool()
                    .ok_or_else(|| ApiError::bad_request("invalid inject flag"))?,
                context: crate::stream::DictationContext {
                    site: params
                        .and_then(|body| body["site"].as_str())
                        .map(str::to_owned),
                    project: params
                        .and_then(|body| body["project"].as_str())
                        .map(str::to_owned),
                },
                reply: Some(reply),
            },
            None => DaemonCommand::Start {
                mode,
                reply: Some(reply),
            },
        },
        ("practice", _) => DaemonCommand::Practice {
            arm: params
                .and_then(|body| body.get("arm"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
            reply,
        },
        ("narrate", _) => DaemonCommand::Narrate { reply },
        ("disarm-practice", _) => DaemonCommand::DisarmPractice { reply },
        ("repaste", _) => DaemonCommand::Repaste { reply: Some(reply) },
        ("pause", _) => DaemonCommand::SetPaused {
            paused: params
                .and_then(|body| body.get("paused"))
                .and_then(Value::as_bool)
                .ok_or_else(|| ApiError::bad_request("missing boolean `paused`"))?,
            reply,
        },
        ("stop" | "cancel", Some(session_id)) => DaemonCommand::ControlSession {
            session_id,
            action: if action == "stop" {
                crate::coordinator::ControlCommand::Stop
            } else {
                crate::coordinator::ControlCommand::Cancel
            },
            reply,
        },
        ("stop", None) => DaemonCommand::Stop { reply: Some(reply) },
        ("cancel", None) => DaemonCommand::Cancel { reply: Some(reply) },
        (other, _) => {
            return Err(ApiError(
                StatusCode::NOT_FOUND,
                format!("unknown session action: {other}"),
            ))
        }
    };
    daemon
        .commands
        .send(command)
        .await
        .map_err(ApiError::internal)?;
    // The gateway's verdict: a rejected command (start while recording,
    // stop while idle) is 409 with the reason, so the page can say why.
    let outcome = verdict
        .await
        .map_err(ApiError::internal)?
        .map_err(|message| ApiError(StatusCode::CONFLICT, message))?;
    Ok(Json(json!({
        "ok": true,
        "outcome": crate::daemon::outcome_name(outcome.outcome),
        "session_id": outcome.session_id,
        "armed_for_ms": (outcome.outcome == Some(crate::daemon::ActivateOutcome::PracticeArmed)).then_some(crate::daemon::PRACTICE_ARM_MS),
    })))
}

#[derive(serde::Deserialize)]
struct ReloadRequest {
    revision: Option<String>,
}

/// Queue the saved revision for validation once the daemon is idle.
async fn reload(
    State(state): State<Arc<ConsoleState>>,
    body: Option<Json<ReloadRequest>>,
) -> ApiResult {
    let revision = body.and_then(|Json(body)| body.revision);
    config_command(state.daemon()?, |reply| DaemonCommand::Reload {
        revision,
        reply,
    })
    .await
}

async fn config_command(
    daemon: &DaemonShared,
    command: impl FnOnce(crate::daemon::ConfigApplyReply) -> DaemonCommand,
) -> ApiResult {
    let (reply, receiver) = tokio::sync::oneshot::channel();
    daemon
        .commands
        .send(command(reply))
        .await
        .map_err(ApiError::internal)?;
    let config_apply = receiver
        .await
        .map_err(ApiError::internal)?
        .map_err(|error| ApiError(StatusCode::CONFLICT, error))?;
    Ok(Json(json!({"ok": true, "config_apply": config_apply})))
}

#[derive(serde::Deserialize)]
struct RestoreConfigRequest {
    revision: String,
}

async fn restore_config(
    State(state): State<Arc<ConsoleState>>,
    Json(body): Json<RestoreConfigRequest>,
) -> ApiResult {
    config_command(state.daemon()?, |reply| DaemonCommand::Restore {
        revision: body.revision,
        reply,
    })
    .await
}

/// `GET /api/logs` (console API "Diagnostics"): `{"lines": [...]}`, the
/// daemon's last 500 log lines, oldest first; newer lines arrive as `log`
/// events. 503 without a daemon.
async fn logs(State(state): State<Arc<ConsoleState>>) -> ApiResult {
    Ok(Json(json!({ "lines": state.daemon()?.logs() })))
}

/// `GET /api/doctor` (console API "Diagnostics"): the `voiceless doctor`
/// checks as `[{"name","present","required","detail"}]`. Needs no daemon.
/// The checks spawn external commands and read files, so they run on the
/// blocking pool, off the runtime thread; 500 only if that task panics.
async fn doctor() -> ApiResult {
    let checks = tokio::task::spawn_blocking(crate::doctor::run_checks)
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(
        serde_json::to_value(checks).map_err(ApiError::internal)?,
    ))
}

async fn desktop_audio(State(state): State<Arc<ConsoleState>>) -> ApiResult {
    let (selected, recording) = match &state.daemon {
        Some(daemon) => (
            daemon.active_file().config.audio.source.clone(),
            daemon.session_active(),
        ),
        None => (state.config()?.audio.source, false),
    };
    let devices = match crate::desktop::audio::audio_devices().await {
        Ok(devices) => devices,
        Err(error) => {
            return Ok(Json(json!({
                "microphones": [], "system_default": null, "effective": null, "selected": selected, "error": error,
            })))
        }
    };
    let connected = |name: &&String| devices.microphones.iter().any(|mic| &mic.id == *name);
    let configured = selected.as_ref().filter(connected);
    let default = devices.system_default.as_ref().filter(connected);
    let effective = if recording {
        devices.capturing.as_ref()
    } else if selected.is_some() {
        configured
    } else {
        default
    };
    let error = if let Some(selected) = selected.as_ref().filter(|_| configured.is_none()) {
        Some(format!("Microphone {selected} is unavailable. Choose another microphone or follow the system default."))
    } else if devices.microphones.is_empty() {
        Some("No microphones are available from the operating system.".to_owned())
    } else if selected.is_none() && devices.system_default.is_some() && default.is_none() {
        Some(
            "The system default microphone is unavailable. Choose a connected microphone."
                .to_owned(),
        )
    } else {
        None
    };
    Ok(Json(json!({
        "microphones": devices.microphones,
        "system_default": devices.system_default,
        "selected": selected,
        "effective": effective,
        "error": error,
    })))
}

async fn desktop_overlay() -> ApiResult {
    Ok(Json(json!(
        crate::desktop::overlay::overlay_support().await
    )))
}

/// Browser `KeyboardEvent.code` → evdev key name, for keys that make good
/// push-to-talk keys (ones applications rarely use).
///
/// This is the whole vocabulary of the page's key-capture widget: a key
/// outside the table is reported as unmappable. It does not limit
/// `[hotkey] key` in `config.toml`, which accepts any evdev `KEY_*` name.
const HOTKEY_KEYS: &[(&str, &str)] = &[
    ("ScrollLock", "KEY_SCROLLLOCK"),
    ("Escape", "KEY_ESC"),
    ("Pause", "KEY_PAUSE"),
    ("CapsLock", "KEY_CAPSLOCK"),
    ("Insert", "KEY_INSERT"),
    ("PrintScreen", "KEY_SYSRQ"),
    ("ContextMenu", "KEY_COMPOSE"),
    ("AltRight", "KEY_RIGHTALT"),
    ("ControlRight", "KEY_RIGHTCTRL"),
    ("ShiftRight", "KEY_RIGHTSHIFT"),
    ("MetaRight", "KEY_RIGHTMETA"),
    ("NumLock", "KEY_NUMLOCK"),
    ("F1", "KEY_F1"),
    ("F2", "KEY_F2"),
    ("F3", "KEY_F3"),
    ("F4", "KEY_F4"),
    ("F5", "KEY_F5"),
    ("F6", "KEY_F6"),
    ("F7", "KEY_F7"),
    ("F8", "KEY_F8"),
    ("F9", "KEY_F9"),
    ("F10", "KEY_F10"),
    ("F11", "KEY_F11"),
    ("F12", "KEY_F12"),
    ("F13", "KEY_F13"),
    ("F14", "KEY_F14"),
    ("F15", "KEY_F15"),
    ("F16", "KEY_F16"),
    ("F17", "KEY_F17"),
    ("F18", "KEY_F18"),
    ("F19", "KEY_F19"),
    ("F20", "KEY_F20"),
    ("F21", "KEY_F21"),
    ("F22", "KEY_F22"),
    ("F23", "KEY_F23"),
    ("F24", "KEY_F24"),
    ("Numpad0", "KEY_KP0"),
    ("NumpadEnter", "KEY_KPENTER"),
    ("NumpadAdd", "KEY_KPPLUS"),
    ("NumpadSubtract", "KEY_KPMINUS"),
    ("NumpadMultiply", "KEY_KPASTERISK"),
    ("NumpadDivide", "KEY_KPSLASH"),
    ("NumpadDecimal", "KEY_KPDOT"),
];

/// `GET /api/hotkey/keys` (console API "Hotkey & injection"):
/// `HOTKEY_KEYS` as `[{"code","evdev"}]`, used by the page to turn a
/// captured `KeyboardEvent.code` into the evdev name stored in
/// `[hotkey] key`. Always 200; needs no daemon.
async fn hotkey_keys() -> ApiResult {
    Ok(Json(json!(HOTKEY_KEYS
        .iter()
        .filter(|(_, key)| crate::hotkey::parse_key(key).is_ok())
        .map(|(code, evdev)| json!({"code": code, "evdev": evdev}))
        .collect::<Vec<_>>())))
}

/// `POST /api/inject/test` (console API "Hotkey & injection"): body
/// `{"text": "…", "delay_ms": 3000}`. Waits `delay_ms` (default 3000,
/// capped at 10 000) so the user can focus a target field, then injects
/// `text` with the saved `[inject]` settings, after capturing the target
/// that has focus at the end of the countdown. The response is
/// `{"ok": <injection.ok>, "injection": InjectionResult}`; a failed
/// injection is still HTTP 200 with `ok: false` and the method's message.
/// 400 when `text` is missing or empty, 500 if the config cannot be read.
/// Runs in the console process rather than through the daemon, so it also
/// works under `history serve`, and it uses the settings on disk, not
/// unsaved edits in the page.
async fn inject_test(State(state): State<Arc<ConsoleState>>, Json(body): Json<Value>) -> ApiResult {
    let text = body["text"]
        .as_str()
        .filter(|text| !text.is_empty())
        .ok_or_else(|| ApiError::bad_request("missing `text`"))?
        .to_owned();
    // Capped so a stray request cannot park a task for long.
    let delay = body["delay_ms"].as_u64().unwrap_or(3_000).min(10_000);
    let config = state.config()?;
    // Async sleep: the page shows its 3-2-1 countdown meanwhile; no thread held.
    tokio::time::sleep(Duration::from_millis(delay)).await;
    let mut injector = crate::stream::ConfiguredInjector::new(config.inject.clone());
    let injection = crate::coordinator::TextInjector::inject(&mut injector, &text).await;
    Ok(Json(json!({ "ok": injection.ok, "injection": injection })))
}
