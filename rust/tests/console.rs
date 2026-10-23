//! Contract tests for the web console (`docs/console-api.md`) served
//! without a daemon (`voiceless history serve`): loopback Host and token
//! checks, history and audio endpoints, comment-preserving config edits
//! with write-only secrets, text preview, modes, correction suggestions,
//! and the live endpoints answering 503.
//!
//! Fixture (`start`): a temp dir with a `config.toml` (one comment, one
//! secret with a trailing comment, one hotword), a 1 068-byte synthetic WAV
//! and a history database holding one record that points at it;
//! `console::serve_listener` is spawned on an ephemeral loopback port with
//! `daemon = None` and the fixed token `TOKEN`. Requests are blocking
//! `ureq` calls, hence the multi-thread runtime on every test. Modes are
//! written under a process-wide `XDG_CONFIG_HOME` temp dir so no test
//! touches the real `modes.json`. The daemon-backed endpoints (session
//! control, events, reload, re-transcription, benchmark) are not covered
//! here; only their 503 fallback is.

use std::path::PathBuf;

use serde_json::{json, Value};
use tempfile::tempdir;
use tokio::net::TcpListener;
use voiceless::capture::wav_header;
use voiceless::coordinator::HistoryEntry;
use voiceless::history::HistoryStore;

/// Token handed to `serve_listener` instead of a random one.
const TOKEN: &str = "test-token";

/// One running console: its base URL, the config file it edits, the id of
/// the seeded record, and the temp dir kept alive for the test's duration.
struct Fixture {
    base: String,
    config_path: PathBuf,
    record_id: String,
    _root: tempfile::TempDir,
}

/// A 16 kHz mono PCM16 WAV of 512 ramp samples (44-byte header + 1 024
/// bytes of PCM = 1 068 bytes, the length the Range test asserts).
fn wav_bytes() -> Vec<u8> {
    let pcm: Vec<u8> = (0..512_u32)
        .flat_map(|i| (i as i16).to_le_bytes())
        .collect();
    let mut bytes = wav_header(pcm.len() as u64, 16_000).to_vec();
    bytes.extend_from_slice(&pcm);
    bytes
}

/// Build the fixture and spawn the server without a daemon; the task is
/// dropped with the runtime at the end of the test.
async fn start() -> Fixture {
    // Modes live under XDG_CONFIG_HOME: one scratch dir for the whole
    // binary, set once, so parallel tests never race on the variable.
    static XDG: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    let xdg = XDG.get_or_init(|| tempdir().unwrap());
    if std::env::var_os("XDG_CONFIG_HOME").as_deref() != Some(xdg.path().as_os_str()) {
        std::env::set_var("XDG_CONFIG_HOME", xdg.path());
    }
    let root = tempdir().unwrap();
    let store_path = root.path().join("history.sqlite3");
    let config_path = root.path().join("config.toml");
    std::fs::write(
        &config_path,
        "# my settings\n[cloud]\napi_key = \"sk-secret\" # keep me\n\n[text]\nhotwords = [\"张三丰\"]\n",
    )
    .unwrap();
    std::fs::set_permissions(
        &config_path,
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )
    .unwrap();
    let audio_path = root.path().join("sample.wav");
    std::fs::write(&audio_path, wav_bytes()).unwrap();
    let record = HistoryStore::open(store_path.clone())
        .unwrap()
        .insert(
            &HistoryEntry {
                raw_text: "我去找张三峰".to_owned(),
                processed_text: None,
                final_text: "我去找张三峰".to_owned(),
                processing_mode: "quick".to_owned(),
                status: "completed".to_owned(),
                backend: "local-offline".to_owned(),
                model: None,
                duration_seconds: Some(0.016),
                audio_path: Some(audio_path.display().to_string()),
            },
            "local-offline",
        )
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(voiceless::console::serve_listener(
        listener,
        None,
        store_path,
        config_path.clone(),
        TOKEN.to_owned(),
    ));
    Fixture {
        base: format!("http://127.0.0.1:{port}"),
        config_path,
        record_id: record.id,
        _root: root,
    }
}

/// HTTP status of a `ureq` result, treating 4xx/5xx as a status rather
/// than an error; panics on transport failures.
fn status(result: Result<ureq::Response, ureq::Error>) -> u16 {
    match result {
        Ok(response) => response.status(),
        Err(ureq::Error::Status(code, _)) => code,
        Err(error) => panic!("transport error: {error}"),
    }
}

/// Authenticated GET expecting 2xx JSON.
fn get(fixture: &Fixture, path: &str) -> Value {
    ureq::get(&format!("{}{path}", fixture.base))
        .set("X-Voiceless-Token", TOKEN)
        .call()
        .unwrap_or_else(|error| panic!("GET {path}: {error}"))
        .into_string()
        .map(|body| serde_json::from_str(&body).unwrap())
        .unwrap()
}

/// Authenticated JSON request (no `Origin`, so the same-origin check does
/// not apply); `Err((status, body))` for non-2xx so tests can assert the
/// `{"error"}` payload.
fn send(fixture: &Fixture, method: &str, path: &str, body: Value) -> Result<Value, (u16, Value)> {
    let json = |response: ureq::Response| -> Value {
        serde_json::from_str(&response.into_string().unwrap()).unwrap()
    };
    match ureq::request(method, &format!("{}{path}", fixture.base))
        .set("X-Voiceless-Token", TOKEN)
        .set("Content-Type", "application/json")
        .send_string(&body.to_string())
    {
        Ok(response) => Ok(json(response)),
        Err(ureq::Error::Status(code, response)) => Err((code, json(response))),
        Err(error) => panic!("transport error: {error}"),
    }
}

/// The served page carries the token in its meta tag with the placeholder
/// gone; `/app.js` needs no token; `/api/*` is 403 without a token or with
/// a wrong `?token=`, and 200 with the right one in the query string.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn page_embeds_the_token_and_api_requires_it() {
    let fixture = start().await;
    let page = ureq::get(&fixture.base)
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    assert!(page.contains(&format!("content=\"{TOKEN}\"")), "token meta");
    assert!(!page.contains("{{VOICELESS_TOKEN}}"));
    assert_eq!(
        status(ureq::get(&format!("{}/app.js", fixture.base)).call()),
        200
    );
    assert_eq!(
        status(ureq::get(&format!("{}/api/totals", fixture.base)).call()),
        403
    );
    assert_eq!(
        status(ureq::get(&format!("{}/api/totals?token=wrong", fixture.base)).call()),
        403
    );
    assert_eq!(
        status(ureq::get(&format!("{}/api/totals?token={TOKEN}", fixture.base)).call()),
        200
    );
}

/// A non-loopback `Host` is 421 even with a valid token; a POST with a
/// valid token but a foreign `Origin` is 403.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreign_host_and_cross_origin_writes_are_rejected() {
    let fixture = start().await;
    // DNS rebinding: a non-loopback Host never reaches a handler.
    let rebound = ureq::get(&format!("{}/api/totals", fixture.base))
        .set("Host", "evil.example:80")
        .set("X-Voiceless-Token", TOKEN)
        .call();
    assert_eq!(status(rebound), 421);
    let cross = ureq::post(&format!("{}/api/prompt", fixture.base))
        .set("X-Voiceless-Token", TOKEN)
        .set("Origin", "https://evil.example")
        .set("Content-Type", "application/json")
        .send_string(&json!({"prompt": "pwned"}).to_string());
    assert_eq!(status(cross), 403);
}

/// The seeded record is listed with `has_audio`, totals count it, a byte
/// range on its WAV answers 206 with the right `Content-Range`, and
/// deleting the record brings totals to zero.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn history_records_audio_and_delete() {
    let fixture = start().await;
    let records = get(&fixture, "/api/records?limit=10");
    let record = &records["records"][0];
    assert_eq!(record["id"], fixture.record_id);
    assert_eq!(record["has_audio"], true);
    assert_eq!(get(&fixture, "/api/totals")["records"], 1);

    let audio = ureq::get(&format!(
        "{}/api/audio/{}?token={TOKEN}",
        fixture.base, fixture.record_id
    ))
    .set("Range", "bytes=0-43")
    .call()
    .unwrap();
    assert_eq!(audio.status(), 206);
    assert_eq!(audio.header("Content-Range"), Some("bytes 0-43/1068"));
    let reversed = ureq::get(&format!(
        "{}/api/audio/{}?token={TOKEN}",
        fixture.base, fixture.record_id
    ))
    .set("Range", "bytes=50-10")
    .call();
    assert_eq!(
        status(reversed),
        416,
        "a reversed range is unsatisfiable, not a panic"
    );

    send(
        &fixture,
        "DELETE",
        &format!("/api/records/{}", fixture.record_id),
        json!(null),
    )
    .unwrap();
    assert_eq!(get(&fixture, "/api/totals")["records"], 0);
}

/// Reading masks the secret and reports `has_api_key`; writing `""` keeps
/// it while other keys update, both comments survive and replacements are
/// written as `[[text.replacements]]`; a hotword change sets
/// `restart_engine`; an invalid value is 400 and leaves the file
/// byte-identical; `null` clears the secret.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config_edits_keep_comments_and_secrets_are_write_only() {
    let fixture = start().await;
    let config = get(&fixture, "/api/config");
    assert_eq!(config["cloud"]["api_key"], "");
    assert_eq!(config["cloud"]["has_api_key"], true);

    // "" keeps the secret; other keys update; comments survive.
    let saved = send(
        &fixture,
        "PUT",
        "/api/config",
        json!({"cloud": {"api_key": "", "has_api_key": true}, "text": {"hotwords": ["张三丰", "通义千问"], "replacements": [{"from": "a", "to": "b"}]}}),
    )
    .unwrap();
    assert_eq!(saved["restart_engine"], true, "hotwords bias the engine");
    let text = std::fs::read_to_string(&fixture.config_path).unwrap();
    assert!(
        text.contains("# my settings") && text.contains("# keep me"),
        "{text}"
    );
    assert!(text.contains("sk-secret"));
    assert!(text.contains("[[text.replacements]]"), "{text}");

    // An invalid value is rejected before anything is written.
    let (code, error) = send(
        &fixture,
        "PUT",
        "/api/config",
        json!({"local": {"provider": "rocm"}}),
    )
    .unwrap_err();
    assert_eq!(code, 400);
    assert!(error["error"].as_str().unwrap().contains("provider"));
    assert_eq!(std::fs::read_to_string(&fixture.config_path).unwrap(), text);

    // null clears a secret.
    send(
        &fixture,
        "PUT",
        "/api/config",
        json!({"cloud": {"api_key": null}}),
    )
    .unwrap();
    assert_eq!(get(&fixture, "/api/config")["cloud"]["has_api_key"], false);
}

/// A `config` in the body (hotword 张三丰 plus the default filler removal)
/// is applied instead of the saved rules; a regex rule that does not
/// compile is 400.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn text_preview_uses_the_unsaved_rules() {
    let fixture = start().await;
    let preview = send(
        &fixture,
        "POST",
        "/api/text/preview",
        json!({"text": "呃，我去找张三峰", "config": {"hotwords": ["张三丰"]}}),
    )
    .unwrap();
    assert_eq!(preview["text"], "我去找张三丰");
    let (code, _) = send(
        &fixture,
        "POST",
        "/api/text/preview",
        json!({"text": "x", "config": {"rules": [{"pattern": "(", "replace": ""}]}}),
    )
    .unwrap_err();
    assert_eq!(code, 400);
}

/// Custom mode create/update/delete round-trips and deleting a built-in is
/// 409; the organize prompt saves through the comment-preserving writer;
/// the correction diff suggests exactly `张三峰 → 张三丰`; `/api/status`
/// and `/api/logs` are 503 without a daemon.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn modes_prompt_corrections_and_live_endpoints() {
    let fixture = start().await;
    let created = send(
        &fixture,
        "POST",
        "/api/modes",
        json!({"name": "邮件", "prompt": "写成邮件：{text}", "processing_label": "写邮件中"}),
    )
    .unwrap();
    let id = created["id"].as_str().unwrap().to_owned();
    let updated = send(
        &fixture,
        "PUT",
        &format!("/api/modes/{id}"),
        json!({"name": "正式邮件"}),
    )
    .unwrap();
    assert_eq!(updated["name"], "正式邮件");
    let builtin = get(&fixture, "/api/modes")[0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let (code, _) = send(
        &fixture,
        "DELETE",
        &format!("/api/modes/{builtin}"),
        json!(null),
    )
    .unwrap_err();
    assert_eq!(code, 409);
    send(&fixture, "DELETE", &format!("/api/modes/{id}"), json!(null)).unwrap();

    send(
        &fixture,
        "POST",
        "/api/prompt",
        json!({"prompt": "整理：{text}"}),
    )
    .unwrap();
    assert_eq!(get(&fixture, "/api/prompt")["prompt"], "整理：{text}");
    assert!(std::fs::read_to_string(&fixture.config_path)
        .unwrap()
        .contains("# keep me"));

    let suggestions = send(
        &fixture,
        "POST",
        "/api/corrections",
        json!({"original": "我去找张三峰聊聊", "corrected": "我去找张三丰聊聊"}),
    )
    .unwrap();
    assert_eq!(
        suggestions["suggestions"],
        json!([{"from": "张三峰", "to": "张三丰"}])
    );

    for path in ["/api/status", "/api/logs"] {
        let response = ureq::get(&format!("{}{path}", fixture.base))
            .set("X-Voiceless-Token", TOKEN)
            .call();
        assert_eq!(status(response), 503, "{path} without a daemon");
    }
}
