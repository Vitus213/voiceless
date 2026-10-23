//! Contract of the `benchmark` module behind `benchmark asr`: content
//! normalization and CER, JSONL corpus loading (including the dev/test
//! disjointness of the repository corpus when it is present), replay
//! through a `BackendFactory`, per-sample failure accounting and the
//! report field names. Fakes: `ScriptedFactory` returns a scripted
//! transcript per sample index and fails from a chosen index;
//! `ChunkRecorder` records the buffer sizes it was handed. No network and
//! no audio files; corpus fixtures are temp JSONL files.

use std::time::Duration;

use voiceless::benchmark::{
    cer, load_corpus, normalize_content, run_asr_benchmark, BackendFactory, BenchmarkReport,
    CorpusEntry,
};

#[test]
fn normalize_content_strips_punctuation_case_and_whitespace() {
    assert_eq!(
        normalize_content("你好，世界！ Hello World 123"),
        normalize_content("你好世界 hello  world123"),
        "content comparison must ignore punctuation, case and spaces"
    );
    assert_eq!(
        normalize_content("ＡＢＣ"),
        normalize_content("abc"),
        "NFKC"
    );
}

#[test]
fn cer_is_edit_distance_over_reference_length() {
    assert_eq!(cer("你好世界", "你好世界"), 0.0);
    assert_eq!(cer("你好世界", "你好"), 0.5);
    assert_eq!(cer("", "abc"), 0.0, "empty reference means nothing to miss");
    assert!(cer("abc", "") > 0.0);
}

/// Two well-formed lines load in order; then, only when the repository
/// corpus is present, no `test` id may reappear in `dev`.
#[test]
fn corpus_loader_parses_jsonl_and_keeps_dev_test_separate() {
    let root = tempfile::tempdir().expect("temporary root");
    let path = root.path().join("split.jsonl");
    std::fs::write(
        &path,
        "{\"id\": \"a\", \"reference\": \"你好\", \"voice\": \"x\"}\n\
         {\"id\": \"b\", \"reference\": \"世界\", \"voice\": \"y\"}\n",
    )
    .expect("write corpus");

    let entries = load_corpus(&path).expect("load corpus");
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].id, "a");

    // dev and test corpus files must not overlap ids. The repository
    // corpus is optional here because the Nix sandbox only ships rust/.
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("repo root");
    let dev_path = repo.join("scripts/asr_benchmark/corpus/dev.jsonl");
    let test_path = repo.join("scripts/asr_benchmark/corpus/test.jsonl");
    if dev_path.is_file() && test_path.is_file() {
        let dev = load_corpus(&dev_path).expect("dev");
        let test = load_corpus(&test_path).expect("test");
        assert!(!dev.is_empty() && !test.is_empty());
        let dev_ids: std::collections::HashSet<&str> =
            dev.iter().map(|entry| entry.id.as_str()).collect();
        assert!(
            test.iter()
                .all(|entry| !dev_ids.contains(entry.id.as_str())),
            "test split must be disjoint from dev"
        );
    }
}

#[test]
fn corpus_loader_rejects_malformed_lines() {
    let root = tempfile::tempdir().expect("temporary root");
    let path = root.path().join("bad.jsonl");
    std::fs::write(&path, "{\"id\": \"a\"}\nnot json\n").expect("write");
    assert!(load_corpus(&path).is_err());
}

/// Fake backend that returns a scripted transcript per sample.
struct ScriptedFactory {
    transcripts: Vec<String>,
    fail_from: Option<usize>,
}

impl BackendFactory for ScriptedFactory {
    fn replay(
        &mut self,
        index: usize,
        pcm: &[u8],
        _chunk_interval: Duration,
    ) -> Result<String, String> {
        let _ = pcm.len();
        if self.fail_from == Some(index) {
            return Err("transport failure".to_owned());
        }
        Ok(self.transcripts[index].clone())
    }
}

fn entry(id: &str, reference: &str) -> CorpusEntry {
    CorpusEntry {
        id: id.to_owned(),
        reference: reference.to_owned(),
        voice: "fixture".to_owned(),
    }
}

/// Three samples: punctuation-only difference scores 0, one wrong
/// character scores above 0, and a transport failure on the third is
/// recorded on that sample (error set) and counted in `failures` while
/// the run still completes with a positive aggregate CER.
#[tokio::test]
async fn benchmark_aggregates_content_cer_with_per_sample_errors() {
    let entries = vec![
        entry("s1", "你好世界"),
        entry("s2", "语音输入"),
        entry("s3", "失败样本"),
    ];
    let mut factory = ScriptedFactory {
        transcripts: vec![
            "你好，世界！".to_owned(),
            "语音输出".to_owned(),
            String::new(),
        ],
        fail_from: Some(2),
    };
    let pcm_by_id: Vec<(String, Vec<u8>)> = entries
        .iter()
        .map(|entry| (entry.id.clone(), vec![0_u8; 64]))
        .collect();

    let report = run_asr_benchmark(
        &entries,
        &pcm_by_id,
        &mut factory,
        "cloud-realtime",
        "test-model",
        "corpus-v1",
        Duration::ZERO,
    );

    assert_eq!(report.backend, "cloud-realtime");
    assert_eq!(report.model, "test-model");
    assert_eq!(report.corpus_version, "corpus-v1");
    assert_eq!(report.samples.len(), 3);
    assert_eq!(report.failures, 1);
    let s1 = report
        .samples
        .iter()
        .find(|sample| sample.id == "s1")
        .unwrap();
    assert_eq!(s1.cer, 0.0, "punctuation must not count as errors");
    let s2 = report
        .samples
        .iter()
        .find(|sample| sample.id == "s2")
        .unwrap();
    assert!(s2.cer > 0.0);
    assert!(report.content_cer > 0.0);
    let failed = report
        .samples
        .iter()
        .find(|sample| sample.id == "s3")
        .unwrap();
    assert!(failed.error.is_some());
}

#[test]
fn report_serializes_with_versioned_shape() {
    let report = BenchmarkReport {
        backend: "cloud-realtime".to_owned(),
        model: "m".to_owned(),
        corpus_version: "v".to_owned(),
        split: "test".to_owned(),
        samples: Vec::new(),
        failures: 0,
        content_cer: 0.01,
        generated_at: "2026-08-15T00:00:00Z".to_owned(),
    };
    let value = serde_json::to_value(&report).expect("serialize");
    for field in [
        "backend",
        "model",
        "corpus_version",
        "split",
        "samples",
        "failures",
        "content_cer",
        "generated_at",
    ] {
        assert!(value.get(field).is_some(), "missing field {field}");
    }
}

/// The replay loop hands each sample's whole PCM buffer to the factory
/// exactly once; splitting into 32 ms frames is the backend's job
/// (`benchmark_cmd::replay_cloud` / `replay_local`).
#[test]
fn replay_hands_each_sample_to_the_factory_once() {
    use std::sync::{Arc, Mutex};

    struct ChunkRecorder {
        seen: Arc<Mutex<Vec<usize>>>,
        transcript: String,
    }

    impl BackendFactory for ChunkRecorder {
        fn replay(
            &mut self,
            _index: usize,
            pcm: &[u8],
            interval: Duration,
        ) -> Result<String, String> {
            // The factory receives the whole sample and the 32 ms pacing
            // interval; chunking happens inside the real backends.
            let _ = interval;
            self.seen.lock().unwrap().push(pcm.len());
            Ok(self.transcript.clone())
        }
    }

    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut factory = ChunkRecorder {
        seen: seen.clone(),
        transcript: "文本".to_owned(),
    };
    let entries = vec![entry("c1", "文本")];
    let pcm: Vec<u8> = vec![1; 1024 * 3 + 500]; // three full chunks + tail
    let pcm_by_id = vec![("c1".to_owned(), pcm)];

    let report = run_asr_benchmark(
        &entries,
        &pcm_by_id,
        &mut factory,
        "local-streaming",
        "m",
        "v",
        Duration::ZERO,
    );
    assert_eq!(report.failures, 0);
    // The factory saw the whole buffer; the chunking contract is enforced by
    // the realtime session boundary, here we only assert the replay ran once.
    assert_eq!(seen.lock().unwrap().len(), 1);
}
