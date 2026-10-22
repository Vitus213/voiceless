//! Contract of `voiceless::models::ModelManager`: a download is accepted
//! only when its SHA-256 matches the pinned SRI, tarball members outside
//! the allow-list abort the install, the payload lands under
//! `versions/<id>/<version>-<digest12>` with a `manifest.json` and a
//! `current` symlink, `check` detects corrupted, missing and stray files,
//! `remove` deletes pointer and payloads, and a reinstall repairs a
//! corrupted payload in place. Also pins the supply-chain fields of the
//! streaming Paraformer catalog entry.
//!
//! Fakes and fixtures: `FixtureDownloader` writes caller-supplied bytes
//! instead of fetching over HTTPS; `CorruptingDownloader` writes garbage
//! to provoke a digest mismatch; `fixture_archive` builds a tar.bz2 whose
//! members sit under a `fixture-model/` top-level directory, and
//! `fixture_spec` pins its SRI and byte count so the archive verifies.
//! Every test gets a fresh `tempdir` as data and cache root.

use std::fs;
use std::io::Write;
use std::path::Path;

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
fn install_fixture(root: &Path) -> (ModelManager, ModelSpec, std::path::PathBuf, Vec<u8>) {
    let archive = fixture_archive(&[
        ("encoder.int8.onnx", b"encoder-bytes"),
        ("decoder.int8.onnx", b"decoder-bytes"),
        ("tokens.txt", b"a\nb\n"),
    ]);
    let spec = fixture_spec(&archive);
    let manager = ModelManager::new(&root.join("data"), &root.join("cache"));
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
/// `manifest.json`, `models/<id>/current` is a symlink, and `resolve`
/// returns the same payload directory `install` returned.
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
    assert!(
        fs::symlink_metadata(&pointer)
            .expect("pointer exists")
            .file_type()
            .is_symlink(),
        "current must be a symlink"
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
