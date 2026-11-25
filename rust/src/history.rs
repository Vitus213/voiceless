//! Recognition history persisted in SQLite with the Python-compatible
//! `recognition_history` schema, WAL journaling and 0600 permissions.
//!
//! File: `<XDG data dir>/voiceless/history.sqlite3`
//! (`models::default_data_dir`). Opened by `stream::build_pipeline` when
//! `[history].enabled` (one `insert` per successful session through
//! `coordinator::HistoryRecorder`), by `history_cmd` for
//! `voiceless history list|delete|export|totals|usage`, and by the console
//! (the `GET /api/records` family, `GET /api/totals` and `DELETE
//! `/api/records/{id}`; audio playback and re-transcription locate the WAV
//! through `get`).
//!
//! Schema: `PRAGMA user_version` 1 is the original table; version 2 adds
//! the nullable `audio_path` column (the retained WAV of the session, see
//! `[history].save_audio`); version 3 adds the nullable `app_id` column
//! (the application the dictation went to, `insert_with_app`) and the
//! index that listing and search walk. `open` migrates forward inside one
//! `BEGIN IMMEDIATE` transaction and refuses a database whose version is
//! newer than `SCHEMA_VERSION`. Rows are inserted once and only ever
//! deleted (together with their recording); nothing updates them.
//! `created_at` is RFC 3339 UTC with a `Z` suffix and `id` a random
//! UUID-v4-style string, so `(created_at DESC, id DESC)` is the total order
//! used for listing, CSV export and the keyset cursors.
//!
//! Search (`search`, and the filter of `write_csv`): a `HistoryFilter`
//! keeps the rows whose raw or final text contains a text and that fall in
//! a time range, a mode, an application, a model and the recording state
//! asked for. The text test is a plain substring comparison on the stored
//! text, case-insensitive for ASCII letters only, so a Chinese word or an
//! English API name is found without a tokenizer or an index to keep in
//! step. A page walks the order index from the newest row and stops at the
//! page size, whatever the filter: it costs the rows it visits, not a sort
//! of the library. A cursor names the row the next page continues after,
//! so rows appended while the user pages (a dictation finishing in the
//! background) are neither repeated nor skipped; one issued for a filter
//! is refused under any other.
//!
//! Concurrency: the store keeps only the path. Every call opens its own
//! connection (`connect`) with WAL journaling and a 5 s busy timeout, so
//! the daemon, the CLI and the console can read and write the same file at
//! the same time and the handle can be shared in an `Arc`. All methods are
//! synchronous and block on SQLite. Audio files are removed after the
//! owning rows, best effort: a row never outlives its file by design, but
//! an unlink failure can leave a file behind.

pub mod retention;
pub mod revisions;

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::{params, Connection, ErrorCode, OptionalExtension};
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::coordinator::HistoryEntry;

/// Newest `user_version` this build understands; older files are
/// migrated, newer ones rejected.
const SCHEMA_VERSION: i64 = 4;
/// How long a connection waits for a writer lock before failing; covers
/// the daemon inserting while the console or CLI reads.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
/// The columns of every `SELECT` of a row, in the order `record_from_row`
/// reads them; also the CSV export's field order (`CSV_HEADER`).
const COLUMNS: &str = "id, created_at, duration_seconds, raw_text, processing_mode, \
                       processed_text, final_text, status, character_count, \
                       asr_provider, asr_model, audio_path, app_id";
/// Longest search text accepted, in characters. Nothing a person looks for
/// is longer, and the comparison and the cursor both grow with it.
const MAX_SEARCH_CHARS: usize = 200;

/// Failures of the store. `stream` logs them and keeps the session
/// result; the CLI prints them and exits 1; the console answers 500, or
/// 400 for `Filter`.
#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    /// A SQLite error, a schema newer than this build, or an argument
    /// rejected up front (page size, usage window).
    #[error("history database error: {0}")]
    Database(String),
    /// The current time could not be formatted as RFC 3339.
    #[error("history timestamp invalid: {0}")]
    Timestamp(String),
    /// A page cursor that `search` did not produce (bad base64 or JSON),
    /// or one issued for another filter.
    #[error("history cursor invalid")]
    Cursor,
    /// A filter rejected up front: a time bound that is not an RFC 3339
    /// time on a whole second, `from` not before `to`, a search text with
    /// a NUL character or longer than `MAX_SEARCH_CHARS`, an `audio` value
    /// other than `with` or `without`.
    #[error("history filter invalid: {0}")]
    Filter(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Conflict(String),
    /// Directory creation, chmod or CSV write failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// One row of `recognition_history`, serialized as is to JSON by the CLI
/// (`history list`) and the console (`/api/records`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HistoryRecord {
    /// Random UUID-v4-style identifier (`new_record_id`); the console's
    /// audio URLs carry only this id and look the path up here.
    pub id: String,
    /// Insertion time, RFC 3339 UTC (`…Z`); ordering compares it as text.
    pub created_at: String,
    /// Length of the captured audio (PCM bytes / 32 000 at 16 kHz mono
    /// 16-bit); `None` when the session captured no audio or the row
    /// predates audio retention.
    pub duration_seconds: Option<f64>,
    /// Recognizer output before the `[text]` cleanup.
    pub raw_text: String,
    /// Mode id of the session (`quick`, `prompt-optimize`, a user mode's
    /// UUID). Nullable in the schema; the pipeline always sets it.
    pub processing_mode: Option<String>,
    /// LLM output for a processing mode, or the cleaned text in `quick`
    /// mode when cleanup changed something; `None` when processing failed
    /// or cleanup changed nothing.
    pub processed_text: Option<String>,
    /// The text that was injected (authoritative).
    pub final_text: String,
    /// `completed` for every row this crate writes; failed, cancelled and
    /// empty sessions are never recorded.
    pub status: String,
    /// `final_text.chars().count()`: Unicode scalar values, not bytes.
    pub character_count: i64,
    /// Streaming backend of the session (`cloud-realtime`, …).
    pub asr_provider: Option<String>,
    /// Always `None` for rows written by this crate; kept for schema
    /// compatibility and the `usage` breakdown.
    pub asr_model: Option<String>,
    /// Absolute path of the retained WAV (schema v2); `None` when retention
    /// was off or the row predates it. Deleted together with the row.
    pub audio_path: Option<String>,
    /// Application the dictation went to (schema v3): the Wayland `app-id`
    /// or X11 window class of the window that had the keyboard focus when
    /// the session began, `insert_with_app`. `None` when the desktop named
    /// no window, the session typed nowhere, or the row predates it; the
    /// console shows all of these as "not recorded".
    pub app_id: Option<String>,
}

/// One page of `query`: newest first, with the opaque cursor of the next
/// page (`None` on the last page).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HistoryPage {
    pub records: Vec<HistoryRecord>,
    pub next_cursor: Option<String>,
}

/// Aggregates over every row: `COUNT(*)`, summed `character_count` and
/// summed `duration_seconds` (NULLs count as 0).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HistoryTotals {
    pub records: i64,
    pub characters: i64,
    pub duration_seconds: f64,
}

/// Which rows a search selects, as the query string or the command line
/// spell it: every field is optional, a blank one selects anything and the
/// set ones must all match. `search`, `write_csv` and `export_csv` check
/// the values (`HistoryError::Filter` for one they reject).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HistoryFilter {
    /// Substring of `raw_text` or `final_text`, compared as stored:
    /// whitespace around it is ignored and ASCII letter case does not
    /// matter, everything else must match character for character.
    pub text: String,
    /// Rows created at or after this time: RFC 3339 on a whole second, any
    /// offset (it is compared in UTC). Which day a time belongs to is the
    /// caller's to decide; the console sends the bounds of a local day.
    pub from: Option<String>,
    /// Rows created before this time, in the form of `from`.
    pub to: Option<String>,
    /// Exact `processing_mode`.
    pub mode: Option<String>,
    /// Exact `app_id`.
    pub app: Option<String>,
    /// Exact `asr_model`.
    pub model: Option<String>,
    /// `with`: the row keeps a recording (`audio_path` set, whether or not
    /// the file still exists); `without`: it keeps none.
    pub audio: Option<String>,
}

/// A `HistoryFilter` once checked: blank fields dropped and the time bounds
/// as whole seconds in UTC. This is what the SQL compares and what a
/// filtered cursor carries back.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct Conditions {
    #[serde(skip_serializing_if = "String::is_empty")]
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    app: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    audio: Option<bool>,
}

/// One value of a `HistoryFacets` list with the rows that carry it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HistoryFacet {
    /// The stored `processing_mode`, `app_id` or `asr_model`.
    pub value: String,
    /// Rows with that value.
    pub records: i64,
}

/// What the filter menus of a search offer: every mode, application and
/// model the library holds. A value no row carries is left out.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct HistoryFacets {
    pub modes: Vec<HistoryFacet>,
    pub apps: Vec<HistoryFacet>,
    pub models: Vec<HistoryFacet>,
}

/// Handle to the history database. Holds only the path: every method
/// opens a fresh connection, so the handle is cheap to share and never
/// keeps a lock between calls.
pub struct HistoryStore {
    path: PathBuf,
}

/// Switching an existing rollback journal to WAL upgrades a read lock. SQLite
/// can return BUSY immediately on this upgrade even with a busy handler. Retry
/// the pragma outside any transaction so another opener can finish first.
fn enable_wal(connection: &Connection) -> rusqlite::Result<()> {
    let deadline = Instant::now() + BUSY_TIMEOUT;
    loop {
        match connection.pragma_update(None, "journal_mode", "WAL") {
            Ok(()) => return connection.busy_timeout(BUSY_TIMEOUT),
            Err(error) if error.sqlite_error_code() == Some(ErrorCode::DatabaseBusy) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(error);
                }
                std::thread::sleep(remaining.min(Duration::from_millis(10)));
                connection.busy_timeout(deadline.saturating_duration_since(Instant::now()))?;
            }
            Err(error) => return Err(error),
        }
    }
}

impl HistoryStore {
    /// Create the parent directory, create or migrate the database and
    /// chmod it 0600. Errors: directory or chmod I/O, SQLite failures, or
    /// a `user_version` above `SCHEMA_VERSION` (written by a newer
    /// Voiceless). Safe to call concurrently from several processes.
    pub fn open(path: PathBuf) -> Result<Self, HistoryError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let store = Self { path };
        store.migrate()?;
        Ok(store)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Open a connection to the store. WAL persists in the database after migration.
    fn connect(&self) -> Result<Connection, HistoryError> {
        let connection = Connection::open(&self.path)
            .map_err(|error| HistoryError::Database(error.to_string()))?;
        connection
            .busy_timeout(BUSY_TIMEOUT)
            .map_err(|error| HistoryError::Database(error.to_string()))?;
        connection
            .pragma_update(None, "foreign_keys", true)
            .map_err(|error| HistoryError::Database(error.to_string()))?;
        Ok(connection)
    }

    /// Bring the schema to `SCHEMA_VERSION` (v1 table, then the v2
    /// `audio_path` column) and tighten the file mode. Idempotent.
    fn migrate(&self) -> Result<(), HistoryError> {
        let connection = self.connect()?;
        enable_wal(&connection).map_err(|error| HistoryError::Database(error.to_string()))?;
        // Take the write lock before reading the version so two processes
        // opening the same new file cannot both run the DDL; an early
        // `return` drops the connection and SQLite rolls the transaction
        // back.
        connection
            .execute_batch("BEGIN IMMEDIATE")
            .map_err(|error| HistoryError::Database(error.to_string()))?;
        let version: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(|error| HistoryError::Database(error.to_string()))?;
        // Never touch a file written by a newer schema: an older binary
        // must not reinterpret or "migrate" columns it does not know.
        if version > SCHEMA_VERSION {
            return Err(HistoryError::Database(format!(
                "history database version too new: {version}"
            )));
        }
        if version < 1 {
            connection
                .execute_batch(
                    "CREATE TABLE IF NOT EXISTS recognition_history (
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
                    PRAGMA user_version = 1;",
                )
                .map_err(|error| HistoryError::Database(error.to_string()))?;
        }
        // v1 -> v2: existing rows get NULL (no recording), which the
        // console shows as the "no recording" chip.
        if version < 2 {
            connection
                .execute_batch(
                    "ALTER TABLE recognition_history ADD COLUMN audio_path TEXT;
                     PRAGMA user_version = 2;",
                )
                .map_err(|error| HistoryError::Database(error.to_string()))?;
        }
        // v2 -> v3: existing rows get NULL (the application was not
        // recorded), which the console shows as the "not recorded" chip;
        // the index gives listing and search their order without a sort.
        if version < 3 {
            connection
                .execute_batch(
                    "ALTER TABLE recognition_history ADD COLUMN app_id TEXT;
                     CREATE INDEX IF NOT EXISTS recognition_history_order
                         ON recognition_history(created_at, id);
                     PRAGMA user_version = 3;",
                )
                .map_err(|error| HistoryError::Database(error.to_string()))?;
        }
        if version < 4 {
            connection.execute_batch(
                "CREATE TABLE history_revisions (
                    id TEXT PRIMARY KEY, record_id TEXT NOT NULL REFERENCES recognition_history(id) ON DELETE CASCADE,
                    position INTEGER NOT NULL, created_at TEXT NOT NULL, adopted_at TEXT,
                    kind TEXT NOT NULL, state TEXT NOT NULL, raw_text TEXT NOT NULL,
                    processed_text TEXT, final_text TEXT NOT NULL, asr_provider TEXT, asr_model TEXT, device TEXT,
                    UNIQUE(record_id, position)
                 );
                 CREATE UNIQUE INDEX history_revisions_current ON history_revisions(record_id) WHERE state = 'current';
                 PRAGMA user_version = 4;",
            ).map_err(|error| HistoryError::Database(error.to_string()))?;
        }
        connection
            .execute_batch("COMMIT")
            .map_err(|error| HistoryError::Database(error.to_string()))?;
        // SQLite creates the file with the process umask; transcripts are
        // private data, so tighten it once the schema exists.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let permissions = std::fs::Permissions::from_mode(0o600);
            std::fs::set_permissions(&self.path, permissions)?;
        }
        Ok(())
    }

    /// Append one row for a successful session and return it as stored,
    /// without the application the dictation went to. Same as
    /// `insert_with_app` with `None`.
    pub fn insert(
        &self,
        entry: &HistoryEntry,
        backend: &str,
    ) -> Result<HistoryRecord, HistoryError> {
        self.insert_with_app(entry, backend, None)
    }

    /// Append one row for a successful session that went to `app_id` and
    /// return it as stored. The id and `created_at` are generated here,
    /// `character_count` is derived from `final_text`, `backend` becomes
    /// `asr_provider` and `entry.model` becomes `asr_model` (NULL when
    /// unknown). `app_id` is the application the user was typing into when
    /// the session began, the `app_id` of
    /// `desktop::focus::FocusedWindow` (Wayland `app-id`, X11 window
    /// class); pass `None` when the desktop named no window or the session
    /// typed nowhere, and the row reads as "not recorded" like one that
    /// predates schema v3. Whitespace around the id is dropped; an id that
    /// is then empty is `None`. Does not touch the audio file: the
    /// recorder already finalized it at `entry.audio_path`.
    pub fn insert_with_app(
        &self,
        entry: &HistoryEntry,
        backend: &str,
        app_id: Option<&str>,
    ) -> Result<HistoryRecord, HistoryError> {
        let app_id = app_id.map(str::trim).filter(|app| !app.is_empty());
        let id = new_record_id();
        let created_at = timestamp_now()?;
        let character_count = entry.final_text.chars().count() as i64;
        let connection = self.connect()?;
        connection
            .execute(
                "INSERT INTO recognition_history (
                    id, created_at, duration_seconds, raw_text, processing_mode,
                    processed_text, final_text, status, character_count,
                    asr_provider, asr_model, audio_path, app_id
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    id,
                    created_at,
                    entry.duration_seconds,
                    entry.raw_text,
                    entry.processing_mode,
                    entry.processed_text,
                    entry.final_text,
                    entry.status,
                    character_count,
                    backend,
                    entry.model,
                    entry.audio_path,
                    app_id,
                ],
            )
            .map_err(|error| HistoryError::Database(error.to_string()))?;
        Ok(HistoryRecord {
            id,
            created_at,
            duration_seconds: entry.duration_seconds,
            raw_text: entry.raw_text.clone(),
            processing_mode: Some(entry.processing_mode.clone()),
            processed_text: entry.processed_text.clone(),
            final_text: entry.final_text.clone(),
            status: entry.status.clone(),
            character_count,
            asr_provider: Some(backend.to_owned()),
            asr_model: entry.model.clone(),
            audio_path: entry.audio_path.clone(),
            app_id: app_id.map(str::to_owned),
        })
    }

    /// List rows newest first with keyset pagination: every row, in the
    /// order and with the cursor rules of `search`. `limit` must be within
    /// 1..=1000 (`Database` error otherwise); `cursor` is the
    /// `next_cursor` of a previous page (`Cursor` error when malformed or
    /// issued for a filter). The last page carries `next_cursor: None`.
    pub fn query(&self, limit: i64, cursor: Option<&str>) -> Result<HistoryPage, HistoryError> {
        self.search(&HistoryFilter::default(), limit, cursor)
    }

    /// One page of the rows `filter` selects: newest first, at most
    /// `limit` (1..=1000, else `Database`), with the opaque cursor of the
    /// next page (`None` on the last page).
    ///
    /// The order is `(created_at DESC, id DESC)`, read from the index, so
    /// rows that share a timestamp are told apart by id and a page costs
    /// the rows it visits. A cursor points after the last row of the page
    /// it came from, not at an offset: rows inserted or deleted while the
    /// caller pages (a dictation finishing in the background) repeat
    /// nothing and skip nothing of what the filter selected. A cursor
    /// belongs to the filter it was issued for: under another filter, or
    /// with none, it is a `Cursor` error. One from an unfiltered page keeps
    /// the form earlier builds wrote and continues only an unfiltered
    /// listing. `Filter` names the first value the filter rejects.
    pub fn search(
        &self,
        filter: &HistoryFilter,
        limit: i64,
        cursor: Option<&str>,
    ) -> Result<HistoryPage, HistoryError> {
        if !(1..=1_000).contains(&limit) {
            return Err(HistoryError::Database(format!(
                "page size must be between 1 and 1000, got {limit}"
            )));
        }
        let conditions = filter.conditions()?;
        let after = match cursor.map(decode_cursor).transpose()? {
            None => None,
            Some(CursorKey::Filtered { after, filter }) => {
                if filter != conditions {
                    return Err(HistoryError::Cursor);
                }
                Some(after)
            }
            Some(CursorKey::Unfiltered(after)) => {
                if !conditions.is_empty() {
                    return Err(HistoryError::Cursor);
                }
                Some(after)
            }
        };
        let connection = self.connect()?;
        let (sql, params) = page_query(&conditions, after.as_ref(), limit);
        let mut statement = connection
            .prepare(&sql)
            .map_err(|error| HistoryError::Database(error.to_string()))?;
        let mut records: Vec<HistoryRecord> = statement
            .query_map(rusqlite::params_from_iter(params), record_from_row)
            .map_err(|error| HistoryError::Database(error.to_string()))?
            .collect::<Result<_, _>>()
            .map_err(|error| HistoryError::Database(error.to_string()))?;
        let has_more = records.len() as i64 > limit;
        records.truncate(limit as usize);
        let next_cursor = has_more
            .then(|| records.last())
            .flatten()
            .map(|last| encode_cursor(&last.created_at, &last.id, &conditions));
        Ok(HistoryPage {
            records,
            next_cursor,
        })
    }

    /// Delete the rows with these ids, then remove their recordings (best
    /// effort). Returns the number of rows deleted; unknown ids are
    /// ignored and an empty list is a no-op. Not transactional across the
    /// two steps: an unlink failure leaves a file without a row, never a
    /// row without a file.
    pub fn delete(&self, ids: &[String]) -> Result<i64, HistoryError> {
        if ids.is_empty() {
            return Ok(0);
        }
        let connection = self.connect()?;
        // Collect the paths first; after the DELETE the rows are gone.
        let audio_paths = audio_paths_for(&connection, ids)?;
        let placeholders = vec!["?"; ids.len()].join(", ");
        let deleted = connection
            .execute(
                &format!("DELETE FROM recognition_history WHERE id IN ({placeholders})"),
                rusqlite::params_from_iter(ids),
            )
            .map_err(|error| HistoryError::Database(error.to_string()))?;
        remove_audio_files(&audio_paths);
        Ok(deleted as i64)
    }

    /// Delete every row, then remove every retained recording the rows
    /// referenced (best effort). Returns the number of rows deleted. Files
    /// in the audio directory that no row references are not touched.
    pub fn delete_all(&self) -> Result<i64, HistoryError> {
        let connection = self.connect()?;
        let mut statement = connection
            .prepare("SELECT audio_path FROM recognition_history WHERE audio_path IS NOT NULL")
            .map_err(|error| HistoryError::Database(error.to_string()))?;
        let audio_paths: Vec<String> = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|error| HistoryError::Database(error.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| HistoryError::Database(error.to_string()))?;
        let deleted = connection
            .execute("DELETE FROM recognition_history", [])
            .map_err(|error| HistoryError::Database(error.to_string()))?;
        remove_audio_files(&audio_paths);
        Ok(deleted as i64)
    }

    /// Row count, summed characters and summed duration over the whole
    /// table (one aggregate query).
    pub fn totals(&self) -> Result<HistoryTotals, HistoryError> {
        let connection = self.connect()?;
        connection
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(character_count), 0),
                        COALESCE(SUM(duration_seconds), 0.0)
                 FROM recognition_history",
                [],
                |row| {
                    Ok(HistoryTotals {
                        records: row.get(0)?,
                        characters: row.get(1)?,
                        duration_seconds: row.get(2)?,
                    })
                },
            )
            .map_err(|error| HistoryError::Database(error.to_string()))
    }

    /// Row counts per `asr_provider` and per `asr_model` over the trailing
    /// `days` (only 1, 7 or 30 are accepted; `Database` error otherwise),
    /// as `{"days", "since", "providers": {name: n}, "models": {name: n}}`.
    /// `since` is the RFC 3339 cutoff; it is compared with `created_at` as
    /// text, which orders correctly because both are UTC `Z` timestamps
    /// from the same formatter. Rows with a NULL provider or model are left
    /// out; `models` is empty for rows written by this crate (see
    /// `HistoryRecord::asr_model`).
    pub fn usage(&self, days: i64) -> Result<serde_json::Value, HistoryError> {
        if !matches!(days, 1 | 7 | 30) {
            return Err(HistoryError::Database(format!(
                "usage window must be 1, 7 or 30 days, got {days}"
            )));
        }
        let since = (OffsetDateTime::now_utc() - time::Duration::days(days))
            .format(&Rfc3339)
            .map_err(|error| HistoryError::Timestamp(error.to_string()))?;
        let connection = self.connect()?;
        let mut providers: std::collections::BTreeMap<String, i64> =
            std::collections::BTreeMap::new();
        let mut models: std::collections::BTreeMap<String, i64> = std::collections::BTreeMap::new();
        let mut statement = connection
            .prepare(
                "SELECT COALESCE(asr_provider, ''), COUNT(*) FROM recognition_history
                 WHERE created_at >= ?1 GROUP BY 1",
            )
            .map_err(|error| HistoryError::Database(error.to_string()))?;
        let rows = statement
            .query_map(params![since], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .map_err(|error| HistoryError::Database(error.to_string()))?;
        for row in rows {
            let (key, count) = row.map_err(|error| HistoryError::Database(error.to_string()))?;
            if !key.is_empty() {
                providers.insert(key, count);
            }
        }
        let mut statement = connection
            .prepare(
                "SELECT COALESCE(asr_model, ''), COUNT(*) FROM recognition_history
                 WHERE created_at >= ?1 GROUP BY 1",
            )
            .map_err(|error| HistoryError::Database(error.to_string()))?;
        let rows = statement
            .query_map(params![since], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .map_err(|error| HistoryError::Database(error.to_string()))?;
        for row in rows {
            let (key, count) = row.map_err(|error| HistoryError::Database(error.to_string()))?;
            if !key.is_empty() {
                models.insert(key, count);
            }
        }
        Ok(serde_json::json!({
            "days": days,
            "since": since,
            "providers": providers,
            "models": models,
        }))
    }

    /// Write the rows `filter` selects, newest first, as CSV to `writer`:
    /// the header (`CSV_HEADER`) then one line per record, row by row, so
    /// a large export is never held whole in memory. Fields containing a
    /// comma, a quote or a line break are quoted with doubled quotes
    /// (RFC 4180); NULL fields are empty. Returns the number of rows
    /// written. `Filter` for a value the filter rejects.
    pub fn write_csv<W: Write>(
        &self,
        writer: &mut W,
        filter: &HistoryFilter,
    ) -> Result<i64, HistoryError> {
        let conditions = filter.conditions()?;
        let connection = self.connect()?;
        let mut params = Vec::new();
        let clause = conditions_sql(&conditions, None, &mut params);
        let sql = format!(
            "SELECT {COLUMNS} FROM recognition_history{clause}
             ORDER BY created_at DESC, id DESC"
        );
        let mut statement = connection
            .prepare(&sql)
            .map_err(|error| HistoryError::Database(error.to_string()))?;
        let rows = statement
            .query_map(rusqlite::params_from_iter(params), record_from_row)
            .map_err(|error| HistoryError::Database(error.to_string()))?;
        writer.write_all(CSV_HEADER.as_bytes())?;
        let mut written = 0;
        for row in rows {
            let record = row.map_err(|error| HistoryError::Database(error.to_string()))?;
            writer.write_all(csv_line(&record).as_bytes())?;
            written += 1;
        }
        writer.flush()?;
        Ok(written)
    }

    /// Write the rows `filter` selects to `destination` as CSV (see
    /// `write_csv`), creating parent directories and overwriting an
    /// existing file; a filter that is rejected leaves no file behind.
    /// Returns the number of rows written.
    pub fn export_csv(
        &self,
        destination: &Path,
        filter: &HistoryFilter,
    ) -> Result<i64, HistoryError> {
        filter.conditions()?;
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut writer = std::io::BufWriter::new(std::fs::File::create(destination)?);
        self.write_csv(&mut writer, filter)
    }

    /// Fetch one row by id; `Ok(None)` when there is no such row.
    pub fn get(&self, record_id: &str) -> Result<Option<HistoryRecord>, HistoryError> {
        let connection = self.connect()?;
        let record = connection
            .query_row(
                &format!("SELECT {COLUMNS} FROM recognition_history WHERE id = ?1"),
                params![record_id],
                record_from_row,
            )
            .optional()
            .map_err(|error| HistoryError::Database(error.to_string()))?;
        Ok(record)
    }

    /// What the filter menus of a search offer: every `processing_mode`,
    /// `app_id` and `asr_model` the library holds, each with the number of
    /// rows carrying it, the most frequent first and by value among equals
    /// (one query over the table). Rows that hold no value for a field are
    /// not counted there.
    pub fn facets(&self) -> Result<HistoryFacets, HistoryError> {
        let connection = self.connect()?;
        let mut statement = connection
            .prepare(
                "SELECT processing_mode, app_id, asr_model, COUNT(*)
                 FROM recognition_history GROUP BY 1, 2, 3",
            )
            .map_err(|error| HistoryError::Database(error.to_string()))?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })
            .map_err(|error| HistoryError::Database(error.to_string()))?;
        let (mut modes, mut apps, mut models) = (
            BTreeMap::<String, i64>::new(),
            BTreeMap::<String, i64>::new(),
            BTreeMap::<String, i64>::new(),
        );
        for row in rows {
            let (mode, app, model, records) =
                row.map_err(|error| HistoryError::Database(error.to_string()))?;
            let count = |map: &mut BTreeMap<String, i64>, value: Option<String>| {
                if let Some(value) = value {
                    *map.entry(value).or_insert(0) += records;
                }
            };
            count(&mut modes, mode);
            count(&mut apps, app);
            count(&mut models, model);
        }
        Ok(HistoryFacets {
            modes: ranked(modes),
            apps: ranked(apps),
            models: ranked(models),
        })
    }
}

/// Read one selected row; the statement must have selected `COLUMNS`.
fn record_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<HistoryRecord> {
    Ok(HistoryRecord {
        id: row.get(0)?,
        created_at: row.get(1)?,
        duration_seconds: row.get(2)?,
        raw_text: row.get(3)?,
        processing_mode: row.get(4)?,
        processed_text: row.get(5)?,
        final_text: row.get(6)?,
        status: row.get(7)?,
        character_count: row.get(8)?,
        asr_provider: row.get(9)?,
        asr_model: row.get(10)?,
        audio_path: row.get(11)?,
        app_id: row.get(12)?,
    })
}

/// Current UTC time as RFC 3339 with a `Z` suffix: the `created_at`
/// format, chosen so lexical order is chronological order.
fn timestamp_now() -> Result<String, HistoryError> {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|error| HistoryError::Timestamp(error.to_string()))
}

/// Random UUID v4 in hyphenated form, from the OS CSPRNG (the layout the
/// console validates before looking a record up). Panics only when the
/// OS random source is unavailable.
fn new_record_id() -> String {
    let mut bytes = [0_u8; 16];
    getrandom::getrandom(&mut bytes).expect("OS random source available");
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

impl HistoryFilter {
    /// The filter checked: whitespace around every value dropped, blank
    /// values dropped, the time bounds as whole seconds in UTC and `audio`
    /// as a flag. `Filter` names the first value that is rejected.
    fn conditions(&self) -> Result<Conditions, HistoryError> {
        let text = self.text.trim();
        let length = text.chars().count();
        if text.contains('\0') {
            return Err(HistoryError::Filter(
                "the search text must not contain a NUL character".to_owned(),
            ));
        }
        if length > MAX_SEARCH_CHARS {
            return Err(HistoryError::Filter(format!(
                "the search text must be at most {MAX_SEARCH_CHARS} characters, got {length}"
            )));
        }
        let bound = |field: &str, value: &Option<String>| match word(value) {
            Some(value) => whole_second(field, &value).map(Some),
            None => Ok(None),
        };
        let (from, to) = (bound("from", &self.from)?, bound("to", &self.to)?);
        if matches!((&from, &to), (Some(from), Some(to)) if from >= to) {
            return Err(HistoryError::Filter(
                "from must be earlier than to".to_owned(),
            ));
        }
        let audio = match word(&self.audio).as_deref() {
            None => None,
            Some("with") => Some(true),
            Some("without") => Some(false),
            Some(other) => {
                return Err(HistoryError::Filter(format!(
                    "audio must be with or without, got {other}"
                )))
            }
        };
        Ok(Conditions {
            text: text.to_owned(),
            from,
            to,
            mode: word(&self.mode),
            app: word(&self.app),
            model: word(&self.model),
            audio,
        })
    }
}

impl Conditions {
    /// Whether nothing is set: the selection is the whole library.
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// `value` without surrounding whitespace, or `None` when nothing is left.
fn word(value: &Option<String>) -> Option<String> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// A time bound as the SQL compares it: `YYYY-MM-DDTHH:MM:SSZ` in UTC. The
/// RFC 3339 text may carry any offset; its fractional seconds must be
/// zero, because a bound names a whole second (see `conditions_sql`). A
/// value that is neither is a `Filter` error naming `field`.
fn whole_second(field: &str, value: &str) -> Result<String, HistoryError> {
    let invalid = || {
        HistoryError::Filter(format!(
            "{field} must be an RFC 3339 time on a whole second, got {value:?}"
        ))
    };
    let time = OffsetDateTime::parse(value, &Rfc3339).map_err(|_| invalid())?;
    if time.nanosecond() != 0 {
        return Err(invalid());
    }
    time.to_utc().format(&Rfc3339).map_err(|_| invalid())
}

fn page_query(
    conditions: &Conditions,
    after: Option<&[String; 2]>,
    limit: i64,
) -> (String, Vec<rusqlite::types::Value>) {
    let mut params = Vec::new();
    let clause = conditions_sql(conditions, after, &mut params);
    // The extra row tells the caller whether another page exists.
    params.push(rusqlite::types::Value::Integer(limit + 1));
    let sql = format!(
        "SELECT {COLUMNS} FROM recognition_history{clause}
         ORDER BY created_at DESC, id DESC LIMIT ?"
    );
    (sql, params)
}

/// The `WHERE` clause of a selection, empty when nothing narrows it:
/// `conditions` and, when a cursor is given, the rows after its row. The
/// values of the `?` go to `params` in order.
fn conditions_sql(
    conditions: &Conditions,
    after: Option<&[String; 2]>,
    params: &mut Vec<rusqlite::types::Value>,
) -> String {
    use rusqlite::types::Value;
    let mut clauses: Vec<String> = Vec::new();
    if let Some([created_at, record_id]) = after {
        clauses.push("(created_at, id) < (?, ?)".to_owned());
        params.push(Value::Text(created_at.clone()));
        params.push(Value::Text(record_id.clone()));
    }
    // Time bounds are whole seconds. A row's `created_at` carries its
    // subsecond only when it is not zero, and `.` sorts before `Z`, so
    // `…:00.5Z` is textually below `…:00Z`: the second followed by a `.`
    // has every row of that second at or above it and no earlier row.
    for (comparison, bound) in [
        ("created_at >=", &conditions.from),
        ("created_at <", &conditions.to),
    ] {
        if let Some(bound) = bound {
            clauses.push(format!("{comparison} ?"));
            params.push(Value::Text(format!("{}.", &bound[..19])));
        }
    }
    for (comparison, value) in [
        ("processing_mode =", &conditions.mode),
        ("app_id =", &conditions.app),
        ("asr_model =", &conditions.model),
    ] {
        if let Some(value) = value {
            clauses.push(format!("{comparison} ?"));
            params.push(Value::Text(value.clone()));
        }
    }
    if let Some(with_recording) = conditions.audio {
        clauses.push(if with_recording {
            "audio_path IS NOT NULL".to_owned()
        } else {
            "audio_path IS NULL".to_owned()
        });
    }
    if !conditions.text.is_empty() {
        // LIKE is a substring test that ignores the case of ASCII letters;
        // `%`, `_` and the escape character in the text itself are
        // escaped, so they match only literally.
        clauses.push("(raw_text LIKE ? ESCAPE '\\' OR final_text LIKE ? ESCAPE '\\')".to_owned());
        let pattern = Value::Text(format!("%{}%", escape_like(&conditions.text)));
        params.push(pattern.clone());
        params.push(pattern);
    }
    if clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", clauses.join(" AND "))
    }
}

/// `text` for `LIKE … ESCAPE '\'`, with the pattern characters written so
/// they match only themselves.
fn escape_like(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for ch in text.chars() {
        if matches!(ch, '%' | '_' | '\\') {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

/// What a page cursor names: the row the next page continues after and,
/// for a filtered page, the checked filter it was selected with.
#[derive(Deserialize)]
#[serde(untagged)]
enum CursorKey {
    Filtered {
        after: [String; 2],
        filter: Conditions,
    },
    Unfiltered([String; 2]),
}

/// Opaque page token: unpadded base64url of JSON, URL-safe so the console
/// can pass it as a query parameter unchanged. An unfiltered page keeps
/// the array `[created_at, id]` of its last row that earlier builds wrote;
/// a filtered page is `{"after": [created_at, id], "filter": …}`.
fn encode_cursor(created_at: &str, record_id: &str, conditions: &Conditions) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    let payload = if conditions.is_empty() {
        serde_json::json!([created_at, record_id])
    } else {
        serde_json::json!({ "after": [created_at, record_id], "filter": conditions })
    };
    URL_SAFE_NO_PAD.encode(payload.to_string().as_bytes())
}

/// Inverse of `encode_cursor`; every decoding failure is
/// `HistoryError::Cursor` so callers can answer "bad cursor" uniformly.
fn decode_cursor(cursor: &str) -> Result<CursorKey, HistoryError> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    let bytes = URL_SAFE_NO_PAD
        .decode(cursor.as_bytes())
        .map_err(|_| HistoryError::Cursor)?;
    serde_json::from_slice(&bytes).map_err(|_| HistoryError::Cursor)
}

/// The first line of the CSV export: `COLUMNS`.
const CSV_HEADER: &str = "id,created_at,duration_seconds,raw_text,processing_mode,processed_text,final_text,status,character_count,asr_provider,asr_model,audio_path,app_id\n";

/// One record as a CSV line, fields in `COLUMNS` order: a field with a
/// comma, a quote or a line break is quoted with its quotes doubled
/// (RFC 4180); NULL fields are empty.
fn csv_line(record: &HistoryRecord) -> String {
    let duration = record
        .duration_seconds
        .map(|value| value.to_string())
        .unwrap_or_default();
    let count = record.character_count.to_string();
    let fields = [
        record.id.as_str(),
        record.created_at.as_str(),
        duration.as_str(),
        record.raw_text.as_str(),
        record.processing_mode.as_deref().unwrap_or_default(),
        record.processed_text.as_deref().unwrap_or_default(),
        record.final_text.as_str(),
        record.status.as_str(),
        count.as_str(),
        record.asr_provider.as_deref().unwrap_or_default(),
        record.asr_model.as_deref().unwrap_or_default(),
        record.audio_path.as_deref().unwrap_or_default(),
        record.app_id.as_deref().unwrap_or_default(),
    ];
    let mut line = String::new();
    for (index, field) in fields.iter().enumerate() {
        if index > 0 {
            line.push(',');
        }
        if field.contains([',', '"', '\n', '\r']) {
            line.push('"');
            line.push_str(&field.replace('"', "\"\""));
            line.push('"');
        } else {
            line.push_str(field);
        }
    }
    line.push('\n');
    line
}

/// A facet's values with their counts, the most frequent first and by
/// value among equals.
fn ranked(counts: BTreeMap<String, i64>) -> Vec<HistoryFacet> {
    let mut facets: Vec<HistoryFacet> = counts
        .into_iter()
        .map(|(value, records)| HistoryFacet { value, records })
        .collect();
    facets.sort_by(|a, b| {
        b.records
            .cmp(&a.records)
            .then_with(|| a.value.cmp(&b.value))
    });
    facets
}

/// Best-effort cleanup: retained recordings whose row is gone must not
/// linger. Missing files are not an error.
fn remove_audio_files(paths: &[String]) {
    for path in paths {
        let _ = std::fs::remove_file(path);
    }
}

/// Recording paths of the given ids (rows without one are skipped), read
/// before a `DELETE` so the files can still be located afterwards.
fn audio_paths_for(
    connection: &rusqlite::Connection,
    ids: &[String],
) -> Result<Vec<String>, HistoryError> {
    let placeholders = vec!["?"; ids.len()].join(", ");
    let mut statement = connection
        .prepare(&format!(
            "SELECT audio_path FROM recognition_history
             WHERE id IN ({placeholders}) AND audio_path IS NOT NULL"
        ))
        .map_err(|error| HistoryError::Database(error.to_string()))?;
    let rows = statement
        .query_map(rusqlite::params_from_iter(ids), |row| {
            row.get::<_, String>(0)
        })
        .map_err(|error| HistoryError::Database(error.to_string()))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| HistoryError::Database(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A store over a fresh temporary database.
    fn database() -> (tempfile::TempDir, HistoryStore) {
        let root = tempfile::tempdir().expect("temporary root");
        let store = HistoryStore::open(root.path().join("history.sqlite3")).expect("open store");
        (root, store)
    }

    /// `EXPLAIN QUERY PLAN` of a page selected by `conditions`: one line
    /// per step.
    fn plan(store: &HistoryStore, conditions: &Conditions, after: bool) -> String {
        let cursor = [String::new(), String::new()];
        let (sql, params) = page_query(conditions, after.then_some(&cursor), 20);
        let connection = store.connect().expect("connect");
        let steps = connection
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .expect("prepare")
            .query_map(rusqlite::params_from_iter(params), |row| {
                row.get::<_, String>(3)
            })
            .expect("plan")
            .map(|step| step.expect("step"))
            .collect::<Vec<_>>();
        steps.join("\n")
    }

    /// Whatever the filter and whether a cursor is given, a page reads the
    /// library in order from the index and stops at its size; a plan that
    /// scanned the table or sorted it first would make every page cost the
    /// whole library.
    #[test]
    fn every_page_walks_the_order_index_without_a_sort() {
        let (_root, store) = database();
        let text = |text: &str| HistoryFilter {
            text: text.to_owned(),
            ..HistoryFilter::default()
        };
        let narrow = HistoryFilter {
            text: "会议".to_owned(),
            from: Some("2025-01-01T00:00:00Z".to_owned()),
            to: Some("2026-01-01T00:00:00Z".to_owned()),
            mode: Some("quick".to_owned()),
            app: Some("kitty".to_owned()),
            model: Some("qwen3-asr-0.6b-gguf".to_owned()),
            audio: Some("with".to_owned()),
        };
        for filter in [
            HistoryFilter::default(),
            text("会议"),
            text("postgres"),
            narrow,
        ] {
            let conditions = filter.conditions().expect("valid filter");
            for after in [false, true] {
                let steps = plan(&store, &conditions, after);
                assert!(
                    steps.contains("USING INDEX recognition_history_order"),
                    "{steps}"
                );
                assert!(!steps.contains("TEMP B-TREE"), "{steps}");
            }
        }
    }

    /// Bounds land on whole UTC seconds and blank fields drop out; a
    /// fractional or unparsable bound, a reversed range, a long text, a
    /// NUL and a stray `audio` value are each named.
    #[test]
    fn conditions_normalize_the_filter_and_name_what_they_reject() {
        let mut filter = HistoryFilter {
            text: "  PostgreSQL_ ".to_owned(),
            from: Some("2025-01-01T08:00:00.000+08:00".to_owned()),
            to: Some(" 2026-01-01T00:00:00Z ".to_owned()),
            mode: Some(" ".to_owned()),
            audio: Some("without".to_owned()),
            ..HistoryFilter::default()
        };
        assert_eq!(
            filter.conditions().expect("valid filter"),
            Conditions {
                text: "PostgreSQL_".to_owned(),
                from: Some("2025-01-01T00:00:00Z".to_owned()),
                to: Some("2026-01-01T00:00:00Z".to_owned()),
                mode: None,
                app: None,
                model: None,
                audio: Some(false),
            }
        );
        let rejected = |filter: &HistoryFilter| match filter.conditions() {
            Err(HistoryError::Filter(message)) => message,
            other => panic!("expected a filter error, got {other:?}"),
        };
        filter.text = "x".repeat(MAX_SEARCH_CHARS + 1);
        assert!(rejected(&filter).contains("at most"));
        filter.text = "a\0b".to_owned();
        assert!(rejected(&filter).contains("NUL"));
        filter.text = String::new();
        filter.audio = Some("yes".to_owned());
        assert!(rejected(&filter).contains("with or without"));
        filter.audio = None;
        filter.from = Some("2025-01-01".to_owned());
        assert!(rejected(&filter).contains("RFC 3339"));
        filter.from = Some("2025-01-01T00:00:00.5Z".to_owned());
        assert!(rejected(&filter).contains("whole second"));
        filter.from = Some("2026-01-02T00:00:00Z".to_owned());
        filter.to = Some("2026-01-01T00:00:00Z".to_owned());
        assert!(rejected(&filter).contains("earlier than to"));
    }
}
