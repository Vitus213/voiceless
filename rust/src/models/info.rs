//! What the model lists show beyond the supply-chain fields of a
//! `ModelSpec`: the display name, the languages a model recognizes and the
//! devices this build can run it on. `model list` and the console's model
//! list both read it, so the CLI and the page show the same facts.
//!
//! Catalog entries carry their facts here; an imported model carries its
//! own in `manifest.json` (`import.rs`). `devices` names the
//! `[local].provider` values this build was compiled to run a model with:
//! a GGUF model lists none without the transcribe.cpp engine, and a GPU
//! only when its engine is in the build. It says nothing about where a
//! model really ran; the loaded decoder reports that
//! (`offline_asr::Decoder::device`).

use super::{ImportedModel, ModelSpec};

/// Languages of the Qwen3-ASR family, the BCP-47 codes of the engine's
/// `qwen3_asr` language table (transcribe.cpp `arch/qwen3_asr`).
const QWEN3_ASR: [&str; 30] = [
    "zh", "en", "yue", "ar", "de", "fr", "es", "pt", "id", "it", "ko", "ru", "th", "vi", "ja",
    "tr", "hi", "ms", "nl", "sv", "da", "fi", "pl", "cs", "fil", "fa", "el", "ro", "hu", "mk",
];
const SENSE_VOICE: [&str; 5] = ["zh", "en", "ja", "ko", "yue"];
const PARAFORMER: [&str; 2] = ["zh", "en"];
const CPU_ONLY: [&str; 1] = ["cpu"];
const GGUF_CPU: [&str; 1] = ["cpu"];
const GGUF_CUDA: [&str; 3] = ["cpu", "cuda", "auto"];
const GGUF_VULKAN: [&str; 3] = ["cpu", "vulkan", "auto"];
const GGUF_BOTH: [&str; 4] = ["cpu", "cuda", "vulkan", "auto"];

/// Display name, languages and devices of one model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelInfo {
    /// Name to show; the id itself for a model without one.
    pub name: String,
    /// BCP-47 codes of the languages the model recognizes; empty for a
    /// model that recognizes none (a VAD, a denoiser) or does not say.
    pub languages: Vec<String>,
    /// `[local].provider` values this build can run the model with.
    pub devices: Vec<String>,
}

/// The facts of `spec` as a list shows them: its own when it is an
/// imported `model`, else those of its catalog entry.
pub fn model_info(spec: &ModelSpec, imported: Option<&ImportedModel>) -> ModelInfo {
    let (name, languages) = match imported {
        Some(model) => (model.name.clone(), model.languages.clone()),
        None => (
            catalog_name(spec).to_owned(),
            owned(catalog_languages(spec)),
        ),
    };
    ModelInfo {
        name,
        languages,
        devices: owned(devices(spec)),
    }
}

/// Languages of an imported GGUF `architecture` when its file does not
/// list them.
#[cfg(any(feature = "transcribe", test))]
pub(super) fn known_languages(architecture: &str) -> &'static [&'static str] {
    match architecture {
        "qwen3_asr" => &QWEN3_ASR,
        _ => &[],
    }
}

fn catalog_name(spec: &ModelSpec) -> &str {
    match spec.id.as_str() {
        "streaming-paraformer-bilingual-zh-en" => "Streaming Paraformer zh+en",
        "qwen3-asr-1.7b-gguf" => "Qwen3-ASR 1.7B (GGUF Q8_0)",
        "qwen3-asr-0.6b-gguf" => "Qwen3-ASR 0.6B (GGUF Q8_0)",
        "qwen3-asr-0.6b-int8" => "Qwen3-ASR 0.6B (ONNX int8)",
        "sense-voice-int8" => "SenseVoice Small (ONNX int8)",
        "silero-vad" => "Silero VAD",
        "gtcrn" => "GTCRN speech enhancement",
        _ => &spec.id,
    }
}

fn catalog_languages(spec: &ModelSpec) -> &'static [&'static str] {
    match spec.id.as_str() {
        "streaming-paraformer-bilingual-zh-en" => &PARAFORMER,
        "qwen3-asr-1.7b-gguf" | "qwen3-asr-0.6b-gguf" | "qwen3-asr-0.6b-int8" => &QWEN3_ASR,
        "sense-voice-int8" => &SENSE_VOICE,
        _ => &[],
    }
}

/// `[local].provider` values this build can run `spec` with. GGUF models
/// are told by their id, as everywhere else (`OfflineFamily::for_model`).
fn devices(spec: &ModelSpec) -> &'static [&'static str] {
    if !spec.id.ends_with("-gguf") {
        return &CPU_ONLY;
    }
    match (
        cfg!(feature = "transcribe"),
        cfg!(feature = "cuda"),
        cfg!(feature = "vulkan"),
    ) {
        (false, _, _) => &[],
        (true, false, false) => &GGUF_CPU,
        (true, true, false) => &GGUF_CUDA,
        (true, false, true) => &GGUF_VULKAN,
        (true, true, true) => &GGUF_BOTH,
    }
}

fn owned(values: &'static [&'static str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every catalog entry has a name of its own and the languages the
    /// catalog documents for it; nothing that recognizes speech lists none.
    #[test]
    fn every_catalog_entry_has_a_name_and_its_languages() {
        let mut saw_speech = false;
        for spec in super::super::catalog() {
            let info = model_info(&spec, None);
            assert_ne!(info.name, spec.id, "{} has no display name", spec.id);
            match spec.id.as_str() {
                "silero-vad" | "gtcrn" => assert!(info.languages.is_empty()),
                _ => saw_speech = true,
            }
        }
        assert!(saw_speech);
        let by_id = |id: &str| {
            let spec = super::super::catalog()
                .into_iter()
                .find(|spec| spec.id == id)
                .expect(id);
            model_info(&spec, None)
        };
        assert_eq!(
            by_id("streaming-paraformer-bilingual-zh-en").languages,
            ["zh", "en"]
        );
        assert_eq!(by_id("sense-voice-int8").languages.len(), 5);
        for id in [
            "qwen3-asr-1.7b-gguf",
            "qwen3-asr-0.6b-gguf",
            "qwen3-asr-0.6b-int8",
        ] {
            let languages = by_id(id).languages;
            assert_eq!(languages.len(), 30, "{id}");
            assert!(languages.contains(&"zh".to_owned()) && languages.contains(&"en".to_owned()));
        }
        assert!(known_languages("qwen3_asr") == by_id("qwen3-asr-1.7b-gguf").languages.as_slice());
        assert!(known_languages("whisper").is_empty());
    }

    /// Sherpa models run on the CPU. A GGUF model lists the devices this
    /// build was compiled for and none without the engine.
    #[test]
    fn devices_describe_what_this_build_was_compiled_for() {
        let by_id = |id: &str| {
            let spec = super::super::catalog()
                .into_iter()
                .find(|spec| spec.id == id)
                .expect(id);
            model_info(&spec, None).devices
        };
        assert_eq!(by_id("sense-voice-int8"), ["cpu"]);
        assert_eq!(by_id("silero-vad"), ["cpu"]);
        let gguf = by_id("qwen3-asr-1.7b-gguf");
        if cfg!(feature = "transcribe") {
            assert_eq!(gguf.first().map(String::as_str), Some("cpu"));
            assert!(gguf.contains(&"auto".to_owned()));
            assert_eq!(gguf.contains(&"cuda".to_owned()), cfg!(feature = "cuda"));
            assert_eq!(
                gguf.contains(&"vulkan".to_owned()),
                cfg!(feature = "vulkan")
            );
        } else {
            assert!(gguf.is_empty());
        }
    }

    /// An imported model shows the name and languages found at import.
    #[test]
    fn an_imported_model_shows_its_own_name_and_languages() {
        let spec = super::super::qwen3_asr_1_7b_gguf_spec();
        let model = ImportedModel {
            spec: super::ModelSpec {
                id: "imported-whisper-0123abcdef456-gguf".to_owned(),
                ..spec
            },
            name: "large-v3-turbo.bin".to_owned(),
            languages: vec!["en".to_owned(), "ja".to_owned()],
            family: "whisper".to_owned(),
        };
        let info = model_info(&model.spec, Some(&model));
        assert_eq!(info.name, "large-v3-turbo.bin");
        assert_eq!(info.languages, ["en", "ja"]);
        assert_eq!(model_info(&model.spec, None).name, model.spec.id);
    }
}
