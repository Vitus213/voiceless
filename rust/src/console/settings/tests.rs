//! `settings`: revisions and three-way merges, the writers' lock, and the
//! JSON answers of `/api/config`. The handlers are called directly (the
//! HTTP layer is `tests/console.rs`); each test has its own `Baselines`, so
//! parallel tests never evict each other's revisions.

use super::*;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

/// A comment to keep, a secret to keep out of every answer, and a `[local]`
/// section with two fields to edit.
const SEED: &str = concat!(
    "# my settings\n[cloud]\napi_key = \"sk-stored\" # keep me\n\n",
    "[local]\nprovider = \"cpu\" # device\nnum_threads = 4\n",
);

struct Fixture {
    config: PathBuf,
    root: tempfile::TempDir,
}

/// A `config.toml` holding `text`, 0600, in a temp dir of its own.
fn fixture(text: &str) -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config.toml");
    write(&config, text);
    Fixture { config, root }
}

/// Replace the file the way a file editor does: no lock, no validation.
fn write(config: &Path, text: &str) {
    fs::write(config, text).unwrap();
    #[cfg(unix)]
    fs::set_permissions(config, PermissionsExt::from_mode(0o600)).unwrap();
}

/// The permission bits of `path`.
#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn read(config: &Path, baselines: &Mutex<Baselines>) -> ConfigFile {
    read_config(config, baselines).unwrap()
}

fn save(
    fixture: &Fixture,
    baselines: &Mutex<Baselines>,
    edit: &Edit,
) -> Result<Saved, UpdateError> {
    save_config(&fixture.config, baselines, edit)
}

/// A revision-aware update of one field.
fn checked(revision: &str, path: &str, value: Value) -> Edit {
    Edit::Checked {
        revision: revision.to_owned(),
        fields: vec![Field::at(path, value).unwrap()],
    }
}

/// The conflict an update was refused with.
fn refused(result: Result<Saved, UpdateError>) -> Conflict {
    match result {
        Err(UpdateError::Conflict(conflict)) => conflict,
        Err(error) => panic!("expected a conflict, got {error:?}"),
        Ok(_) => panic!("expected a conflict, but the update was written"),
    }
}

/// The config loader's message for an update it rejected.
fn rejected(result: Result<Saved, UpdateError>) -> String {
    match result {
        Err(UpdateError::Invalid(message)) => message,
        Err(error) => panic!("expected an invalid update, got {error:?}"),
        Ok(_) => panic!("expected an invalid update, but it was written"),
    }
}

/// A console over the fixture, as `serve_listener` builds one without a
/// daemon.
fn console(fixture: &Fixture) -> Arc<ConsoleState> {
    let store = fixture.root.path().join("history.sqlite3");
    Arc::new(ConsoleState {
        daemon: None,
        store: Arc::new(crate::history::HistoryStore::open(store).unwrap()),
        config_path: fixture.config.clone(),
        token: String::new(),
        port: 0,
        installs: crate::console::library::Installs::default(),
    })
}

/// Status and JSON body of a handler's reply, its error included.
async fn respond(response: Result<Response, ApiError>) -> (StatusCode, Value) {
    let response = response.unwrap_or_else(|error| error.into_response());
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

/// `PUT /api/config` with `body`.
async fn put(state: &Arc<ConsoleState>, body: Value) -> (StatusCode, Value) {
    respond(put_config(State(state.clone()), Json(body)).await).await
}

/// What `GET /api/config` answers.
async fn get(state: &Arc<ConsoleState>) -> Value {
    get_config(State(state.clone())).await.unwrap().0
}

/// `Mac` under the zero-padded `key` gives `expected` for `message`.
fn assert_mac(key: &[u8], message: &[u8], expected: &str) {
    let mut padded = [0_u8; 32];
    padded[..key.len()].copy_from_slice(key);
    let tag = Mac::new(padded).tag(message);
    let hex: String = tag.iter().map(|byte| format!("{byte:02x}")).collect();
    assert_eq!(hex, expected);
}

#[test]
fn mac_is_hmac_sha256() {
    // RFC 4231, test cases 1 to 4. Their keys are shorter than the process
    // key, and HMAC zero-pads a key to the block size either way.
    assert_mac(
        &[0x0b; 20],
        b"Hi There",
        "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7",
    );
    assert_mac(
        b"Jefe",
        b"what do ya want for nothing?",
        "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843",
    );
    assert_mac(
        &[0xaa; 20],
        &[0xdd; 50],
        "773ea91e36800e46854db8ebd09181a72959098b3ef8c122d9635514ced565fe",
    );
    let ascending: Vec<u8> = (0x01..=0x19).collect();
    assert_mac(
        &ascending,
        &[0xcd; 50],
        "82558a389a443c0ea4cc819899f2083a85f0faa3e578f8077a2e3ff46729665b",
    );

    // A key as long as the process key.
    let message = concat!(
        "This is a test using a larger than block-size key and a larger than ",
        "block-size data. The key needs to be hashed before being used by the ",
        "HMAC algorithm."
    );
    assert_mac(
        &[0xaa; 32],
        message.as_bytes(),
        "de22d3217492ee0e9ce716e75fbbf8060d46530ab10e6c81dc8823c23c52b1e0",
    );
}

#[test]
fn edits_of_different_fields_from_one_revision_are_both_kept() {
    let fixture = fixture(SEED);
    let baselines = Mutex::new(Baselines::default());
    let revision = read(&fixture.config, &baselines).revision;
    let device = checked(&revision, "local.provider", json!("vulkan"));
    let threads = checked(&revision, "local.num_threads", json!(8));

    let device = save(&fixture, &baselines, &device).unwrap();
    let threads = save(&fixture, &baselines, &threads).unwrap();

    assert!(
        device.restart_engine && threads.restart_engine,
        "[local] builds the engine"
    );
    assert_ne!(threads.file.revision, revision);
    let config = AppConfig::load(&fixture.config).unwrap();
    assert_eq!(config.local.provider.as_deref(), Some("vulkan"));
    assert_eq!(config.local.num_threads, 8);
    let text = fs::read_to_string(&fixture.config).unwrap();
    for comment in ["# my settings", "# keep me", "# device"] {
        assert!(text.contains(comment), "{text}");
    }
}

#[test]
fn a_field_changed_by_another_writer_is_not_overwritten() {
    let fixture = fixture(SEED);
    let baselines = Mutex::new(Baselines::default());
    let revision = read(&fixture.config, &baselines).revision;
    let device = checked(&revision, "local.provider", json!("vulkan"));
    let written = save(&fixture, &baselines, &device).unwrap();

    let again = checked(&revision, "local.provider", json!("cuda"));
    let conflict = refused(save(&fixture, &baselines, &again));

    assert_eq!(conflict.fields, ["local.provider"]);
    assert_eq!(conflict.current.revision, written.file.revision);
    let provider = conflict.current.config.local.provider.clone();
    assert_eq!(provider.as_deref(), Some("vulkan"));
}

#[test]
fn a_field_changed_by_a_file_editor_is_not_overwritten() {
    let fixture = fixture(SEED);
    let baselines = Mutex::new(Baselines::default());
    let revision = read(&fixture.config, &baselines).revision;
    write(&fixture.config, &SEED.replace("\"cpu\"", "\"cuda\""));

    let device = checked(&revision, "local.provider", json!("vulkan"));
    let conflict = refused(save(&fixture, &baselines, &device));
    assert_eq!(conflict.fields, ["local.provider"]);

    // Another field of the same stale revision still merges onto the edited
    // file.
    let threads = checked(&revision, "local.num_threads", json!(8));
    save(&fixture, &baselines, &threads).unwrap();
    let config = AppConfig::load(&fixture.config).unwrap();
    assert_eq!(config.local.provider.as_deref(), Some("cuda"));
    assert_eq!(config.local.num_threads, 8);
}

#[test]
fn an_unrelated_file_edit_is_merged_and_kept() {
    let fixture = fixture(SEED);
    let baselines = Mutex::new(Baselines::default());
    let revision = read(&fixture.config, &baselines).revision;
    let edited = format!("{SEED}\n# added by hand\n[audio]\ncues = true\n");
    write(&fixture.config, &edited);
    let audio = checked(&revision, "history.save_audio", json!(false));

    let saved = save(&fixture, &baselines, &audio).unwrap();

    assert!(
        !saved.restart_engine,
        "history is not read when the engine is built"
    );
    let text = fs::read_to_string(&fixture.config).unwrap();
    assert!(
        text.contains("# added by hand") && text.contains("cues = true"),
        "{text}"
    );
    let config = AppConfig::load(&fixture.config).unwrap();
    assert!(!config.history.save_audio && config.audio.cues);
}

#[test]
fn clearing_and_replacing_a_secret_from_one_revision_conflict() {
    let fixture = fixture(SEED);
    let baselines = Mutex::new(Baselines::default());
    let revision = read(&fixture.config, &baselines).revision;
    let clear = checked(&revision, "cloud.api_key", Value::Null);
    save(&fixture, &baselines, &clear).unwrap();
    let text = fs::read_to_string(&fixture.config).unwrap();
    assert!(!text.contains("sk-stored"), "{text}");

    let replace = checked(&revision, "cloud.api_key", json!("sk-new"));
    let conflict = refused(save(&fixture, &baselines, &replace));

    assert_eq!(conflict.fields, ["cloud.api_key"]);
    assert!(conflict.current.config.cloud.api_key.is_empty());
}

#[test]
fn an_unknown_revision_conflicts_in_every_field_and_writes_nothing() {
    let fixture = fixture(SEED);
    let baselines = Mutex::new(Baselines::default());
    let provider = Field::at("local.provider", json!("vulkan")).unwrap();
    let threads = Field::at("local.num_threads", json!(8)).unwrap();
    let edit = Edit::Checked {
        revision: "never-handed-out".to_owned(),
        fields: vec![provider, threads],
    };

    let conflict = refused(save(&fixture, &baselines, &edit));

    assert_eq!(conflict.fields, ["local.provider", "local.num_threads"]);
    assert_eq!(fs::read_to_string(&fixture.config).unwrap(), SEED);
}

#[test]
fn a_revision_the_runtime_forgot_conflicts_in_every_field() {
    let fixture = fixture(SEED);
    let baselines = Mutex::new(Baselines::default());
    let revision = read(&fixture.config, &baselines).revision;
    let threads = checked(&revision, "local.num_threads", json!(8));
    save(&fixture, &baselines, &threads).unwrap();
    let mut known = baselines.lock().unwrap();
    for index in 0..REVISIONS_REMEMBERED {
        let older = ConfigFile {
            revision: format!("older-{index}"),
            text: String::new(),
            config: AppConfig::default(),
        };
        known.remember(&older).unwrap();
    }
    drop(known);

    let device = checked(&revision, "local.provider", json!("vulkan"));
    let conflict = refused(save(&fixture, &baselines, &device));

    assert_eq!(conflict.fields, ["local.provider"]);
}

#[test]
fn an_empty_secret_keeps_the_stored_one_and_writes_nothing() {
    let fixture = fixture(SEED);
    let baselines = Mutex::new(Baselines::default());
    let revision = read(&fixture.config, &baselines).revision;
    let keep = checked(&revision, "cloud.api_key", json!(""));

    let saved = save(&fixture, &baselines, &keep).unwrap();

    assert_eq!(saved.file.revision, revision);
    assert!(!saved.restart_engine);
    assert_eq!(fs::read_to_string(&fixture.config).unwrap(), SEED);
}

#[test]
fn an_invalid_value_is_refused_before_anything_is_written() {
    let fixture = fixture(SEED);
    let baselines = Mutex::new(Baselines::default());
    let revision = read(&fixture.config, &baselines).revision;
    let device = checked(&revision, "local.provider", json!("rocm"));

    let message = rejected(save(&fixture, &baselines, &device));

    assert!(message.contains("provider"), "{message}");
    assert_eq!(fs::read_to_string(&fixture.config).unwrap(), SEED);
}

#[test]
fn only_section_key_paths_are_fields() {
    for path in ["local", "local.provider.extra", ".provider", "local.", ""] {
        let mut body = json!({ "revision": "r", "fields": {} });
        body["fields"][path] = json!(1);
        let message = match requested_edit(&body) {
            Err(message) => message,
            Ok(_) => panic!("{path:?} must not be a field"),
        };
        assert!(
            message.contains("is not a `section.key` path"),
            "{path}: {message}"
        );
    }

    // `has_*` flags belong to the answer; they are not fields of the config.
    let fixture = fixture(SEED);
    let baselines = Mutex::new(Baselines::default());
    let revision = read(&fixture.config, &baselines).revision;
    let body = json!({ "revision": revision, "fields": { "cloud.has_api_key": true } });
    let edit = requested_edit(&body).unwrap();
    let message = rejected(save(&fixture, &baselines, &edit));
    assert!(
        message.contains("unknown field") && message.contains("has_api_key"),
        "{message}"
    );
}

#[test]
fn a_missing_file_is_created_with_only_the_edited_fields() {
    let fixture = fixture("");
    fs::remove_file(&fixture.config).unwrap();
    let baselines = Mutex::new(Baselines::default());
    let revision = read(&fixture.config, &baselines).revision;
    let cues = checked(&revision, "audio.cues", json!(true));

    save(&fixture, &baselines, &cues).unwrap();

    let text = fs::read_to_string(&fixture.config).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines, ["[audio]", "cues = true"]);
    let config = AppConfig::load(&fixture.config).unwrap();
    assert_eq!(config.local.provider, None, "absent stays absent");
    #[cfg(unix)]
    assert_eq!(mode(&fixture.config), 0o600);
}

#[cfg(unix)]
#[test]
fn the_config_and_its_lock_file_stay_private() {
    let fixture = fixture("# no secrets yet\n[local]\nnum_threads = 4\n");
    let lock = fixture.config.with_extension("toml.lock");
    assert!(!lock.exists());
    fs::set_permissions(&fixture.config, PermissionsExt::from_mode(0o644)).unwrap();
    let baselines = Mutex::new(Baselines::default());
    let revision = read(&fixture.config, &baselines).revision;
    let threads = checked(&revision, "local.num_threads", json!(2));

    save(&fixture, &baselines, &threads).unwrap();

    assert_eq!((mode(&fixture.config), mode(&lock)), (0o600, 0o600));
}

#[test]
fn a_second_writer_waits_for_the_lock_and_gives_up_after_its_wait() {
    let fixture = fixture(SEED);
    let held = lock_writers(&fixture.config, WRITER_WAIT).unwrap();

    let error = lock_writers(&fixture.config, Duration::from_millis(20)).unwrap_err();

    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    drop(held);
    lock_writers(&fixture.config, Duration::from_millis(20)).unwrap();
}

#[test]
fn concurrent_writers_each_keep_their_own_field() {
    let fixture = fixture(SEED);
    let baselines = Mutex::new(Baselines::default());
    let revision = read(&fixture.config, &baselines).revision;
    let edits = [
        ("asr.batch_backend", json!("local")),
        ("asr.fallback_backend", json!("local-offline")),
        ("audio.pause_media", json!(false)),
        ("audio.cues", json!(true)),
        ("cloud.timeout_seconds", json!(12.5)),
        ("console.port", json!(8791)),
        ("doubao.enable_ddc", json!(true)),
        ("history.save_audio", json!(false)),
        ("hotkey.mode", json!("quick")),
        ("inject.preedit", json!(false)),
        ("local.fast_preview", json!(true)),
        ("local.num_threads", json!(3)),
        ("processing.model", json!("other-model")),
        ("text.strip_trailing_period", json!(true)),
    ];
    std::thread::scope(|scope| {
        for (path, value) in &edits {
            let (config, revision) = (fixture.config.as_path(), revision.as_str());
            let baselines = &baselines;
            scope.spawn(move || {
                let edit = checked(revision, path, value.clone());
                save_config(config, baselines, &edit).unwrap();
            });
        }
    });

    let loaded = AppConfig::load(&fixture.config).unwrap();
    let config: Value = serde_json::to_value(loaded).unwrap();
    for (path, value) in edits {
        let (section, key) = path.split_once('.').unwrap();
        assert_eq!(config[section][key], value, "{path}");
    }
    let text = fs::read_to_string(&fixture.config).unwrap();
    assert!(text.contains("# keep me"), "{text}");
}

#[tokio::test]
async fn get_config_keeps_its_shape_and_adds_the_revision() {
    let fixture = fixture(SEED);
    let state = console(&fixture);

    let config = get(&state).await;

    let mut plain = config.clone();
    let revision = plain.as_object_mut().unwrap().remove("revision").unwrap();
    let expected = masked(&AppConfig::load(&fixture.config).unwrap()).unwrap();
    assert_eq!(
        plain, expected,
        "everything but `revision` is the old answer"
    );
    assert_eq!(
        get(&state).await["revision"],
        revision,
        "same file, same revision"
    );
    write(&fixture.config, &format!("# a note\n{SEED}"));
    assert_ne!(
        get(&state).await["revision"],
        revision,
        "a comment is a change too"
    );
}

#[tokio::test]
async fn a_field_update_answers_the_new_revision_and_config() {
    let fixture = fixture(SEED);
    let state = console(&fixture);
    let revision = get(&state).await["revision"].as_str().unwrap().to_owned();
    let body = json!({ "revision": revision, "fields": { "local.num_threads": 8 } });

    let (status, saved) = put(&state, body).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(saved["ok"], true);
    assert_eq!(saved["restart_engine"], true);
    let reloaded = get(&state).await;
    assert_eq!(saved["revision"], reloaded["revision"]);
    let mut plain = reloaded;
    plain.as_object_mut().unwrap().remove("revision");
    assert_eq!(
        saved["config"], plain,
        "the answer is the next GET, without a round trip"
    );
}

#[tokio::test]
async fn an_update_without_fields_answers_the_current_state_and_writes_nothing() {
    let fixture = fixture(SEED);
    let state = console(&fixture);
    let before = get(&state).await;

    let (status, answer) = put(&state, json!({ "revision": "stale", "fields": {} })).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(answer["restart_engine"], false);
    assert_eq!(answer["revision"], before["revision"]);
    assert_eq!(fs::read_to_string(&fixture.config).unwrap(), SEED);
}

#[tokio::test]
async fn a_conflict_answers_409_with_the_paths_and_the_current_config() {
    let fixture = fixture(SEED);
    let state = console(&fixture);
    let revision = get(&state).await["revision"].as_str().unwrap().to_owned();
    let device = json!({ "revision": revision, "fields": { "local.provider": "vulkan" } });
    assert_eq!(put(&state, device).await.0, StatusCode::OK);

    let again = json!({ "revision": revision, "fields": { "local.provider": "cuda" } });
    let (status, answer) = put(&state, again).await;

    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(answer["conflicts"], json!(["local.provider"]));
    assert_eq!(answer["config"]["local"]["provider"], "vulkan");
    assert_eq!(answer["revision"], get(&state).await["revision"]);
    let message = answer["error"].as_str().unwrap();
    assert!(
        message.contains("local.provider") && !message.contains("vulkan"),
        "{message}"
    );
    let text = fs::read_to_string(&fixture.config).unwrap();
    assert!(text.contains("\"vulkan\""), "{text}");
}

#[tokio::test]
async fn a_secret_replaced_by_another_writer_conflicts_and_stays_out_of_the_answer() {
    let fixture = fixture(SEED);
    let state = console(&fixture);
    let revision = get(&state).await["revision"].as_str().unwrap().to_owned();
    let first = json!({ "revision": revision, "fields": { "cloud.api_key": "sk-first" } });
    assert_eq!(put(&state, first).await.0, StatusCode::OK);

    let second = json!({ "revision": revision, "fields": { "cloud.api_key": "sk-second" } });
    let (status, answer) = put(&state, second).await;

    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(answer["conflicts"], json!(["cloud.api_key"]));
    assert_eq!(answer["config"]["cloud"]["api_key"], "");
    assert_eq!(answer["config"]["cloud"]["has_api_key"], true);
    let answer = answer.to_string();
    for secret in ["sk-stored", "sk-first", "sk-second"] {
        assert!(!answer.contains(secret), "{answer}");
    }
    let text = fs::read_to_string(&fixture.config).unwrap();
    assert!(
        text.contains("sk-first") && !text.contains("sk-second"),
        "{text}"
    );
}

#[tokio::test]
async fn put_without_a_revision_keeps_the_last_writer_wins_contract() {
    let fixture = fixture(SEED);
    let state = console(&fixture);
    let patch = json!({
        "cloud": { "api_key": "", "has_api_key": true },
        "text": { "hotwords": ["张三丰"], "replacements": [{"from": "a", "to": "b"}] },
    });

    let (status, answer) = put(&state, patch).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(answer, json!({ "ok": true, "restart_engine": true }));
    let text = fs::read_to_string(&fixture.config).unwrap();
    for kept in [
        "# my settings",
        "# keep me",
        "sk-stored",
        "[[text.replacements]]",
    ] {
        assert!(text.contains(kept), "{text}");
    }

    // Nothing to set: the file is left alone.
    write(&fixture.config, "# untouched\n");
    let patch = json!({ "cloud": { "api_key": "", "has_api_key": true } });
    let (status, answer) = put(&state, patch).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(answer, json!({ "ok": true, "restart_engine": false }));
    let text = fs::read_to_string(&fixture.config).unwrap();
    assert_eq!(text, "# untouched\n");
}

#[tokio::test]
async fn invalid_updates_are_400_and_leave_the_file_alone() {
    let fixture = fixture(SEED);
    let state = console(&fixture);
    let revision = get(&state).await["revision"].as_str().unwrap().to_owned();
    for body in [
        json!({ "revision": revision, "fields": { "local.provider": "rocm" } }),
        json!({ "revision": revision, "fields": { "local.gpu": "vulkan" } }),
        json!({ "revision": revision, "fields": { "local.provider.extra": "x" } }),
        json!({ "revision": "", "fields": {} }),
        json!({ "revision": revision, "fields": {}, "local": { "num_threads": 8 } }),
        json!({ "local": { "provider": "rocm" } }),
        json!(null),
    ] {
        let (status, answer) = put(&state, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{answer}");
        assert!(answer["error"].is_string(), "{answer}");
        assert_eq!(fs::read_to_string(&fixture.config).unwrap(), SEED);
    }
}

#[tokio::test]
async fn saving_the_prompt_shares_the_locked_writer() {
    let fixture = fixture(SEED);
    let state = console(&fixture);
    let body = json!({ "prompt": "整理：{text}" });

    assert_eq!(
        save_prompt(State(state.clone()), Json(body))
            .await
            .unwrap()
            .0["ok"],
        true
    );

    let prompt = get_prompt(State(state.clone())).await.unwrap().0;
    assert_eq!(prompt["prompt"], "整理：{text}");
    let text = fs::read_to_string(&fixture.config).unwrap();
    assert!(
        text.contains("# keep me") && text.contains("sk-stored"),
        "{text}"
    );
}
