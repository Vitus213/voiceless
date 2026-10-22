//! Session side effects that make dictation feel native (from voxtype and
//! hyprwhspr): pause MPRIS players while recording and resume exactly the
//! ones paused, and optional start/stop cues played through PipeWire.
//!
//! Used only by the daemon's state transitions (`daemon::serve_with_gateway`)
//! under `[audio] pause_media` (default on) and `[audio] cues` (default
//! off). Everything here is best effort and never affects the session: a
//! player that fails to pause is skipped, a missing `pw-play` or an
//! unwritable runtime directory just means no cue. The cue WAV files are
//! generated once into `$XDG_RUNTIME_DIR` (or the temp dir) and reused.

use std::path::PathBuf;
use std::time::Duration;

/// MPRIS players own a session bus name under this prefix.
const MPRIS_PREFIX: &str = "org.mpris.MediaPlayer2.";
/// The one MPRIS object path every player exports.
const MPRIS_PATH: &str = "/org/mpris/MediaPlayer2";
/// Interface with `PlaybackStatus`, `Pause` and `Play`.
const PLAYER: &str = "org.mpris.MediaPlayer2.Player";

/// Pause every player that is currently playing; returns their bus names
/// for `resume`. Best effort: errors just skip a player. Only players whose
/// `PlaybackStatus` is `"Playing"` are touched, so an already paused player
/// is never resumed by us later. Each player is queried sequentially over
/// the session bus; the daemon runs this in its own task.
pub async fn pause_playing() -> Vec<String> {
    let Ok(connection) = zbus::Connection::session().await else {
        return Vec::new();
    };
    let Ok(dbus) = zbus::fdo::DBusProxy::new(&connection).await else {
        return Vec::new();
    };
    let Ok(names) = dbus.list_names().await else {
        return Vec::new();
    };
    let mut paused = Vec::new();
    for name in names {
        let name = name.to_string();
        if !name.starts_with(MPRIS_PREFIX) {
            continue;
        }
        let Ok(proxy) = zbus::Proxy::new(&connection, name.clone(), MPRIS_PATH, PLAYER).await
        else {
            continue;
        };
        let playing = proxy
            .get_property::<String>("PlaybackStatus")
            .await
            .is_ok_and(|status| status == "Playing");
        if playing && proxy.call_method("Pause", &()).await.is_ok() {
            paused.push(name);
        }
    }
    paused
}

/// Resume the players `pause_playing` paused, by calling `Play` on each
/// name; a player that has since quit is skipped.
pub async fn resume(names: Vec<String>) {
    if names.is_empty() {
        return;
    }
    let Ok(connection) = zbus::Connection::session().await else {
        return;
    };
    for name in names {
        if let Ok(proxy) = zbus::Proxy::new(&connection, name, MPRIS_PATH, PLAYER).await {
            let _ = proxy.call_method("Play", &()).await;
        }
    }
}

/// Which cue to play.
#[derive(Debug, Clone, Copy)]
pub enum Cue {
    /// Recording started (rising 660 -> 880 Hz).
    Start,
    /// Stop requested (falling 880 -> 660 Hz).
    Stop,
}

/// Play a short soft tone (rising for start, falling for stop) through
/// `pw-play`, without waiting for it. Spawns `pw-play --volume=0.35 <wav>`
/// with all stdio detached; a spawn failure (no `pw-play` in PATH) is
/// ignored. Synchronous but non-blocking, so it is safe from the daemon's
/// command loop.
pub fn play_cue(cue: Cue) {
    let Ok(path) = cue_file(cue) else {
        return;
    };
    let _ = std::process::Command::new("pw-play")
        .arg("--volume=0.35")
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Path of the generated cue WAV, writing it on first use. An existing file
/// is trusted and never regenerated.
fn cue_file(cue: Cue) -> std::io::Result<PathBuf> {
    // Per-user, tmpfs-backed and cleared at logout; the temp dir is only a
    // fallback for sessions without a runtime directory.
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let (name, tones) = match cue {
        Cue::Start => ("voiceless-cue-start.wav", [660.0, 880.0]),
        Cue::Stop => ("voiceless-cue-stop.wav", [880.0, 660.0]),
    };
    let path = dir.join(name);
    if !path.exists() {
        std::fs::write(&path, tone_wav(&tones, Duration::from_millis(60)))?;
    }
    Ok(path)
}

/// 16 kHz mono PCM16 WAV: each frequency for `each`, with short fades so
/// the tone does not click. Pure (no I/O); the 44-byte header is written by
/// hand to avoid a WAV dependency for two tiny files.
fn tone_wav(frequencies: &[f64], each: Duration) -> Vec<u8> {
    const RATE: u32 = 16_000;
    let per_tone = (RATE as f64 * each.as_secs_f64()) as usize;
    let fade = per_tone / 8;
    let mut pcm = Vec::with_capacity(per_tone * frequencies.len() * 2);
    for frequency in frequencies {
        for index in 0..per_tone {
            // Linear fade-in/out over the first and last eighth of each tone,
            // peak amplitude 0.4 of full scale.
            let envelope = (index.min(per_tone - index) as f64 / fade as f64).min(1.0);
            let phase = 2.0 * std::f64::consts::PI * frequency * index as f64 / RATE as f64;
            let sample = (phase.sin() * envelope * 0.4 * f64::from(i16::MAX)) as i16;
            pcm.extend_from_slice(&sample.to_le_bytes());
        }
    }
    let mut wav = Vec::with_capacity(44 + pcm.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + pcm.len() as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16_u32.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1_u16.to_le_bytes()); // mono
    wav.extend_from_slice(&RATE.to_le_bytes());
    wav.extend_from_slice(&(RATE * 2).to_le_bytes());
    wav.extend_from_slice(&2_u16.to_le_bytes());
    wav.extend_from_slice(&16_u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
    wav.extend_from_slice(&pcm);
    wav
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two 60 ms tones at 16 kHz = 2 * 960 samples of 2 bytes, and the
    /// RIFF/data sizes must agree with the buffer length.
    #[test]
    fn cue_wav_has_a_valid_header_and_length() {
        let wav = tone_wav(&[660.0, 880.0], Duration::from_millis(60));
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..16], b"WAVEfmt ");
        let data_len = u32::from_le_bytes([wav[40], wav[41], wav[42], wav[43]]) as usize;
        assert_eq!(data_len, 2 * 960 * 2);
        assert_eq!(wav.len(), 44 + data_len);
    }
}
