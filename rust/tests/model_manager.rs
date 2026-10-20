use std::fs;
use std::io::Write;
use std::path::Path;

use sha2::{Digest, Sha256};
use tempfile::tempdir;
use voiceless::models::{
    ArchiveType, CheckReport, Downloader, ModelError, ModelManager, ModelSpec,
};

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

struct CorruptingDownloader;

impl Downloader for CorruptingDownloader {
    fn download(&self, _spec: &ModelSpec, destination: &Path) -> Result<(), ModelError> {
        let mut file = fs::File::create(destination)?;
        file.write_all(b"this is not the expected archive")?;
        Ok(())
    }
}

fn sri_for(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    use base64::Engine;
    format!(
        "sha256-{}",
        base64::engine::general_purpose::STANDARD.encode(digest)
    )
}

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
