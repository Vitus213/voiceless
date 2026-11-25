//! Editable meeting transcript. Source audio, capture journal and queue results stay intact.
mod speakers;

use crate::{
    meeting_capture::{Meeting, TrackKind, TrackView},
    transcription_jobs::{JobSummary, JobView, Status, Transport},
};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{self, BufReader, Write},
    path::Path,
};

pub(crate) fn track_for_job<'a>(meeting: &'a Meeting, job: &JobSummary) -> Option<&'a TrackView> {
    meeting.tracks.iter().find(|track| {
        track.path.as_ref().is_some_and(|path| {
            track.job_id.as_deref() == Some(job.id.as_str())
                || job.input.path.as_deref().map(Path::new) == Some(path.as_path())
        })
    })
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Invalid(&'static str),
    #[error("The meeting review has changed. Reload before saving.")]
    Conflict,
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[derive(Clone, Deserialize, Serialize)]
pub struct Paragraph {
    pub id: String,
    pub track: TrackKind,
    pub start: u64,
    pub end: u64,
    pub original: String,
    pub text: String,
    pub speaker: Option<String>,
    /// Retain the acoustic suggestion when a reviewer overrides it.
    #[serde(default)]
    pub local_pitch_speaker: Option<String>,
    /// Local estimates are always tentative, even when a cluster repeats.
    pub speaker_source: SpeakerSource,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SpeakerSource {
    Unknown,
    LocalPitch,
    Manual,
}

#[derive(Serialize, Deserialize)]
pub struct TranscriptSource {
    pub track: TrackKind,
    pub job: String,
    pub model: String,
    pub transport: Transport,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Destination {
    pub provider: String,
    pub endpoint: String,
    pub model: String,
}

#[derive(Serialize, Deserialize)]
pub struct Highlights {
    pub text: String,
    pub generated: String,
    pub segments: Vec<String>,
    #[serde(default)]
    pub excerpts: String,
    pub source_revision: u64,
    /// None means local extractive summary. No credential is persisted.
    pub destination: Option<Destination>,
}

#[derive(Serialize, Deserialize)]
pub struct Review {
    pub meeting_id: String,
    pub revision: u64,
    pub title: String,
    pub sources: Vec<TranscriptSource>,
    pub paragraphs: Vec<Paragraph>,
    pub highlights: Option<Highlights>,
}

impl Review {
    pub fn load(meeting: &Meeting) -> Result<Self, Error> {
        match File::open(meeting.data_location.join("review.json")) {
            Ok(file) => Ok(serde_json::from_reader(BufReader::new(file))?),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self {
                meeting_id: meeting.id.clone(),
                revision: 0,
                title: meeting.started_at.clone(),
                sources: vec![],
                paragraphs: vec![],
                highlights: None,
            }),
            Err(e) => Err(e.into()),
        }
    }

    /// The revision guards both manual edits and slow analysis/provider replies.
    pub fn save(&mut self, meeting: &Meeting) -> Result<(), Error> {
        let directory = File::open(&meeting.data_location)?;
        directory.lock()?;
        if Self::load(meeting)?.revision != self.revision {
            return Err(Error::Conflict);
        }
        let mut tmp = tempfile::NamedTempFile::new_in(&meeting.data_location)?;
        self.revision += 1;
        let result = (|| -> Result<(), Error> {
            let mut writer = io::BufWriter::new(tmp.as_file_mut());
            serde_json::to_writer(&mut writer, self as &Self)?;
            writer.flush()?;
            drop(writer);
            tmp.as_file().sync_all()?;
            tmp.persist(meeting.data_location.join("review.json"))
                .map_err(|e| e.error)?;
            directory.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            self.revision -= 1;
        }
        result
    }

    pub fn attach(&mut self, meeting: &Meeting, job: JobView) -> Result<(), Error> {
        let source = &job.summary;
        let track = track_for_job(meeting, source).ok_or(Error::Invalid(
            "Job does not belong to a finalized meeting track",
        ))?;
        if source.status != Status::Completed || self.sources.iter().any(|s| s.track == track.kind)
        {
            return Err(Error::Invalid(
                "Choose a completed job for a track without a transcript",
            ));
        }
        if job
            .segments
            .iter()
            .any(|s| s.end <= s.start || s.end > track.committed_bytes / 2)
        {
            return Err(Error::Invalid("Segment falls outside the meeting audio"));
        }
        self.paragraphs
            .extend(
                job.segments
                    .into_iter()
                    .enumerate()
                    .map(|(i, segment)| Paragraph {
                        id: format!("{}-{i}", track.kind.name()),
                        track: track.kind,
                        start: segment.start,
                        end: segment.end,
                        original: segment.text.clone(),
                        text: segment.text,
                        speaker: None,
                        local_pitch_speaker: None,
                        speaker_source: SpeakerSource::Unknown,
                    }),
            );
        self.paragraphs.sort_by_key(|p| p.start);
        self.sources.push(TranscriptSource {
            track: track.kind,
            job: source.id.clone(),
            model: source.model.clone(),
            transport: source.transport,
        });
        Ok(())
    }

    pub fn separate(&mut self, meeting: &Meeting) -> Result<(), Error> {
        for track in meeting.transcription_inputs() {
            speakers::label(
                &track.path,
                self.paragraphs
                    .iter_mut()
                    .filter(|p| p.track == track.track),
            )?;
        }
        Ok(())
    }

    /// Selection is validated before any provider is constructed or called. The
    /// prompt contains only the selected, saved text in transcript order.
    pub fn selected(&self, ids: &[String]) -> Result<Vec<&str>, Error> {
        let wanted: std::collections::HashSet<_> = ids.iter().map(String::as_str).collect();
        let text: Vec<_> = self
            .paragraphs
            .iter()
            .filter(|p| wanted.contains(p.id.as_str()))
            .map(|p| p.text.as_str())
            .collect();
        if text.is_empty() || text.len() != ids.len() {
            return Err(Error::Invalid("Choose valid, distinct paragraphs"));
        }
        Ok(text)
    }
}

/// Rank paragraphs by recurring adjacent character pairs, including text
/// without whitespace-delimited words. Return up to five in reading order.
pub fn local_highlights(lines: &[&str]) -> String {
    use std::collections::{HashMap, HashSet};
    let terms: Vec<HashSet<(char, char)>> = lines
        .iter()
        .map(|line| {
            line.chars()
                .zip(line.chars().skip(1))
                .filter(|(a, b)| a.is_alphanumeric() && b.is_alphanumeric())
                .collect()
        })
        .collect();
    let mut frequency = HashMap::<_, usize>::new();
    for term in terms.iter().flatten() {
        *frequency.entry(*term).or_default() += 1;
    }
    let mut ranked: Vec<_> = terms
        .iter()
        .enumerate()
        .map(|(i, terms)| {
            let score = terms.iter().map(|k| frequency[k] - 1).sum::<usize>() as f64
                / (terms.len().max(1) as f64).sqrt();
            (i, score)
        })
        .collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    ranked.truncate(5);
    ranked.sort_by_key(|r| r.0);
    ranked
        .into_iter()
        .map(|(i, _)| format!("• {}", lines[i]))
        .collect::<Vec<_>>()
        .join("\n")
}
