//! Contract of `history::HistoryStore` against a real SQLite file in a
//! temporary directory (`tempfile`): insert/get round trip, schema version
//! and 0600 mode, v1 and v2 -> v3 migration of hand-seeded files, search
//! over the whole library (text, time range, mode, application, model,
//! recording), keyset pagination that stays exact across timestamp ties
//! and rows appended while paging, cursors bound to their filter, the CSV
//! export of the same selection, deletion together with the retained
//! recordings, totals and usage aggregates, CSV escaping and idempotent
//! reopening. `entry` builds a minimal successful-session
//! `coordinator::HistoryEntry`; `seed` writes rows with the times and the
//! metadata a test chooses.

use std::path::Path;
use std::time::{Duration, Instant};

use rusqlite::{params, Connection};
use tempfile::tempdir;
use voiceless::coordinator::HistoryEntry;
use voiceless::history::{
    HistoryError, HistoryFacet, HistoryFacets, HistoryFilter, HistoryPage, HistoryStore,
};

/// The first line of every CSV export: the record's fields in order.
const CSV_HEADER: &str = "id,created_at,duration_seconds,raw_text,processing_mode,processed_text,final_text,status,character_count,asr_provider,asr_model,audio_path,app_id";

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

/// A row as `seed` writes it: raw and final text are both `text` unless
/// `raw` is set, the mode is `quick` and nothing else is recorded.
struct Row {
    id: &'static str,
    created_at: &'static str,
    text: &'static str,
    raw: Option<&'static str>,
    mode: Option<&'static str>,
    app: Option<&'static str>,
    model: Option<&'static str>,
    /// Path of the saved recording; `None` when the row keeps none.
    audio: Option<&'static str>,
}

fn row(id: &'static str, created_at: &'static str, text: &'static str) -> Row {
    Row {
        id,
        created_at,
        text,
        raw: None,
        mode: Some("quick"),
        app: None,
        model: None,
        audio: None,
    }
}

/// Write `rows` straight into `path` in one transaction: the store picks a
/// row's time itself, and ties and dates are what these tests are about.
/// The file must already have the current schema (`HistoryStore::open`).
fn seed(path: &Path, rows: &[Row]) {
    let mut connection = Connection::open(path).expect("open sqlite");
    let transaction = connection.transaction().expect("begin");
    {
        let mut insert = transaction
            .prepare(
                "INSERT INTO recognition_history (id, created_at, duration_seconds, raw_text,
                    processing_mode, processed_text, final_text, status, character_count,
                    asr_provider, asr_model, audio_path, app_id)
                 VALUES (?1, ?2, NULL, ?3, ?4, NULL, ?5, 'completed', 0,
                         'local-offline', ?6, ?7, ?8)",
            )
            .expect("prepare insert");
        for row in rows {
            insert
                .execute(params![
                    row.id,
                    row.created_at,
                    row.raw.unwrap_or(row.text),
                    row.mode,
                    row.text,
                    row.model,
                    row.audio,
                    row.app,
                ])
                .expect("insert row");
        }
    }
    transaction.commit().expect("commit");
}

/// `PRAGMA user_version` of the file.
fn user_version(path: &Path) -> i64 {
    let connection = Connection::open(path).expect("open sqlite");
    connection
        .pragma_query_value(None, "user_version", |value| value.get(0))
        .expect("user_version")
}

/// Ids of a page's rows, in order.
fn ids(page: &HistoryPage) -> Vec<String> {
    page.records
        .iter()
        .map(|record| record.id.clone())
        .collect()
}

/// Every row a search selects, read page by page of `limit` rows.
fn collect(store: &HistoryStore, filter: &HistoryFilter, limit: i64) -> Vec<String> {
    let mut found = Vec::new();
    let mut cursor = None;
    for _ in 0..1_000 {
        let page = store
            .search(filter, limit, cursor.as_deref())
            .expect("search page");
        found.extend(ids(&page));
        cursor = page.next_cursor;
        if cursor.is_none() {
            return found;
        }
    }
    panic!("the search never reached its last page");
}

/// A filter with only its text set.
fn text(text: &str) -> HistoryFilter {
    HistoryFilter {
        text: text.to_owned(),
        ..HistoryFilter::default()
    }
}

/// The ids a page (or several pages) returned, in order, as the test expects.
fn assert_ids(actual: Vec<String>, expected: &[&str]) {
    let expected: Vec<String> = expected.iter().map(|id| (*id).to_owned()).collect();
    assert_eq!(actual, expected);
}

/// Time `run` and report it with the test's output (`-- --nocapture`).
fn timed<T>(label: &str, run: impl FnOnce() -> T) -> (T, Duration) {
    let started = Instant::now();
    let value = run();
    let elapsed = started.elapsed();
    eprintln!("{label}: {} ms", elapsed.as_millis());
    (value, elapsed)
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

/// `insert_with_app` keeps the application the dictation went to, as the
/// `app` filter and the row's `app_id` read it; whitespace around the id
/// is dropped and nothing left, like `insert`, records no application.
#[test]
fn insert_with_app_keeps_the_target_application() {
    let root = tempdir().expect("temporary root");
    let store = HistoryStore::open(root.path().join("history.sqlite3")).expect("open store");
    let entry = entry("你好", "你好", "quick");

    let stored = store
        .insert_with_app(&entry, "cloud-realtime", Some("  org.mozilla.firefox\n"))
        .expect("insert");
    let app = Some("org.mozilla.firefox");
    assert_eq!(stored.app_id.as_deref(), app);
    let fetched = store.get(&stored.id).expect("get").expect("record exists");
    assert_eq!(fetched.app_id.as_deref(), app);

    let blank = store
        .insert_with_app(&entry, "cloud-realtime", Some("  "))
        .expect("insert");
    assert_eq!(blank.app_id, None);
    let plain = store.insert(&entry, "cloud-realtime").expect("insert");
    assert_eq!(plain.app_id, None);

    let kitty = HistoryFilter {
        app: Some("kitty".to_owned()),
        ..HistoryFilter::default()
    };
    assert!(store
        .search(&kitty, 10, None)
        .expect("search")
        .records
        .is_empty());
    let firefox = HistoryFilter {
        app: Some("org.mozilla.firefox".to_owned()),
        ..HistoryFilter::default()
    };
    assert_ids(
        ids(&store.search(&firefox, 10, None).expect("search")),
        &[stored.id.as_str()],
    );
}

#[test]
fn schema_is_version_four_with_the_order_index_and_private_permissions() {
    let root = tempdir().expect("temporary root");
    let path = root.path().join("history.sqlite3");
    HistoryStore::open(path.clone()).expect("open store");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "history file must be 0600");
    }
    assert_eq!(user_version(&path), 4);

    let index: i64 = Connection::open(&path)
        .expect("open sqlite")
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'index' AND name = 'recognition_history_order'",
            [],
            |value| value.get(0),
        )
        .expect("index");
    assert_eq!(index, 1, "listing and search read this index");
}

#[test]
fn opening_history_waits_for_a_writer_before_switching_to_wal() {
    let root = tempdir().unwrap();
    let path = root.path().join("history.sqlite3");
    let writer = Connection::open(&path).unwrap();
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();

    let (starting, started) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        starting.send(()).unwrap();
        HistoryStore::open(path).unwrap()
    });
    started.recv().unwrap();
    // Hold the rollback journal while the other connection requests WAL.
    std::thread::sleep(Duration::from_millis(100));
    writer.execute_batch("COMMIT").unwrap();

    let store = reader.join().unwrap();
    let row = store
        .insert(&entry("hello", "hello", "quick"), "local")
        .unwrap();
    assert_eq!(store.get(&row.id).unwrap().unwrap().final_text, "hello");
    let mode: String = writer
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
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

/// A file seeded with the v1 table (no `audio_path`) opens, keeps its row
/// with `audio_path` and `app_id` unset, accepts current inserts and
/// reports version 4.
#[test]
fn v1_database_migrates_and_keeps_records_without_audio() {
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
    assert_eq!(fetched.app_id, None);
    assert_ids(
        ids(&store.search(&text("旧记录"), 10, None).expect("search")),
        &["old-id"],
    );

    let mut entry = entry("新记录", "新记录", "quick");
    entry.audio_path = Some("new.wav".to_owned());
    let stored = store.insert(&entry, "cloud-realtime").expect("insert");
    assert_eq!(stored.audio_path.as_deref(), Some("new.wav"));

    assert_eq!(user_version(&path), 4);
}

/// A file an earlier build left in schema v2 (no `app_id`), holding the
/// same month and day in two years, opens, is brought to v4, and its rows
/// are found, told apart and exported like any other; what it never
/// recorded stays unset.
#[test]
fn v2_database_migrates_and_its_cross_year_rows_stay_searchable() {
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
                asr_model TEXT,
                audio_path TEXT
            );
            INSERT INTO recognition_history (id, created_at, duration_seconds, raw_text,
                processing_mode, processed_text, final_text, status, character_count,
                asr_provider, asr_model, audio_path)
            VALUES ('last-year', '2025-03-14T02:00:00.123456Z', NULL, '张三丰的会议', 'quick', NULL,
                '张三丰的会议', 'completed', 6, 'cloud-realtime', NULL, NULL),
                   ('this-year', '2026-03-14T02:00:00.123456Z', NULL, '张三丰的会议', 'quick', NULL,
                '张三丰的会议', 'completed', 6, 'cloud-realtime', NULL, NULL),
                   ('unrelated', '2026-06-01T00:00:00Z', NULL, '别的事情', 'quick', NULL,
                '别的事情', 'completed', 4, 'cloud-realtime', NULL, NULL);
            PRAGMA user_version = 2;",
        )
        .expect("seed v2 schema");
    drop(connection);

    let store = HistoryStore::open(path.clone()).expect("open migrated store");
    assert_eq!(user_version(&path), 4);

    let page = store.search(&text("张三丰"), 10, None).expect("search");
    assert_ids(ids(&page), &["this-year", "last-year"]);
    let years: Vec<&str> = page
        .records
        .iter()
        .map(|record| &record.created_at[..4])
        .collect();
    assert_eq!(years, ["2026", "2025"], "the two years stay apart");
    assert!(page.records.iter().all(|record| record.app_id.is_none()));

    let kitty = HistoryFilter {
        app: Some("kitty".to_owned()),
        ..HistoryFilter::default()
    };
    assert!(store
        .search(&kitty, 10, None)
        .expect("search")
        .records
        .is_empty());

    let destination = root.path().join("export.csv");
    let count = store
        .export_csv(&destination, &text("张三丰"))
        .expect("export");
    assert_eq!(count, 2);
    let content = std::fs::read_to_string(&destination).expect("read csv");
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(lines.len(), 3);
    assert!(lines[1].starts_with("this-year,2026-03-14T02:00:00.123456Z,"));
    assert!(lines[1].ends_with("cloud-realtime,,,"), "{}", lines[1]);
    assert!(lines[2].starts_with("last-year,2025-03-14T02:00:00.123456Z,"));
}

/// The text matches a substring of the raw or the final text as stored: a
/// Chinese word inside a sentence, an English API name whatever the letter
/// case of either side, and `%`, `_` and `\` only as themselves.
#[test]
fn search_finds_substrings_of_the_raw_and_the_final_text() {
    let root = tempdir().expect("temporary root");
    let path = root.path().join("history.sqlite3");
    let store = HistoryStore::open(path.clone()).expect("open store");
    seed(
        &path,
        &[
            row("zh", "2026-03-05T10:00:00Z", "我们下午去开会讨论季度目标"),
            row("api", "2026-03-05T09:00:00Z", "用 PostgreSQL 存数据"),
            Row {
                raw: Some("原始 temp_text"),
                ..row("raw-only", "2026-03-05T08:00:00Z", "最终文字")
            },
            Row {
                raw: Some("原文"),
                ..row("final-only", "2026-03-05T07:00:00Z", "the FINAL word")
            },
            row("wild", "2026-03-05T06:00:00Z", "进度 100% 完成 a_b"),
            row("none", "2026-03-05T05:00:00Z", "无关内容"),
        ],
    );

    let page = |needle: &str| ids(&store.search(&text(needle), 100, None).expect("search"));
    assert_ids(page("开会"), &["zh"]);
    assert_ids(page("季度目标"), &["zh"]);
    assert_ids(page("PostgreSQL"), &["api"]);
    assert_ids(page("postgresql"), &["api"]);
    assert_ids(page("存数据"), &["api"]);
    assert_ids(page("temp_text"), &["raw-only"]);
    assert_ids(page("final word"), &["final-only"]);
    // Unescaped, `%` and `_` would match every row.
    assert_ids(page("%"), &["wild"]);
    assert_ids(page("_"), &["raw-only", "wild"]);
    assert_ids(page("a_b"), &["wild"]);
    assert_ids(page("100% 完成"), &["wild"]);
    assert_ids(page("\\"), &[]);
    assert_ids(page("会议"), &[]);
    // Whitespace around the text is not part of it.
    assert_ids(page("  开会 \n"), &["zh"]);
}

/// `from` keeps its whole second and `to` leaves it out, whatever the
/// subsecond a row carries and the offset a bound is written in.
#[test]
fn search_keeps_whole_seconds_from_inclusive_to_exclusive() {
    let root = tempdir().expect("temporary root");
    let path = root.path().join("history.sqlite3");
    let store = HistoryStore::open(path.clone()).expect("open store");
    seed(
        &path,
        &[
            row("before", "2024-12-31T23:59:59.999999Z", "之前"),
            row("first-second", "2025-01-01T00:00:00Z", "第一秒"),
            row("first-fraction", "2025-01-01T00:00:00.500Z", "第一秒的后半"),
            row("last-second", "2025-12-31T23:59:59.999Z", "最后一秒"),
            row("at-to", "2026-01-01T00:00:00Z", "到点"),
            row("after-to", "2026-01-01T00:00:00.001Z", "刚过"),
        ],
    );

    let mut in_2025 = collect(
        &store,
        &HistoryFilter {
            from: Some("2025-01-01T00:00:00Z".to_owned()),
            to: Some("2026-01-01T00:00:00Z".to_owned()),
            ..HistoryFilter::default()
        },
        2,
    );
    in_2025.sort();
    assert_ids(in_2025, &["first-fraction", "first-second", "last-second"]);

    // The same instants written east of UTC select the same rows.
    let mut from_shanghai = collect(
        &store,
        &HistoryFilter {
            from: Some("2025-01-01T08:00:00.000+08:00".to_owned()),
            to: Some("2026-01-01T08:00:00+08:00".to_owned()),
            ..HistoryFilter::default()
        },
        2,
    );
    from_shanghai.sort();
    assert_ids(
        from_shanghai,
        &["first-fraction", "first-second", "last-second"],
    );

    let from = HistoryFilter {
        from: Some("2026-01-01T00:00:00Z".to_owned()),
        ..HistoryFilter::default()
    };
    let mut from_2026 = collect(&store, &from, 10);
    from_2026.sort();
    assert_ids(from_2026, &["after-to", "at-to"]);
    let to = HistoryFilter {
        to: Some("2025-01-01T00:00:00Z".to_owned()),
        ..HistoryFilter::default()
    };
    assert_ids(collect(&store, &to, 10), &["before"]);
}

/// Mode, application, model and the recording state each compare as a
/// column; a row that never recorded one is only selected without it.
#[test]
fn search_filters_by_mode_app_model_and_recording() {
    let root = tempdir().expect("temporary root");
    let path = root.path().join("history.sqlite3");
    let store = HistoryStore::open(path.clone()).expect("open store");
    seed(
        &path,
        &[
            Row {
                app: Some("kitty"),
                model: Some("m-a"),
                audio: Some("/tmp/one.wav"),
                ..row("quick-terminal", "2026-02-01T00:00:03Z", "一")
            },
            Row {
                mode: Some("prompt-optimize"),
                app: Some("org.mozilla.firefox"),
                ..row("polished-browser", "2026-02-01T00:00:02Z", "二")
            },
            Row {
                mode: None,
                model: Some("m-b"),
                ..row("no-mode", "2026-02-01T00:00:01Z", "三")
            },
        ],
    );

    let quick = HistoryFilter {
        mode: Some("quick".to_owned()),
        ..HistoryFilter::default()
    };
    assert_ids(collect(&store, &quick, 10), &["quick-terminal"]);
    let kitty = HistoryFilter {
        app: Some("kitty".to_owned()),
        ..HistoryFilter::default()
    };
    assert_ids(collect(&store, &kitty, 10), &["quick-terminal"]);
    let model = HistoryFilter {
        model: Some("m-b".to_owned()),
        ..HistoryFilter::default()
    };
    assert_ids(collect(&store, &model, 10), &["no-mode"]);
    let with = HistoryFilter {
        audio: Some("with".to_owned()),
        ..HistoryFilter::default()
    };
    assert_ids(collect(&store, &with, 10), &["quick-terminal"]);
    let without = HistoryFilter {
        audio: Some("without".to_owned()),
        ..HistoryFilter::default()
    };
    assert_ids(
        collect(&store, &without, 10),
        &["polished-browser", "no-mode"],
    );
    let browser = HistoryFilter {
        app: Some("org.mozilla.firefox".to_owned()),
        audio: Some("without".to_owned()),
        ..HistoryFilter::default()
    };
    assert_ids(collect(&store, &browser, 10), &["polished-browser"]);
}

/// A cursor is the row its page ended with. One from an unfiltered page
/// keeps the form earlier builds wrote and continues only an unfiltered
/// listing; one from a filtered page also names its filter and is refused
/// under any other, while the same filter spelled differently continues.
#[test]
fn cursors_belong_to_the_filter_that_issued_them() {
    use base64::Engine;

    let root = tempdir().expect("temporary root");
    let path = root.path().join("history.sqlite3");
    let store = HistoryStore::open(path.clone()).expect("open store");
    seed(
        &path,
        &[
            row("r6", "2026-04-06T00:00:06Z", "会议 六"),
            row("r5", "2026-04-05T00:00:05Z", "别的事"),
            row("r4", "2026-04-04T00:00:04Z", "会议 四"),
            row("r3", "2026-04-03T00:00:03Z", "会议 三"),
            row("r2", "2026-04-02T00:00:02Z", "别的事"),
            row("r1", "2026-04-01T00:00:01Z", "会议 一"),
        ],
    );

    let first = store.query(2, None).expect("first page");
    let unfiltered = first.next_cursor.expect("more pages");
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(unfiltered.as_bytes())
        .expect("base64");
    let payload: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(
        payload,
        serde_json::json!([first.records[1].created_at, first.records[1].id])
    );
    assert_ids(
        ids(&store.query(2, Some(&unfiltered)).expect("second page")),
        &["r4", "r3"],
    );

    let meetings = text("会议");
    let page = store.search(&meetings, 1, None).expect("filtered page");
    assert_ids(ids(&page), &["r6"]);
    let filtered = page.next_cursor.expect("more pages");
    let next = store
        .search(&meetings, 1, Some(&filtered))
        .expect("next page");
    assert_ids(ids(&next), &["r4"]);

    for other in [
        HistoryFilter::default(),
        text("别的事"),
        HistoryFilter {
            audio: Some("with".to_owned()),
            ..meetings.clone()
        },
    ] {
        assert!(
            matches!(
                store.search(&other, 1, Some(&filtered)),
                Err(HistoryError::Cursor)
            ),
            "a cursor of {other:?} must not continue another filter"
        );
    }
    assert!(matches!(
        store.search(&meetings, 1, Some(&unfiltered)),
        Err(HistoryError::Cursor)
    ));

    let padded = text("  会议 ");
    assert_eq!(
        ids(&store
            .search(&padded, 1, Some(&filtered))
            .expect("next page")),
        ids(&next)
    );
    assert!(store.query(2, Some("not-a-cursor")).is_err());
}

/// Nine rows share one timestamp and fill pages of four. While the reader
/// is between pages, dictations land through the store's own insert and a
/// row with the tied timestamp is written by another connection: every
/// row of the selection shows exactly once, and what landed only once the
/// listing starts over.
#[test]
fn pages_stay_exact_across_timestamp_ties_and_appends_in_between() {
    let root = tempdir().expect("temporary root");
    let path = root.path().join("history.sqlite3");
    let store = HistoryStore::open(path.clone()).expect("open store");
    let tied = [
        "t01", "t02", "t03", "t04", "t05", "t06", "t07", "t08", "t09",
    ];
    let rows: Vec<Row> = tied
        .iter()
        .map(|id| row(id, "2026-05-05T05:05:05.123456Z", "同一秒"))
        .collect();
    seed(&path, &rows);
    let tied = text("同一秒");

    let first = store.search(&tied, 4, None).expect("first page");
    assert_ids(ids(&first), &["t09", "t08", "t07", "t06"]);
    let cursor = first.next_cursor.expect("more pages");

    let one = store
        .insert(&entry("新的一条", "新的一条", "quick"), "cloud-realtime")
        .expect("insert");
    let two = store
        .insert(&entry("新的两条", "新的两条", "quick"), "cloud-realtime")
        .expect("insert");
    // Same second as the page, an id that sorts above every row it showed.
    seed(
        &path,
        &[row("t09a", "2026-05-05T05:05:05.123456Z", "同一秒")],
    );

    let second = store.search(&tied, 4, Some(&cursor)).expect("second page");
    assert_ids(ids(&second), &["t05", "t04", "t03", "t02"]);
    let third = store
        .search(&tied, 4, second.next_cursor.as_deref())
        .expect("third page");
    assert_ids(ids(&third), &["t01"]);
    assert!(third.next_cursor.is_none());

    assert_ids(
        collect(&store, &tied, 4),
        &[
            "t09a", "t09", "t08", "t07", "t06", "t05", "t04", "t03", "t02", "t01",
        ],
    );
    let mut newest = ids(&store.query(3, None).expect("newest rows"));
    assert_eq!(newest.pop().as_deref(), Some("t09a"));
    newest.sort();
    let mut landed = vec![one.id, two.id];
    landed.sort();
    assert_eq!(newest, landed);
}

/// A filter the store cannot apply names the value it rejects, and an
/// export with one writes no file.
#[test]
fn a_rejected_filter_names_its_value_and_exports_nothing() {
    let root = tempdir().expect("temporary root");
    let store = HistoryStore::open(root.path().join("history.sqlite3")).expect("open store");
    store
        .insert(&entry("一", "一", "quick"), "cloud-realtime")
        .expect("insert");
    let stray = HistoryFilter {
        audio: Some("yes".to_owned()),
        ..HistoryFilter::default()
    };
    match store.search(&stray, 10, None) {
        Err(HistoryError::Filter(message)) => {
            assert!(message.contains("with or without"), "{message}")
        }
        other => panic!("expected the store to reject the filter, got {other:?}"),
    }
    let destination = root.path().join("nested").join("export.csv");
    assert!(store.export_csv(&destination, &stray).is_err());
    assert!(!destination.exists(), "a rejected filter writes no file");
}

/// Whatever the page size, the export holds the rows the pages of the same
/// filter hold, in their order, quoted per RFC 4180, with a blank where a
/// row never recorded a value; `export_csv` writes the same text to a file.
#[test]
fn export_csv_follows_the_search_that_issued_it() {
    let root = tempdir().expect("temporary root");
    let path = root.path().join("history.sqlite3");
    let store = HistoryStore::open(path.clone()).expect("open store");
    seed(
        &path,
        &[
            Row {
                app: Some("kitty"),
                model: Some("m-a"),
                ..row("quoted", "2026-04-03T00:00:03Z", "会议,带\"引号\"")
            },
            Row {
                mode: None,
                ..row("old", "2026-04-03T00:00:02Z", "张三丰的会议")
            },
            row("meeting", "2026-04-02T00:00:00Z", "周一 会议 纪要"),
            row("other", "2026-04-01T00:00:00Z", "别的事情"),
            row("none", "2026-03-31T00:00:00Z", "张三丰的会议"),
        ],
    );
    let meetings = text("会议");

    for limit in [1, 2, 100] {
        let mut buffer = Vec::new();
        let count = store.write_csv(&mut buffer, &meetings).expect("write csv");
        let csv = String::from_utf8(buffer).expect("UTF-8 CSV");
        assert_eq!(count, 4);
        let mut lines = csv.lines();
        assert_eq!(lines.next(), Some(CSV_HEADER));
        let exported: Vec<&str> = lines
            .map(|line| line.split(',').next().expect("id"))
            .collect();
        assert_eq!(exported, collect(&store, &meetings, limit));
        assert_eq!(exported, ["quoted", "old", "meeting", "none"]);
    }

    let mut buffer = Vec::new();
    store.write_csv(&mut buffer, &meetings).expect("write csv");
    let csv = String::from_utf8(buffer).expect("UTF-8 CSV");
    let lines: Vec<&str> = csv.lines().collect();
    // A comma or a quote inside a field: quoted, with the quotes doubled.
    assert_eq!(
        lines[1],
        "quoted,2026-04-03T00:00:03Z,,\"会议,带\"\"引号\"\"\",quick,,\"会议,带\"\"引号\"\"\",completed,0,local-offline,m-a,,kitty"
    );
    // What a row never recorded is an empty field.
    assert_eq!(
        lines[2],
        "old,2026-04-03T00:00:02Z,,张三丰的会议,,,张三丰的会议,completed,0,local-offline,,,"
    );

    let everything = root.path().join("all.csv");
    assert_eq!(
        store
            .export_csv(&everything, &HistoryFilter::default())
            .expect("export"),
        5
    );
    let mut buffer = Vec::new();
    store
        .write_csv(&mut buffer, &HistoryFilter::default())
        .expect("write csv");
    assert_eq!(std::fs::read(&everything).expect("read csv"), buffer);
}

/// Facets offer every mode, application and model the library holds, most
/// frequent first and by value among equals; an empty library has none.
#[test]
fn facets_offer_what_the_library_holds() {
    let root = tempdir().expect("temporary root");
    let path = root.path().join("history.sqlite3");
    let store = HistoryStore::open(path.clone()).expect("open store");
    seed(
        &path,
        &[
            Row {
                app: Some("org.mozilla.firefox"),
                model: Some("qwen3-asr-0.6b-gguf"),
                ..row("a", "2026-03-01T00:00:04Z", "一")
            },
            Row {
                app: Some("org.mozilla.firefox"),
                ..row("b", "2026-03-01T00:00:03Z", "二")
            },
            Row {
                app: Some("kitty"),
                model: Some("qwen3-asr-0.6b-gguf"),
                ..row("c", "2026-03-01T00:00:02Z", "三")
            },
            Row {
                mode: Some("prompt-optimize"),
                app: Some("alpha"),
                model: Some("zeta"),
                ..row("d", "2026-03-01T00:00:01Z", "四")
            },
            Row {
                mode: None,
                ..row("e", "2026-03-01T00:00:00Z", "五")
            },
        ],
    );

    let facet = |value: &str, records: i64| HistoryFacet {
        value: value.to_owned(),
        records,
    };
    assert_eq!(
        store.facets().expect("facets"),
        HistoryFacets {
            modes: vec![facet("quick", 3), facet("prompt-optimize", 1)],
            apps: vec![
                facet("org.mozilla.firefox", 2),
                facet("alpha", 1),
                facet("kitty", 1)
            ],
            models: vec![facet("qwen3-asr-0.6b-gguf", 2), facet("zeta", 1)],
        }
    );

    let empty = tempdir().expect("temporary root");
    let store = HistoryStore::open(empty.path().join("history.sqlite3")).expect("open store");
    assert_eq!(store.facets().expect("facets"), HistoryFacets::default());
}

/// A library of 20 000 rows, read the way the console reads it. The
/// timings go to the test's output; the assertions are that deep pages and
/// the export of the same filter agree on a library of this size.
#[test]
fn large_library_pages_and_exports_exactly() {
    const ROWS: u32 = 20_000;
    const TEXTS: [&str; 4] = [
        "季度会议 记录",
        "数据库 迁移",
        "语音 输入 模型",
        "项目 进度 会议",
    ];
    const APPS: [Option<&str>; 3] = [Some("kitty"), Some("org.mozilla.firefox"), None];

    let root = tempdir().expect("temporary root");
    let path = root.path().join("history.sqlite3");
    let store = HistoryStore::open(path.clone()).expect("open store");
    {
        let mut connection = Connection::open(&path).expect("open sqlite");
        let transaction = connection.transaction().expect("begin");
        {
            let mut insert = transaction
                .prepare(
                    "INSERT INTO recognition_history (id, created_at, duration_seconds, raw_text,
                        processing_mode, processed_text, final_text, status, character_count,
                        asr_provider, asr_model, audio_path, app_id)
                     VALUES (?1, ?2, NULL, ?3, 'quick', NULL, ?3, 'completed', 0,
                             'local-offline', NULL, NULL, ?4)",
                )
                .expect("prepare insert");
            let start =
                time::OffsetDateTime::from_unix_timestamp(1_768_000_000).expect("timestamp");
            for index in 0..ROWS {
                let created_at = (start + time::Duration::minutes(i64::from(index) * 3))
                    .format(&time::format_description::well_known::Rfc3339)
                    .expect("RFC 3339");
                insert
                    .execute(params![
                        format!("row-{index:05}"),
                        created_at,
                        TEXTS[(index % 4) as usize],
                        APPS[(index % 3) as usize],
                    ])
                    .expect("insert row");
            }
        }
        transaction.commit().expect("commit");
    }

    let (page, first) = timed("no filter, first page of 100", || {
        store.query(100, None).expect("page")
    });
    assert_eq!(page.records.len(), 100);
    // A page that had to read the library to sort it would not be this quick.
    assert!(first < Duration::from_secs(2), "first page took {first:?}");

    let (_, searched) = timed("search \"会议\", first page of 100", || {
        store.search(&text("会议"), 100, None).expect("page")
    });
    assert!(
        searched < Duration::from_secs(5),
        "search took {searched:?}"
    );
    timed("search for a word no row has (the whole library)", || {
        assert!(store
            .search(&text("不存在的词"), 100, None)
            .expect("page")
            .records
            .is_empty())
    });
    timed("one day by date, first page of 100", || {
        let day = HistoryFilter {
            from: Some("2026-01-10T00:00:00Z".to_owned()),
            to: Some("2026-01-11T00:00:00Z".to_owned()),
            ..HistoryFilter::default()
        };
        assert_eq!(
            store.search(&day, 100, None).expect("page").records.len(),
            100
        );
    });
    let (meetings, walked) = timed("search \"会议\", every page, 1000 at a time", || {
        collect(&store, &text("会议"), 1_000)
    });
    let mut unique = meetings.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), 10_000, "no row repeated or missing");
    assert!(walked < Duration::from_secs(20), "walking took {walked:?}");
    let (exported, _) = timed("export of the search to memory", || {
        let mut buffer = Vec::new();
        let count = store
            .write_csv(&mut buffer, &text("会议"))
            .expect("write csv");
        (count, buffer.len())
    });
    assert_eq!(exported.0, 10_000);
    let (exported_all, _) = timed("export of the whole library to memory", || {
        let mut buffer = Vec::new();
        store
            .write_csv(&mut buffer, &HistoryFilter::default())
            .expect("write csv")
    });
    assert_eq!(exported_all, i64::from(ROWS));
    timed("facets", || {
        assert!(!store.facets().expect("facets").apps.is_empty())
    });
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
    let count = store
        .export_csv(&destination, &HistoryFilter::default())
        .expect("export");
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
