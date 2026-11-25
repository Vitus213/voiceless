//! Export of a job's result as plain text, segmented JSON, SRT or VTT.
//!
//! Every cue time is the media time the segment was committed with, in
//! samples of the decoded 16 kHz stream, so a subtitle file lines up with
//! the input whatever the input's own format was.

use std::fmt;

use serde::Serialize;

use super::job::{seconds, JobView, Segment, Status, Transport, SAMPLE_RATE};

/// Layout of an export.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    /// One line per segment.
    Text,
    /// The job and its segments, with times in seconds.
    Json,
    /// SubRip subtitles.
    Srt,
    /// WebVTT subtitles.
    Vtt,
}

impl ExportFormat {
    /// The format named `txt` (or `text`), `json`, `srt` or `vtt`.
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "txt" | "text" => Some(Self::Text),
            "json" => Some(Self::Json),
            "srt" => Some(Self::Srt),
            "vtt" => Some(Self::Vtt),
            _ => None,
        }
    }

    /// File extension, with its dot.
    pub fn extension(self) -> &'static str {
        match self {
            Self::Text => ".txt",
            Self::Json => ".json",
            Self::Srt => ".srt",
            Self::Vtt => ".vtt",
        }
    }

    /// MIME type of the file.
    pub fn media_type(self) -> &'static str {
        match self {
            Self::Text => "text/plain; charset=utf-8",
            Self::Json => "application/json",
            Self::Srt => "application/x-subrip",
            Self::Vtt => "text/vtt",
        }
    }
}

impl fmt::Display for ExportFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.extension()[1..])
    }
}

/// One exported file, ready to be written or served.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportDocument {
    /// Suggested file name: the input's, with the format's extension.
    pub file_name: String,
    pub media_type: &'static str,
    pub content: String,
}

/// Layout version of the JSON export.
const JSON_VERSION: u32 = 1;

/// Render the result of `job` in `format`.
pub(crate) fn render(job: &JobView, format: ExportFormat) -> ExportDocument {
    let content = match format {
        ExportFormat::Text => text(&job.segments),
        ExportFormat::Json => json(job),
        ExportFormat::Srt => cues(&job.segments, Style::Srt),
        ExportFormat::Vtt => format!("WEBVTT\n\n{}", cues(&job.segments, Style::Vtt)),
    };
    ExportDocument {
        file_name: format!(
            "{}{}",
            file_stem(&job.summary.input.name),
            format.extension()
        ),
        media_type: format.media_type(),
        content,
    }
}

/// The input's name without its last extension, or `transcript`.
fn file_stem(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let stem = base.rsplit_once('.').map_or(base, |(stem, _)| stem);
    (if stem.is_empty() { "transcript" } else { stem }).to_owned()
}

fn text(segments: &[Segment]) -> String {
    let mut text = segments
        .iter()
        .map(|segment| segment.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    if !text.is_empty() {
        text.push('\n');
    }
    text
}

#[derive(Serialize)]
struct Transcript<'a> {
    format: &'static str,
    version: u32,
    job: ExportedJob<'a>,
    segments: Vec<ExportedSegment<'a>>,
}

#[derive(Serialize)]
struct ExportedJob<'a> {
    id: &'a str,
    input: &'a str,
    model: &'a str,
    language: &'a str,
    transport: Transport,
    /// The status says whether this is a whole transcript (`completed`)
    /// or the part committed so far.
    status: Status,
    duration_seconds: Option<f64>,
}

#[derive(Serialize)]
struct ExportedSegment<'a> {
    /// 1-based position in media order.
    index: u64,
    start: f64,
    end: f64,
    text: &'a str,
}

fn json(job: &JobView) -> String {
    let summary = &job.summary;
    let segments = job
        .segments
        .iter()
        .enumerate()
        .map(|(index, segment)| ExportedSegment {
            index: index as u64 + 1,
            start: seconds(segment.start),
            end: seconds(segment.end),
            text: segment.text.as_str(),
        })
        .collect();
    let transcript = Transcript {
        format: "voiceless-transcript",
        version: JSON_VERSION,
        job: ExportedJob {
            id: &summary.id,
            input: &summary.input.name,
            model: &summary.model,
            language: &summary.language,
            transport: summary.transport,
            status: summary.status,
            duration_seconds: summary.media.duration_seconds,
        },
        segments,
    };
    // Plain values only: numbers, strings and enumerations.
    serde_json::to_string_pretty(&transcript).expect("transcript export serializes")
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Style {
    Srt,
    Vtt,
}

/// The segments as subtitle cues, in media order.
fn cues(segments: &[Segment], style: Style) -> String {
    let mut ordered: Vec<&Segment> = segments
        .iter()
        .filter(|segment| !segment.text.trim().is_empty())
        .collect();
    ordered.sort_by_key(|segment| segment.start);
    let separator = match style {
        Style::Srt => ',',
        Style::Vtt => '.',
    };
    let mut output = String::new();
    for (index, segment) in ordered.iter().enumerate() {
        let start = milliseconds(segment.start);
        // A cue is shown for at least a millisecond, whatever its segment's length.
        let end = milliseconds(segment.end).max(start + 1);
        if style == Style::Srt {
            output.push_str(&(index + 1).to_string());
            output.push('\n');
        }
        output.push_str(&clock(start, separator));
        output.push_str(" --> ");
        output.push_str(&clock(end, separator));
        output.push('\n');
        output.push_str(segment.text.trim());
        output.push_str("\n\n");
    }
    output
}

fn milliseconds(samples: u64) -> u64 {
    samples * 1_000 / u64::from(SAMPLE_RATE)
}

/// `HH:MM:SS<separator>mmm`.
fn clock(milliseconds: u64, separator: char) -> String {
    format!(
        "{:02}:{:02}:{:02}{}{:03}",
        milliseconds / 3_600_000,
        milliseconds / 60_000 % 60,
        milliseconds / 1_000 % 60,
        separator,
        milliseconds % 1_000
    )
}
