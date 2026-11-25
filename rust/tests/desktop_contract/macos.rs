use cpal::traits::{DeviceTrait, HostTrait};
use std::time::Duration;
use voiceless::capture::RawCapture;

#[test]
fn macos_cli_embeds_microphone_usage_description() {
    let plist = std::process::Command::new("/usr/bin/otool")
        .args(["-X", "-P", env!("CARGO_BIN_EXE_voiceless")])
        .output()
        .expect("read the CLI's embedded Info.plist");
    assert!(
        plist.status.success(),
        "{}",
        String::from_utf8_lossy(&plist.stderr)
    );
    let embedded = String::from_utf8(plist.stdout).unwrap();
    assert!(
        embedded.contains("<key>NSMicrophoneUsageDescription</key>"),
        "{embedded}"
    );
}

// Run on an explicitly provisioned TCC test account. Keeping the hardware
// cases opt-in prevents an ordinary cargo test from triggering a TCC prompt.
#[test]
#[ignore = "requires a previously denied microphone permission for the test binary"]
fn macos_denied_microphone_reports_privacy_settings() {
    let error = RawCapture::start(None)
        .err()
        .expect("capture unexpectedly started");
    assert!(
        error
            .to_string()
            .contains("Privacy & Security > Microphone"),
        "{error}"
    );
}

#[tokio::test]
#[ignore = "requires microphone consent and a connected default input"]
async fn macos_first_capture_returns_pcm_and_releases_the_device() {
    for _ in 0..2 {
        let mut capture = RawCapture::start(None).expect("open default CoreAudio input");
        let frame = capture
            .next_chunk()
            .await
            .expect("read samples")
            .expect("a PCM frame");
        assert_eq!(frame.len(), voiceless::capture::CHUNK_BYTES);
        capture.stop().await.expect("stop and release device");
    }
}

#[tokio::test]
#[ignore = "select a detachable default microphone, then unplug it when prompted"]
async fn macos_unplugging_default_input_ends_capture() {
    let selected = cpal::default_host()
        .default_input_device()
        .unwrap()
        .id()
        .unwrap();
    let mut capture = RawCapture::start(Some(selected)).expect("open detachable microphone");
    capture
        .next_chunk()
        .await
        .expect("read before unplug")
        .expect("a PCM frame");
    eprintln!("Unplug the selected microphone now");
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match capture.next_chunk().await {
                Ok(Some(_)) => continue,
                Err(_) => break,
                Ok(None) => panic!("disconnect reported as a clean recording end"),
            }
        }
    })
    .await
    .expect("capture did not report the device loss");
    assert!(
        capture.stop().await.is_err(),
        "a disconnected capture must not commit"
    );
}
