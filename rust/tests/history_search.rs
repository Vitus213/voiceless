//! The console's history search over authenticated loopback HTTP, served
//! without a daemon as `voiceless history serve` does. `GET /api/records`
//! searches a library an earlier build left (schema v2; the same month and
//! day in two years; nine rows in one second) by text, time range, mode,
//! application, model and recording, and old rows stay readable with what
//! they never recorded unset; pages keep their cursor exact while rows are
//! appended between requests; `/api/records/facets` lists what the menus
//! offer and `/api/records/export` selects the same rows as the list. A
//! filter value that does not read is 400; a call without filter
//! parameters and a cursor written by an earlier build page as before;
//! everything under `/api` needs the token.
//!
//! Fixture (`start`): a temp dir with a hand-seeded v2 `history.sqlite3`, a
//! `config.toml` and a WAV; `console::serve_listener` is spawned on an
//! ephemeral loopback port with `daemon = None` and the fixed token
//! `TOKEN`. Requests are blocking `ureq` calls, hence the multi-thread
//! runtime on every test.

use std::path::PathBuf;

use rusqlite::{params, Connection};
use serde_json::{json, Value};
use tempfile::tempdir;
use tokio::net::TcpListener;
use voiceless::capture::wav_header;

/// Token handed to `serve_listener` instead of a random one.
const TOKEN: &str = "test-token";

/// The rows that share `2026-01-01T00:00:00Z`, oldest first by id.
const TIED: [&str; 9] = [
    "tied-1", "tied-2", "tied-3", "tied-4", "tied-5", "tied-6", "tied-7", "tied-8", "tied-9",
];

/// Every row with "会议" in it, newest first.
const MEETINGS: [&str; 14] = [
    "app-unknown",
    "app-firefox",
    "app-kitty",
    "legacy-2026",
    "tied-9",
    "tied-8",
    "tied-7",
    "tied-6",
    "tied-5",
    "tied-4",
    "tied-3",
    "tied-2",
    "tied-1",
    "legacy-2025",
];

/// The first line of the CSV export: the record's fields in order.
const CSV_HEADER: &str = "id,created_at,duration_seconds,raw_text,processing_mode,processed_text,final_text,status,character_count,asr_provider,asr_model,audio_path,app_id";

/// One running console: its base URL, the history database (which the
/// tests append to while it runs), its WAV, and the temp dir kept alive.
struct Console {
    base: String,
    store: PathBuf,
    wav: PathBuf,
    _root: tempfile::TempDir,
}

/// A row of the current schema as a test appends it while the server runs.
struct Added {
    id: &'static str,
    created_at: &'static str,
    text: &'static str,
    mode: &'static str,
    model: Option<&'static str>,
    /// Path of a saved recording; `None` when the row keeps none.
    audio: Option<String>,
    app: Option<&'static str>,
}

fn added(id: &'static str, created_at: &'static str, text: &'static str) -> Added {
    Added {
        id,
        created_at,
        text,
        mode: "quick",
        model: None,
        audio: None,
        app: None,
    }
}

/// A 16 kHz mono PCM16 WAV of 256 silent samples.
fn wav_bytes() -> Vec<u8> {
    let pcm = vec![0_u8; 512];
    let mut bytes = wav_header(pcm.len() as u64, 16_000).to_vec();
    bytes.extend_from_slice(&pcm);
    bytes
}

/// Start a console over the library an earlier build left: schema v2, the
/// same month and day in two years, nine rows in one second and one row
/// about something else. The server brings the file to the current schema
/// when it opens it.
async fn start() -> Console {
    let root = tempdir().unwrap();
    let store = root.path().join("history.sqlite3");
    let wav = root.path().join("sample.wav");
    std::fs::write(&wav, wav_bytes()).unwrap();
    let config = root.path().join("config.toml");
    std::fs::write(&config, "[history]\nenabled = true\n").unwrap();
    std::fs::set_permissions(&config, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();

    let connection = Connection::open(&store).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE recognition_history (
                id TEXT PRIMARY KEY,
                created_at TEXT NOT NULL,
                duration_seconds REAL,
                raw_text TEXT NOT NULL,
                processing_mode TEXT,
                processed_text TEXT,
                final_text TEXT NOT NULL,
                status TEXT NOT NULL,
                character_count INTEGER,
                asr_provider TEXT,
                asr_model TEXT,
                audio_path TEXT
            );
            PRAGMA user_version = 2;",
        )
        .unwrap();
    {
        let mut insert = connection
            .prepare(
                "INSERT INTO recognition_history (id, created_at, duration_seconds, raw_text,
                    processing_mode, processed_text, final_text, status, character_count,
                    asr_provider, asr_model, audio_path)
                 VALUES (?1, ?2, NULL, ?3, 'quick', NULL, ?3, 'completed', 0,
                         'cloud-realtime', NULL, NULL)",
            )
            .unwrap();
        let legacy = [
            (
                "legacy-2026",
                "2026-03-14T02:00:00.123456Z",
                "张三丰的 PostgreSQL 会议",
            ),
            (
                "legacy-2025",
                "2025-03-14T02:00:00.123456Z",
                "张三丰的 PostgreSQL 会议",
            ),
            ("plain", "2026-06-01T00:00:00Z", "别的事情"),
        ];
        for (id, created_at, text) in legacy {
            insert.execute(params![id, created_at, text]).unwrap();
        }
        for id in TIED {
            insert
                .execute(params![id, "2026-01-01T00:00:00Z", "同一秒的会议"])
                .unwrap();
        }
    }
    drop(connection);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(voiceless::console::serve_listener(
        listener,
        None,
        store.clone(),
        config,
        TOKEN.to_owned(),
    ));
    Console {
        base: format!("http://127.0.0.1:{port}"),
        store,
        wav,
        _root: root,
    }
}

/// Write `rows` to the running console's database, the way a session that
/// finished in the background leaves them.
fn append(console: &Console, rows: Vec<Added>) {
    let connection = Connection::open(&console.store).unwrap();
    let mut insert = connection
        .prepare(
            "INSERT INTO recognition_history (id, created_at, duration_seconds, raw_text,
                processing_mode, processed_text, final_text, status, character_count,
                asr_provider, asr_model, audio_path, app_id)
             VALUES (?1, ?2, NULL, ?3, ?4, NULL, ?3, 'completed', 0, 'local-offline',
                     ?5, ?6, ?7)",
        )
        .unwrap();
    for row in rows {
        insert
            .execute(params![
                row.id,
                row.created_at,
                row.text,
                row.mode,
                row.model,
                row.audio,
                row.app,
            ])
            .unwrap();
    }
}

/// A console of `start` once the server has opened, and so migrated, the
/// store, holding the rows the current schema adds: an application, a
/// model and a recording to filter by.
async fn started() -> Console {
    let console = start().await;
    let (status, _) = get(&console, "/api/totals");
    assert_eq!(status, 200);
    let sample = console.wav.display().to_string();
    let gone = console
        .store
        .with_file_name("gone.wav")
        .display()
        .to_string();
    append(
        &console,
        vec![
            Added {
                mode: "prompt-optimize",
                model: Some("qwen3-asr-0.6b-gguf"),
                app: Some("kitty"),
                ..added("app-kitty", "2026-07-01T00:00:00Z", "kitty 里的会议")
            },
            Added {
                audio: Some(sample),
                app: Some("org.mozilla.firefox"),
                ..added("app-firefox", "2026-07-02T00:00:00Z", "Firefox 的会议")
            },
            // Names a recording whose file is gone.
            Added {
                audio: Some(gone),
                ..added("app-unknown", "2026-07-03T00:00:00Z", "没有应用的会议")
            },
        ],
    );
    console
}

/// `value` as a query-string component: unreserved characters as they are,
/// every other byte of its UTF-8 as `%XX`.
fn encoded(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// A query string from `parameters`, without its `?`.
fn query(parameters: &[(&str, &str)]) -> String {
    let mut query = String::new();
    for (index, (name, value)) in parameters.iter().enumerate() {
        if index > 0 {
            query.push('&');
        }
        query.push_str(name);
        query.push('=');
        query.push_str(&encoded(value));
    }
    query
}

/// Authenticated GET; `(status, JSON body)`, so a refusal can be asserted.
fn get(console: &Console, path: &str) -> (u16, Value) {
    let body = |response: ureq::Response| -> Value {
        serde_json::from_str(&response.into_string().unwrap()).unwrap()
    };
    match ureq::get(&format!("{}{path}", console.base))
        .set("X-Voiceless-Token", TOKEN)
        .call()
    {
        Ok(response) => (response.status(), body(response)),
        Err(ureq::Error::Status(code, response)) => (code, body(response)),
        Err(error) => panic!("transport error: {error}"),
    }
}

/// Status of a GET without the token.
fn unsigned(console: &Console, path: &str) -> u16 {
    match ureq::get(&format!("{}{path}", console.base)).call() {
        Ok(response) => response.status(),
        Err(ureq::Error::Status(code, _)) => code,
        Err(error) => panic!("transport error: {error}"),
    }
}

/// `GET /api/records?…`; `(status, JSON body)`.
fn records(console: &Console, parameters: &[(&str, &str)]) -> (u16, Value) {
    get(console, &format!("/api/records?{}", query(parameters)))
}

/// Ids of a page's records, in order.
fn ids(page: &Value) -> Vec<String> {
    page["records"]
        .as_array()
        .expect("records")
        .iter()
        .map(|record| record["id"].as_str().expect("id").to_owned())
        .collect()
}

/// Every id the search for `text` selects, following `next_cursor` with
/// pages of `limit` rows.
fn all(console: &Console, text: &str, limit: i64) -> Vec<String> {
    let mut found = Vec::new();
    let mut cursor = None;
    for _ in 0..1_000 {
        let limit = limit.to_string();
        let mut parameters = vec![("q", text), ("limit", limit.as_str())];
        if let Some(cursor) = cursor.as_deref() {
            parameters.push(("cursor", cursor));
        }
        let (status, page) = records(console, &parameters);
        assert_eq!(status, 200, "{}", page);
        found.extend(ids(&page));
        cursor = page["next_cursor"].as_str().map(str::to_owned);
        if cursor.is_none() {
            return found;
        }
    }
    panic!("the search never reached its last page");
}

/// A caller from before the search gets every row as before; a Chinese
/// word and an English API name are found wherever they are, last year's
/// result among them; fourteen matches cross pages of five without a
/// repeat or a gap; the column filters select their rows; a filter value
/// that does not read is 400, a cursor under another filter 500, and a
/// cursor an earlier build wrote still pages.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn records_search_the_whole_library_and_old_rows_stay_readable() {
    use base64::Engine;

    let console = started().await;
    let list = |parameters: &[(&str, &str)]| -> Value {
        let (status, body) = records(&console, parameters);
        assert_eq!(status, 200, "{}", body);
        body
    };

    // No filter: newest first, paged by cursor.
    let page = list(&[("limit", "3")]);
    assert_eq!(ids(&page), ["app-unknown", "app-firefox", "app-kitty"]);
    let cursor = page["next_cursor"].as_str().expect("more pages");
    let page = list(&[("limit", "3"), ("cursor", cursor)]);
    assert_eq!(ids(&page), ["plain", "legacy-2026", "tied-9"]);

    // A cursor an earlier build wrote: the array of a row's time and id.
    let written_earlier = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(br#"["2026-06-01T00:00:00Z","plain"]"#);
    let page = list(&[("limit", "2"), ("cursor", written_earlier.as_str())]);
    assert_eq!(ids(&page), ["legacy-2026", "tied-9"]);

    // What the earlier build never recorded reads as unset.
    for needle in ["postgresql", "POSTGRESQL"] {
        let page = list(&[("q", needle), ("limit", "2")]);
        assert_eq!(ids(&page), ["legacy-2026", "legacy-2025"]);
        assert!(page["next_cursor"].is_null());
        for record in page["records"].as_array().unwrap() {
            assert_eq!(record["app_id"], Value::Null);
            assert_eq!(record["has_audio"], false);
        }
    }

    // Last year's result, by the bounds of the year as UTC instants.
    let page = list(&[
        ("q", "张三丰"),
        ("from", "2025-01-01T00:00:00Z"),
        ("to", "2026-01-01T00:00:00Z"),
    ]);
    assert_eq!(ids(&page), ["legacy-2025"]);
    let page = list(&[
        ("from", "2025-01-01T00:00:00Z"),
        ("to", "2026-01-01T00:00:00Z"),
    ]);
    assert_eq!(ids(&page), ["legacy-2025"]);
    let page = list(&[
        ("from", "2026-01-01T00:00:00Z"),
        ("to", "2026-01-01T00:00:01Z"),
    ]);
    let newest_first: Vec<String> = TIED.iter().rev().map(|id| (*id).to_owned()).collect();
    assert_eq!(ids(&page), newest_first);

    // Across pages: five, five, four; nothing repeated, nothing missing.
    let mut seen = Vec::new();
    let mut cursor = None;
    for expected in [5, 5, 4] {
        let mut parameters = vec![("q", "会议"), ("limit", "5")];
        if let Some(cursor) = cursor.as_deref() {
            parameters.push(("cursor", cursor));
        }
        let page = list(&parameters);
        assert_eq!(page["records"].as_array().unwrap().len(), expected);
        seen.extend(ids(&page));
        cursor = page["next_cursor"].as_str().map(str::to_owned);
    }
    assert!(cursor.is_none());
    assert_eq!(seen, MEETINGS);

    // The column filters.
    assert_eq!(ids(&list(&[("mode", "prompt-optimize")])), ["app-kitty"]);
    assert_eq!(ids(&list(&[("app", "kitty")])), ["app-kitty"]);
    assert_eq!(
        ids(&list(&[("model", "qwen3-asr-0.6b-gguf")])),
        ["app-kitty"]
    );
    // A row keeps a recording when it names one, also when the file is gone.
    assert_eq!(
        ids(&list(&[("audio", "with")])),
        ["app-unknown", "app-firefox"]
    );
    assert_eq!(
        ids(&list(&[("audio", "without"), ("app", "kitty")])),
        ["app-kitty"]
    );
    assert_eq!(ids(&list(&[("audio", "without")])).len(), 13);
    let page = list(&[("app", "org.mozilla.firefox")]);
    assert_eq!(page["records"][0]["has_audio"], true);
    assert_eq!(page["records"][0]["app_id"], "org.mozilla.firefox");

    // A filter value that does not read is the client's mistake.
    let (status, body) = records(&console, &[("from", "2025-13-01")]);
    assert_eq!(status, 400);
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("from must be an RFC 3339 time on a whole second"),
        "{body}"
    );
    assert_eq!(records(&console, &[("audio", "maybe")]).0, 400);
    assert_eq!(
        records(
            &console,
            &[
                ("from", "2026-01-01T00:00:00Z"),
                ("to", "2025-01-01T00:00:00Z")
            ]
        )
        .0,
        400
    );

    // A cursor belongs to the filter that issued it.
    let issued = list(&[("q", "会议"), ("limit", "1")]);
    let cursor = issued["next_cursor"].as_str().unwrap();
    let (status, body) = records(&console, &[("q", "别的事情"), ("cursor", cursor)]);
    assert_eq!(status, 500);
    assert_eq!(body, json!({ "error": "history cursor invalid" }));

    assert_eq!(unsigned(&console, "/api/records"), 403);
    assert_eq!(unsigned(&console, "/api/records?q=%E4%BC%9A%E8%AE%AE"), 403);
}

/// Nine rows share a timestamp and fill pages of four. Between two pages a
/// dictation finishes and a row lands in the tied second above the cursor:
/// the next pages hold exactly the rows the first page left off with, and
/// what landed shows once the search starts over.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pages_stay_exact_while_dictations_land_between_requests() {
    let console = started().await;
    let (status, page) = records(&console, &[("q", "同一秒"), ("limit", "4")]);
    assert_eq!(status, 200);
    assert_eq!(ids(&page), ["tied-9", "tied-8", "tied-7", "tied-6"]);
    let cursor = page["next_cursor"].as_str().expect("more pages").to_owned();

    append(
        &console,
        vec![
            added("late", "2099-01-01T00:00:00Z", "同一秒的会议"),
            added("tied-9a", "2026-01-01T00:00:00Z", "同一秒的会议"),
        ],
    );

    let (status, page) = records(
        &console,
        &[("q", "同一秒"), ("limit", "4"), ("cursor", cursor.as_str())],
    );
    assert_eq!(status, 200);
    assert_eq!(ids(&page), ["tied-5", "tied-4", "tied-3", "tied-2"]);
    let cursor = page["next_cursor"].as_str().expect("one more page");
    let (status, page) = records(
        &console,
        &[("q", "同一秒"), ("limit", "4"), ("cursor", cursor)],
    );
    assert_eq!(status, 200);
    assert_eq!(ids(&page), ["tied-1"]);
    assert!(page["next_cursor"].is_null());

    let mut expected = vec!["late", "tied-9a"];
    expected.extend(TIED.iter().rev().copied());
    assert_eq!(all(&console, "同一秒", 4), expected);
}

/// The facets list what the library holds and the export holds the rows
/// the list shows under the same parameters, with what a row never
/// recorded as an empty field; both need the token and both refuse a
/// filter value the store rejects.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn facets_and_export_follow_the_library_and_the_filter() {
    let console = started().await;

    let (status, facets) = get(&console, "/api/records/facets");
    assert_eq!(status, 200);
    assert_eq!(
        facets,
        json!({
            "modes": [
                { "value": "quick", "records": 14 },
                { "value": "prompt-optimize", "records": 1 }
            ],
            "apps": [
                { "value": "kitty", "records": 1 },
                { "value": "org.mozilla.firefox", "records": 1 }
            ],
            "models": [{ "value": "qwen3-asr-0.6b-gguf", "records": 1 }],
        })
    );

    // `limit` and `cursor` do not narrow an export.
    let path = format!(
        "/api/records/export?{}",
        query(&[("q", "会议"), ("limit", "1")])
    );
    let (status, exported) = get(&console, &path);
    assert_eq!(status, 200);
    assert_eq!(exported["exported"], 14);
    let lines: Vec<&str> = exported["csv"].as_str().unwrap().lines().collect();
    assert_eq!(lines.len(), 15);
    assert_eq!(lines[0], CSV_HEADER);
    let listed: Vec<&str> = lines[1..]
        .iter()
        .map(|line| line.split(',').next().unwrap())
        .collect();
    assert_eq!(listed, MEETINGS);
    assert_eq!(listed, all(&console, "会议", 3));
    // The last field is the application: unset, a value, unset.
    assert!(
        lines[1].ends_with(&format!("{},", gone_path(&console))),
        "{}",
        lines[1]
    );
    assert!(
        lines[2].ends_with("sample.wav,org.mozilla.firefox"),
        "{}",
        lines[2]
    );
    assert!(
        lines[3].ends_with("qwen3-asr-0.6b-gguf,,kitty"),
        "{}",
        lines[3]
    );
    assert!(lines[4].ends_with("cloud-realtime,,,"), "{}", lines[4]);

    assert_eq!(
        get(&console, "/api/records/export?from=yesterday").0,
        400,
        "an export refuses what the list refuses"
    );
    assert_eq!(unsigned(&console, "/api/records/export"), 403);
    assert_eq!(unsigned(&console, "/api/records/facets"), 403);
}

/// The path of the recording whose file is gone (see `started`).
fn gone_path(console: &Console) -> String {
    console
        .store
        .with_file_name("gone.wav")
        .display()
        .to_string()
}

/// Bringing a library of an earlier build to the current schema leaves it
/// as private as the store keeps every history: readable by its owner only.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_migrated_database_stays_private() {
    use std::os::unix::fs::PermissionsExt;

    let console = started().await;
    let mode = std::fs::metadata(&console.store)
        .expect("metadata")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}
