//! Contract of `history::HistoryStore` against a real SQLite file in a
//! temporary directory (`tempfile`): insert/get round trip, schema version
//! and 0600 mode, v1 -> v2 migration of a hand-seeded file, keyset
//! pagination, deletion together with the retained recordings, totals and
//! usage aggregates, CSV escaping and idempotent reopening. `entry` builds
//! a minimal successful-session `coordinator::HistoryEntry`.

use tempfile::tempdir;
use voiceless::coordinator::HistoryEntry;
use voiceless::history::HistoryStore;

fn entry(raw: &str, final_text: &str, mode: &str) -> HistoryEntry {
    HistoryEntry {
        raw_text: raw.to_owned(),
        processed_text: Some(final_text.to_owned()),
        final_text: final_text.to_owned(),
        processing_mode: mode.to_owned(),
        status: "completed".to_owned(),
        backend: "cloud-realtime".to_owned(),
        model: None,
        duration_seconds: None,
        audio_path: None,
    }
}

/// The stored row carries the backend as `asr_provider`, a character
/// count in Unicode scalars and a UTC `Z` timestamp.
#[test]
fn insert_and_get_roundtrip_records_authoritative_text() {
    let root = tempdir().expect("temporary root");
    let store = HistoryStore::open(root.path().join("history.sqlite3")).expect("open store");

    let stored = store
        .insert(&entry("原始", "最终文本", "quick"), "cloud-realtime")
        .expect("insert");
    assert_eq!(stored.raw_text, "原始");
    assert_eq!(stored.final_text, "最终文本");
    assert_eq!(stored.character_count, 4);
    assert_eq!(stored.asr_provider.as_deref(), Some("cloud-realtime"));
    assert!(stored.created_at.ends_with('Z'));

    let fetched = store.get(&stored.id).expect("get").expect("record exists");
    assert_eq!(fetched.raw_text, "原始");
    assert_eq!(fetched.processing_mode.as_deref(), Some("quick"));
}

#[test]
fn schema_is_version_two_and_file_permissions_are_private() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempdir().expect("temporary root");
    let path = root.path().join("history.sqlite3");
    HistoryStore::open(path.clone()).expect("open store");

    let mode = std::fs::metadata(&path)
        .expect("metadata")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600, "history file must be 0600");

    let connection = rusqlite::Connection::open(&path).expect("open sqlite");
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .expect("user_version");
    assert_eq!(version, 2);
}

#[test]
fn insert_records_audio_path_and_duration_for_playback() {
    let root = tempdir().expect("temporary root");
    let store = HistoryStore::open(root.path().join("history.sqlite3")).expect("open store");
    let mut entry = entry("你好", "你好", "quick");
    entry.duration_seconds = Some(1.25);
    entry.audio_path = Some("/tmp/audio/1.wav".to_owned());

    let stored = store.insert(&entry, "local-streaming").expect("insert");
    assert_eq!(stored.duration_seconds, Some(1.25));
    assert_eq!(stored.audio_path.as_deref(), Some("/tmp/audio/1.wav"));

    let fetched = store.get(&stored.id).expect("get").expect("record exists");
    assert_eq!(fetched.audio_path.as_deref(), Some("/tmp/audio/1.wav"));

    let page = store.query(10, None).expect("query");
    assert_eq!(page.records.len(), 1);
    assert_eq!(
        page.records[0].audio_path.as_deref(),
        Some("/tmp/audio/1.wav")
    );
}

/// A file seeded with the v1 table (no `audio_path`) opens, keeps its
/// row with `audio_path = None`, accepts v2 inserts and reports version 2.
#[test]
fn v1_database_migrates_to_v2_and_keeps_records_without_audio() {
    use rusqlite::Connection;

    let root = tempdir().expect("temporary root");
    let path = root.path().join("history.sqlite3");
    let connection = Connection::open(&path).expect("open sqlite");
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
                asr_model TEXT
            );
            INSERT INTO recognition_history (id, created_at, duration_seconds, raw_text,
                processing_mode, processed_text, final_text, status, character_count,
                asr_provider, asr_model)
            VALUES ('old-id', '2026-01-01T00:00:00Z', NULL, '旧记录', 'quick', NULL,
                '旧记录', 'completed', 3, 'cloud-realtime', NULL);
            PRAGMA user_version = 1;",
        )
        .expect("seed v1 schema");
    drop(connection);

    let store = HistoryStore::open(path.clone()).expect("open migrated store");
    let fetched = store.get("old-id").expect("get").expect("record exists");
    assert_eq!(fetched.raw_text, "旧记录");
    assert_eq!(fetched.audio_path, None);

    let mut entry = entry("新记录", "新记录", "quick");
    entry.audio_path = Some("new.wav".to_owned());
    let stored = store.insert(&entry, "cloud-realtime").expect("insert");
    assert_eq!(stored.audio_path.as_deref(), Some("new.wav"));

    let connection = Connection::open(&path).expect("reopen sqlite");
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .expect("user_version");
    assert_eq!(version, 2);
}

/// Deleting one row unlinks only its WAV; `delete_all` unlinks the rest.
#[test]
fn delete_and_delete_all_remove_retained_audio_files() {
    let root = tempdir().expect("temporary root");
    let store = HistoryStore::open(root.path().join("history.sqlite3")).expect("open store");

    let audio_one = root.path().join("one.wav");
    let audio_two = root.path().join("two.wav");
    std::fs::write(&audio_one, b"wav").expect("write one");
    std::fs::write(&audio_two, b"wav").expect("write two");

    let mut first = entry("一", "一", "quick");
    first.audio_path = Some(audio_one.display().to_string());
    let mut second = entry("二", "二", "quick");
    second.audio_path = Some(audio_two.display().to_string());
    let first = store.insert(&first, "cloud-realtime").expect("insert");
    store.insert(&second, "cloud-realtime").expect("insert");

    store.delete(&[first.id]).expect("delete one");
    assert!(!audio_one.exists(), "deleted record must drop its audio");
    assert!(audio_two.exists(), "unrelated audio must survive");

    store.delete_all().expect("delete all");
    assert!(!audio_two.exists(), "delete_all must drop remaining audio");
}
/// Five rows in pages of two: no row repeats across pages, the last
/// page has one row and no cursor; limit 0 and a garbage cursor fail.
#[test]
fn query_paginates_with_stable_cursors() {
    let root = tempdir().expect("temporary root");
    let store = HistoryStore::open(root.path().join("history.sqlite3")).expect("open store");
    for index in 0..5 {
        store
            .insert(
                &entry(&format!("raw{index}"), &format!("text{index}"), "quick"),
                "cloud-realtime",
            )
            .expect("insert");
    }

    let page = store.query(2, None).expect("first page");
    assert_eq!(page.records.len(), 2);
    let cursor = page.next_cursor.expect("more pages");

    let second = store.query(2, Some(&cursor)).expect("second page");
    assert_eq!(second.records.len(), 2);
    let first_ids: Vec<&str> = page
        .records
        .iter()
        .map(|record| record.id.as_str())
        .collect();
    assert!(
        second
            .records
            .iter()
            .all(|record| !first_ids.contains(&record.id.as_str())),
        "cursor pages must not repeat records"
    );

    let third = store
        .query(2, second.next_cursor.as_deref())
        .expect("third page");
    assert_eq!(third.records.len(), 1);
    assert!(third.next_cursor.is_none());

    assert!(store.query(0, None).is_err(), "page size must be validated");
    assert!(store.query(2, Some("not-a-cursor")).is_err());
}

#[test]
fn delete_targets_specific_ids_and_delete_all_clears() {
    let root = tempdir().expect("temporary root");
    let store = HistoryStore::open(root.path().join("history.sqlite3")).expect("open store");
    let first = store
        .insert(&entry("a", "a", "quick"), "cloud-realtime")
        .unwrap();
    let second = store
        .insert(&entry("b", "b", "quick"), "cloud-realtime")
        .unwrap();

    assert_eq!(
        store
            .delete(std::slice::from_ref(&first.id))
            .expect("delete"),
        1
    );
    assert!(store.get(&first.id).expect("get").is_none());
    assert!(store.get(&second.id).expect("get").is_some());

    assert_eq!(store.delete_all().expect("delete all"), 1);
    assert_eq!(store.totals().expect("totals").records, 0);
}

/// `characters` sums the final texts (3 + 2); `usage(7)` counts rows per
/// provider; a 3-day window is rejected.
#[test]
fn totals_and_usage_aggregate_character_counts() {
    let root = tempdir().expect("temporary root");
    let store = HistoryStore::open(root.path().join("history.sqlite3")).expect("open store");
    store
        .insert(&entry("一", "一二三", "quick"), "cloud-realtime")
        .unwrap();
    store
        .insert(&entry("二", "四五", "quick"), "local-streaming")
        .unwrap();

    let totals = store.totals().expect("totals");
    assert_eq!(totals.records, 2);
    assert_eq!(totals.characters, 5);

    let usage = store.usage(7).expect("usage");
    let providers = usage["providers"].as_object().expect("providers");
    assert_eq!(providers.get("cloud-realtime"), Some(&serde_json::json!(1)));
    assert_eq!(
        providers.get("local-streaming"),
        Some(&serde_json::json!(1))
    );
    assert!(store.usage(3).is_err(), "window must be 1, 7 or 30 days");
}

/// Commas and quotes inside fields are quoted and doubled (RFC 4180).
#[test]
fn export_csv_contains_header_and_escaped_records() {
    let root = tempdir().expect("temporary root");
    let store = HistoryStore::open(root.path().join("history.sqlite3")).expect("open store");
    store
        .insert(&entry("带,逗号", "含\"引号\"", "quick"), "cloud-realtime")
        .expect("insert");

    let destination = root.path().join("export.csv");
    let count = store.export_csv(&destination).expect("export");
    assert_eq!(count, 1);
    let content = std::fs::read_to_string(&destination).expect("read csv");
    assert!(content.starts_with("id,created_at,duration_seconds,raw_text,"));
    assert!(content.contains("\"带,逗号\""));
    assert!(content.contains("\"含\"\"引号\"\"\""));
}

#[test]
fn reopening_store_migrates_idempotently_and_keeps_records() {
    let root = tempdir().expect("temporary root");
    let path = root.path().join("history.sqlite3");
    let store = HistoryStore::open(path.clone()).expect("open store");
    let record = store
        .insert(&entry("持久", "持久", "quick"), "cloud-realtime")
        .unwrap();
    drop(store);

    let store = HistoryStore::open(path).expect("reopen store");
    assert!(store.get(&record.id).expect("get").is_some());
}
