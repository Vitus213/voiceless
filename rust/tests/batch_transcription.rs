//! Contract tests for the batch path (`batch.rs`) without a network or a
//! model: the WAV parser (`load_wav_pcm16`) on hand-written minimal RIFF
//! files, and `CloudBatchClient` driven through `ScriptedBatchPoster`, a
//! `BatchPoster` fake that records every call (URL, bearer) and pops
//! scripted `(status, body)` responses. The local SenseVoice backend has
//! no test here (it needs a model).

mod common;

use std::io::Write;
use std::time::Duration;

use parking_lot::Mutex;
use std::sync::Arc;
use tempfile::tempdir;
use voiceless::batch::{load_wav_pcm16, BatchError, BatchPoster, CloudBatchClient};

/// Write a minimal 44-byte-header WAV: RIFF/WAVE, a 16-byte PCM `fmt `
/// chunk (mono, 16 kHz, 16 bit) and a `data` chunk with `samples`.
fn write_wav(path: &std::path::Path, samples: &[i16]) {
    let mut file = std::fs::File::create(path).expect("create wav");
    let data_bytes: Vec<u8> = samples
        .iter()
        .flat_map(|sample| sample.to_le_bytes())
        .collect();
    let data_len = data_bytes.len() as u32;
    let riff_len = 36 + data_len;
    file.write_all(b"RIFF").unwrap();
    file.write_all(&riff_len.to_le_bytes()).unwrap();
    file.write_all(b"WAVE").unwrap();
    file.write_all(b"fmt ").unwrap();
    file.write_all(&16u32.to_le_bytes()).unwrap();
    file.write_all(&1u16.to_le_bytes()).unwrap(); // PCM
    file.write_all(&1u16.to_le_bytes()).unwrap(); // mono
    file.write_all(&16000u32.to_le_bytes()).unwrap();
    file.write_all(&32000u32.to_le_bytes()).unwrap(); // byte rate
    file.write_all(&2u16.to_le_bytes()).unwrap(); // block align
    file.write_all(&16u16.to_le_bytes()).unwrap(); // bits
    file.write_all(b"data").unwrap();
    file.write_all(&data_len.to_le_bytes()).unwrap();
    file.write_all(&data_bytes).unwrap();
}

#[test]
fn wav_loader_accepts_mono_pcm16_16khz() {
    let root = tempdir().expect("temporary root");
    let path = root.path().join("sample.wav");
    write_wav(&path, &[1, -2, 3, -4]);

    let pcm = load_wav_pcm16(&path).expect("load wav");
    assert_eq!(pcm.len(), 8);
    assert_eq!(&pcm[0..2], &1i16.to_le_bytes());
    assert_eq!(&pcm[2..4], &(-2i16).to_le_bytes());
}

/// Stereo and 8 kHz files are rejected with messages naming the rule; a
/// `data` size of 0xFFFFFFFF is clamped to the bytes actually present.
#[test]
fn wav_loader_rejects_wrong_format_and_clamps_oversized_data_length() {
    let root = tempdir().expect("temporary root");

    // Stereo rejected.
    let path = root.path().join("stereo.wav");
    write_wav(&path, &[1, 2]);
    std::fs::write(
        &path,
        patch_wav_fields(&std::fs::read(&path).unwrap(), Some(2), None, None),
    )
    .unwrap();
    assert!(matches!(load_wav_pcm16(&path), Err(BatchError::Wav(msg)) if msg.contains("mono")));

    // Wrong rate rejected.
    let path = root.path().join("rate.wav");
    write_wav(&path, &[1, 2]);
    std::fs::write(
        &path,
        patch_wav_fields(&std::fs::read(&path).unwrap(), None, Some(8000), None),
    )
    .unwrap();
    assert!(matches!(load_wav_pcm16(&path), Err(BatchError::Wav(msg)) if msg.contains("16000")));

    // RF64-style huge data length must clamp to the actual bytes.
    let path = root.path().join("clamp.wav");
    write_wav(&path, &[1, 2]);
    std::fs::write(
        &path,
        patch_wav_fields(&std::fs::read(&path).unwrap(), None, None, Some(0xFFFFFFFF)),
    )
    .unwrap();
    let pcm = load_wav_pcm16(&path).expect("clamped load");
    assert_eq!(pcm.len(), 4);
}

/// Overwrite header fields in place of a `write_wav` file: channels at
/// bytes 22..24, sample rate at 24..28, `data` size at 40..44.
fn patch_wav_fields(
    bytes: &[u8],
    channels: Option<u16>,
    rate: Option<u32>,
    data_len: Option<u32>,
) -> Vec<u8> {
    let mut out = bytes.to_vec();
    if let Some(channels) = channels {
        out[22..24].copy_from_slice(&channels.to_le_bytes());
    }
    if let Some(rate) = rate {
        out[24..28].copy_from_slice(&rate.to_le_bytes());
    }
    if let Some(len) = data_len {
        // data chunk header starts at offset 36 for this minimal layout.
        out[40..44].copy_from_slice(&len.to_le_bytes());
    }
    out
}

/// `BatchPoster` fake: `responses` is a stack (popped from the end, so
/// script them in reverse order; exhausted = HTTP 500), `calls` records
/// `(url, bearer)` per request. Never returns a transport error.
#[derive(Clone)]
struct ScriptedBatchPoster {
    responses: Arc<Mutex<Vec<(u16, String)>>>,
    calls: Arc<Mutex<Vec<(String, String)>>>,
}

impl BatchPoster for ScriptedBatchPoster {
    fn post_json(
        &self,
        url: &str,
        _body: &str,
        bearer: &str,
        _timeout: Duration,
    ) -> Result<(u16, String), BatchError> {
        self.calls.lock().push((url.to_owned(), bearer.to_owned()));
        let response = self
            .responses
            .lock()
            .pop()
            .unwrap_or_else(|| (500, String::new()));
        Ok(response)
    }
}

fn client_with(poster: ScriptedBatchPoster) -> CloudBatchClient<ScriptedBatchPoster> {
    CloudBatchClient::new(
        "https://dashscope.example.com/".to_owned(),
        "sk-test".to_owned(),
        "qwen3-asr-flash-2026-02-10".to_owned(),
        Duration::from_secs(1),
        poster,
    )
}

/// A 200 with one content item: the trimmed text is returned, the request
/// went to `<base_url>/api/v1/services/aigc/multimodal-generation/generation`
/// (trailing slash of the base URL removed) with the configured key.
#[test]
fn cloud_client_sends_base64_wav_and_returns_first_text() {
    let poster = ScriptedBatchPoster {
        responses: Arc::new(Mutex::new(vec![(
            200,
            serde_json::json!({
                "output": {"choices": [{"message": {"content": [{"text": "  你好世界  "}]}}]}
            })
            .to_string(),
        )])),
        calls: Arc::new(Mutex::new(Vec::new())),
    };
    let calls = poster.calls.clone();
    let client = client_with(poster);

    let text = client
        .transcribe_wav_bytes(b"RIFFfake")
        .expect("transcribe");
    assert_eq!(text, "你好世界");
    let (url, bearer) = &calls.lock()[0];
    assert_eq!(
        url,
        "https://dashscope.example.com/api/v1/services/aigc/multimodal-generation/generation"
    );
    assert_eq!(bearer, "sk-test");
}

#[test]
fn cloud_client_treats_401_403_as_auth_failures_without_retry() {
    let poster = ScriptedBatchPoster {
        responses: Arc::new(Mutex::new(vec![(403, String::new())])),
        calls: Arc::new(Mutex::new(Vec::new())),
    };
    let calls = poster.calls.clone();
    let client = client_with(poster);

    let error = client.transcribe_wav_bytes(b"x").expect_err("auth failure");
    assert!(matches!(error, BatchError::CloudAuth(403)), "{error:?}");
    assert_eq!(calls.lock().len(), 1, "auth failures must not retry");
}

/// A 503 followed by a 200: two calls, the second one's text is returned.
#[test]
fn cloud_client_retries_transient_errors_then_succeeds() {
    let poster = ScriptedBatchPoster {
        responses: Arc::new(Mutex::new(vec![
            (
                200,
                serde_json::json!({
                    "output": {"choices": [{"message": {"content": [{"text": "重试成功"}]}}]}
                })
                .to_string(),
            ),
            (503, String::new()),
        ])),
        calls: Arc::new(Mutex::new(Vec::new())),
    };
    let calls = poster.calls.clone();
    let client = client_with(poster);

    let text = client.transcribe_wav_bytes(b"x").expect("eventual success");
    assert_eq!(text, "重试成功");
    assert_eq!(calls.lock().len(), 2);
}

#[test]
fn cloud_client_requires_api_key() {
    let poster = ScriptedBatchPoster {
        responses: Arc::new(Mutex::new(Vec::new())),
        calls: Arc::new(Mutex::new(Vec::new())),
    };
    let calls = poster.calls.clone();
    let mut client = client_with(poster);
    client.api_key = String::new();

    let error = client.transcribe_wav_bytes(b"x").expect_err("missing key");
    assert!(matches!(error, BatchError::CloudAuth(0)), "{error:?}");
    assert!(calls.lock().is_empty(), "no network call without a key");
}

/// `content: []` (silence) is `Ok("")`, not an error.
#[test]
fn empty_content_array_maps_to_empty_text() {
    let poster = ScriptedBatchPoster {
        responses: Arc::new(Mutex::new(vec![(
            200,
            serde_json::json!({
                "output": {"choices": [{"message": {"content": []}}]}
            })
            .to_string(),
        )])),
        calls: Arc::new(Mutex::new(Vec::new())),
    };
    let client = client_with(poster);
    assert_eq!(client.transcribe_wav_bytes(b"x").expect("silent"), "");
}

/// Writes an executable shell script and returns its path.
fn fake_recorder(dir: &std::path::Path, script: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("fake-pw-record");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// `record_seconds_with` stops the recorder after the requested time: the
/// fake runs until SIGINT, then writes the destination (its last argument)
/// and exits 0, like pw-cat finalizing the WAV. The call returns close to
/// the requested length instead of waiting for the recorder to end itself.
#[test]
fn record_seconds_interrupts_the_recorder_after_the_requested_time() {
    use voiceless::batch::record_seconds_with;

    let dir = tempdir().unwrap();
    let recorder = fake_recorder(
        dir.path(),
        "#!/bin/sh\nfor arg; do dest=\"$arg\"; done\ntrap 'printf RIFF > \"$dest\"; exit 0' INT\nwhile :; do sleep 0.02; done\n",
    );
    let destination = dir.path().join("take.wav");

    let started = std::time::Instant::now();
    record_seconds_with(recorder.to_str().unwrap(), 0.3, &destination).unwrap();
    let elapsed = started.elapsed();

    assert!(elapsed >= Duration::from_millis(300), "{elapsed:?}");
    assert!(
        elapsed < Duration::from_secs(3),
        "the recorder was not interrupted: {elapsed:?}"
    );
    assert_eq!(std::fs::read(&destination).unwrap(), b"RIFF");
}

/// A recorder that exits on its own before the timer (no device, bad
/// destination) is reported immediately with its exit status.
#[test]
fn record_seconds_reports_a_recorder_that_exits_early() {
    use voiceless::batch::record_seconds_with;

    let dir = tempdir().unwrap();
    let recorder = fake_recorder(dir.path(), "#!/bin/sh\nexit 3\n");
    let started = std::time::Instant::now();
    let error = record_seconds_with(recorder.to_str().unwrap(), 5.0, &dir.path().join("x.wav"))
        .unwrap_err();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "must not wait out the timer"
    );
    assert!(
        matches!(error, BatchError::Local(ref message) if message.contains("exited")),
        "{error:?}"
    );
}
