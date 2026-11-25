use super::{Paragraph, SpeakerSource};
use std::{
    fs::File,
    io::{self, BufReader, Read, Seek, SeekFrom},
    path::Path,
};

/// Coarse, track-local pitch clustering. Sample voiced frames throughout each
/// ASR paragraph. Silence, aperiodic sound, unstable pitch and ambiguous
/// matches stay unassigned. A cluster is only a suggestion, never identity.
pub(super) fn label<'a>(
    path: &Path,
    paragraphs: impl Iterator<Item = &'a mut Paragraph>,
) -> io::Result<()> {
    let mut source = BufReader::new(File::open(path)?);
    let mut centers: Vec<f64> = Vec::new();
    let mut estimates = Vec::with_capacity(96);
    for p in paragraphs {
        estimates.clear();
        let samples = p.end - p.start;
        if samples >= 640 {
            // Sample 40 ms frames at a 20 ms hop, with bounded work on long paragraphs.
            let count = (1 + (samples - 640) / 320).min(96);
            for i in 0..count {
                let at = p.start + (samples - 640) * (2 * i + 1) / (2 * count);
                // Capture finalizes mono PCM16 with its fixed 44-byte WAV header.
                source.seek(SeekFrom::Start(44 + 2 * at))?;
                let mut raw = [0u8; 1280];
                source.read_exact(&mut raw)?;
                if let Some(pitch) = pitch(&raw) {
                    estimates.push(pitch.log2());
                }
            }
        }
        estimates.sort_unstable_by(f64::total_cmp);
        let center = (estimates.len() >= 3).then(|| estimates[estimates.len() / 2]);
        let cluster = center.and_then(|value| {
            let n = estimates.len();
            // Intonation can span several semitones for one speaker. Check the
            // uncertainty of the median pitch using central order statistics.
            let mid = n / 2;
            let radius = n.isqrt();
            let low = estimates[mid.saturating_sub(radius)];
            let high = estimates[(mid + radius).min(n - 1)];
            if high - low > 0.35 {
                return None;
            }
            let closest = centers
                .iter()
                .enumerate()
                .map(|(i, c)| (i, (value - c).abs()))
                .min_by(|a, b| a.1.total_cmp(&b.1));
            match closest {
                Some((i, d)) if d <= 0.20 => Some(i),
                Some((_, d)) if d < 0.45 => None,
                _ => {
                    centers.push(value);
                    Some(centers.len() - 1)
                }
            }
        });
        p.local_pitch_speaker = cluster.map(|i| format!("{}-{}", p.track.name(), i + 1));
        if p.speaker_source != SpeakerSource::Manual {
            p.speaker.clone_from(&p.local_pitch_speaker);
            p.speaker_source = if cluster.is_some() {
                SpeakerSource::LocalPitch
            } else {
                SpeakerSource::Unknown
            };
        }
    }
    Ok(())
}

fn pitch(raw: &[u8; 1280]) -> Option<f64> {
    // Average four adjacent samples to 4 kHz. Normalized autocorrelation
    // searches 70–400 Hz; prefer the first strong peak over its harmonics.
    let mut frame = [0.0; 160];
    for (slot, bytes) in frame.iter_mut().zip(raw.chunks_exact(8)) {
        *slot = bytes
            .chunks_exact(2)
            .map(|b| f64::from(i16::from_le_bytes([b[0], b[1]])))
            .sum::<f64>()
            / 4.0;
    }
    let mean = frame.iter().sum::<f64>() / 160.;
    for sample in &mut frame {
        *sample -= mean;
    }
    if frame.iter().map(|x| x * x).sum::<f64>() / 160. < 64. * 64. {
        return None;
    }
    let mut scores = [0.; 59];
    for (lag, score) in scores.iter_mut().enumerate().skip(9) {
        let (mut xy, mut xx, mut yy) = (0., 0., 0.);
        for (&x, &y) in frame.iter().zip(&frame[lag..]) {
            xy += x * y;
            xx += x * x;
            yy += y * y;
        }
        *score = xy / (xx * yy).sqrt().max(1.);
    }
    let threshold = scores
        .iter()
        .copied()
        .fold(0., f64::max)
        .mul_add(0.95, 0.)
        .max(0.78);
    (10..58)
        .find(|&i| {
            scores[i] >= threshold && scores[i] > scores[i - 1] && scores[i] >= scores[i + 1]
        })
        .map(|lag| 4000. / lag as f64)
}
