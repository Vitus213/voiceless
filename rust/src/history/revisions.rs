//! Stored versions of a record's recognized text: what a correction or a
//! re-transcription proposed, what was adopted, and what that replaced.
//!
//! A row of `recognition_history` always reads as its *current* version:
//! `raw_text`, `processed_text`, `final_text`, `asr_provider` and
//! `asr_model` are what the current version holds, so the listing, the
//! search, the CSV export and a reader of an earlier schema all see the
//! text the user settled on. Every version the record has had is a row of
//! `history_revisions` (schema v4), in one of three states:
//!
//! - `pending`: kept for comparison; it replaces nothing until it is
//!   adopted (`HistoryStore::propose_revision` without `adopt`);
//! - `current`: the one the record shows (`HistoryStore::adopt_revision`);
//! - `superseded`: replaced by another version. `adopt_revision` makes it
//!   current again and deletes nothing.
//!
//! A record no one corrected or re-transcribed has no version rows: it is
//! its own single version. The first proposal stores that state as the
//! `original` version, so a record with any version rows always has
//! exactly one `current` row that equals the record row (the partial
//! unique index `history_revisions_current` keeps it so).
//!
//! Adopting is one `BEGIN IMMEDIATE` transaction: the current version
//! becomes `superseded`, the chosen one `current`, and the record row
//! takes its text and provenance. A caller that confirmed a replacement
//! against the text it was shown passes that text as `expected`; when the
//! record no longer holds it, the call is a `HistoryError::Conflict` and
//! nothing changes. Version rows go with their record (`ON DELETE
//! CASCADE`, which the store's connections enforce; `forget` does it for
//! a connection that does not). Nothing here reads or writes a recording:
//! a version keeps text and provenance only.

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSqlOutput, ValueRef};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::Serialize;

use super::{
    new_record_id, record_from_row, timestamp_now, HistoryError, HistoryRecord, HistoryStore,
    COLUMNS,
};

/// The columns of every `SELECT` of a version, in the order
/// `revision_from_row` reads them.
const REVISION_COLUMNS: &str = "id, position, created_at, adopted_at, kind, state, raw_text, \
                                processed_text, final_text, asr_provider, asr_model, device";

/// What produced a version's text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RevisionKind {
    /// The session's own result, as the record was written.
    Original,
    /// Text the user corrected.
    Correction,
    /// The recording recognized again with a chosen offline model.
    Retranscription,
}

/// Where a version stands in the record's history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RevisionState {
    /// Kept for comparison; it replaces nothing until it is adopted.
    Pending,
    /// The version the record shows.
    Current,
    /// Replaced by another version; it can be made current again.
    Superseded,
}

impl RevisionKind {
    /// The stored spelling of the kind.
    const fn as_str(self) -> &'static str {
        match self {
            Self::Original => "original",
            Self::Correction => "correction",
            Self::Retranscription => "retranscription",
        }
    }
}

impl RevisionState {
    /// The stored spelling of the state.
    const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Current => "current",
            Self::Superseded => "superseded",
        }
    }
}

impl rusqlite::ToSql for RevisionKind {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(self.as_str().into())
    }
}

impl rusqlite::ToSql for RevisionState {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(self.as_str().into())
    }
}

impl FromSql for RevisionKind {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        match value.as_str()? {
            "original" => Ok(Self::Original),
            "correction" => Ok(Self::Correction),
            "retranscription" => Ok(Self::Retranscription),
            _ => Err(FromSqlError::InvalidType),
        }
    }
}

impl FromSql for RevisionState {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        match value.as_str()? {
            "pending" => Ok(Self::Pending),
            "current" => Ok(Self::Current),
            "superseded" => Ok(Self::Superseded),
            _ => Err(FromSqlError::InvalidType),
        }
    }
}

/// One row of `history_revisions`, serialized as is to JSON by the console
/// (`/api/records/{id}/revisions` and the calls that change the versions).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HistoryRevision {
    /// Random UUID-v4-style identifier (`new_record_id`).
    pub id: String,
    /// The original is position zero. Later drafts retain their order on adoption.
    pub position: i64,
    pub created_at: String,
    pub adopted_at: Option<String>,
    pub kind: RevisionKind,
    pub state: RevisionState,
    pub raw_text: String,
    pub processed_text: Option<String>,
    pub final_text: String,
    /// Backend that recognized the audio the version is based on
    /// (`local-offline` for a re-transcription).
    pub asr_provider: Option<String>,
    /// Model that recognized it: the model a re-transcription was run
    /// with, else what the record already held.
    pub asr_model: Option<String>,
    /// Where the model really ran for a re-transcription, as its decoder
    /// reported it; `None` for every other kind.
    pub device: Option<String>,
}

/// What a new version holds: the text and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevisionDraft {
    /// `Correction` or `Retranscription`: an original is stored by the
    /// store itself, from the record.
    pub kind: RevisionKind,
    pub raw_text: String,
    pub processed_text: Option<String>,
    /// The text of the version; a blank one is refused.
    pub final_text: String,
    pub asr_provider: Option<String>,
    pub asr_model: Option<String>,
    /// Where the model really ran, for a re-transcription.
    pub device: Option<String>,
}

impl HistoryStore {
    /// Every stored version of the record, oldest first. A record that
    /// was never corrected or re-transcribed has none: it is its own
    /// single version. `NotFound` for an unknown record.
    pub fn revisions(&self, record_id: &str) -> Result<Vec<HistoryRevision>, HistoryError> {
        let mut connection = self.connect()?;
        let transaction = connection.transaction().map_err(database)?;
        record_of(&transaction, record_id)?;
        let mut statement = transaction
            .prepare(&format!(
                "SELECT {REVISION_COLUMNS} FROM history_revisions
                 WHERE record_id = ?1 ORDER BY position"
            ))
            .map_err(database)?;
        let revisions = statement
            .query_map(params![record_id], revision_from_row)
            .map_err(database)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(database)?;
        Ok(revisions)
    }

    /// Store `draft` as a pending version of the record and return it. With
    /// `adopt` the record shows it at once instead: the call then answers
    /// like `adopt_revision`. `expected` is the text the caller confirmed
    /// the replacement against (see `adopt_revision`). Nothing is written
    /// when the call fails. `NotFound` for an unknown record, `Conflict`
    /// for another `expected`, `Database` for a blank `final_text`.
    pub fn propose_revision(
        &self,
        record_id: &str,
        draft: &RevisionDraft,
        adopt: bool,
        expected: Option<&str>,
    ) -> Result<(HistoryRecord, HistoryRevision), HistoryError> {
        if draft.final_text.trim().is_empty() {
            return Err(HistoryError::Database(
                "a version's text must not be empty".to_owned(),
            ));
        }
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(database)?;
        let record = record_of(&transaction, record_id)?;
        confirm_expected(&record, expected)?;
        // The record as it reads now is its first version, current until
        // something replaces it.
        if next_position(&transaction, record_id)? == 0 {
            let original = HistoryRevision {
                id: new_record_id(),
                position: 0,
                created_at: record.created_at.clone(),
                adopted_at: Some(record.created_at.clone()),
                kind: RevisionKind::Original,
                state: RevisionState::Current,
                raw_text: record.raw_text.clone(),
                processed_text: record.processed_text.clone(),
                final_text: record.final_text.clone(),
                asr_provider: record.asr_provider.clone(),
                asr_model: record.asr_model.clone(),
                device: None,
            };
            insert_revision(&transaction, record_id, &original)?;
        }
        let mut revision = HistoryRevision {
            id: new_record_id(),
            position: next_position(&transaction, record_id)?,
            created_at: timestamp_now()?,
            adopted_at: None,
            kind: draft.kind,
            state: RevisionState::Pending,
            raw_text: draft.raw_text.clone(),
            processed_text: draft.processed_text.clone(),
            final_text: draft.final_text.clone(),
            asr_provider: draft.asr_provider.clone(),
            asr_model: draft.asr_model.clone(),
            device: draft.device.clone(),
        };
        insert_revision(&transaction, record_id, &revision)?;
        if !adopt {
            transaction.commit().map_err(database)?;
            return Ok((record, revision));
        }
        make_current(&transaction, &record, &mut revision)?;
        let record = record_with(&record, &revision);
        transaction.commit().map_err(database)?;
        Ok((record, revision))
    }

    /// Make version `revision_id` the one the record shows: a pending one
    /// is adopted, a replaced one restored. The version that was current
    /// is kept as replaced; nothing is deleted, and the record row takes
    /// the text and provenance of the chosen version. Both are returned as
    /// they read after the change.
    ///
    /// `expected` is the text the caller confirmed the replacement against;
    /// when the record no longer holds it the call is refused, so a
    /// confirmation made against text another client has since replaced
    /// never overwrites what the user did not see. `NotFound` for an
    /// unknown record or a version of another record, `Conflict` for the
    /// version that is already current or for another `expected`.
    pub fn adopt_revision(
        &self,
        record_id: &str,
        revision_id: &str,
        expected: Option<&str>,
    ) -> Result<(HistoryRecord, HistoryRevision), HistoryError> {
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(database)?;
        let record = record_of(&transaction, record_id)?;
        let mut revision = revision_of(&transaction, record_id, revision_id)?;
        if revision.state == RevisionState::Current {
            return Err(HistoryError::Conflict(
                "this version is already the current one".to_owned(),
            ));
        }
        confirm_expected(&record, expected)?;
        make_current(&transaction, &record, &mut revision)?;
        let record = record_with(&record, &revision);
        transaction.commit().map_err(database)?;
        Ok((record, revision))
    }

    /// Remove a pending version of the record and return it as it was. A
    /// version the record showed is history and stays: `Conflict` for one
    /// that is current or replaced, `NotFound` for an unknown record or a
    /// version of another record.
    pub fn discard_revision(
        &self,
        record_id: &str,
        revision_id: &str,
    ) -> Result<HistoryRevision, HistoryError> {
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(database)?;
        let revision = revision_of(&transaction, record_id, revision_id)?;
        if revision.state != RevisionState::Pending {
            return Err(HistoryError::Conflict(
                "only a pending version can be discarded".to_owned(),
            ));
        }
        transaction
            .execute(
                "DELETE FROM history_revisions WHERE id = ?1",
                params![revision.id],
            )
            .map_err(database)?;
        transaction.commit().map_err(database)?;
        Ok(revision)
    }
}

/// Inside the transaction that holds the write lock: the current version
/// becomes replaced, `revision` current, and the record takes its text and
/// provenance. `revision` is updated to what it now is.
fn make_current(
    transaction: &Transaction<'_>,
    record: &HistoryRecord,
    revision: &mut HistoryRevision,
) -> Result<(), HistoryError> {
    transaction
        .execute(
            "UPDATE history_revisions SET state = ?2
             WHERE record_id = ?1 AND state = ?3",
            params![record.id, RevisionState::Superseded, RevisionState::Current],
        )
        .map_err(database)?;
    let adopted_at = timestamp_now()?;
    transaction
        .execute(
            "UPDATE history_revisions SET state = ?2, adopted_at = ?3 WHERE id = ?1",
            params![revision.id, RevisionState::Current, adopted_at],
        )
        .map_err(database)?;
    transaction
        .execute(
            "UPDATE recognition_history
             SET raw_text = ?2, processed_text = ?3, final_text = ?4, character_count = ?5,
                 asr_provider = ?6, asr_model = ?7
             WHERE id = ?1",
            params![
                record.id,
                revision.raw_text,
                revision.processed_text,
                revision.final_text,
                revision.final_text.chars().count() as i64,
                revision.asr_provider,
                revision.asr_model,
            ],
        )
        .map_err(database)?;
    revision.state = RevisionState::Current;
    revision.adopted_at = Some(adopted_at);
    Ok(())
}

/// `record` with the text and provenance of `revision`: what the row reads
/// once `revision` is its current version.
fn record_with(record: &HistoryRecord, revision: &HistoryRevision) -> HistoryRecord {
    HistoryRecord {
        raw_text: revision.raw_text.clone(),
        processed_text: revision.processed_text.clone(),
        final_text: revision.final_text.clone(),
        character_count: revision.final_text.chars().count() as i64,
        asr_provider: revision.asr_provider.clone(),
        asr_model: revision.asr_model.clone(),
        ..record.clone()
    }
}

/// Refuse a confirmation made against another text than the record holds.
fn confirm_expected(record: &HistoryRecord, expected: Option<&str>) -> Result<(), HistoryError> {
    match expected {
        Some(expected) if expected != record.final_text => Err(HistoryError::Conflict(
            "the record's text changed since this version was prepared; compare it again before replacing it"
                .to_owned(),
        )),
        _ => Ok(()),
    }
}

/// The record, or `NotFound`.
fn record_of(connection: &Connection, record_id: &str) -> Result<HistoryRecord, HistoryError> {
    connection
        .query_row(
            &format!("SELECT {COLUMNS} FROM recognition_history WHERE id = ?1"),
            params![record_id],
            record_from_row,
        )
        .optional()
        .map_err(database)?
        .ok_or_else(|| HistoryError::NotFound("no such record".to_owned()))
}

/// The version `revision_id` of the record. A version of another record is
/// as good as none: `NotFound`.
fn revision_of(
    connection: &Connection,
    record_id: &str,
    revision_id: &str,
) -> Result<HistoryRevision, HistoryError> {
    connection
        .query_row(
            &format!(
                "SELECT {REVISION_COLUMNS} FROM history_revisions
                 WHERE id = ?1 AND record_id = ?2"
            ),
            params![revision_id, record_id],
            revision_from_row,
        )
        .optional()
        .map_err(database)?
        .ok_or_else(|| HistoryError::NotFound("no such version of this record".to_owned()))
}

/// Where the next version of the record goes: one past the last stored.
fn next_position(connection: &Connection, record_id: &str) -> Result<i64, HistoryError> {
    connection
        .query_row(
            "SELECT COALESCE(MAX(position), -1) + 1 FROM history_revisions WHERE record_id = ?1",
            params![record_id],
            |row| row.get(0),
        )
        .map_err(database)
}

fn insert_revision(
    connection: &Connection,
    record_id: &str,
    revision: &HistoryRevision,
) -> Result<(), HistoryError> {
    connection
        .execute(
            "INSERT INTO history_revisions (
                id, record_id, position, created_at, adopted_at, kind, state, raw_text,
                processed_text, final_text, asr_provider, asr_model, device
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                revision.id,
                record_id,
                revision.position,
                revision.created_at,
                revision.adopted_at,
                revision.kind,
                revision.state,
                revision.raw_text,
                revision.processed_text,
                revision.final_text,
                revision.asr_provider,
                revision.asr_model,
                revision.device,
            ],
        )
        .map_err(database)?;
    Ok(())
}

/// Read one selected version; the statement must have selected
/// `REVISION_COLUMNS`.
fn revision_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<HistoryRevision> {
    Ok(HistoryRevision {
        id: row.get(0)?,
        position: row.get(1)?,
        created_at: row.get(2)?,
        adopted_at: row.get(3)?,
        kind: row.get(4)?,
        state: row.get(5)?,
        raw_text: row.get(6)?,
        processed_text: row.get(7)?,
        final_text: row.get(8)?,
        asr_provider: row.get(9)?,
        asr_model: row.get(10)?,
        device: row.get(11)?,
    })
}

fn database(error: rusqlite::Error) -> HistoryError {
    HistoryError::Database(error.to_string())
}
