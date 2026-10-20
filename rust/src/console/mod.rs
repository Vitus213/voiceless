//! Web console (`docs/console-api.md`): live control, settings, models,
//! modes, history and diagnostics over a loopback HTTP API, served by the
//! daemon — or, without the live parts, by `voiceless history serve`.
//!
//! Every request must name a loopback `Host` (DNS rebinding), every `/api`
//! request must carry the per-start token embedded in the page (cross-site
//! requests cannot read it), and mutating requests from a browser must come
//! from the console's own origin.

mod library;
mod settings;

use std::convert::Infallible;
use std::net::SocketAddr;
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

const INDEX: &str = include_str!("../../assets/console/index.html");
const APP_JS: &str = include_str!("../../assets/console/app.js");
const STYLE_CSS: &str = include_str!("../../assets/console/style.css");

pub struct ConsoleState {
    daemon: Option<Arc<DaemonShared>>,
    store: Arc<HistoryStore>,
    config_path: PathBuf,
    token: String,
    port: u16,
    installs: library::Installs,
}

impl ConsoleState {
    fn daemon(&self) -> Result<&Arc<DaemonShared>, ApiError> {
        self.daemon.as_ref().ok_or_else(|| {
            ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "daemon not running".to_owned(),
            )
        })
    }

    fn config(&self) -> Result<crate::config::AppConfig, ApiError> {
        if self.config_path.exists() {
            crate::config::AppConfig::load(&self.config_path).map_err(ApiError::internal)
        } else {
            Ok(crate::config::AppConfig::default())
        }
    }
}

/// `{"error": message}` with a status code.
pub struct ApiError(StatusCode, String);

impl ApiError {
    fn bad_request(message: impl ToString) -> Self {
        Self(StatusCode::BAD_REQUEST, message.to_string())
    }

    fn internal(message: impl ToString) -> Self {
        Self(StatusCode::INTERNAL_SERVER_ERROR, message.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

type ApiResult = Result<Json<Value>, ApiError>;

/// Console served by the daemon: full API, including live control.
pub async fn serve_daemon(daemon: Arc<DaemonShared>, host: &str, port: u16) -> Result<(), String> {
    let store_path = crate::models::default_data_dir().join("history.sqlite3");
    let config_path = daemon.config_path.clone();
    let listener = bind(host, port).await?;
    daemon.log(format!(
        "Voiceless console: http://{}/",
        listener.local_addr().map_err(|error| error.to_string())?
    ));
    serve_listener(
        listener,
        Some(daemon),
        store_path,
        config_path,
        random_token(),
    )
    .await
}

/// `voiceless history serve`: the console without live control.
pub async fn serve_standalone(store_path: PathBuf, host: &str, port: u16) -> Result<i32, String> {
    let listener = bind(host, port).await?;
    println!(
        "Voiceless console: http://{}/",
        listener.local_addr().map_err(|error| error.to_string())?
    );
    serve_listener(
        listener,
        None,
        store_path,
        crate::config::default_config_path(),
        random_token(),
    )
    .await
    .map(|()| 0)
}

async fn bind(host: &str, port: u16) -> Result<tokio::net::TcpListener, String> {
    let address: SocketAddr = format!("{host}:{port}")
        .parse()
        .map_err(|error| format!("invalid listen address: {error}"))?;
    tokio::net::TcpListener::bind(address)
        .await
        .map_err(|error| format!("cannot bind {address}: {error}"))
}

/// Serve on a bound listener; tests bind an ephemeral port and pass their
/// own token.
pub async fn serve_listener(
    listener: tokio::net::TcpListener,
    daemon: Option<Arc<DaemonShared>>,
    store_path: PathBuf,
    config_path: PathBuf,
    token: String,
) -> Result<(), String> {
    let port = listener
        .local_addr()
        .map_err(|error| error.to_string())?
        .port();
    let store = Arc::new(HistoryStore::open(store_path).map_err(|error| error.to_string())?);
    let state = Arc::new(ConsoleState {
        daemon,
        store,
        config_path,
        token,
        port,
        installs: library::Installs::default(),
    });
    axum::serve(listener, router(state))
        .await
        .map_err(|error| error.to_string())
}

fn router(state: Arc<ConsoleState>) -> Router {
    let api = Router::new()
        .route("/status", get(status))
        .route("/events", get(events))
        .route("/session/{action}", post(session))
        .route("/engine/reload", post(reload))
        .route(
            "/config",
            get(settings::get_config).put(settings::put_config),
        )
        .route("/text/preview", post(settings::preview))
        .route(
            "/prompt",
            get(settings::get_prompt).post(settings::save_prompt),
        )
        .route("/modes", get(settings::list_modes).post(settings::add_mode))
        .route(
            "/modes/{id}",
            put(settings::update_mode).delete(settings::remove_mode),
        )
        .route("/models", get(library::list_models))
        .route("/models/{id}/install", post(library::install_model))
        .route("/models/{id}", delete(library::remove_model))
        .route("/records", get(library::records))
        .route("/records/{id}", delete(library::delete_record))
        .route("/records/{id}/retranscribe", post(library::retranscribe))
        .route("/totals", get(library::totals))
        .route("/audio/{id}", get(library::audio))
        .route("/corrections", post(library::corrections))
        .route("/benchmark", post(library::benchmark))
        .route("/hotkey/keys", get(hotkey_keys))
        .route("/inject/test", post(inject_test))
        .route("/doctor", get(doctor))
        .route("/logs", get(logs))
        .layer(middleware::from_fn_with_state(state.clone(), require_token));
    Router::new()
        .route("/", get(index))
        .route("/index.html", get(index))
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
        )
        .nest("/api", api)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_loopback_host,
        ))
        .with_state(state)
}

fn random_token() -> String {
    let mut bytes = [0_u8; 24];
    getrandom::getrandom(&mut bytes).expect("OS random source available");
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Constant-time comparison, so the token cannot be guessed byte by byte.
fn same_token(given: &str, expected: &str) -> bool {
    given.len() == expected.len()
        && given
            .bytes()
            .zip(expected.bytes())
            .fold(0_u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

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
    let allowed = [
        format!("127.0.0.1:{}", state.port),
        format!("localhost:{}", state.port),
        format!("[::1]:{}", state.port),
    ];
    if !allowed.iter().any(|candidate| candidate == host) {
        return (StatusCode::MISDIRECTED_REQUEST, "unexpected Host").into_response();
    }
    next.run(request).await
}

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
    let query_token = request.uri().query().and_then(|query| {
        query
            .split('&')
            .find_map(|pair| pair.strip_prefix("token="))
            .map(str::to_owned)
    });
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
    if request.method() != Method::GET && !same_origin(request.headers(), state.port) {
        return ApiError(StatusCode::FORBIDDEN, "cross-origin request".to_owned()).into_response();
    }
    next.run(request).await
}

/// Browsers send `Origin` on mutating requests; it must be the console.
/// Non-browser clients (curl, tests) send none and pass on the token alone.
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

async fn index(State(state): State<Arc<ConsoleState>>) -> Response {
    let page = INDEX.replace("{{VOICELESS_TOKEN}}", &state.token);
    (
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::REFERRER_POLICY, "no-referrer"),
        ],
        Html(page),
    )
        .into_response()
}

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

async fn events(
    State(state): State<Arc<ConsoleState>>,
) -> Result<Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let receiver = state.daemon()?.events.subscribe();
    let stream = futures_util::stream::unfold(receiver, |mut receiver| async move {
        loop {
            match receiver.recv().await {
                Ok(line) => return Some((Ok(Event::default().data(line)), receiver)),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
}

async fn session(
    State(state): State<Arc<ConsoleState>>,
    axum::extract::Path(action): axum::extract::Path<String>,
    body: Option<Json<Value>>,
) -> ApiResult {
    let daemon = state.daemon()?;
    let mode = body
        .as_ref()
        .and_then(|Json(body)| body["mode"].as_str())
        .unwrap_or_default()
        .to_owned();
    let command = match action.as_str() {
        "toggle" => DaemonCommand::Toggle(mode),
        "start" => DaemonCommand::Start(mode),
        "stop" => DaemonCommand::Stop,
        "cancel" => DaemonCommand::Cancel,
        other => {
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
    Ok(Json(json!({ "ok": true })))
}

async fn reload(State(state): State<Arc<ConsoleState>>) -> ApiResult {
    let daemon = state.daemon()?;
    let (reply, receiver) = tokio::sync::oneshot::channel();
    daemon
        .commands
        .send(DaemonCommand::Reload(reply))
        .await
        .map_err(ApiError::internal)?;
    let engine = receiver
        .await
        .map_err(ApiError::internal)?
        .map_err(|error| ApiError(StatusCode::CONFLICT, error))?;
    Ok(Json(json!({
        "ok": true,
        "engine": engine.description,
        "load_seconds": engine.load_seconds,
        "error": engine.error,
    })))
}

async fn logs(State(state): State<Arc<ConsoleState>>) -> ApiResult {
    Ok(Json(json!({ "lines": state.daemon()?.logs() })))
}

async fn doctor() -> ApiResult {
    let checks = tokio::task::spawn_blocking(crate::doctor::run_checks)
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(
        serde_json::to_value(checks).map_err(ApiError::internal)?,
    ))
}

/// Browser `KeyboardEvent.code` → evdev key name, for keys that make good
/// push-to-talk keys (ones applications rarely use).
const HOTKEY_KEYS: &[(&str, &str)] = &[
    ("ScrollLock", "KEY_SCROLLLOCK"),
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

async fn hotkey_keys() -> ApiResult {
    Ok(Json(json!(HOTKEY_KEYS
        .iter()
        .map(|(code, evdev)| json!({"code": code, "evdev": evdev}))
        .collect::<Vec<_>>())))
}

async fn inject_test(State(state): State<Arc<ConsoleState>>, Json(body): Json<Value>) -> ApiResult {
    let text = body["text"]
        .as_str()
        .filter(|text| !text.is_empty())
        .ok_or_else(|| ApiError::bad_request("missing `text`"))?
        .to_owned();
    let delay = body["delay_ms"].as_u64().unwrap_or(3_000).min(10_000);
    let config = state.config()?;
    tokio::time::sleep(Duration::from_millis(delay)).await;
    let mut injector = crate::stream::ConfiguredInjector::new(config.inject.clone());
    if config.inject.prefer == "fcitx5" {
        injector = injector.with_fcitx5(crate::fcitx5::Fcitx5Client::spawn());
    }
    let injection = crate::coordinator::TextInjector::inject(&mut injector, &text).await;
    Ok(Json(json!({ "ok": injection.ok, "injection": injection })))
}
