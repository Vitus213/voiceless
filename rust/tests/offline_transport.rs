//! Contract tests for the `local-offline` backend that need no model:
//! config validation, catalog consistency, requested-device failures and
//! the event order around a load. Fixtures: `AppConfig::from_toml` snippets,
//! the in-binary model catalog, and an `OfflineTransport` started with a
//! loader closure that fails (standing in for a missing model directory).
//! The order after a successful load (`Ready`, `DevicesLoaded`, then
//! recognition) is covered by the unit tests in `offline_asr`, which can
//! stand in for the model.

use std::path::Path;

use voiceless::config::AppConfig;
use voiceless::offline_asr::{
    create_decoder, ComputeDevice, OfflineAsrError, OfflineFamily, OfflineOptions, OfflineTransport,
};
use voiceless::realtime::RealtimeEvent;

/// GGUF uses build-specific device defaults and accepts explicit providers.
/// Sherpa models are CPU-only and have a lower segment limit.
#[test]
fn local_offline_config_is_validated() {
    let config = AppConfig::from_toml("[asr]\nstreaming_backend = \"local-offline\"\n").unwrap();
    assert_eq!(config.local.model, "qwen3-asr-1.7b-gguf");
    let gguf_default = if cfg!(feature = "cuda") {
        "cuda"
    } else if cfg!(feature = "vulkan") {
        "auto"
    } else {
        "cpu"
    };
    assert_eq!(config.local.device(), gguf_default);
    assert_eq!(config.local.segment_limit(), 30.0);
    for provider in ["cpu", "cuda", "vulkan", "auto"] {
        let explicit =
            AppConfig::from_toml(&format!("[local]\nprovider = {provider:?}\n")).unwrap();
        assert_eq!(explicit.local.device(), provider);
    }
    // Switching to a Sherpa model alone picks its CPU defaults.
    let sherpa = AppConfig::from_toml("[local]\nmodel = \"sense-voice-int8\"\n").unwrap();
    assert_eq!(sherpa.local.device(), "cpu");
    assert_eq!(sherpa.local.segment_limit(), 15.0);
    assert!(AppConfig::from_toml("[local]\nprovider = \"rocm\"\n").is_err());
    assert!(AppConfig::from_toml("[local]\nmodel = \"whisper\"\n").is_err());
    assert!(AppConfig::from_toml("[local]\nmax_segment_seconds = 61.0\n").is_err());
    // The Sherpa Qwen3-ASR export holds ~25 s; GGUF models allow more.
    assert!(AppConfig::from_toml(
        "[local]\nmodel = \"qwen3-asr-0.6b-int8\"\nprovider = \"cpu\"\nmax_segment_seconds = 30.0\n"
    )
    .is_err());
    assert!(AppConfig::from_toml("[local]\nmax_segment_seconds = 45.0\n").is_ok());
    assert!(AppConfig::from_toml(
        "[local]\nmodel = \"sense-voice-int8\"\nprovider = \"cpu\"\nhotwords = [\"豆包\"]\n"
    )
    .is_ok());
    for provider in ["cuda", "vulkan", "auto"] {
        assert!(AppConfig::from_toml(&format!(
            "[local]\nmodel = \"sense-voice-int8\"\nprovider = {provider:?}\n"
        ))
        .is_err());
    }
}

/// Every offline model and the VAD have an https URL, an SRI hash, and
/// required paths that are also allowed members; the id -> family mapping
/// matches the catalog ids.
#[test]
fn offline_catalog_entries_are_consistent() {
    let catalog = voiceless::models::catalog();
    for id in [
        "qwen3-asr-1.7b-gguf",
        "qwen3-asr-0.6b-gguf",
        "qwen3-asr-0.6b-int8",
        "sense-voice-int8",
        "silero-vad",
    ] {
        let spec = catalog
            .iter()
            .find(|spec| spec.id == id)
            .unwrap_or_else(|| panic!("{id} in catalog"));
        assert!(spec.url.starts_with("https://"));
        assert!(spec.sha256_sri.starts_with("sha256-"));
        for required in &spec.required_paths {
            assert!(
                spec.allowed_members.contains(required),
                "{id}: required {required} must be an allowed member"
            );
        }
    }
    assert_eq!(
        OfflineFamily::for_model("qwen3-asr-0.6b-int8").unwrap(),
        OfflineFamily::Qwen3Asr
    );
    assert_eq!(
        OfflineFamily::for_model("sense-voice-int8").unwrap(),
        OfflineFamily::SenseVoice
    );
    assert_eq!(
        OfflineFamily::for_model("qwen3-asr-1.7b-gguf").unwrap(),
        OfflineFamily::Gguf
    );
}

/// The transport reports Ready before the loader runs; audio sent in the
/// meantime is accepted (or rejected harmlessly once the worker is gone);
/// the loader's error then arrives as an `Error` event naming the path.
/// A failed load reports no devices: `Ready` and `Error` are the whole
/// stream, which then closes.
#[tokio::test]
async fn load_failure_is_reported_after_ready() {
    let transport = OfflineTransport::start(|| {
        Err(OfflineAsrError::ModelDirectoryMissing(
            "/nonexistent".into(),
        ))
    });
    // Capture may start before the model is up; audio sent meanwhile is
    // accepted, and the load failure ends the session with an error.
    assert_eq!(transport.next_event().await.unwrap(), RealtimeEvent::Ready);
    transport.send_audio(&[0_u8; 1_024]).ok();
    match transport.next_event().await.unwrap() {
        RealtimeEvent::Error(message) => assert!(message.contains("/nonexistent")),
        other => panic!("expected load error, got {other:?}"),
    }
    assert!(
        transport.next_event().await.is_err(),
        "a failed load reports no devices and closes the stream"
    );
}

/// The requested device is settled before anything loads: a Sherpa-ONNX
/// model asked for a GPU, or for `auto`, fails naming that request instead
/// of running on the CPU and passing for it. An unknown device name is
/// rejected for every model.
#[test]
fn gpu_request_for_a_cpu_model_fails_before_loading() {
    let load = |provider: &str| {
        create_decoder(
            OfflineFamily::SenseVoice,
            Path::new("/nonexistent"),
            &OfflineOptions {
                provider: provider.to_owned(),
                num_threads: 1,
                hotwords: Vec::new(),
            },
        )
    };
    let refused = |provider: &str| load(provider).err().expect("the device must be refused");
    for (provider, requested) in [
        ("cuda", ComputeDevice::Cuda),
        ("vulkan", ComputeDevice::Vulkan),
        ("auto", ComputeDevice::Auto),
    ] {
        match refused(provider) {
            OfflineAsrError::DeviceUnsupported {
                requested: actual, ..
            } => {
                assert_eq!(actual, requested)
            }
            error => panic!("{provider}: {error:?}"),
        }
    }
    assert_eq!(
        refused("cuda").to_string(),
        "Sherpa-ONNX models run on cpu; /nonexistent cannot use cuda"
    );
    assert_eq!(
        refused("rocm").to_string(),
        "unknown compute device \"rocm\"; expected cpu, cuda, vulkan or auto"
    );
}

/// A GGUF model is asked for its device first. Without the engine in this
/// build the request is refused naming the device; with it, the model file
/// is what gets checked.
#[test]
fn gguf_request_is_refused_before_any_model_is_read() {
    let error = create_decoder(
        OfflineFamily::Gguf,
        Path::new("/nonexistent"),
        &OfflineOptions {
            provider: "auto".to_owned(),
            num_threads: 1,
            hotwords: Vec::new(),
        },
    )
    .err()
    .expect("no GGUF model is installed");
    #[cfg(not(feature = "transcribe"))]
    match error {
        OfflineAsrError::DeviceUnavailable {
            requested, cause, ..
        } => {
            assert_eq!(requested, ComputeDevice::Auto);
            assert!(cause.starts_with("GGUF models need the transcribe.cpp engine"));
        }
        error => panic!("{error:?}"),
    }
    #[cfg(feature = "transcribe")]
    match error {
        OfflineAsrError::MissingModelFile(path) => assert!(path.ends_with("model.gguf")),
        error => panic!("{error:?}"),
    }
}
