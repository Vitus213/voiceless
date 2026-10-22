//! `voiceless model` subcommands backed by the integrity-checked manager.
//!
//! Thin command layer over `models.rs`: `main.rs` maps the clap
//! `ModelAction` to a `ModelCommand` and prints nothing itself. Every
//! command works on the default XDG directories (`~/.local/share/voiceless`
//! for payloads, `~/.cache/voiceless` for downloads) and returns a process
//! exit code: 0 on success, 1 on any failure (unknown id, download or
//! integrity error, corrupt payload, not installed).
//!
//! Output contract: `list` prints one line per catalog entry, as NDJSON
//! (`id`, `version`, `size_bytes`, `sha256_sri`, `license_status`) with
//! `--json`, otherwise `<id> (version <version>)`. `install` and `check`
//! print the payload directory on stdout; diagnostics and the per-file
//! `missing:`/`extra:`/`corrupt:`/`error:` lines of a failed check go to
//! stderr. `--json` is accepted globally but only affects `list`.
//!
//! `install` blocks for the whole download and verification (gigabytes
//! for the GGUF models) with no progress output; the console's
//! `/api/models` is the interactive path. Nothing here checks whether a
//! running daemon still uses a model being removed.

use crate::models::{default_cache_dir, default_data_dir, HttpDownloader, ModelManager};

/// Parsed `voiceless model <action> [id]`.
#[derive(Debug, Clone)]
pub enum ModelCommand {
    /// Print the pinned catalog.
    List,
    /// Download, verify and activate a catalog model (idempotent; repairs
    /// a corrupt install).
    Install { id: String },
    /// Re-verify the active payload of a catalog model.
    Check { id: String },
    /// Delete the pointer and every installed version of a model.
    Remove { id: String },
}

/// Execute one command and return its exit code. Blocking; the HTTPS
/// downloader is used for installs.
pub fn run(command: ModelCommand, json: bool) -> i32 {
    let manager = ModelManager::new(&default_data_dir(), &default_cache_dir());
    match command {
        ModelCommand::List => list(json),
        ModelCommand::Install { id } => install(&manager, &id),
        ModelCommand::Check { id } => check(&manager, &id),
        ModelCommand::Remove { id } => remove(&manager, &id),
    }
}

/// Print the catalog (never consults the disk, so it works before any
/// install). JSON mode emits one object per line, not an array.
fn list(json: bool) -> i32 {
    for spec in crate::models::catalog() {
        if json {
            println!(
                "{}",
                serde_json::json!({
                    "id": spec.id,
                    "version": spec.version,
                    "size_bytes": spec.size_bytes,
                    "sha256_sri": spec.sha256_sri,
                    "license_status": spec.license_status,
                })
            );
        } else {
            println!("{} (version {})", spec.id, spec.version);
        }
    }
    0
}

/// Install `id`: prints the payload directory on success, the manager
/// error on stderr otherwise.
fn install(manager: &ModelManager, id: &str) -> i32 {
    let Some(spec) = find_spec(id) else {
        eprintln!("unknown model id: {id}");
        return 1;
    };
    match manager.install(&spec, &HttpDownloader) {
        Ok(path) => {
            println!("{}", path.display());
            0
        }
        Err(error) => {
            eprintln!("model install failed: {error}");
            1
        }
    }
}

/// Check `id`: the payload path always goes to stdout once the pointer
/// resolves; a clean report prints "model `<id>` is valid" and returns 0, a
/// dirty one lists every finding on stderr and returns 1. A missing or
/// malformed pointer is reported as "model check failed".
fn check(manager: &ModelManager, id: &str) -> i32 {
    let Some(spec) = find_spec(id) else {
        eprintln!("unknown model id: {id}");
        return 1;
    };
    match manager.check(&spec) {
        Ok((path, report)) => {
            println!("{}", path.display());
            if report.ok() {
                println!("model {id} is valid");
                0
            } else {
                for missing in &report.missing {
                    eprintln!("missing: {missing}");
                }
                for extra in &report.extra {
                    eprintln!("extra: {extra}");
                }
                for corrupt in &report.corrupt {
                    eprintln!("corrupt: {corrupt}");
                }
                for error in &report.errors {
                    eprintln!("error: {error}");
                }
                1
            }
        }
        Err(error) => {
            eprintln!("model check failed: {error}");
            1
        }
    }
}

/// Remove `id`. Unlike the other commands this does not validate the id
/// against the catalog, so leftovers of a model that was dropped from the
/// catalog can still be deleted; "not installed" is an error (exit 1).
fn remove(manager: &ModelManager, id: &str) -> i32 {
    match manager.remove(id) {
        Ok(true) => 0,
        Ok(false) => {
            eprintln!("model {id} is not installed");
            1
        }
        Err(error) => {
            eprintln!("model remove failed: {error}");
            1
        }
    }
}

/// Catalog lookup by exact id.
fn find_spec(id: &str) -> Option<crate::models::ModelSpec> {
    crate::models::catalog()
        .into_iter()
        .find(|spec| spec.id == id)
}
