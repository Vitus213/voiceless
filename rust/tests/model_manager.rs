//! Contract of `voiceless::models::ModelManager`: a download is accepted
//! only when its SHA-256 matches the pinned SRI, tarball members outside
//! the allow-list abort the install, the payload lands under
//! `versions/<id>/<version>-<digest12>` with a `manifest.json` and a
//! `current` symlink, `check` detects corrupted, missing and stray files,
//! `remove` deletes pointer and payloads, and a reinstall repairs a
//! corrupted payload in place. A failed repair leaves the damaged payload
//! behind its pointer until a replacement is verified.
//!
//! Guards: a model a running engine or session holds (`lease`) is refused
//! to `remove` with `InUse`, from the CLI process as from the console's;
//! one operation at a time changes a model (`Busy`); an install reclaims
//! what a killed installer left behind and keeps the staging of an
//! install that is still running. `model list --json` carries the name,
//! languages and devices of every entry.
//!
//! Imports: without the transcribe.cpp engine nothing is staged. The
//! import checks that need the engine (a container the engine cannot load,
//! disk space, an interrupted upload, and the real-model smoke behind
//! `VOICELESS_IMPORT_GGUF`) run on a `transcribe`, `cuda` or `vulkan`
//! build.
//!
//! Fakes and fixtures: `FixtureDownloader` writes caller-supplied bytes
//! instead of fetching over HTTPS; `CorruptingDownloader` writes garbage
//! to provoke a digest mismatch; `BlockingDownloader` waits for the test,
//! so an install can be held in flight; `fixture_archive` builds a
//! tar.bz2 whose members sit under a `fixture-model/` top-level directory,
//! and `fixture_spec` pins its SRI and byte count so the archive verifies.
//! Every test gets a fresh `tempdir` as data and cache root, and the CLI
//! runs with every XDG root inside one.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use tempfile::tempdir;
use voiceless::models::{
    ArchiveType, CheckReport, Downloader, ModelError, ModelManager, ModelSpec,
};

/// Downloader fake: writes `bytes` to the destination, no network.
struct FixtureDownloader {
    bytes: Vec<u8>,
}

impl Downloader for FixtureDownloader {
    fn download(&self, _spec: &ModelSpec, destination: &Path) -> Result<(), ModelError> {
        let mut file = fs::File::create(destination)?;
        file.write_all(&self.bytes)?;
        Ok(())
    }
}

/// Downloader fake that always delivers bytes other than the pinned ones.
struct CorruptingDownloader;

impl Downloader for CorruptingDownloader {
    fn download(&self, _spec: &ModelSpec, destination: &Path) -> Result<(), ModelError> {
        let mut file = fs::File::create(destination)?;
        file.write_all(b"this is not the expected archive")?;
        Ok(())
    }
}

/// SRI string (`sha256-<base64>`) of `bytes`, the format of the catalog.
fn sri_for(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    use base64::Engine;
    format!(
        "sha256-{}",
        base64::engine::general_purpose::STANDARD.encode(digest)
    )
}

/// Build a tar.bz2 with regular-file members `fixture-model/<name>` (no
/// explicit directory entries), as the real Sherpa release archives are
/// shaped.
fn fixture_archive(members: &[(&str, &[u8])]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (name, content) in members {
        let path = format!("fixture-model/{name}");
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, &path, *content)
            .expect("append archive member");
    }
    let tar_bytes = builder.into_inner().expect("finish tar");
    let mut encoder = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::best());
    encoder.write_all(&tar_bytes).expect("compress tar");
    encoder.finish().expect("finish bz2")
}

/// A tarball carrying an explicit directory entry (`fixture-model/sub`)
/// installs: directory entries are not subject to the file allow-list.
#[test]
fn install_accepts_explicit_directory_entries_in_archives() {
    let root = tempdir().expect("temporary root");
    // Real tarballs carry explicit directory entries (e.g. test_wavs/);
    // they must not trip the file-member allowlist.
    let mut builder = tar::Builder::new(Vec::new());
    let mut dir_header = tar::Header::new_gnu();
    dir_header.set_size(0);
    dir_header.set_mode(0o755);
    dir_header.set_entry_type(tar::EntryType::Directory);
    dir_header.set_cksum();
    builder
        .append_data(&mut dir_header, "fixture-model/sub", std::io::empty())
        .expect("append directory entry");
    for (name, content) in [
        ("encoder.int8.onnx", b"e".as_ref()),
        ("decoder.int8.onnx", b"d".as_ref()),
        ("tokens.txt", b"t".as_ref()),
    ] {
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, format!("fixture-model/{name}"), content)
            .expect("append member");
    }
    let tar_bytes = builder.into_inner().expect("finish tar");
    let mut encoder = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::best());
    encoder.write_all(&tar_bytes).expect("compress");
    let archive = encoder.finish().expect("finish bz2");

    let spec = fixture_spec(&archive);
    let manager = ModelManager::new(&root.path().join("data"), &root.path().join("cache"));
    let payload = manager
        .install(&spec, &FixtureDownloader { bytes: archive })
        .expect("install must accept directory entries");
    assert!(payload.join("tokens.txt").is_file());
}

/// Catalog-shaped spec for `archive`: SRI and size derived from the bytes,
/// `fixture-model` as the stripped top level, three allowed members that
/// are all required.
fn fixture_spec(archive: &[u8]) -> ModelSpec {
    ModelSpec {
        id: "fixture-online".to_owned(),
        version: "test-1".to_owned(),
        url: "https://example.invalid/fixture.tar.bz2".to_owned(),
        sha256_sri: sri_for(archive),
        size_bytes: archive.len() as u64,
        archive_type: ArchiveType::TarBz2,
        top_level_directory: Some("fixture-model".to_owned()),
        allowed_members: ["encoder.int8.onnx", "decoder.int8.onnx", "tokens.txt"]
            .iter()
            .map(|member| (*member).to_owned())
            .collect(),
        required_paths: ["encoder.int8.onnx", "decoder.int8.onnx", "tokens.txt"]
            .iter()
            .map(|member| (*member).to_owned())
            .collect(),
        license_status: "test fixture".to_owned(),
    }
}

/// Install the standard three-member fixture under `root` and return the
/// manager, spec, payload directory and archive bytes for further steps.
fn install_fixture(root: &Path) -> (ModelManager, ModelSpec, PathBuf, Vec<u8>) {
    install_fixture_in_store(ModelManager::new(&root.join("data"), &root.join("cache")))
}

fn install_fixture_in_store(manager: ModelManager) -> (ModelManager, ModelSpec, PathBuf, Vec<u8>) {
    let archive = fixture_archive(&[
        ("encoder.int8.onnx", b"encoder-bytes"),
        ("decoder.int8.onnx", b"decoder-bytes"),
        ("tokens.txt", b"a\nb\n"),
    ]);
    let spec = fixture_spec(&archive);
    let payload = manager
        .install(
            &spec,
            &FixtureDownloader {
                bytes: archive.clone(),
            },
        )
        .expect("install fixture model");
    (manager, spec, payload, archive)
}

/// Happy path: members land in `versions/<id>/test-1-<digest12>/` next to
/// `manifest.json`, `models/<id>/current` points at them (a symlink; a path
/// file on Windows), and `resolve` returns the same payload directory
/// `install` returned.
#[test]
fn install_extracts_members_writes_manifest_and_activates_pointer() {
    let root = tempdir().expect("temporary root");
    let (manager, spec, payload, _archive) = install_fixture(root.path());

    assert!(payload.join("encoder.int8.onnx").is_file());
    assert!(payload.join("decoder.int8.onnx").is_file());
    assert!(payload.join("tokens.txt").is_file());
    assert!(payload.join("manifest.json").is_file());
    assert!(
        payload
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("test-1-"),
        "versioned directory carries version and digest prefix"
    );

    let pointer = root.path().join("data/models/fixture-online/current");
    #[cfg(unix)]
    assert!(
        fs::symlink_metadata(&pointer)
            .expect("pointer exists")
            .file_type()
            .is_symlink(),
        "current must be a symlink"
    );
    // Creating a symlink needs a privilege Windows users lack by default:
    // there the pointer is a small file naming the payload.
    #[cfg(windows)]
    assert!(
        fs::read_to_string(&pointer)
            .expect("pointer exists")
            .replace('\\', "/")
            .ends_with(&format!(
                "/{}",
                payload.file_name().unwrap().to_str().unwrap()
            )),
        "current must name the installed payload"
    );

    let resolved = manager
        .resolve(&spec.id)
        .expect("resolve")
        .expect("installed");
    assert_eq!(resolved, payload);
}

/// A fresh install checks clean; overwriting a file is reported as
/// `corrupt`, deleting a required file as `missing`, and neither is `ok`.
#[test]
fn check_reports_clean_payload_and_detects_corruption() {
    let root = tempdir().expect("temporary root");
    let (manager, spec, payload, _archive) = install_fixture(root.path());

    let (_, report) = manager.check(&spec).expect("check installed model");
    assert!(report.ok(), "{report:?}");

    fs::write(payload.join("encoder.int8.onnx"), b"tampered").expect("corrupt payload file");
    let (_, report) = manager.check(&spec).expect("check corrupted model");
    assert!(
        report.corrupt.contains(&"encoder.int8.onnx".to_owned()),
        "{report:?}"
    );
    assert!(!report.ok());

    fs::remove_file(payload.join("tokens.txt")).expect("delete required file");
    let (_, report) = manager.check(&spec).expect("check missing model file");
    assert!(
        report.missing.contains(&"tokens.txt".to_owned()),
        "{report:?}"
    );
}

/// A file added to the payload after installation shows up as `extra`.
#[test]
fn check_flags_unexpected_members() {
    let root = tempdir().expect("temporary root");
    let (manager, spec, payload, _archive) = install_fixture(root.path());

    fs::write(payload.join("rogue.onnx"), b"not in manifest").expect("add rogue file");
    let (_, report) = manager.check(&spec).expect("check payload with rogue file");
    assert!(
        report.extra.contains(&"rogue.onnx".to_owned()),
        "{report:?}"
    );
}

/// A member below the top level is recorded and compared exactly as the
/// catalog spells it (`test_wavs/en.wav`) on every platform. On Windows a
/// plain directory walk answers `test_wavs\en.wav`, which the allow-list
/// does not contain: a correct install then reads as corrupted and no
/// session can load the model.
#[test]
fn nested_members_check_clean_in_the_catalog_spelling() {
    let root = tempdir().expect("temporary root");
    let archive = fixture_archive(&[
        ("encoder.int8.onnx", b"encoder-bytes"),
        ("decoder.int8.onnx", b"decoder-bytes"),
        ("tokens.txt", b"a\nb\n"),
        ("test_wavs/en.wav", b"rigid audio bytes"),
    ]);
    let mut spec = fixture_spec(&archive);
    spec.allowed_members.push("test_wavs/en.wav".to_owned());
    let manager = ModelManager::new(&root.path().join("data"), &root.path().join("cache"));
    let payload = manager
        .install(&spec, &FixtureDownloader { bytes: archive })
        .expect("install a member below the top level");
    assert!(payload.join("test_wavs").join("en.wav").is_file());

    let (checked, report) = manager.check(&spec).expect("check installed model");
    assert_eq!(checked, payload);
    assert!(report.ok(), "{report:?}");

    let manifest: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(payload.join("manifest.json")).expect("read the manifest"),
    )
    .expect("parse the manifest");
    let files = manifest["files"]
        .as_object()
        .expect("the manifest's file table");
    assert!(
        files.contains_key("test_wavs/en.wav"),
        "the file table must spell members as the catalog does: {files:?}"
    );
    assert!(
        !files.keys().any(|name| name.contains('\\')),
        "no file table key carries a platform separator: {files:?}"
    );
}

/// Bytes that do not hash to the pinned SRI fail with
/// `ModelError::Integrity` mentioning SHA-256, and nothing gets activated.
#[test]
fn install_refuses_wrong_digest_and_reports_integrity_error() {
    let root = tempdir().expect("temporary root");
    let archive = fixture_archive(&[
        ("encoder.int8.onnx", b"e"),
        ("decoder.int8.onnx", b"d"),
        ("tokens.txt", b"t"),
    ]);
    let spec = fixture_spec(&archive);
    let manager = ModelManager::new(&root.path().join("data"), &root.path().join("cache"));

    let error = manager
        .install(&spec, &CorruptingDownloader)
        .expect_err("digest mismatch must fail");
    assert!(
        matches!(&error, ModelError::Integrity(id, message) if *id == spec.id && message.contains("SHA-256")),
        "{error:?}"
    );
    assert!(manager.resolve(&spec.id).expect("resolve").is_none());
}

/// A correctly hashed archive that carries a member outside the allow-list
/// (`malware.bin`) is rejected with `ModelError::Archive` naming it.
#[test]
fn install_refuses_disallowed_archive_members() {
    let root = tempdir().expect("temporary root");
    let archive = fixture_archive(&[
        ("encoder.int8.onnx", b"e"),
        ("decoder.int8.onnx", b"d"),
        ("tokens.txt", b"t"),
        ("malware.bin", b"payload"),
    ]);
    let spec = fixture_spec(&archive);
    let manager = ModelManager::new(&root.path().join("data"), &root.path().join("cache"));

    let error = manager
        .install(&spec, &FixtureDownloader { bytes: archive })
        .expect_err("disallowed member must fail");
    assert!(
        matches!(&error, ModelError::Archive(message) if message.contains("malware.bin")),
        "{error:?}"
    );
}

/// `remove` returns true and deletes payload and pointer; a second call
/// returns false and `check` then fails with `NotInstalled`.
#[test]
fn remove_deletes_pointer_and_versions_and_reports_absence() {
    let root = tempdir().expect("temporary root");
    let (manager, spec, payload, _archive) = install_fixture(root.path());

    assert!(manager.remove(&spec.id).expect("remove"));
    assert!(manager.resolve(&spec.id).expect("resolve").is_none());
    assert!(!payload.exists());
    assert!(!manager.remove(&spec.id).expect("second remove"));

    let error = manager.check(&spec).expect_err("check after remove");
    assert!(matches!(&error, ModelError::NotInstalled(id) if *id == spec.id));
}

/// Installing again over a corrupted payload wipes and re-downloads it
/// into the same directory, after which `check` is clean again.
#[test]
fn reinstall_after_corruption_repairs_payload() {
    let root = tempdir().expect("temporary root");
    let (manager, spec, payload, archive) = install_fixture(root.path());

    fs::write(payload.join("encoder.int8.onnx"), b"tampered").expect("corrupt");
    let (_, report) = manager.check(&spec).expect("check");
    assert!(!report.ok());
    let lease = manager.lease([&spec.id]);
    let busy = manager.install(
        &spec,
        &FixtureDownloader {
            bytes: archive.clone(),
        },
    );
    assert!(matches!(&busy, Err(ModelError::InUse(id)) if id == &spec.id));
    assert_eq!(
        fs::read(payload.join("encoder.int8.onnx")).unwrap(),
        b"tampered"
    );
    drop(lease);

    let repaired = manager
        .install(&spec, &FixtureDownloader { bytes: archive })
        .expect("reinstall repairs");
    assert_eq!(repaired, payload);
    let (_, report) = manager.check(&spec).expect("check after repair");
    assert!(report.ok(), "{report:?}");
}

/// The real Paraformer entry pins HTTPS, an sha256 SRI, a size, the int8
/// members and a license note; an empty `CheckReport` is `ok`.
#[test]
fn streaming_paraformer_catalog_entry_pins_supply_chain_fields() {
    let spec = voiceless::models::streaming_paraformer_spec();
    assert!(spec.url.starts_with("https://"));
    assert!(spec.sha256_sri.starts_with("sha256-"));
    assert!(spec.size_bytes > 0);
    assert!(spec
        .allowed_members
        .contains(&"encoder.int8.onnx".to_owned()));
    assert!(spec
        .required_paths
        .contains(&"decoder.int8.onnx".to_owned()));
    assert!(!spec.license_status.is_empty());
    let report = CheckReport::default();
    assert!(report.ok());
}

/// Downloader fake that waits for the test before it writes, so an
/// install can be held in flight.
struct BlockingDownloader {
    bytes: Vec<u8>,
    release: mpsc::Receiver<()>,
}

impl Downloader for BlockingDownloader {
    fn download(&self, _spec: &ModelSpec, destination: &Path) -> Result<(), ModelError> {
        self.release
            .recv()
            .expect("the test releases the installer");
        let mut file = fs::File::create(destination)?;
        file.write_all(&self.bytes)?;
        Ok(())
    }
}

/// The standard fixture archive under another model id.
fn fixture_spec_named(id: &str, archive: &[u8]) -> ModelSpec {
    let mut spec = fixture_spec(archive);
    spec.id = id.to_owned();
    spec
}

/// Names of the entries directly under `directory`, sorted; none when the
/// directory does not exist.
#[cfg(feature = "transcribe")]
fn entries(directory: &Path) -> Vec<String> {
    let Ok(read) = fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut names: Vec<String> = read
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// The built `voiceless` binary with every XDG root inside `home`.
fn voiceless(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_voiceless"));
    command
        .env("HOME", home)
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_CACHE_HOME", home.join("cache"))
        .env("XDG_CONFIG_HOME", home.join("config"));
    command
}

/// A model a running engine or session holds (`ModelManager::lease`, as
/// the daemon's warm engine and each session do) is refused to `remove`
/// with `InUse` and keeps its pointer and its files; once the lease is
/// dropped it can be removed.
#[test]
fn remove_refuses_a_model_a_running_engine_holds() {
    let root = tempdir().expect("temporary root");
    let (manager, spec, payload, _archive) = install_fixture(root.path());

    let leases = manager.lease([&spec.id]);
    assert!(leases.holds(&spec.id));
    assert!(manager.in_use(&spec.id));
    let error = manager
        .remove(&spec.id)
        .expect_err("a leased model is not removed");
    assert!(matches!(error, ModelError::InUse(id) if id == spec.id));
    assert!(payload.exists(), "the leased payload stays");
    assert_eq!(
        manager.resolve(&spec.id).expect("resolve"),
        Some(payload.clone()),
        "the pointer still leads to it"
    );
    assert!(
        manager.in_use(&spec.id),
        "the refused removal left the lease alone"
    );

    drop(leases);
    assert!(!manager.in_use(&spec.id));
    assert!(manager.remove(&spec.id).expect("remove"));
    assert!(!payload.exists());
}

/// One operation at a time changes a model, in whichever process it runs:
/// while an install is in flight, a second install and a removal of that
/// model are `Busy` and nothing is activated. An install of another model
/// reclaims what a killed installer left behind (its staging directory and
/// its `.partial` download) and keeps the staging of the install still
/// running.
#[test]
fn installs_are_serialized_per_model_and_reclaim_what_killed_ones_left() {
    let root = tempdir().expect("temporary root");
    let archive = fixture_archive(&[
        ("encoder.int8.onnx", b"encoder-bytes"),
        ("decoder.int8.onnx", b"decoder-bytes"),
        ("tokens.txt", b"a\nb\n"),
    ]);
    let repaired = fixture_spec_named("fixture-repaired", &archive);
    let running = fixture_spec_named("fixture-running", &archive);
    let data = root.path().join("data");
    let cache = root.path().join("cache");
    let store = ModelManager::new(&data, &cache);
    let versions = data.join("models/versions");

    // What a killed installer of `repaired` left behind.
    let killed = versions.join(&repaired.id).join(".staging-killed");
    fs::create_dir_all(&killed).expect("staging directory of the killed installer");
    fs::write(killed.join("encoder.int8.onnx"), b"half a download").expect("staged bytes");
    fs::create_dir_all(cache.join("model-downloads")).expect("download cache");
    fs::write(store.download_path(&repaired), b"interrupted download").expect("partial download");

    // An install of `running` is in flight, waiting for its download.
    let (release, blocked) = mpsc::channel();
    let installer = {
        let spec = running.clone();
        let manager = ModelManager::new(&data, &cache);
        let downloader = BlockingDownloader {
            bytes: archive.clone(),
            release: blocked,
        };
        thread::spawn(move || manager.install(&spec, &downloader))
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if matches!(store.remove(&running.id), Err(ModelError::Busy(_))) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the installer never took its model"
        );
        thread::sleep(Duration::from_millis(5));
    }
    // The directory that installer writes while it downloads.
    let in_flight = versions.join(&running.id).join(".staging-in-flight");
    fs::create_dir_all(&in_flight).expect("staging directory of the running install");

    // Another model's install reclaims the leftovers of the killed one.
    let payload = store
        .install(
            &repaired,
            &FixtureDownloader {
                bytes: archive.clone(),
            },
        )
        .expect("install");
    assert!(payload.join("tokens.txt").is_file());
    assert!(
        !killed.exists(),
        "the staging directory of the killed installer is reclaimed"
    );
    assert!(
        !store.download_path(&repaired).exists(),
        "the interrupted download is reclaimed"
    );
    assert!(
        in_flight.exists(),
        "the staging directory of the running install is kept"
    );
    assert!(store.check(&repaired).expect("check").1.ok());

    // The running install is alone on its model.
    assert!(matches!(
        store
            .install(
                &running,
                &FixtureDownloader {
                    bytes: archive.clone(),
                }
            )
            .expect_err("a second installer"),
        ModelError::Busy(id) if id == running.id
    ));
    assert!(matches!(
        store.remove(&running.id).expect_err("a removal"),
        ModelError::Busy(id) if id == running.id
    ));
    assert!(
        store.resolve(&running.id).expect("resolve").is_none(),
        "nothing is activated before the download is verified"
    );

    release.send(()).expect("release the installer");
    let installed = installer
        .join()
        .expect("installer thread")
        .expect("install in flight");
    assert!(installed.join("tokens.txt").is_file());
    assert_eq!(
        store.resolve(&running.id).expect("resolve"),
        Some(installed)
    );
    assert!(store.check(&running).expect("check").1.ok());
    assert!(in_flight.exists());
}

/// A corrupt payload is replaced only once the new download is verified:
/// a repair that fails leaves the pointer and the payload in place (still
/// reported as corrupt), and a later good one repairs them in place.
#[test]
fn a_failed_repair_keeps_the_installed_model_in_place() {
    let root = tempdir().expect("temporary root");
    let (manager, spec, payload, archive) = install_fixture(root.path());
    fs::write(payload.join("encoder.int8.onnx"), b"tampered").expect("corrupt");

    let error = manager
        .install(&spec, &CorruptingDownloader)
        .expect_err("digest mismatch");
    assert!(
        matches!(&error, ModelError::Integrity(_, message) if message.contains("SHA-256")),
        "{error:?}"
    );
    assert!(payload.exists(), "the damaged payload is not deleted");
    assert_eq!(
        manager.resolve(&spec.id).expect("resolve"),
        Some(payload.clone()),
        "behind the same pointer"
    );
    assert!(
        !manager.check(&spec).expect("check").1.ok(),
        "and still reported as corrupt"
    );

    let repaired = manager
        .install(&spec, &FixtureDownloader { bytes: archive })
        .expect("repair");
    assert_eq!(repaired, payload);
    assert!(manager.check(&spec).expect("check after the repair").1.ok());
}

/// Catalog payloads are not imports, and a directory that merely sits
/// where imported payloads live lists nothing without a pointer.
#[test]
fn imported_list_ignores_catalog_payloads_and_uninstalled_directories() {
    let root = tempdir().expect("temporary root");
    let (manager, _spec, _payload, _archive) = install_fixture(root.path());
    assert!(manager.list_imported().expect("list").is_empty());
    fs::create_dir_all(
        root.path()
            .join("data/models/versions/imported-broken-gguf/leftover"),
    )
    .expect("leftover payload directory");
    assert!(
        manager.list_imported().expect("list").is_empty(),
        "without a pointer nothing is installed"
    );
}

/// The guard holds across processes: `voiceless model remove` fails while
/// another process (here this test, as a running engine would) leases the
/// model, and succeeds once the lease is gone.
#[test]
fn model_remove_refuses_across_processes_while_a_runtime_leases_the_model() {
    let root = tempdir().expect("temporary root");
    let manager = ModelManager::new(
        &root.path().join("data/voiceless"),
        &root.path().join("cache/voiceless"),
    );
    let (manager, spec, payload, _archive) = install_fixture_in_store(manager);

    let leases = manager.lease([&spec.id]);
    let output = voiceless(root.path())
        .arg("model")
        .arg("remove")
        .arg(&spec.id)
        .output()
        .expect("run model remove");
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).expect("UTF-8");
    assert!(
        stderr.contains("is in use by a running engine or session"),
        "{stderr}"
    );
    assert!(payload.exists());

    drop(leases);
    let output = voiceless(root.path())
        .arg("model")
        .arg("remove")
        .arg(&spec.id)
        .output()
        .expect("run model remove");
    assert!(output.status.success(), "{output:?}");
    assert!(!payload.exists());
}

/// `model list --json` names every catalog entry with its languages and
/// the devices this build has for it, and tells imports from catalog
/// entries.
#[test]
fn model_list_json_carries_name_languages_devices_and_import_flag() {
    let root = tempdir().expect("temporary root");
    let output = voiceless(root.path())
        .args(["model", "list", "--json"])
        .output()
        .expect("run model list");
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).expect("UTF-8");
    let entries: Vec<serde_json::Value> = stdout
        .lines()
        .map(|line| serde_json::from_str(line).expect("JSON line"))
        .collect();
    assert!(entries.len() >= 7, "{} entries", entries.len());
    for entry in &entries {
        assert_eq!(entry["imported"], serde_json::json!(false), "{entry}");
        assert!(
            entry["name"].is_string()
                && entry["languages"].is_array()
                && entry["devices"].is_array()
                && entry["sha256_sri"].is_string(),
            "{entry}"
        );
    }
    let entry = |id: &str| {
        entries
            .iter()
            .find(|entry| entry["id"].as_str() == Some(id))
            .unwrap_or_else(|| panic!("{id} is listed"))
    };
    let qwen = entry("qwen3-asr-1.7b-gguf");
    assert!(qwen["languages"]
        .as_array()
        .expect("languages")
        .iter()
        .any(|language| language.as_str() == Some("zh")));
    assert_eq!(
        entry("sense-voice-int8")["devices"],
        serde_json::json!(["cpu"])
    );
    assert_eq!(
        entry("sense-voice-int8")["name"],
        "SenseVoice Small (ONNX int8)"
    );
}

/// Without the transcribe.cpp engine nothing is staged for an import and
/// `voiceless model import` says which package can.
#[cfg(not(feature = "transcribe"))]
#[test]
fn model_import_refuses_without_the_engine_and_stages_nothing() {
    let root = tempdir().expect("temporary root");
    let manager = ModelManager::new(&root.path().join("data"), &root.path().join("cache"));
    assert!(matches!(
        manager.begin_import(1024),
        Err(ModelError::Unsupported(_))
    ));

    let file = root.path().join("model.gguf");
    fs::write(&file, b"GGUF model file").expect("file");
    let output = voiceless(root.path())
        .arg("model")
        .arg("import")
        .arg(&file)
        .output()
        .expect("run model import");
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).expect("UTF-8");
    assert!(stderr.contains("cannot import GGUF models"), "{stderr}");
    assert!(stderr.contains("voiceless-vulkan"), "{stderr}");
    assert!(!root.path().join("data").exists(), "nothing was staged");
}

/// A GGUF v3 container of one tensor under `architecture`, with no model
/// in it: well formed for the header check, nothing an engine can load.
#[cfg(feature = "transcribe")]
fn header_only_gguf(architecture: &str) -> Vec<u8> {
    fn string(out: &mut Vec<u8>, value: &[u8]) {
        out.extend_from_slice(&(value.len() as u64).to_le_bytes());
        out.extend_from_slice(value);
    }
    let mut out = 0x4655_4747_u32.to_le_bytes().to_vec();
    out.extend_from_slice(&3_u32.to_le_bytes());
    out.extend_from_slice(&1_u64.to_le_bytes());
    out.extend_from_slice(&1_u64.to_le_bytes());
    string(&mut out, b"general.architecture");
    out.extend_from_slice(&8_u32.to_le_bytes());
    string(&mut out, architecture.as_bytes());
    string(&mut out, b"weight");
    out.extend_from_slice(&1_u32.to_le_bytes());
    out.extend_from_slice(&4_u64.to_le_bytes());
    out.extend_from_slice(&0_u32.to_le_bytes());
    out.extend_from_slice(&0_u64.to_le_bytes());
    out.resize(out.len().div_ceil(32) * 32 + 16, 0);
    out
}

/// A file that is a GGUF container but no model the engine has passes the
/// header check and is refused by the production load; nothing of it stays
/// in the store.
#[cfg(feature = "transcribe")]
#[test]
fn a_gguf_container_the_engine_cannot_load_is_refused() {
    let root = tempdir().expect("temporary root");
    let manager = ModelManager::new(&root.path().join("data"), &root.path().join("cache"));
    let bytes = header_only_gguf("voiceless_no_such_family");

    let mut source = bytes.as_slice();
    let error = manager
        .import_from(&mut source, "empty.gguf", bytes.len() as u64)
        .expect_err("no such family");
    assert!(
        matches!(&error, ModelError::Invalid(message) if message.contains("the engine cannot use it")),
        "{error:?}"
    );
    assert!(entries(&root.path().join("data/models/versions")).is_empty());
    assert!(manager.list_imported().expect("list").is_empty());
}

/// A file larger than the free space of the data filesystem is refused
/// before a byte is staged, naming the room it needs (the file and the
/// payload around it) and what is free.
#[cfg(feature = "transcribe")]
#[test]
fn an_import_too_large_for_the_disk_is_refused_before_staging() {
    let root = tempdir().expect("temporary root");
    let manager = ModelManager::new(&root.path().join("data"), &root.path().join("cache"));
    let announced = 1_u64 << 60;
    let error = manager
        .begin_import(announced)
        .expect_err("no disk holds an exbibyte");
    assert!(
        matches!(&error, ModelError::NoSpace { required, available } if *required > announced && *available < *required),
        "{error:?}"
    );
    assert!(entries(&root.path().join("data/models/versions")).is_empty());
}

/// An upload that stops before the announced size is `Incomplete`, and the
/// staged bytes go with it.
#[cfg(feature = "transcribe")]
#[test]
fn an_interrupted_upload_leaves_nothing_staged() {
    let root = tempdir().expect("temporary root");
    let manager = ModelManager::new(&root.path().join("data"), &root.path().join("cache"));
    let versions = root.path().join("data/models/versions");

    let mut file = manager.begin_import(4096).expect("staged upload");
    file.write_all(&[7_u8; 100]).expect("partial upload");
    assert_eq!(entries(&versions).len(), 1, "staged on the data filesystem");
    let error = manager
        .finish_import(file, "upload.gguf", 4096)
        .expect_err("incomplete");
    assert!(matches!(
        error,
        ModelError::Incomplete {
            expected: 4096,
            actual: 100
        }
    ));
    assert!(entries(&versions).is_empty(), "no staged bytes left");
    assert!(manager.list_imported().expect("list").is_empty());

    // An upload abandoned by its client (the handle dropped) goes too.
    drop(manager.begin_import(64).expect("staged upload"));
    assert!(entries(&versions).is_empty());
}

/// A real model, end to end: `VOICELESS_IMPORT_GGUF` names a speech GGUF
/// file. It is imported through the code the CLI and the console share,
/// under a content-addressed id no catalog entry has, with the name and
/// languages the engine reported; it is listed, checked and resolved like
/// a catalog model; the configuration takes its id and the production
/// decoder loads it; the same bytes import to the same model. Uses the
/// default XDG store like the other real-model smokes (point
/// `XDG_DATA_HOME` and `XDG_CACHE_HOME` at a scratch directory) and
/// removes what it imported.
#[cfg(feature = "transcribe")]
#[test]
#[ignore = "requires VOICELESS_IMPORT_GGUF=<model.gguf> on a build with the transcribe.cpp engine"]
fn import_accepts_a_real_model_for_list_check_and_selection() {
    use voiceless::config::AppConfig;
    use voiceless::engine::WarmEngine;
    use voiceless::model_cmd;
    use voiceless::models::{default_cache_dir, default_data_dir, model_info};
    use voiceless::models::{installed_model_dir, ImportedModel};
    use voiceless::offline_asr::{create_decoder, ComputeApi, OfflineFamily, OfflineOptions};

    let Ok(path) = std::env::var("VOICELESS_IMPORT_GGUF") else {
        eprintln!("set VOICELESS_IMPORT_GGUF=<model.gguf> to run the real import");
        return;
    };
    let manager = ModelManager::new(&default_data_dir(), &default_cache_dir());
    let before: Vec<String> = manager
        .list_imported()
        .expect("list")
        .iter()
        .map(|model: &ImportedModel| model.spec.id.clone())
        .collect();
    let bytes = fs::metadata(&path).expect("model file").len();
    let name = Path::new(&path)
        .file_name()
        .and_then(|name| name.to_str())
        .expect("file name");
    let mut source = std::io::BufReader::new(fs::File::open(&path).expect("open model file"));
    let imported = manager
        .import_from(&mut source, name, bytes)
        .expect("import the real model");
    let id = imported.spec.id.clone();
    let was_imported = before.contains(&id);

    assert!(id.starts_with("imported-") && id.ends_with("-gguf"), "{id}");
    assert!(
        voiceless::models::catalog()
            .iter()
            .all(|spec| spec.id != id),
        "no catalog entry has the id"
    );
    assert_eq!(imported.name, name);
    assert!(
        !imported.family.is_empty() && !imported.languages.is_empty(),
        "{imported:?}"
    );
    assert!(
        matches!(OfflineFamily::for_model(&id), Ok(OfflineFamily::Gguf)),
        "the engine takes it for a GGUF model"
    );

    // Listed, checked and resolved like a catalog model.
    assert!(manager
        .list_imported()
        .expect("list")
        .iter()
        .any(|model| model.spec.id == id));
    let (_, report) = manager.check(&imported.spec).expect("check");
    assert!(report.ok(), "{report:?}");
    let payload = installed_model_dir(&id).expect("the cold path resolves the import");
    assert_eq!(
        manager.resolve(&id).expect("resolve"),
        Some(payload.clone())
    );
    assert_eq!(
        fs::metadata(payload.join("model.gguf"))
            .expect("payload")
            .len(),
        bytes
    );
    assert_eq!(
        model_cmd::run(model_cmd::ModelCommand::Check { id: id.clone() }, false),
        0
    );
    assert_eq!(model_info(&imported.spec, Some(&imported)).name, name);

    // Selected: the configuration takes the id, and the engines load it.
    let config = AppConfig::from_toml(&format!(
        "[asr]\nstreaming_backend = \"local-offline\"\n[local]\nmodel = \"{id}\"\n"
    ))
    .expect("the configuration accepts an imported id");
    let decoder = create_decoder(
        OfflineFamily::Gguf,
        &payload,
        &OfflineOptions {
            provider: "cpu".to_owned(),
            num_threads: 4,
            hotwords: Vec::new(),
        },
    )
    .expect("the production decoder loads the import");
    assert_eq!(decoder.device().api, ComputeApi::Cpu);
    drop(decoder);
    if installed_model_dir("silero-vad").is_ok() {
        let engine = WarmEngine::load(&config)
            .expect("the warm engine loads")
            .expect("an offline engine");
        assert!(engine.model_ids().contains(&id));
        assert!(manager.in_use(&id), "the warm engine leases the import");
        assert!(matches!(
            manager.remove(&id).expect_err("a leased model"),
            ModelError::InUse(leased) if leased == id
        ));
        drop(engine);
    } else {
        eprintln!("silero-vad is not installed: the warm engine was not exercised");
    }

    // The same bytes import to the same model.
    let again = manager
        .import_from(
            &mut std::io::BufReader::new(fs::File::open(&path).expect("open model file")),
            name,
            bytes,
        )
        .expect("import again");
    assert_eq!(again, imported);
    assert_eq!(
        manager.list_imported().expect("list").len(),
        before.len() + usize::from(!was_imported)
    );

    // Leave the store as it was found.
    if !was_imported {
        assert!(manager.remove(&id).expect("remove the import"));
    }
}
