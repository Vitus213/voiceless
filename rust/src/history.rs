//! Recognition history persisted in SQLite with the Python-compatible
//! `recognition_history` schema, WAL journaling and 0600 permissions.
//!
//! File: `<XDG data dir>/voiceless/history.sqlite3`
//! (`models::default_data_dir`). Opened by `stream::build_pipeline` when
//! `[history].enabled` (one `insert` per successful session through
//! `coordinator::HistoryRecorder`), by `history_cmd` for
//! `voiceless history list|delete|export|totals|usage`, and by the console
//! (`GET /api/records`, `GET /api/totals`, `DELETE /api/records/{id}`;
//! audio playback and re-transcription locate the WAV through `get`).
//!
//! Schema: `PRAGMA user_version` 1 is the original table; version 2 adds
//! the nullable `audio_path` column (the retained WAV of the session, see
//! `[history].save_audio`). `open` migrates forward inside one
//! `BEGIN IMMEDIATE` transaction and refuses a database whose version is
//! newer than `SCHEMA_VERSION`. Rows are inserted once and only ever
//! deleted (together with their recording); nothing updates them.
//! `created_at` is RFC 3339 UTC with a `Z` suffix and `id` a random
//! UUID-v4-style string, so `(created_at DESC, id DESC)` is the total order
//! used for listing, CSV export and the keyset cursors.
//!
//! Concurrency: the store keeps only the path. Every call opens its own
//! connection (`connect`) with WAL journaling and a 5 s busy timeout, so
//! the daemon, the CLI and the console can read and write the same file at
//! the same time and the handle can be shared in an `Arc`. All methods are
//! synchronous and block on SQLite. Audio files are removed after the
//! owning rows, best effort: a row never outlives its file by design, but
//! an unlink failure can leave a file behind.

use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};

use crate::coordinator::HistoryEntry;

/// Newest `user_version` this build understands; older files are
/// migrated, newer ones rejected.
const SCHEMA_VERSION: i64 = 2;
/// How long a connection waits for a writer lock before failing; covers
/// the daemon inserting while the console or CLI reads.
const BUSY_TIMEOUT_MS: i64 = 5_000;

/// Failures of the store. `stream` logs them and keeps the session
/// result; the CLI prints them and exits 1; the console answers 500.
#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    /// A SQLite error, a schema newer than this build, or an argument
    /// rejected up front (page size, usage window).
    #[error("history database error: {0}")]
    Database(String),
    /// The current time could not be formatted as RFC 3339.
    #[error("history timestamp invalid: {0}")]
    Timestamp(String),
    /// A page cursor that `query` did not produce (bad base64 or JSON).
    #[error("history cursor invalid")]
    Cursor,
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

/// Handle to the history database. Holds only the path: every method
/// opens a fresh connection, so the handle is cheap to share and never
/// keeps a lock between calls.
pub struct HistoryStore {
    path: PathBuf,
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

    /// Open one connection with the busy timeout and WAL journaling. WAL
    /// lets readers (console, CLI) proceed while the daemon inserts; the
    /// setting is persistent in the file, so every opener ends up in WAL
    /// mode whichever process created the database.
    fn connect(&self) -> Result<Connection, HistoryError> {
        let connection = Connection::open(&self.path)
            .map_err(|error| HistoryError::Database(error.to_string()))?;
        connection
            .pragma_update(None, "busy_timeout", BUSY_TIMEOUT_MS)
            .map_err(|error| HistoryError::Database(error.to_string()))?;
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .map_err(|error| HistoryError::Database(error.to_string()))?;
        Ok(connection)
    }

    /// Bring the schema to `SCHEMA_VERSION` (v1 table, then the v2
    /// `audio_path` column) and tighten the file mode. Idempotent.
    fn migrate(&self) -> Result<(), HistoryError> {
        let connection = self.connect()?;
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

    /// Append one row for a successful session and return it as stored.
    /// The id and `created_at` are generated here, `character_count` is
    /// derived from `final_text`, `backend` becomes `asr_provider` and
    /// `entry.model` becomes `asr_model` (NULL when unknown). Does not touch the audio file: the
    /// recorder already finalized it at `entry.audio_path`.
    pub fn insert(
        &self,
        entry: &HistoryEntry,
        backend: &str,
    ) -> Result<HistoryRecord, HistoryError> {
        let id = new_record_id();
        let created_at = timestamp_now()?;
        let connection = self.connect()?;
        connection
            .execute(
                "INSERT INTO recognition_history (
                    id, created_at, duration_seconds, raw_text, processing_mode,
                    processed_text, final_text, status, character_count,
                    asr_provider, asr_model, audio_path
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    id,
                    created_at,
                    entry.duration_seconds,
                    entry.raw_text,
                    entry.processing_mode,
                    entry.processed_text,
                    entry.final_text,
                    entry.status,
                    entry.final_text.chars().count() as i64,
                    backend,
                    entry.model,
                    entry.audio_path,
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
            character_count: entry.final_text.chars().count() as i64,
            asr_provider: Some(backend.to_owned()),
            asr_model: entry.model.clone(),
            audio_path: entry.audio_path.clone(),
        })
    }

    /// List rows newest first with keyset pagination. `limit` must be
    /// within 1..=1000 (`Database` error otherwise); `cursor` is the
    /// `next_cursor` of a previous page (`Cursor` error when malformed).
    /// A cursor points after a specific `(created_at, id)` rather than at
    /// an offset, so pages stay consistent while rows are inserted or
    /// deleted in between: nothing is repeated or skipped. The last page
    /// carries `next_cursor: None`.
    pub fn query(&self, limit: i64, cursor: Option<&str>) -> Result<HistoryPage, HistoryError> {
        if !(1..=1_000).contains(&limit) {
            return Err(HistoryError::Database(format!(
                "page size must be between 1 and 1000, got {limit}"
            )));
        }
        let connection = self.connect()?;
        let mut statement;
        // Both branches fetch `limit + 1` rows: the extra row says whether
        // another page exists without a second COUNT query.
        let rows = if let Some(cursor) = cursor {
            let (created_at, record_id) = decode_cursor(cursor)?;
            statement = connection
                .prepare(
                    "SELECT id, created_at, duration_seconds, raw_text, processing_mode,
                            processed_text, final_text, status, character_count,
                            asr_provider, asr_model, audio_path
                     FROM recognition_history
                     WHERE (created_at < ?1 OR (created_at = ?1 AND id < ?2))
                     ORDER BY created_at DESC, id DESC LIMIT ?3",
                )
                .map_err(|error| HistoryError::Database(error.to_string()))?;
            statement
                .query_map(params![created_at, record_id, limit + 1], record_from_row)
                .map_err(|error| HistoryError::Database(error.to_string()))?
        } else {
            statement = connection
                .prepare(
                    "SELECT id, created_at, duration_seconds, raw_text, processing_mode,
                            processed_text, final_text, status, character_count,
                            asr_provider, asr_model, audio_path
                     FROM recognition_history
                     ORDER BY created_at DESC, id DESC LIMIT ?1",
                )
                .map_err(|error| HistoryError::Database(error.to_string()))?;
            statement
                .query_map(params![limit + 1], record_from_row)
                .map_err(|error| HistoryError::Database(error.to_string()))?
        };
        let mut records = Vec::new();
        for row in rows {
            records.push(row.map_err(|error| HistoryError::Database(error.to_string()))?);
        }
        let has_more = records.len() as i64 > limit;
        records.truncate(limit as usize);
        let next_cursor = if has_more {
            records
                .last()
                .map(|last| encode_cursor(&last.created_at, &last.id))
        } else {
            None
        };
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
        let since = (OffsetDateTime::now_utc() - Duration::days(days))
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

    /// Write every row, newest first, to `destination` as CSV with a
    /// fixed header (`id,created_at,…,audio_path`), creating parent
    /// directories and overwriting an existing file. Fields containing
    /// `,`, `"` or a newline are quoted with doubled quotes (RFC 4180);
    /// NULL fields are empty. Returns the number of rows written.
    pub fn export_csv(&self, destination: &Path) -> Result<i64, HistoryError> {
        let connection = self.connect()?;
        let mut statement = connection
            .prepare(
                "SELECT id, created_at, duration_seconds, raw_text, processing_mode,
                        processed_text, final_text, status, character_count,
                        asr_provider, asr_model, audio_path
                 FROM recognition_history ORDER BY created_at DESC, id DESC",
            )
            .map_err(|error| HistoryError::Database(error.to_string()))?;
        let records: Vec<HistoryRecord> = statement
            .query_map([], record_from_row)
            .map_err(|error| HistoryError::Database(error.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| HistoryError::Database(error.to_string()))?;
        let mut csv = String::from(
            "id,created_at,duration_seconds,raw_text,processing_mode,processed_text,final_text,status,character_count,asr_provider,asr_model,audio_path\n",
        );
        for record in &records {
            let fields = [
                &record.id,
                &record.created_at,
                &record
                    .duration_seconds
                    .map(|value| value.to_string())
                    .unwrap_or_default(),
                &record.raw_text,
                record.processing_mode.as_deref().unwrap_or_default(),
                record.processed_text.as_deref().unwrap_or_default(),
                &record.final_text,
                &record.status,
                &record.character_count.to_string(),
                record.asr_provider.as_deref().unwrap_or_default(),
                record.asr_model.as_deref().unwrap_or_default(),
                record.audio_path.as_deref().unwrap_or_default(),
            ];
            let escaped: Vec<String> = fields
                .iter()
                .map(|field| {
                    if field.contains(',') || field.contains('"') || field.contains('\n') {
                        format!("\"{}\"", field.replace('"', "\"\""))
                    } else {
                        (*field).to_string()
                    }
                })
                .collect();
            csv.push_str(&escaped.join(","));
            csv.push('\n');
        }
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(destination, csv)?;
        Ok(records.len() as i64)
    }

    /// Fetch one row by id; `Ok(None)` when there is no such row.
    pub fn get(&self, record_id: &str) -> Result<Option<HistoryRecord>, HistoryError> {
        let connection = self.connect()?;
        let record = connection
            .query_row(
                "SELECT id, created_at, duration_seconds, raw_text, processing_mode,
                        processed_text, final_text, status, character_count,
                        asr_provider, asr_model, audio_path
                 FROM recognition_history WHERE id = ?1",
                params![record_id],
                record_from_row,
            )
            .optional()
            .map_err(|error| HistoryError::Database(error.to_string()))?;
        Ok(record)
    }
}

/// Column order shared by every `SELECT` in this module.
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

/// Opaque page token: unpadded base64url of the JSON array
/// `[created_at, id]` of the last row on the page. URL-safe so the
/// console can pass it as a query parameter unchanged.
fn encode_cursor(created_at: &str, record_id: &str) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    let payload = serde_json::json!([created_at, record_id]).to_string();
    URL_SAFE_NO_PAD.encode(payload.as_bytes())
}

/// Inverse of `encode_cursor`; every decoding failure is
/// `HistoryError::Cursor` so callers can answer "bad cursor" uniformly.
fn decode_cursor(cursor: &str) -> Result<(String, String), HistoryError> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    let bytes = URL_SAFE_NO_PAD
        .decode(cursor.as_bytes())
        .map_err(|_| HistoryError::Cursor)?;
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| HistoryError::Cursor)?;
    let created_at = value
        .get(0)
        .and_then(serde_json::Value::as_str)
        .ok_or(HistoryError::Cursor)?
        .to_owned();
    let record_id = value
        .get(1)
        .and_then(serde_json::Value::as_str)
        .ok_or(HistoryError::Cursor)?
        .to_owned();
    Ok((created_at, record_id))
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
