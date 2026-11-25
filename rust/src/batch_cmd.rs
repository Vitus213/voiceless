//! `voiceless transcribe` and `voiceless record` subcommands: the one-shot
//! batch path over `batch.rs`, dispatched from `main.rs`.
//!
//! Both commands load `--config` or the default user config before capture or
//! reading a WAV. They use the same settings for recording and transcription,
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

use std::path::{Path, PathBuf};

use crate::batch::{self, BatchBackend};
use crate::config::AppConfig;
use crate::coordinator::TextInjector;
use crate::stream::ConfiguredInjector;

/// `voiceless transcribe <wav> [--backend] [--inject] [--json]`.
pub struct TranscribeArgs {
    /// Explicit configuration file, if supplied on the command line.
    pub config_path: Option<PathBuf>,
    /// The 16 kHz mono PCM16 WAV to transcribe.
    pub wav: PathBuf,
    /// Overrides `[asr] batch_backend` (`cloud` | `local` | `sensevoice`).
    pub backend: Option<String>,
    /// Type the text into the focused window instead of printing it.
    pub inject: bool,
    /// JSON-lines output instead of plain text.
    pub json: bool,
}

fn load_selection(
    config_path: Option<&Path>,
    override_name: Option<&str>,
) -> Result<(AppConfig, BatchBackend), String> {
    let config = AppConfig::load_optional(config_path).map_err(|error| error.to_string())?;
    let backend = BatchBackend::parse(override_name.unwrap_or(&config.asr.batch_backend))?;
    Ok((config, backend))
}

/// Run `voiceless transcribe`; returns the process exit code. Side effects:
/// reads the config and the WAV, one network request or model load, stdout
/// and stderr output, and with `inject` the configured injector (child
/// processes or the Fcitx5 bridge).
pub async fn transcribe(args: TranscribeArgs) -> i32 {
    let selection = load_selection(args.config_path.as_deref(), args.backend.as_deref());
    let (config, backend) = match selection {
        Ok(selection) => selection,
        Err(error) => {
            eprintln!("Voiceless: {error}");
            return 1;
        }
    };
    let injector = args
        .inject
        .then(|| ConfiguredInjector::new(config.inject.clone()));
    transcribe_wav(args.wav, config, backend, injector, args.json).await
}

async fn transcribe_wav(
    wav: PathBuf,
    config: AppConfig,
    backend: BatchBackend,
    injector: Option<ConfiguredInjector>,
    json: bool,
) -> i32 {
    // ureq and Sherpa block; keep them off the tokio worker threads.
    let result = tokio::task::spawn_blocking(move || {
        backend.transcribe(&wav, &config, config.asr.batch_model_dir.as_deref())
    });
    let batch_result = match result.await {
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

    if json {
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

    if let Some(mut injector) = injector {
        let injected = injector.inject(&batch_result.text).await;
        if json {
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
    } else if !json {
        println!("{}", batch_result.text);
    }
    0
}

/// `voiceless record --seconds N [--backend] [--no-inject] [--json]`.
pub struct RecordArgs {
    /// Explicit configuration file, if supplied on the command line.
    pub config_path: Option<PathBuf>,
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

/// Validate the backend before opening the selected microphone. Use the same
/// settings to transcribe the completed recording.
pub async fn record(args: RecordArgs) -> i32 {
    let selection = load_selection(args.config_path.as_deref(), args.backend.as_deref());
    let (config, backend) = match selection {
        Ok(selection) => selection,
        Err(error) => {
            eprintln!("Voiceless: {error}");
            return 1;
        }
    };
    let injector = (!args.no_inject).then(|| ConfiguredInjector::new(config.inject.clone()));
    let recording = tokio::task::spawn_blocking(move || {
        let path = batch::temp_wav_path();
        batch::record_seconds(args.seconds, &path, config.audio.source.as_deref())?;
        Ok::<_, batch::BatchError>((path, config))
    })
    .await;
    let (wav_path, config) = match recording {
        Ok(Ok(recording)) => recording,
        Ok(Err(error)) => {
            eprintln!("Voiceless: {error}");
            return 1;
        }
        Err(error) => {
            eprintln!("Voiceless: {error}");
            return 1;
        }
    };
    let code = transcribe_wav(wav_path.clone(), config, backend, injector, args.json).await;
    let _ = std::fs::remove_file(&wav_path);
    code
}
