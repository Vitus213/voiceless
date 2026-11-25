use serde_json::{json, Value};
use tokio::net::TcpListener;
use voiceless::{coordinator::HistoryEntry, history::HistoryStore};

fn post(url: &str, body: Value) -> Result<Value, u16> {
    let response = ureq::post(url)
        .set("X-Voiceless-Token", "revision-test")
        .set("Content-Type", "application/json")
        .send_string(&body.to_string())
        .map_err(|error| match error {
            ureq::Error::Status(code, _) => code,
            transport => panic!("POST {url} failed: {transport}"),
        })?;
    Ok(serde_json::from_reader(response.into_reader()).unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn confirmation_checks_the_record_and_restoration_keeps_both_versions() {
    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("history.sqlite3");
    let store = HistoryStore::open(database.clone()).unwrap();
    let original = store
        .insert(
            &HistoryEntry {
                raw_text: "before cleanup".into(),
                final_text: "Before cleanup.".into(),
                processed_text: Some("Before cleanup.".into()),
                processing_mode: "quick".into(),
                status: "completed".into(),
                backend: "local-offline".into(),
                model: Some("original-model".into()),
                duration_seconds: None,
                audio_path: None,
            },
            "local-offline",
        )
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/api/records/{}/revisions",
        listener.local_addr().unwrap(),
        original.id
    );
    tokio::spawn(voiceless::console::serve_listener(
        listener,
        None,
        database,
        root.path().join("config.toml"),
        "revision-test".into(),
    ));

    let proposed = post(
        &url,
        json!({"text":"Corrected.", "expected":original.final_text}),
    )
    .unwrap();
    assert_eq!(proposed["revision"]["state"], "pending");
    assert_eq!(proposed["record"]["final_text"], original.final_text);
    let versions = store.revisions(&original.id).unwrap();
    let adopt = format!("{url}/{}/adopt", versions[1].id);
    assert_eq!(post(&adopt, json!({"expected":"stale"})), Err(409));
    assert_eq!(
        store.get(&original.id).unwrap().unwrap().final_text,
        original.final_text
    );
    let accepted = post(&adopt, json!({"expected": original.final_text})).unwrap();
    assert_eq!(accepted["record"]["raw_text"], original.raw_text);
    assert_eq!(accepted["record"]["final_text"], "Corrected.");
    assert_eq!(accepted["revision"]["kind"], "correction");
    assert_eq!(accepted["revision"]["asr_model"], "original-model");

    let restore = format!("{url}/{}/adopt", versions[0].id);
    let restored = post(&restore, json!({"expected": "Corrected."})).unwrap();
    assert_eq!(restored["record"]["final_text"], original.final_text);
    let response = ureq::get(&url)
        .set("X-Voiceless-Token", "revision-test")
        .call()
        .unwrap();
    let history: Value = serde_json::from_reader(response.into_reader()).unwrap();
    assert_eq!(history["revisions"][0]["state"], "current");
    assert_eq!(history["revisions"][1]["state"], "superseded");
    assert_eq!(history["revisions"][1]["final_text"], "Corrected.");
}
