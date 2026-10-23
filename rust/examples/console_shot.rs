//! Documentation screenshot generator: serves the history console on
//! 127.0.0.1:18792 against a throwaway database with synthetic audio so
//! docs/images/ screenshots never contain real recordings.
//!
//! cargo run --example console_shot   # then screenshot the page
//!
//! Open `http://127.0.0.1:18792/` in a browser: the index page embeds the
//! fixed token `console-shot` in its `voiceless-token` meta tag, so no
//! `?token=` is needed. The server runs with no daemon handle, so the
//! daemon-backed endpoints (status, live events, session control, engine
//! reload, logs) answer 503 "daemon not running"; the history and library
//! views are what to capture. Four demo records (three with a synthesized
//! tone WAV, one legacy record without audio) are re-dated over two days
//! so the date grouping is visible. Everything lives in a `tempdir` that
//! is removed on exit; the process serves until killed.

use tempfile::tempdir;
use tokio::net::TcpListener;
use voiceless::capture::wav_header;
use voiceless::console::serve_listener;
use voiceless::coordinator::HistoryEntry;
use voiceless::history::HistoryStore;

/// Build a 16 kHz mono PCM16 WAV of `seconds`: a sine at `freq` Hz with
/// slight vibrato under a half-sine envelope, so the console's waveform
/// view shows something speech-shaped rather than a flat bar.
fn synth_wav(seconds: f64, freq: f64) -> Vec<u8> {
    let n = (seconds * 16_000.0) as usize;
    let mut pcm = Vec::with_capacity(n * 2);
    for i in 0..n {
        let t = i as f64 / 16_000.0;
        let envelope = (std::f64::consts::PI * t / seconds).sin();
        let vibrato = 1.0 + 0.15 * (2.0 * std::f64::consts::PI * 2.0 * t).sin();
        let sample =
            (envelope * 0.35 * (2.0 * std::f64::consts::PI * freq * vibrato * t).sin()) * 32767.0;
        pcm.extend_from_slice(&(sample as i16).to_le_bytes());
    }
    let mut bytes = wav_header(pcm.len() as u64, 16_000).to_vec();
    bytes.extend_from_slice(&pcm);
    bytes
}

#[tokio::main]
async fn main() {
    let root = tempdir().expect("temp root");
    let audio_dir = root.path().join("audio");
    std::fs::create_dir_all(&audio_dir).expect("audio dir");
    let store = HistoryStore::open(root.path().join("history.sqlite3")).expect("store");

    // (final text, seconds, tone Hz, backend, processing mode, has audio).
    // The last row imitates a record from before WAV retention existed.
    let demos = [
        (
            "演示：云端实时转写，录音已保留",
            4.2,
            220.0,
            "cloud-realtime",
            "quick",
            true,
        ),
        (
            "演示：本地流式后端的波形与播放",
            6.5,
            330.0,
            "local-streaming",
            "quick",
            true,
        ),
        (
            "演示：翻译模式处理后的最终文本",
            3.0,
            440.0,
            "cloud-realtime",
            "translate-en",
            true,
        ),
        (
            "演示：功能上线前的旧记录没有录音",
            0.0,
            0.0,
            "cloud-realtime",
            "quick",
            false,
        ),
    ];
    for (index, (text, seconds, freq, backend, mode, with_audio)) in demos.iter().enumerate() {
        let mut entry = HistoryEntry {
            raw_text: (*text).to_owned(),
            processed_text: None,
            final_text: (*text).to_owned(),
            processing_mode: (*mode).to_owned(),
            status: "completed".to_owned(),
            backend: (*backend).to_owned(),
            model: None,
            duration_seconds: (*seconds > 0.0).then_some(*seconds),
            audio_path: None,
        };
        if *with_audio {
            let path = audio_dir.join(format!("demo-{index}.wav"));
            std::fs::write(&path, synth_wav(*seconds, *freq)).expect("write wav");
            entry.audio_path = Some(path.display().to_string());
        }
        store.insert(&entry, backend).expect("insert");
    }

    // Spread records over two local days so the grouping is visible.
    let connection =
        rusqlite::Connection::open(root.path().join("history.sqlite3")).expect("reopen");
    connection
        .execute_batch(
            "UPDATE recognition_history SET created_at = '2026-08-14T21:47:20Z'
             WHERE final_text LIKE '演示：云端实时%' OR final_text LIKE '演示：翻译%';
             UPDATE recognition_history SET created_at = '2026-08-13T20:12:08Z'
             WHERE final_text LIKE '演示：功能%';",
        )
        .expect("redate");
    drop(connection);

    let listener = TcpListener::bind("127.0.0.1:18792").await.expect("bind");
    println!("console-shot ready on 18792");
    serve_listener(
        listener,
        None,
        root.path().join("history.sqlite3"),
        root.path().join("config.toml"),
        "console-shot".to_owned(),
    )
    .await
    .expect("serve");
}
