//! Retention of the recognition history, the recovery area that keeps the
//! recordings of failed sessions, and the export that takes a selection of
//! the library away. A child module of `history`: like the store's own
//! methods, every call opens its own connection
//! (`HistoryStore::connect`), and an export selects its rows with the
//! filter machinery of the parent module.
//!
//! # Retention
//!
//! The text and recording age limits are in days. Zero keeps items until
//! manual removal, which preserves history from an older `config.toml`:
//! `text_days` for rows, `audio_days` for their recordings,
//! `failed_audio_days` and `failed_audio_mb` for the recovery area. They
//! are independent, so a recording can expire before its text: the row
//! then loses its `audio_path` and keeps its text, which stays listed,
//! searchable and exportable.
//!
//! Nothing goes without a preview. [`preview`] computes what a policy
//! removes right now and returns it with a `snapshot`: an opaque token of
//! the policy, the instants it expires at and a digest of each set of
//! rows. [`apply`] takes only that token, selects the rows again under
//! the same policy and instants, and deletes when, and only what, the
//! digests are still the same: a row added or deleted in between changes
//! a digest and the answer is [`Applied::Changed`] with a fresh preview
//! to confirm again. A token older than `SNAPSHOT_LIFETIME_SECS` is
//! refused, so a preview left on screen is not applied much later.
//! [`apply_policy`] is the same cleanup without the gate, for a runtime
//! that applies the saved policy on its own.
//!
//! Only files Voiceless owns are removed: a recording whose path is a
//! direct child of the audio directory handed to the call (where
//! `[history].save_audio` writes), and only once no row outside the
//! selection references it. A row that points anywhere else keeps its
//! file; the row itself goes with its text when the text expires. A row
//! whose `created_at` is not a UTC `Z` timestamp is never selected: time
//! cannot be compared with it.
//!
//! Recordings are removed before their rows are deleted. A failure after
//! a removal then leaves a row whose file is gone, which the console shows
//! as such and the next cleanup clears, never a recording without a row.
//!
//! # Failed-session recovery
//!
//! Cancellation discards the recording. A failed session with recoverable
//! audio and an opted-in user (`[history].save_failed_audio`) moves its
//! finalized recording into a [`RecoveryStore`]: one directory
//! of `<id>.wav` files, each with a `<id>.json` sidecar, all private
//! (0600 in a 0700 directory). The recordings are listed, replayed,
//! re-transcribed and discarded explicitly; nothing in the area is ever a
//! history record. The area keeps itself within `failed_audio_days` and
//! `failed_audio_mb`, the oldest first.
//!
//! # Export
//!
//! [`HistoryStore::export`] writes the rows a [`HistoryFilter`] selects
//! (the filter of the search and of the CSV export) as text, Markdown,
//! CSV or JSON. Their recordings leave only when the caller asks for them,
//! and then as one ZIP archive holding the document and the WAV files.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use flate2::Crc;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use super::{
    conditions_sql, csv_line, record_from_row, HistoryError, HistoryFilter, HistoryRecord,
    HistoryStore, COLUMNS, CSV_HEADER,
};

/// Longest period a policy can name, in days: a hundred years.
pub const MAX_RETENTION_DAYS: u32 = 36_500;
/// Largest recovery area, in MB: a terabyte.
pub const MAX_RECOVERY_MB: u32 = 1_048_576;
/// Largest export one request builds, in bytes.
pub const MAX_EXPORT_BYTES: u64 = 256 * 1024 * 1024;
/// How long after its preview a snapshot may be applied.
const SNAPSHOT_LIFETIME_SECS: i64 = 10 * 60;
/// How far ahead of the clock a snapshot's issue time may lie.
const SNAPSHOT_SKEW_SECS: i64 = 60;
/// Layout version of a snapshot; one of another version is refused.
const SNAPSHOT_VERSION: u32 = 1;
/// Rows one `DELETE … IN (…)` statement names at once.
const DELETE_CHUNK: usize = 500;
/// Rows whose text the preview shows.
const SAMPLES: i64 = 20;
/// The form of `created_at` a cleanup can compare times with.
const STAMPED: &str =
    "'[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*'";

/// A failure of a cleanup, a preview or an export.
#[derive(Debug, thiserror::Error)]
pub enum RetentionError {
    /// A policy value out of range.
    #[error("retention policy invalid: {0}")]
    Policy(String),
    /// A snapshot that does not decode, is of another version or is no longer current.
    #[error("retention preview invalid: {0}")]
    Snapshot(String),
    /// The export would exceed [`MAX_EXPORT_BYTES`].
    #[error(
        "the export is too large for one file ({total} bytes); export fewer records, or no audio"
    )]
    TooLarge { total: u64 },
    #[error(transparent)]
    Recovery(#[from] RecoveryError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    History(#[from] HistoryError),
}

/// How long text, recordings and failed sessions' recordings are kept.
/// Every period is in days; `0` keeps until deleted by hand.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RetentionPolicy {
    /// Days a row is kept since its session.
    pub text_days: u32,
    /// Days a row's recording is kept; the text stays when it goes.
    pub audio_days: u32,
    /// Days the recording of a failed session stays in the recovery area.
    pub failed_audio_days: u32,
    /// Space the recovery area may take, in decimal MB; past it the oldest recordings go.
    pub failed_audio_mb: u32,
}

impl RetentionPolicy {
    /// The pruning periods of `[history]`.
    pub fn of(config: &crate::config::HistoryConfig) -> Self {
        Self {
            text_days: config.text_days,
            audio_days: config.audio_days,
            failed_audio_days: config.failed_audio_days,
            failed_audio_mb: config.failed_audio_mb,
        }
    }

    /// Whether every period is within range; `Policy` names the first one that is not.
    pub fn check(&self) -> Result<(), RetentionError> {
        for (name, days) in [
            ("text_days", self.text_days),
            ("audio_days", self.audio_days),
            ("failed_audio_days", self.failed_audio_days),
        ] {
            if days > MAX_RETENTION_DAYS {
                return Err(RetentionError::Policy(format!(
                    "{name} must be at most {MAX_RETENTION_DAYS}, got {days}"
                )));
            }
        }
        if self.failed_audio_mb > MAX_RECOVERY_MB {
            return Err(RetentionError::Policy(format!(
                "failed_audio_mb must be at most {MAX_RECOVERY_MB}, got {}",
                self.failed_audio_mb
            )));
        }
        Ok(())
    }

    /// The instants this policy expires things at when it is `now`.
    fn cutoffs(&self, now: OffsetDateTime) -> Result<Cutoffs, RetentionError> {
        Ok(Cutoffs {
            text: cutoff(now, self.text_days)?,
            audio: cutoff(now, self.audio_days)?,
            recovery: cutoff(now, self.failed_audio_days)?,
        })
    }
}

/// The instants a policy expires things at, as `created_at` text (UTC, a
/// whole second, `Z`); `None` where nothing expires.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Cutoffs {
    text: Option<String>,
    audio: Option<String>,
    recovery: Option<String>,
}

/// `now` less `days` whole days as `created_at` text; `None` for `0` days.
fn cutoff(now: OffsetDateTime, days: u32) -> Result<Option<String>, RetentionError> {
    if days == 0 {
        return Ok(None);
    }
    let seconds = now.unix_timestamp() - i64::from(days) * 86_400;
    OffsetDateTime::from_unix_timestamp(seconds)
        .map_err(|error| HistoryError::Timestamp(error.to_string()))?
        .format(&Rfc3339)
        .map(Some)
        .map_err(|error| RetentionError::History(HistoryError::Timestamp(error.to_string())))
}

/// What the library holds right now.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct RetentionUsage {
    /// Rows in the library.
    pub records: i64,
    /// `character_count` summed over the rows.
    pub characters: i64,
    /// Rows whose recording is still on disk, wherever it is.
    pub recordings: i64,
    /// Bytes of those recordings.
    pub audio_bytes: u64,
}

/// What a policy removes right now, and the token [`apply`] takes for it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RetentionPreview {
    /// Handle of exactly this selection (see the module doc).
    pub snapshot: String,
    /// The policy that was previewed.
    pub policy: RetentionPolicy,
    /// The rows that go with their text.
    pub text: TextExpiry,
    /// The recordings that go while their rows stay.
    pub audio: AudioExpiry,
    /// The recordings of failed sessions that go.
    pub recovery: RecoveryExpiry,
}

/// The rows a policy deletes.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct TextExpiry {
    pub records: i64,
    /// `character_count` summed over the rows.
    pub characters: i64,
    /// Their recordings removed with them, and the bytes.
    pub recordings: i64,
    pub bytes: u64,
    /// Their recordings that stay on disk: outside the audio directory, or referenced by a row that stays.
    pub kept: i64,
    /// `created_at` of the oldest and of the newest of the rows; `None` when there are none.
    pub oldest: Option<String>,
    pub newest: Option<String>,
    /// Up to [`SAMPLES`] of the rows, the newest first, to show what goes.
    pub samples: Vec<ExpiringSample>,
}

/// One row a preview shows the text of.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExpiringSample {
    pub id: String,
    pub created_at: String,
    /// The final text, as stored.
    pub text: String,
}

/// The recordings a policy removes from the rows that stay.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct AudioExpiry {
    /// Files removed.
    pub recordings: i64,
    pub bytes: u64,
    /// Rows whose file is already gone: the reference is cleared.
    pub missing: i64,
    /// Rows that keep their recording: it is outside the audio directory, or a row that keeps it shares it.
    pub kept: i64,
}

/// The recordings of failed sessions a policy removes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RecoveryExpiry {
    pub recordings: i64,
    pub bytes: u64,
    /// `saved_at` of the oldest one; `None` when there are none.
    pub oldest: Option<String>,
}

/// What a cleanup removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct RetentionReport {
    /// Rows deleted, with their text.
    pub records: i64,
    /// Recording files removed, with their rows and from the rows that stay.
    pub recordings: i64,
    /// Bytes of those files.
    pub bytes: u64,
    /// Rows that kept their text and lost the reference to a file that was already gone.
    pub missing: i64,
    /// Recordings of failed sessions removed, and their bytes.
    pub failed_recordings: i64,
    pub failed_bytes: u64,
    /// Recording files a cleanup meant to remove but could not.
    pub failed: i64,
}

impl RetentionReport {
    /// Count a recording of a row that stays.
    fn uncouple(&mut self, outcome: Unlinked) {
        match outcome {
            Unlinked::Removed(bytes) => {
                self.recordings += 1;
                self.bytes += bytes;
            }
            Unlinked::Missing => self.missing += 1,
            Unlinked::Failed => self.failed += 1,
        }
    }

    /// Count what the recovery area gave up.
    fn absorb(&mut self, removal: RecoveryRemoval) {
        self.failed_recordings += removal.recordings;
        self.failed_bytes += removal.bytes;
        self.failed += removal.failed;
    }
}

/// The outcome of [`apply`]: what went, or the preview to confirm again.
#[derive(Debug, PartialEq)]
pub enum Applied {
    Removed(RetentionReport),
    /// The library changed since the preview; nothing was removed.
    Changed(RetentionPreview),
}

/// What a preview binds: the selection it showed, to be the same when applied.
#[derive(Serialize, Deserialize)]
struct Snapshot {
    version: u32,
    policy: RetentionPolicy,
    cutoffs: Cutoffs,
    /// Unix seconds of the preview.
    issued: i64,
    text: String,
    audio: String,
    recovery: String,
}

/// A row a cleanup selects.
#[derive(Debug, PartialEq, Eq)]
struct ExpiringRow {
    id: String,
    created_at: String,
    characters: i64,
    audio_path: Option<String>,
}

/// The rows a cleanup selects at given cutoffs: `text` with their rows, `audio` from the rows that stay.
#[derive(Debug, Default)]
struct Plan {
    text: Vec<ExpiringRow>,
    audio: Vec<ExpiringRow>,
}

/// What removing a recording file came to.
#[derive(Debug, PartialEq, Eq)]
enum Unlinked {
    Removed(u64),
    Missing,
    Failed,
}

/// What [`HistoryStore::export`] built.
#[derive(Debug, PartialEq, Eq)]
pub struct Exported {
    pub file_name: String,
    pub media_type: &'static str,
    pub bytes: Vec<u8>,
}

/// The form of an export.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExportFormat {
    #[serde(rename = "txt")]
    Text,
    #[serde(rename = "md")]
    Markdown,
    #[serde(rename = "csv")]
    Csv,
    #[serde(rename = "json")]
    Json,
}

impl ExportFormat {
    /// File extension of a document in this format.
    fn extension(self) -> &'static str {
        match self {
            Self::Text => "txt",
            Self::Markdown => "md",
            Self::Csv => "csv",
            Self::Json => "json",
        }
    }

    /// Media type of a document in this format.
    fn media_type(self) -> &'static str {
        match self {
            Self::Text => "text/plain; charset=utf-8",
            Self::Markdown => "text/markdown; charset=utf-8",
            Self::Csv => "text/csv; charset=utf-8",
            Self::Json => "application/json",
        }
    }
}

/// One recording an export carries.
struct Recording {
    /// Name of the file inside the archive.
    name: String,
    /// Path the row stores, the only place the bytes are read from.
    path: String,
    created_at: String,
}

/// One file of a ZIP archive.
struct ZipEntry {
    name: String,
    data: Vec<u8>,
    modified: OffsetDateTime,
}

/// What a policy removes right now from `store` (recordings under `audio_dir`) and from `recovery`.
pub fn preview(
    store: &HistoryStore,
    recovery: &RecoveryStore,
    policy: &RetentionPolicy,
    audio_dir: &Path,
    now: OffsetDateTime,
) -> Result<RetentionPreview, RetentionError> {
    policy.check()?;
    let cutoffs = policy.cutoffs(now)?;
    let connection = store.connect()?;
    let (plan, text, audio) = library_preview(&connection, &cutoffs, audio_dir)?;
    let samples = match &cutoffs.text {
        Some(cutoff) => samples(&connection, cutoff)?,
        None => Vec::new(),
    };
    drop(connection);
    let expired = recovery.expired(cutoffs.recovery.as_deref(), policy.failed_audio_mb)?;
    let snapshot = Snapshot {
        version: SNAPSHOT_VERSION,
        policy: *policy,
        cutoffs,
        issued: now.unix_timestamp(),
        text: text_digest(&plan.text),
        audio: audio_digest(&plan.audio),
        recovery: recovery_digest(&expired),
    };
    let bytes = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&snapshot).map_err(encode)?);
    Ok(RetentionPreview {
        snapshot: bytes,
        policy: *policy,
        text: TextExpiry { samples, ..text },
        audio,
        recovery: recovery_expiry(&expired),
    })
}

/// Remove what `token` previewed, and only that: the rows are selected again under the token's policy and instants and removed only while the selection is still the one that was shown.
pub fn apply(
    store: &HistoryStore,
    recovery: &RecoveryStore,
    token: &str,
    audio_dir: &Path,
    now: OffsetDateTime,
) -> Result<Applied, RetentionError> {
    let snapshot = decode(token)?;
    let age = now.unix_timestamp().saturating_sub(snapshot.issued);
    if !(-SNAPSHOT_SKEW_SECS..=SNAPSHOT_LIFETIME_SECS).contains(&age) {
        return Err(RetentionError::Snapshot(
            "the preview is no longer current; preview again".to_owned(),
        ));
    }
    let policy = snapshot.policy;
    policy.check()?;
    let changed = || preview(store, recovery, &policy, audio_dir, now);
    let expired = recovery.expired(snapshot.cutoffs.recovery.as_deref(), policy.failed_audio_mb)?;
    if recovery_digest(&expired) != snapshot.recovery {
        return Ok(Applied::Changed(changed()?));
    }
    let connection = store.connect()?;
    connection
        .execute_batch("BEGIN IMMEDIATE")
        .map_err(database)?;
    let plan = plan(&connection, &snapshot.cutoffs)?;
    if text_digest(&plan.text) != snapshot.text || audio_digest(&plan.audio) != snapshot.audio {
        // Dropping the connection rolls the transaction back.
        drop(connection);
        return Ok(Applied::Changed(changed()?));
    }
    let mut report = remove_library(&connection, &snapshot.cutoffs, audio_dir)?;
    connection.execute_batch("COMMIT").map_err(database)?;
    drop(connection);
    report.absorb(recovery.remove(&expired));
    Ok(Applied::Removed(report))
}

/// Remove what `policy` selects now, without a preview: the cleanup of a
/// runtime that applies the saved policy on its own.
pub fn apply_policy(
    store: &HistoryStore,
    recovery: &RecoveryStore,
    policy: &RetentionPolicy,
    audio_dir: &Path,
    now: OffsetDateTime,
) -> Result<RetentionReport, RetentionError> {
    policy.check()?;
    let cutoffs = policy.cutoffs(now)?;
    let connection = store.connect()?;
    connection
        .execute_batch("BEGIN IMMEDIATE")
        .map_err(database)?;
    let mut report = remove_library(&connection, &cutoffs, audio_dir)?;
    connection.execute_batch("COMMIT").map_err(database)?;
    drop(connection);
    report.absorb(recovery.prune(cutoffs.recovery.as_deref(), policy.failed_audio_mb)?);
    Ok(report)
}

impl HistoryStore {
    /// What the library holds: rows, their characters, and the recordings still on disk.
    pub fn retention_usage(&self) -> Result<RetentionUsage, HistoryError> {
        let connection = self.connect()?;
        let (records, characters) = connection
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(character_count), 0) FROM recognition_history",
                [],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .map_err(database)?;
        let mut statement = connection
            .prepare("SELECT audio_path FROM recognition_history WHERE audio_path IS NOT NULL")
            .map_err(database)?;
        let paths = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(database)?;
        let (mut recordings, mut audio_bytes) = (0, 0);
        for path in paths {
            if let Some(bytes) = file_bytes(&path.map_err(database)?) {
                recordings += 1;
                audio_bytes += bytes;
            }
        }
        Ok(RetentionUsage {
            records,
            characters,
            recordings,
            audio_bytes,
        })
    }

    /// The rows `filter` selects, newest first, as a document in `format`, named after `now`.
    ///
    /// With `audio`, the answer is a ZIP archive: the document, and a
    /// `audio/<n>-<time>.wav` file for every selected row whose recording
    /// is still where the row points (`audio_file` names it in the
    /// document). Without it, no byte of a recording is read. A selection
    /// larger than [`MAX_EXPORT_BYTES`] with its recordings is `TooLarge`.
    pub fn export(
        &self,
        filter: &HistoryFilter,
        format: ExportFormat,
        audio: bool,
        now: OffsetDateTime,
    ) -> Result<Exported, RetentionError> {
        let conditions = filter.conditions()?;
        let connection = self.connect()?;
        let mut values = Vec::new();
        let clause = conditions_sql(&conditions, None, &mut values);
        let sql = format!(
            "SELECT {COLUMNS} FROM recognition_history{clause} ORDER BY created_at DESC, id DESC"
        );
        let mut statement = connection.prepare(&sql).map_err(database)?;
        let rows = statement
            .query_map(rusqlite::params_from_iter(values), record_from_row)
            .map_err(database)?;
        let mut document = Vec::new();
        match format {
            ExportFormat::Text => {}
            ExportFormat::Markdown => document.extend_from_slice(b"# Voiceless history\n\n"),
            ExportFormat::Csv => document.extend_from_slice(CSV_HEADER.as_bytes()),
            ExportFormat::Json => document.push(b'['),
        }
        let mut recordings = Vec::new();
        let mut exported = 0_i64;
        for row in rows {
            let record = row.map_err(database)?;
            let index = exported + 1;
            let recording = if audio {
                record
                    .audio_path
                    .as_deref()
                    .filter(|path| Path::new(path).is_file())
                    .map(|path| Recording {
                        name: match compact(&record.created_at) {
                            Some(stamp) => format!("audio/{index:04}-{stamp}.wav"),
                            None => format!("audio/{index:04}.wav"),
                        },
                        path: path.to_owned(),
                        created_at: record.created_at.clone(),
                    })
            } else {
                None
            };
            let file = recording.as_ref().map(|recording| recording.name.as_str());
            write_record(&mut document, &record, format, file, index == 1)
                .map_err(RetentionError::Io)?;
            if let Some(recording) = recording {
                recordings.push(recording);
            }
            exported = index;
        }
        if format == ExportFormat::Json {
            document.extend_from_slice(b"]\n");
        }
        let name = format!("voiceless-history-{}", digits(now));
        if !audio {
            return Ok(Exported {
                file_name: format!("{name}.{}", format.extension()),
                media_type: format.media_type(),
                bytes: document,
            });
        }
        let mut total = document.len() as u64;
        for recording in &recordings {
            total += file_bytes(&recording.path).unwrap_or(0);
        }
        if total > MAX_EXPORT_BYTES {
            return Err(RetentionError::TooLarge { total });
        }
        let mut entries = vec![ZipEntry {
            name: format!("history.{}", format.extension()),
            data: document,
            modified: now,
        }];
        for recording in recordings {
            let modified = OffsetDateTime::parse(&recording.created_at, &Rfc3339).unwrap_or(now);
            entries.push(ZipEntry {
                name: recording.name,
                data: fs::read(&recording.path)?,
                modified,
            });
        }
        Ok(Exported {
            file_name: format!("{name}.zip"),
            media_type: "application/zip",
            bytes: zip(&entries),
        })
    }
}

/// The rows a cleanup selects at `cutoffs`, oldest first in each selection.
fn plan(connection: &Connection, cutoffs: &Cutoffs) -> Result<Plan, HistoryError> {
    let mut plan = Plan::default();
    if let Some(text) = &cutoffs.text {
        plan.text = expiring_rows(
            connection,
            &format!("created_at < ?1 AND created_at GLOB {STAMPED}"),
            params![whole_second(text)],
        )?;
    }
    if let Some(audio) = &cutoffs.audio {
        plan.audio = expiring_rows(
            connection,
            &format!(
                "audio_path IS NOT NULL AND created_at < ?2
                 AND (?1 IS NULL OR created_at >= ?1) AND created_at GLOB {STAMPED}"
            ),
            params![
                cutoffs.text.as_deref().map(whole_second),
                whole_second(audio)
            ],
        )?;
    }
    Ok(plan)
}

/// The rows matching `where_clause` (which must only use `?1` and `?2`),
/// oldest first.
fn expiring_rows(
    connection: &Connection,
    where_clause: &str,
    values: impl rusqlite::Params,
) -> Result<Vec<ExpiringRow>, HistoryError> {
    let mut statement = connection
        .prepare(&format!(
            "SELECT id, created_at, character_count, audio_path
             FROM recognition_history WHERE {where_clause} ORDER BY created_at, id"
        ))
        .map_err(database)?;
    let rows = statement
        .query_map(values, |row| {
            Ok(ExpiringRow {
                id: row.get(0)?,
                created_at: row.get(1)?,
                characters: row.get(2)?,
                audio_path: row.get(3)?,
            })
        })
        .map_err(database)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(database)
}

/// The text of the newest [`SAMPLES`] rows before `cutoff`, newest first.
fn samples(connection: &Connection, cutoff: &str) -> Result<Vec<ExpiringSample>, HistoryError> {
    let mut statement = connection
        .prepare(&format!(
            "SELECT id, created_at, final_text FROM recognition_history
             WHERE created_at < ?1 AND created_at GLOB {STAMPED}
             ORDER BY created_at DESC, id DESC LIMIT ?2"
        ))
        .map_err(database)?;
    let rows = statement
        .query_map(params![whole_second(cutoff), SAMPLES], |row| {
            Ok(ExpiringSample {
                id: row.get(0)?,
                created_at: row.get(1)?,
                text: row.get(2)?,
            })
        })
        .map_err(database)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(database)
}

/// Every recording path a row stores and the rows that store it.
fn reference_counts(connection: &Connection) -> Result<HashMap<String, i64>, HistoryError> {
    let mut statement = connection
        .prepare(
            "SELECT audio_path, COUNT(*) FROM recognition_history
             WHERE audio_path IS NOT NULL GROUP BY audio_path",
        )
        .map_err(database)?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .map_err(database)?;
    rows.collect::<Result<HashMap<_, _>, _>>().map_err(database)
}

/// How many of `rows` store each recording path.
fn path_counts(rows: &[ExpiringRow]) -> HashMap<&str, usize> {
    let mut counts = HashMap::new();
    for path in rows.iter().filter_map(|row| row.audio_path.as_deref()) {
        *counts.entry(path).or_default() += 1;
    }
    counts
}

/// Whether the recording at `path` is Voiceless's to remove for a
/// selection that holds `selected` of its rows: a direct child of
/// `audio_dir`, referenced by no row outside the selection.
fn removable(
    path: &str,
    audio_dir: &Path,
    references: &HashMap<String, i64>,
    selected: usize,
) -> bool {
    Path::new(path).parent() == Some(audio_dir)
        && references.get(path).copied().unwrap_or(0) <= selected as i64
}

/// What a cleanup selects at `cutoffs` and what it holds.
fn library_preview(
    connection: &Connection,
    cutoffs: &Cutoffs,
    audio_dir: &Path,
) -> Result<(Plan, TextExpiry, AudioExpiry), HistoryError> {
    let plan = plan(connection, cutoffs)?;
    let references = reference_counts(connection)?;
    let (text_paths, audio_paths) = (path_counts(&plan.text), path_counts(&plan.audio));
    let (mut text, mut audio) = (TextExpiry::default(), AudioExpiry::default());
    for row in &plan.text {
        text.records += 1;
        text.characters += row.characters;
        if let Some(path) = row.audio_path.as_deref() {
            if removable(
                path,
                audio_dir,
                &references,
                text_paths.get(path).copied().unwrap_or(0),
            ) {
                if let Some(bytes) = file_bytes(path) {
                    text.recordings += 1;
                    text.bytes += bytes;
                }
            } else {
                text.kept += 1;
            }
        }
    }
    // The rows come oldest first.
    text.oldest = plan.text.first().map(|row| row.created_at.clone());
    text.newest = plan.text.last().map(|row| row.created_at.clone());
    for row in &plan.audio {
        let Some(path) = row.audio_path.as_deref() else {
            continue;
        };
        if removable(
            path,
            audio_dir,
            &references,
            audio_paths.get(path).copied().unwrap_or(0),
        ) {
            match file_bytes(path) {
                Some(bytes) => {
                    audio.recordings += 1;
                    audio.bytes += bytes;
                }
                None => audio.missing += 1,
            }
        } else {
            audio.kept += 1;
        }
    }
    Ok((plan, text, audio))
}

/// Remove what `cutoffs` select. Callers hold the write lock
/// (`BEGIN IMMEDIATE`) and commit.
fn remove_library(
    connection: &Connection,
    cutoffs: &Cutoffs,
    audio_dir: &Path,
) -> Result<RetentionReport, HistoryError> {
    let plan = plan(connection, cutoffs)?;
    let mut report = RetentionReport::default();
    // The recordings go before their rows (see the module doc).
    let references = reference_counts(connection)?;
    let selected = path_counts(&plan.text);
    for (path, count) in &selected {
        if !removable(path, audio_dir, &references, *count) {
            continue;
        }
        match unlink(path) {
            Unlinked::Removed(bytes) => {
                report.recordings += 1;
                report.bytes += bytes;
            }
            Unlinked::Missing => {}
            Unlinked::Failed => report.failed += 1,
        }
    }
    for chunk in plan.text.chunks(DELETE_CHUNK) {
        let ids: Vec<String> = chunk.iter().map(|row| row.id.clone()).collect();
        connection
            .execute(
                &format!(
                    "DELETE FROM recognition_history WHERE id IN ({})",
                    placeholders(ids.len())
                ),
                rusqlite::params_from_iter(&ids),
            )
            .map_err(database)?;
        report.records += ids.len() as i64;
    }
    // Counted again now that the deleted rows no longer reference theirs.
    let references = reference_counts(connection)?;
    let selected = path_counts(&plan.audio);
    for (path, count) in &selected {
        if !removable(path, audio_dir, &references, *count) {
            continue;
        }
        let outcome = unlink(path);
        if outcome == Unlinked::Failed {
            report.failed += 1;
            continue;
        }
        report.uncouple(outcome);
        // Every row that stores this path is in the selection.
        connection
            .execute(
                "UPDATE recognition_history SET audio_path = NULL WHERE audio_path = ?1",
                params![path],
            )
            .map_err(database)?;
    }
    Ok(report)
}

/// The rows that go with their text, as the snapshot spells them.
fn text_digest(rows: &[ExpiringRow]) -> String {
    digest(rows.iter().map(|row| {
        format!(
            "{}\0{}\0{}",
            row.id,
            row.created_at,
            row.audio_path.as_deref().unwrap_or_default()
        )
    }))
}

/// The recordings that go while their rows stay, as the snapshot spells them.
fn audio_digest(rows: &[ExpiringRow]) -> String {
    digest(rows.iter().map(|row| {
        format!(
            "{}\0{}",
            row.id,
            row.audio_path.as_deref().unwrap_or_default()
        )
    }))
}

/// The recordings of failed sessions that go, as the snapshot spells them.
fn recovery_digest(recordings: &[RecoveredAudio]) -> String {
    digest(
        recordings
            .iter()
            .map(|recording| format!("{}\0{}", recording.id, recording.saved_at)),
    )
}

fn recovery_expiry(expired: &[RecoveredAudio]) -> RecoveryExpiry {
    RecoveryExpiry {
        recordings: expired.len() as i64,
        bytes: expired.iter().map(|recording| recording.bytes).sum(),
        oldest: expired.first().map(|recording| recording.saved_at.clone()),
    }
}

/// SHA-256 (hex) of `parts`, each closed by a NUL, in order: two
/// selections spell the same only when their parts are the same.
fn digest(parts: impl IntoIterator<Item = String>) -> String {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update(part.as_bytes());
        hash.update(b"\0");
    }
    hash.finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn encode(error: serde_json::Error) -> RetentionError {
    RetentionError::Snapshot(format!("the preview could not be encoded: {error}"))
}

fn decode(token: &str) -> Result<Snapshot, RetentionError> {
    let bytes = URL_SAFE_NO_PAD.decode(token).map_err(|_| {
        RetentionError::Snapshot("the preview does not read; preview again".to_owned())
    })?;
    let snapshot: Snapshot = serde_json::from_slice(&bytes).map_err(|_| {
        RetentionError::Snapshot("the preview does not read; preview again".to_owned())
    })?;
    if snapshot.version != SNAPSHOT_VERSION {
        return Err(RetentionError::Snapshot(format!(
            "the preview is of version {}, this build reads {SNAPSHOT_VERSION}",
            snapshot.version
        )));
    }
    Ok(snapshot)
}

/// A UTC second without its `Z` sorts before both the exact second and any
/// fractional timestamp within it. Only earlier seconds expire.
fn whole_second(cutoff: &str) -> &str {
    cutoff.trim_end_matches('Z')
}

fn placeholders(count: usize) -> String {
    vec!["?"; count].join(", ")
}

/// Remove the recording at `path`.
fn unlink(path: &str) -> Unlinked {
    let bytes = file_bytes(path);
    match fs::remove_file(path) {
        Ok(()) => Unlinked::Removed(bytes.unwrap_or(0)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Unlinked::Missing,
        Err(_) => Unlinked::Failed,
    }
}

/// Bytes of the file at `path`; `None` when there is none to read.
fn file_bytes(path: &str) -> Option<u64> {
    fs::metadata(path).ok().map(|metadata| metadata.len())
}

fn database(error: rusqlite::Error) -> HistoryError {
    HistoryError::Database(error.to_string())
}

/// One record of the document: `file` names its recording in the archive,
/// when it has one there; `first` opens whatever the format starts with.
fn write_record<W: Write>(
    writer: &mut W,
    record: &HistoryRecord,
    format: ExportFormat,
    file: Option<&str>,
    first: bool,
) -> io::Result<()> {
    match format {
        ExportFormat::Text => {
            if let Some(file) = file {
                writeln!(writer, "[{file}]")?;
            }
            writeln!(writer, "{}\n", record.final_text)
        }
        ExportFormat::Markdown => {
            if !first {
                writer.write_all(b"\n")?;
            }
            writeln!(
                writer,
                "## {}\n\n{}",
                readable(&record.created_at),
                record.final_text
            )?;
            let facts: Vec<&str> = [
                record.processing_mode.as_deref(),
                record.app_id.as_deref(),
                record.asr_provider.as_deref(),
            ]
            .into_iter()
            .flatten()
            .collect();
            if !facts.is_empty() {
                writeln!(writer, "- {}", facts.join(" · "))?;
            }
            if let Some(file) = file {
                writeln!(writer, "- {file}")?;
            }
            Ok(())
        }
        ExportFormat::Csv => writer.write_all(csv_line(record).as_bytes()),
        ExportFormat::Json => {
            if !first {
                writer.write_all(b",\n")?;
            }
            let mut value = serde_json::to_value(record).map_err(io::Error::other)?;
            value["audio_file"] = json!(file);
            serde_json::to_writer(writer, &value).map_err(io::Error::other)
        }
    }
}

/// `created_at` for people, `YYYY-MM-DD HH:MM:SS UTC`; as stored when it is not that form.
fn readable(created_at: &str) -> String {
    match created_at.get(..19) {
        Some(head) if is_stamp(head) => format!("{} UTC", head.replace('T', " ")),
        _ => created_at.to_owned(),
    }
}

/// Whether `text` is `YYYY-MM-DDTHH:MM:SS`.
fn is_stamp(text: &str) -> bool {
    text.len() == 19
        && text.bytes().enumerate().all(|(index, byte)| match index {
            4 | 7 => byte == b'-',
            10 => byte == b'T',
            13 | 16 => byte == b':',
            _ => byte.is_ascii_digit(),
        })
}

/// `created_at` as `YYYYMMDD-HHMMSS` for a file name; `None` when it does not read.
fn compact(created_at: &str) -> Option<String> {
    Some(digits(OffsetDateTime::parse(created_at, &Rfc3339).ok()?))
}

/// A time as `YYYYMMDD-HHMMSS`.
fn digits(time: OffsetDateTime) -> String {
    format!(
        "{}{:02}{:02}-{:02}{:02}{:02}",
        time.year(),
        u8::from(time.month()),
        time.day(),
        time.hour(),
        time.minute(),
        time.second()
    )
}

/// A ZIP archive of `entries`, each stored as it is: recordings are PCM
/// and barely compress. Every name is ASCII and every entry below 4 GiB,
/// so no extension is needed.
fn zip(entries: &[ZipEntry]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut directory = Vec::new();
    for entry in entries {
        let mut crc = Crc::new();
        crc.update(&entry.data);
        let (crc, size, name) = (crc.sum(), entry.data.len() as u32, entry.name.len() as u16);
        let offset = out.len() as u32;
        let (date, time) = dos(entry.modified);
        // Local file header.
        put32(&mut out, 0x0403_4b50);
        put16(&mut out, 20);
        put16(&mut out, 0);
        put16(&mut out, 0);
        put16(&mut out, time);
        put16(&mut out, date);
        put32(&mut out, crc);
        put32(&mut out, size);
        put32(&mut out, size);
        put16(&mut out, name);
        put16(&mut out, 0);
        out.extend_from_slice(entry.name.as_bytes());
        out.extend_from_slice(&entry.data);
        // Central directory header.
        put32(&mut directory, 0x0201_4b50);
        put16(&mut directory, 0x031e);
        put16(&mut directory, 20);
        put16(&mut directory, 0);
        put16(&mut directory, 0);
        put16(&mut directory, time);
        put16(&mut directory, date);
        put32(&mut directory, crc);
        put32(&mut directory, size);
        put32(&mut directory, size);
        put16(&mut directory, name);
        put16(&mut directory, 0);
        put16(&mut directory, 0);
        put16(&mut directory, 0);
        put16(&mut directory, 0);
        // A regular file its owner reads and writes.
        put32(&mut directory, 0o100_600 << 16);
        put32(&mut directory, offset);
        directory.extend_from_slice(entry.name.as_bytes());
    }
    let (start, length) = (out.len() as u32, directory.len() as u32);
    out.extend_from_slice(&directory);
    // End of the central directory.
    put32(&mut out, 0x0605_4b50);
    put16(&mut out, 0);
    put16(&mut out, 0);
    put16(&mut out, entries.len() as u16);
    put16(&mut out, entries.len() as u16);
    put32(&mut out, length);
    put32(&mut out, start);
    put16(&mut out, 0);
    out
}

fn put16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

/// The DOS date and time of `time`, clamped to what a ZIP can hold (1980 to 2107).
fn dos(time: OffsetDateTime) -> (u16, u16) {
    let year = i64::from(time.year()).clamp(1_980, 2_107) - 1_980;
    let date = (year << 9 | i64::from(u8::from(time.month())) << 5 | i64::from(time.day())) as u16;
    let clock = (i64::from(time.hour()) << 11)
        | (i64::from(time.minute()) << 5)
        | (i64::from(time.second()) / 2);
    (date, clock as u16)
}

/// The recovery area: the recordings of failed sessions,
/// kept apart from the library. Holds only the directory; the directory
/// is created (0700) by the first write.
pub struct RecoveryStore {
    dir: PathBuf,
}

/// A failure of the recovery area.
#[derive(Debug, thiserror::Error)]
pub enum RecoveryError {
    /// The file is not a recording this app finalized: missing, or a
    /// header whose sizes do not match the file.
    #[error("not a finalized recording: {0}")]
    NotRecording(String),
    /// A time that does not read.
    #[error("recovery time invalid: {0}")]
    Time(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// A recording of a failed session as the recovery area holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveredAudio {
    /// Random id, the shape of a history record's.
    pub id: String,
    /// When the session ended without a record, RFC 3339 UTC.
    pub saved_at: String,
    /// Why: the error the session reported, or that it was cancelled.
    pub reason: String,
    /// Backend of the session.
    pub backend: String,
    /// Processing mode of the session.
    pub mode: String,
    /// Application the dictation was meant for, when the desktop named one.
    pub app_id: Option<String>,
    /// Length of the recording.
    pub duration_ms: u64,
    /// Size of the file now (the sidecar's value is replaced on reading).
    pub bytes: u64,
}

/// What the coordinator knows of a session that failed or was cancelled after audio was captured.
pub struct FailedSession<'a> {
    /// The error the session reported, or that it was cancelled.
    pub reason: &'a str,
    /// `SessionPlan::backend`.
    pub backend: &'a str,
    /// `SessionPlan::mode_id`.
    pub mode: &'a str,
    /// The application the dictation was meant for.
    pub app_id: Option<&'a str>,
}

/// The sidecar of a recording: its metadata.
#[derive(Serialize, Deserialize)]
struct Sidecar {
    version: u32,
    #[serde(flatten)]
    audio: RecoveredAudio,
}

/// What the recovery area gave up.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct RecoveryRemoval {
    pub recordings: i64,
    pub bytes: u64,
    /// Entries a removal meant to take but could not.
    pub failed: i64,
}

/// Layout version of a sidecar.
const SIDECAR_VERSION: u32 = 1;
/// The file the writers of one area hold against each other.
const LOCK: &str = ".lock";

impl RecoveryStore {
    /// The area under the default data dir (`<data dir>/recovery`).
    pub fn default_dir() -> PathBuf {
        crate::models::default_data_dir().join("recovery")
    }

    /// The area at `dir`, created by the first write.
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// Move the finalized recording `wav` of a failed session into the
    /// area and describe it. The file becomes `<id>.wav` (0600) beside a
    /// `<id>.json` sidecar (0600). `NotRecording` for a file that is not
    /// one the recorder finalized; nothing is moved then.
    pub fn keep(
        &self,
        wav: &Path,
        session: FailedSession<'_>,
    ) -> Result<RecoveredAudio, RecoveryError> {
        let (data_bytes, byte_rate) = recording_bytes(wav)?;
        self.prepare()?;
        let _guard = self.lock()?;
        let id = super::new_record_id();
        let destination = self.dir.join(format!("{id}.wav"));
        let bytes = fs::metadata(wav)?.len();
        let saved_at =
            super::timestamp_now().map_err(|error| RecoveryError::Time(error.to_string()))?;
        protect(wav)?;
        relocate(wav, &destination)?;
        let audio = RecoveredAudio {
            bytes,
            id,
            saved_at,
            reason: session.reason.trim().to_owned(),
            backend: session.backend.to_owned(),
            mode: session.mode.to_owned(),
            app_id: session
                .app_id
                .map(str::trim)
                .filter(|app| !app.is_empty())
                .map(str::to_owned),
            duration_ms: data_bytes * 1_000 / u64::from(byte_rate),
        };
        let sidecar = Sidecar {
            version: SIDECAR_VERSION,
            audio: audio.clone(),
        };
        if let Err(error) = self.write_sidecar(&sidecar) {
            // A recording without its sidecar is nobody's: do not leave it.
            let _ = fs::remove_file(&destination);
            return Err(error.into());
        }
        Ok(audio)
    }

    /// Every entry, the newest first. A file that is not half of a
    /// complete pair is not an entry.
    pub fn list(&self) -> Result<Vec<RecoveredAudio>, RecoveryError> {
        Ok(self.scan()?.0)
    }

    /// One entry; `None` for an unknown id.
    pub fn get(&self, id: &str) -> Result<Option<RecoveredAudio>, RecoveryError> {
        Ok(self.list()?.into_iter().find(|audio| audio.id == id))
    }

    /// The WAV of an entry, while it is there; `None` otherwise.
    pub fn audio_path(&self, id: &str) -> Option<PathBuf> {
        if !is_id(id) {
            return None;
        }
        let wav = self.dir.join(format!("{id}.wav"));
        wav.is_file().then_some(wav)
    }

    /// Discard one entry. `Ok(false)` when there is no such entry.
    pub fn discard(&self, id: &str) -> Result<bool, RecoveryError> {
        if !is_id(id) || !self.sidecar_path(id).is_file() {
            return Ok(false);
        }
        self.prepare()?;
        let _guard = self.lock()?;
        let (removed, failed) = self.erase(id);
        if failed {
            return Err(io::Error::other(format!(
                "the recovery recording {id} could not be removed"
            ))
            .into());
        }
        Ok(removed.is_some())
    }

    /// The entries a cleanup with `cutoff` (RFC 3339) and a size cap in MB
    /// removes: those saved before `cutoff`, then the oldest of the rest
    /// while the area holds more than the cap. Oldest first.
    pub fn expired(
        &self,
        cutoff: Option<&str>,
        max_mb: u32,
    ) -> Result<Vec<RecoveredAudio>, RecoveryError> {
        expired_in(&self.scan()?.0, cutoff, max_mb)
    }

    /// Remove `recordings`; best effort, the counts say what went.
    pub fn remove(&self, recordings: &[RecoveredAudio]) -> RecoveryRemoval {
        let mut removal = RecoveryRemoval::default();
        for recording in recordings {
            if !is_id(&recording.id) {
                removal.failed += 1;
                continue;
            }
            match self.erase(&recording.id) {
                (None, failed) if failed => removal.failed += 1,
                (None, _) => {}
                (Some(bytes), failed) => {
                    removal.recordings += 1;
                    removal.bytes += bytes;
                    removal.failed += i64::from(failed);
                }
            }
        }
        removal
    }

    /// Remove what `cutoff` and `max_mb` select ([`Self::expired`]) and
    /// the files that belong to no entry (what an interrupted `keep`
    /// left, a sidecar whose recording was taken away).
    pub fn prune(
        &self,
        cutoff: Option<&str>,
        max_mb: u32,
    ) -> Result<RecoveryRemoval, RecoveryError> {
        self.prepare()?;
        let _guard = self.lock()?;
        let (items, leftovers) = self.scan()?;
        let expired = expired_in(&items, cutoff, max_mb)?;
        let mut removal = self.remove(&expired);
        for path in leftovers {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(_) => removal.failed += 1,
            }
        }
        Ok(removal)
    }

    fn wav_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.wav"))
    }

    fn sidecar_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    /// Create the area, 0700, and tighten one that already exists.
    fn prepare(&self) -> io::Result<()> {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&self.dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&self.dir, fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }

    /// Hold the area against its other writers (`keep`, `discard`,
    /// `prune`) until the file is dropped.
    fn lock(&self) -> io::Result<File> {
        let path = self.dir.join(LOCK);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        protect(&path)?;
        file.lock()?;
        Ok(file)
    }

    /// The complete entries (the newest first) and the files of the area
    /// that belong to none.
    fn scan(&self) -> Result<(Vec<RecoveredAudio>, Vec<PathBuf>), RecoveryError> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok((Vec::new(), Vec::new()))
            }
            Err(error) => return Err(error.into()),
        };
        let mut files = Vec::new();
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_file()
                && path.file_stem().and_then(OsStr::to_str).is_some_and(is_id)
                && matches!(
                    path.extension().and_then(OsStr::to_str),
                    Some("wav" | "json")
                )
            {
                files.push(path);
            }
        }
        let mut items = Vec::new();
        let mut paired = HashSet::new();
        for sidecar in files
            .iter()
            .filter(|path| path.extension() == Some(OsStr::new("json")))
        {
            let Some(audio) = read_sidecar(sidecar) else {
                continue;
            };
            let wav = self.wav_path(&audio.id);
            if !files.contains(&wav) {
                continue;
            }
            let bytes = fs::metadata(&wav)?.len();
            paired.insert(sidecar.clone());
            paired.insert(wav);
            items.push(RecoveredAudio { bytes, ..audio });
        }
        let leftovers = files
            .into_iter()
            .filter(|path| !paired.contains(path))
            .collect();
        items.sort_by(|a, b| b.saved_at.cmp(&a.saved_at).then_with(|| b.id.cmp(&a.id)));
        Ok((items, leftovers))
    }

    /// Write a sidecar whole: into a private temporary file, then renamed into place.
    fn write_sidecar(&self, sidecar: &Sidecar) -> io::Result<()> {
        let mut file = tempfile::NamedTempFile::new_in(&self.dir)?;
        serde_json::to_writer(&mut file, sidecar).map_err(io::Error::other)?;
        file.persist(self.sidecar_path(&sidecar.audio.id))
            .map_err(|error| error.error)?;
        Ok(())
    }

    /// Remove the two files of an entry: the bytes that were there, and
    /// whether one of them stayed.
    fn erase(&self, id: &str) -> (Option<u64>, bool) {
        let mut removed = None;
        let mut failed = false;
        for path in [self.wav_path(id), self.sidecar_path(id)] {
            let length = file_bytes(&path.display().to_string());
            match fs::remove_file(&path) {
                Ok(()) => removed = Some(removed.unwrap_or(0) + length.unwrap_or(0)),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(_) => failed = true,
            }
        }
        (removed, failed)
    }
}

/// The entries of `items` (the newest first) that a cleanup with `cutoff`
/// and a size cap in MB removes, the oldest first.
fn expired_in(
    items: &[RecoveredAudio],
    cutoff: Option<&str>,
    max_mb: u32,
) -> Result<Vec<RecoveredAudio>, RecoveryError> {
    let cutoff = match cutoff {
        Some(text) => Some(
            OffsetDateTime::parse(text, &Rfc3339)
                .map_err(|error| RecoveryError::Time(format!("the cutoff {text:?}: {error}")))?,
        ),
        None => None,
    };
    let mut expired: Vec<RecoveredAudio> = items
        .iter()
        .rev()
        .filter(|recording| {
            cutoff.is_some_and(|cutoff| {
                OffsetDateTime::parse(&recording.saved_at, &Rfc3339)
                    .is_ok_and(|saved| saved < cutoff)
            })
        })
        .cloned()
        .collect();
    // The area may hold `max_bytes`; the oldest of what is left by age go first.
    let max_bytes = u64::from(max_mb) * 1024 * 1024;
    let mut held: u64 = items.iter().map(|recording| recording.bytes).sum::<u64>()
        - expired.iter().map(|recording| recording.bytes).sum::<u64>();
    for recording in items.iter().rev() {
        if max_mb == 0 || held <= max_bytes {
            break;
        }
        if OffsetDateTime::parse(&recording.saved_at, &Rfc3339)
            .is_ok_and(|saved| cutoff.is_some_and(|c| saved < c))
        {
            continue;
        }
        held -= recording.bytes.min(held);
        expired.push(recording.clone());
    }
    Ok(expired)
}

fn is_id(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}

fn read_sidecar(path: &Path) -> Option<RecoveredAudio> {
    let file = File::open(path).ok()?;
    let sidecar: Sidecar = serde_json::from_reader(file.take(1024 * 1024)).ok()?;
    (sidecar.version == SIDECAR_VERSION && path.file_stem()?.to_str()? == sidecar.audio.id)
        .then_some(sidecar.audio)
}

fn recording_bytes(path: &Path) -> Result<(u64, u32), RecoveryError> {
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    let mut header = [0u8; 44];
    let invalid = || RecoveryError::NotRecording(path.display().to_string());
    let data = length.checked_sub(44).ok_or_else(invalid)?;
    if data < 2 || data % 2 != 0 || data > u64::from(u32::MAX - 36) {
        return Err(invalid());
    }
    file.read_exact(&mut header)?;
    let rate = crate::capture::SAMPLE_RATE;
    if header != crate::capture::wav_header(data, rate) {
        return Err(invalid());
    }
    Ok((data, rate * 2))
}

fn protect(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn relocate(source: &Path, destination: &Path) -> io::Result<()> {
    match fs::rename(source, destination) {
        Err(error) if error.kind() == io::ErrorKind::CrossesDevices => {
            let mut copy =
                tempfile::NamedTempFile::new_in(destination.parent().expect("recovery directory"))?;
            io::copy(&mut File::open(source)?, &mut copy)?;
            copy.as_file().sync_all()?;
            copy.persist_noclobber(destination)
                .map_err(|error| error.error)?;
            if let Err(error) = fs::remove_file(source) {
                fs::remove_file(destination)?;
                return Err(error);
            }
            Ok(())
        }
        result => result,
    }
}
