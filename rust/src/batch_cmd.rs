//! `voiceless transcribe` and `voiceless record` subcommands.

use std::path::PathBuf;

use crate::batch;
use crate::config::load_default_config;
use crate::coordinator::TextInjector;
use crate::stream::ConfiguredInjector;

pub struct TranscribeArgs {
    pub wav: PathBuf,
    pub backend: Option<String>,
    pub inject: bool,
    pub json: bool,
}

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
    let model_dir = config.asr.batch_model_dir.clone();

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

pub struct RecordArgs {
    pub seconds: f64,
    pub backend: Option<String>,
    pub no_inject: bool,
    pub json: bool,
}

pub async fn record(args: RecordArgs) -> i32 {
    // Config validation happens before recording, matching the strict-config
    // contract: bad config must fail before any capture starts.
    if let Err(error) = load_default_config() {
        eprintln!("Voiceless: {error}");
        return 1;
    }
    let wav_path = batch::temp_wav_path();
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
