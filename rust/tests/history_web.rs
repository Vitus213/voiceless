//! Contract tests for `voiceless history serve`: embedded console page,
//! JSON endpoints, audio streaming with Range support and id validation.

use std::path::PathBuf;
use std::sync::Arc;

use tempfile::tempdir;
use tokio::net::TcpListener;
use voiceless::capture::wav_header;
use voiceless::coordinator::HistoryEntry;
use voiceless::history::HistoryStore;
use voiceless::history_web::{serve_listener, CONSOLE_PAGE};

struct Fixture {
    base: String,
    _server: tokio::task::JoinHandle<Result<i32, String>>,
    _root: tempfile::TempDir,
    config_path: PathBuf,
}

fn entry(final_text: &str) -> HistoryEntry {
    HistoryEntry {
        raw_text: final_text.to_owned(),
        processed_text: None,
        final_text: final_text.to_owned(),
        processing_mode: "quick".to_owned(),
        status: "completed".to_owned(),
        backend: "cloud-realtime".to_owned(),
        duration_seconds: Some(0.25),
        audio_path: None,
    }
}

fn wav_bytes() -> Vec<u8> {
    let pcm: Vec<u8> = (0..512_u32)
        .flat_map(|i| (i as i16).to_le_bytes())
        .collect();
    let mut bytes = wav_header(pcm.len() as u64, 16_000).to_vec();
    bytes.extend_from_slice(&pcm);
    bytes
}

async fn start_server(with_audio: bool) -> (Fixture, String) {
    let root = tempdir().expect("temporary root");
    let store_path = root.path().join("history.sqlite3");
    let config_path = root.path().join("config.toml");
    let store = HistoryStore::open(store_path).expect("open store");

    let mut seeded = entry("你好，夜声");
    if with_audio {
        let audio_path = root.path().join("sample.wav");
        std::fs::write(&audio_path, wav_bytes()).expect("write wav");
        seeded.audio_path = Some(audio_path.display().to_string());
        seeded.duration_seconds = Some(0.016);
    }
    let record = store
        .insert(&seeded, "cloud-realtime")
        .expect("insert seeded record");

    let store = Arc::new(store);
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let port = listener.local_addr().expect("local address").port();
    let server = tokio::spawn(serve_listener(listener, store, config_path.clone()));
    let fixture = Fixture {
        base: format!("http://127.0.0.1:{port}"),
        _server: server,
        _root: root,
        config_path,
    };
    (fixture, record.id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn console_page_is_served_at_root() {
    let (fixture, _) = start_server(true).await;
    let response = ureq::get(&fixture.base).call().expect("GET /");
    assert_eq!(response.status(), 200);
    assert!(response
        .header("Content-Type")
        .unwrap_or_default()
        .contains("text/html"));
    let body = response.into_string().expect("body");
    assert_eq!(body, CONSOLE_PAGE);
    assert!(
        body.contains("Voiceless"),
        "console must carry the wordmark"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn records_endpoint_returns_json_with_audio_metadata() {
    let (fixture, id) = start_server(true).await;
    let response = ureq::get(&format!("{}/api/records?limit=10", fixture.base))
        .call()
        .expect("GET records");
    let body: serde_json::Value = serde_json::from_reader(response.into_reader()).expect("json");
    assert_eq!(body["records"].as_array().expect("records array").len(), 1);
    let record = &body["records"][0];
    assert_eq!(record["id"], id);
    assert_eq!(record["final_text"], "你好，夜声");
    assert_eq!(record["duration_seconds"], 0.016);
    assert!(record["audio_path"].is_string());
    assert!(body["next_cursor"].is_null());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn totals_endpoint_aggregates_records() {
    let (fixture, _) = start_server(true).await;
    let response = ureq::get(&format!("{}/api/totals", fixture.base))
        .call()
        .expect("GET totals");
    let body: serde_json::Value = serde_json::from_reader(response.into_reader()).expect("json");
    assert_eq!(body["records"], 1);
    assert_eq!(body["characters"], 5);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audio_endpoint_streams_the_full_wav_with_range_support() {
    let (fixture, id) = start_server(true).await;

    let full = ureq::get(&format!("{}/api/audio/{id}.wav", fixture.base))
        .call()
        .expect("GET audio");
    assert_eq!(full.status(), 200);
    assert_eq!(full.header("Content-Type").unwrap_or_default(), "audio/wav");
    assert_eq!(full.header("Accept-Ranges").unwrap_or_default(), "bytes");
    let mut bytes = Vec::new();
    full.into_reader()
        .read_to_end(&mut bytes)
        .expect("read body");
    assert_eq!(bytes, wav_bytes(), "streamed WAV must match the saved file");

    let partial = ureq::get(&format!("{}/api/audio/{id}.wav", fixture.base))
        .set("Range", "bytes=4-11")
        .call()
        .expect("GET range");
    assert_eq!(partial.status(), 206);
    assert_eq!(
        partial.header("Content-Range").unwrap_or_default(),
        format!("bytes 4-11/{}", wav_bytes().len())
    );
    let mut slice = Vec::new();
    partial
        .into_reader()
        .read_to_end(&mut slice)
        .expect("read slice");
    assert_eq!(slice, wav_bytes()[4..12]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audio_endpoint_validates_ids_and_missing_files() {
    let (fixture, _) = start_server(true).await;

    let not_uuid = ureq::get(&format!("{}/api/audio/../history.sqlite3", fixture.base)).call();
    assert_eq!(error_status(not_uuid), 404);

    let unknown = ureq::get(&format!(
        "{}/api/audio/00000000-0000-4000-8000-000000000000.wav",
        fixture.base
    ))
    .call();
    assert_eq!(error_status(unknown), 404);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audio_endpoint_reports_gone_when_the_file_is_missing() {
    let root = tempdir().expect("temporary root");
    let store = HistoryStore::open(root.path().join("history.sqlite3")).expect("open store");
    let mut seeded = entry("丢失录音");
    seeded.audio_path = Some(root.path().join("gone.wav").display().to_string());
    let record = store.insert(&seeded, "cloud-realtime").expect("insert");
    let store = Arc::new(store);

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("local address").port();
    let server = tokio::spawn(serve_listener(
        listener,
        store,
        root.path().join("config.toml"),
    ));

    let gone = ureq::get(&format!(
        "http://127.0.0.1:{port}/api/audio/{}.wav",
        record.id
    ))
    .call();
    assert_eq!(error_status(gone), 410);
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn records_without_audio_are_served_without_an_audio_link() {
    let (fixture, id) = start_server(false).await;
    let response = ureq::get(&format!("{}/api/records", fixture.base))
        .call()
        .expect("GET records");
    let body: serde_json::Value = serde_json::from_reader(response.into_reader()).expect("json");
    assert!(body["records"][0]["audio_path"].is_null());

    let missing = ureq::get(&format!("{}/api/audio/{id}.wav", fixture.base)).call();
    assert_eq!(error_status(missing), 404);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_paths_and_methods_are_rejected() {
    let (fixture, _) = start_server(true).await;
    let missing = ureq::get(&format!("{}/no-such-route", fixture.base)).call();
    assert_eq!(error_status(missing), 404);

    let posted = ureq::post(&fixture.base).send_bytes(b"{}");
    assert_eq!(error_status(posted), 405);
}

fn error_status(result: Result<ureq::Response, ureq::Error>) -> u16 {
    match result {
        Ok(response) => response.status(),
        Err(ureq::Error::Status(status, _)) => status,
        Err(error) => panic!("transport error: {error}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prompt_endpoint_reads_default_and_saves_to_config() {
    let (fixture, _) = start_server(true).await;

    // No config file yet: GET returns the builtin type4me template.
    let response = ureq::get(&format!("{}/api/prompt", fixture.base))
        .call()
        .expect("GET prompt");
    assert_eq!(response.status(), 200);
    let body: serde_json::Value =
        serde_json::from_str(&response.into_string().expect("body")).expect("json body");
    assert!(body["prompt"]
        .as_str()
        .unwrap_or("")
        .contains("清晰、可执行"));

    // POST persists the edited prompt to the config file.
    let payload = serde_json::json!({ "prompt": "整理：{text}" }).to_string();
    let posted = ureq::post(&format!("{}/api/prompt", fixture.base))
        .set("Content-Type", "application/json")
        .send_string(&payload)
        .expect("POST");
    assert_eq!(posted.status(), 200);
    let reply: serde_json::Value =
        serde_json::from_str(&posted.into_string().expect("body")).expect("json body");
    assert_eq!(reply["ok"], true, "save failed: {reply:?}");

    let saved = std::fs::read_to_string(&fixture.config_path).expect("config written");
    assert!(saved.contains("整理：{text}"));
}
