//! `voiceless transcribe` and `voiceless record` subcommands: the one-shot
//! batch path over `batch.rs`, dispatched from `main.rs`.
//!
//! Both commands load the default config (`AppConfig` from the XDG config
//! dir; a bad config fails before any file is read or audio captured),
//! take `--backend` or else `[asr] batch_backend`, run the blocking
//! transcription on `spawn_blocking` and print or inject the text.
//! Unlike `voiceless stream` the result skips `[text]` cleanup, modes and
//! history; injection uses the same `ConfiguredInjector` (`[inject]`) as
//! the streaming path. `transcribe` injects only with `--inject`;
//! `record` injects unless `--no-inject`. Exit codes: 0 on success (also
//! for an empty transcript, with a stderr note), 1 for a config,
//! recording, transcription or injection failure. Every failure is one
//! `Voiceless: ...` line on stderr.
//!
//! `--json` prints one object per line on stdout:
//! `{"type":"transcript","backend":...,"text":...}` and, when injecting,
//! `{"type":"finalized","injection":{...}}` (the `InjectionResult`).
//! Without `--json` and without injection the bare text is printed.

use std::path::PathBuf;

use crate::batch;
use crate::config::load_default_config;
use crate::coordinator::TextInjector;
use crate::stream::ConfiguredInjector;

/// `voiceless transcribe <wav> [--backend] [--inject] [--json]`.
pub struct TranscribeArgs {
    /// The 16 kHz mono PCM16 WAV to transcribe.
    pub wav: PathBuf,
    /// Overrides `[asr] batch_backend` (`cloud` | `local` | `sensevoice`).
    pub backend: Option<String>,
    /// Type the text into the focused window instead of printing it.
    pub inject: bool,
    /// JSON-lines output instead of plain text.
    pub json: bool,
}

/// Run `voiceless transcribe`; returns the process exit code. Side effects:
/// reads the config and the WAV, one network request or model load, stdout
/// and stderr output, and with `inject` the configured injector (child
/// processes or the Fcitx5 bridge).
pub async fn transcribe(args: TranscribeArgs) -> i32 {
    let config = match load_default_config() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("Voiceless: {error}");
            return 1;
        }
    };
    let backend = args
        .backend
        .clone()
        .unwrap_or_else(|| config.asr.batch_backend.clone());
    // Only an explicit `batch_model_dir` reaches the local backend; there
    // is no catalog lookup on the batch path.
    let model_dir = config.asr.batch_model_dir.clone();

    // ureq and Sherpa block; keep them off the tokio worker threads.
    let batch_result = match tokio::task::spawn_blocking({
        let wav = args.wav.clone();
        let config = config.clone();
        move || batch::transcribe(&wav, &backend, &config, model_dir.as_deref())
    })
    .await
    {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => {
            eprintln!("Voiceless: {error}");
            return 1;
        }
        // The blocking task panicked.
        Err(error) => {
            eprintln!("Voiceless: {error}");
            return 1;
        }
    };

    if args.json {
        let payload = serde_json::json!({
            "type": "transcript",
            "backend": batch_result.backend,
            "text": batch_result.text,
        });
        println!("{}", serde_json::to_string(&payload).expect("serialize"));
    }

    // Silence is not an error: nothing to inject, exit 0 (JSON consumers
    // already got the empty transcript line).
    if batch_result.text.is_empty() {
        eprintln!("Voiceless: no speech text was recognized");
        return 0;
    }

    if args.inject {
        let injected = ConfiguredInjector::new(config.inject.clone())
            .inject(&batch_result.text)
            .await;
        if args.json {
            let payload = serde_json::json!({
                "type": "finalized",
                "injection": injected,
            });
            println!("{}", serde_json::to_string(&payload).expect("serialize"));
        }
        if !injected.ok {
            eprintln!("Voiceless: injection failed: {}", injected.message);
            return 1;
        }
    } else if !args.json {
        println!("{}", batch_result.text);
    }
    0
}

/// `voiceless record --seconds N [--backend] [--no-inject] [--json]`.
pub struct RecordArgs {
    /// Requested recording length; passed to `batch::record_seconds`
    /// (see its doc about how it is used).
    pub seconds: f64,
    /// Overrides `[asr] batch_backend` for the transcription step.
    pub backend: Option<String>,
    /// Print instead of injecting (recording injects by default).
    pub no_inject: bool,
    /// JSON-lines output instead of plain text.
    pub json: bool,
}

/// Run `voiceless record`: record to a scratch WAV with `pw-record`, then
/// hand it to `transcribe`. Returns the process exit code. The scratch
/// file (`batch::temp_wav_path`) is removed after transcription whatever
/// the outcome; a recording failure returns before any removal.
pub async fn record(args: RecordArgs) -> i32 {
    // Config validation happens before recording, matching the strict-config
    // contract: bad config must fail before any capture starts.
    if let Err(error) = load_default_config() {
        eprintln!("Voiceless: {error}");
        return 1;
    }
    let wav_path = batch::temp_wav_path();
    // `pw-record` is waited on synchronously; keep that off the runtime.
    match tokio::task::spawn_blocking({
        let wav_path = wav_path.clone();
        move || batch::record_seconds(args.seconds, &wav_path)
    })
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            eprintln!("Voiceless: {error}");
            return 1;
        }
        Err(error) => {
            eprintln!("Voiceless: {error}");
            return 1;
        }
    }
    let code = transcribe(TranscribeArgs {
        wav: wav_path.clone(),
        backend: args.backend,
        inject: !args.no_inject,
        json: args.json,
    })
    .await;
    let _ = std::fs::remove_file(&wav_path);
    code
}
