//! `voiceless model` subcommands backed by the integrity-checked manager.
//!
//! Thin command layer over `models.rs`: `main.rs` maps the clap
//! `ModelAction` to a `ModelCommand` and prints nothing itself. Every
//! command works on the default XDG directories (`~/.local/share/voiceless`
//! for payloads, `~/.cache/voiceless` for downloads) and returns a process
//! exit code: 0 on success, 1 on any failure (unknown id, download or
//! integrity error, corrupt payload, not installed, a model in use).
//!
//! Output contract: `list` prints one line per model, the catalog entries
//! first and the imported ones after them, as NDJSON (`id`, `version`,
//! `size_bytes`, `sha256_sri`, `license_status`, `name`, `languages`,
//! `devices`, `imported`) with `--json`, otherwise `<id> (version
//! <version>)` or `<id> (imported: <name>)`. `install` and `check` print
//! the payload directory on stdout. `import` prints the id of the imported
//! model on stdout, as a JSON object with `--json`. Diagnostics and the
//! per-file `missing:`/`extra:`/`corrupt:`/`error:` lines of a failed check
//! go to stderr.
//!
//! `install` blocks for the whole download and verification (gigabytes
//! for the GGUF models) with no progress output; the console's
//! `/api/models` is the interactive path. `import` runs the same code as
//! the console's `/api/models/import`. A model that a running engine or
//! session holds is refused by `remove`, from the daemon as from a one-shot
//! session.

use std::fs;
use std::io::BufReader;
use std::path::Path;

use crate::models::{
    default_cache_dir, default_data_dir, model_info, HttpDownloader, ModelManager,
};

/// Parsed `voiceless model <action> [argument]`.
#[derive(Debug, Clone)]
pub enum ModelCommand {
    /// Print the catalog and the imported models.
    List,
    /// Download, verify and activate a catalog model (idempotent; repairs
    /// a corrupt install).
    Install { id: String },
    /// Import a local GGUF file after verifying it.
    Import { file: std::path::PathBuf },
    /// Re-verify the active payload of a catalog or an imported model.
    Check { id: String },
    /// Delete the pointer and every installed version of a model.
    Remove { id: String },
}

/// Execute one command and return its exit code. Blocking; the HTTPS
/// downloader is used for installs.
pub fn run(command: ModelCommand, json: bool) -> i32 {
    let manager = ModelManager::new(&default_data_dir(), &default_cache_dir());
    match command {
        ModelCommand::List => list(&manager, json),
        ModelCommand::Install { id } => install(&manager, &id),
        ModelCommand::Import { file } => import(&manager, &file, json),
        ModelCommand::Check { id } => check(&manager, &id),
        ModelCommand::Remove { id } => remove(&manager, &id),
    }
}

/// Print the catalog and then the installed imported models. JSON mode
/// emits one object per line, not an array. The catalog needs no disk
/// access, so it is listed before any install; a store that cannot be read
/// fails the command.
fn list(manager: &ModelManager, json: bool) -> i32 {
    let imported = match manager.list_imported() {
        Ok(imported) => imported,
        Err(error) => {
            eprintln!("model list failed: {error}");
            return 1;
        }
    };
    let entries = crate::models::catalog()
        .into_iter()
        .map(|spec| (spec, None))
        .chain(
            imported
                .into_iter()
                .map(|model| (model.spec.clone(), Some(model))),
        );
    for (spec, imported) in entries {
        let info = model_info(&spec, imported.as_ref());
        if json {
            println!(
                "{}",
                serde_json::json!({
                    "id": spec.id,
                    "version": spec.version,
                    "size_bytes": spec.size_bytes,
                    "sha256_sri": spec.sha256_sri,
                    "license_status": spec.license_status,
                    "name": info.name,
                    "languages": info.languages,
                    "devices": info.devices,
                    "imported": imported.is_some(),
                })
            );
        } else if let Some(imported) = imported {
            println!("{} (imported: {})", spec.id, imported.name);
        } else {
            println!("{} (version {})", spec.id, spec.version);
        }
    }
    0
}

/// Install `id`: prints the payload directory on success, the manager
/// error on stderr otherwise. An imported model has no download source.
fn install(manager: &ModelManager, id: &str) -> i32 {
    let Some(spec) = manager.find_spec(id) else {
        eprintln!("unknown model id: {id}");
        return 1;
    };
    if spec.url.is_empty() {
        eprintln!(
            "model {id} was imported and has no download source; import its file again with `voiceless model import <file>`"
        );
        return 1;
    }
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

/// Import `file` through the same code as the console: prints the id of
/// the imported model on stdout and how to select it on stderr; the
/// manager error on stderr otherwise. The file name is what the model is
/// listed under.
fn import(manager: &ModelManager, file: &Path, json: bool) -> i32 {
    let name = file
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let source = match fs::File::open(file) {
        Ok(source) => source,
        Err(error) => {
            eprintln!("cannot open {}: {error}", file.display());
            return 1;
        }
    };
    let bytes = match source.metadata() {
        Ok(metadata) => metadata.len(),
        Err(error) => {
            eprintln!("cannot read {}: {error}", file.display());
            return 1;
        }
    };
    let mut source = BufReader::new(source);
    match manager.import_from(&mut source, name, bytes) {
        Ok(model) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "id": model.spec.id,
                        "name": model.name,
                        "family": model.family,
                        "languages": model.languages,
                        "size_bytes": model.spec.size_bytes,
                    })
                );
            } else {
                println!("{}", model.spec.id);
            }
            eprintln!(
                "imported {} ({}); set [local] model = \"{}\" to use it",
                model.spec.id, model.name, model.spec.id
            );
            0
        }
        Err(error) => {
            eprintln!("model import failed: {error}");
            1
        }
    }
}

/// Check `id`: the payload path always goes to stdout once the pointer
/// resolves; a clean report prints "model `<id>` is valid" and returns 0, a
/// dirty one lists every finding on stderr and returns 1. A missing or
/// malformed pointer is reported as "model check failed".
fn check(manager: &ModelManager, id: &str) -> i32 {
    let Some(spec) = manager.find_spec(id) else {
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

/// Remove `id`. Unlike the other commands this does not look the id up in
/// the catalog or the imports, so leftovers of a model that is known
/// nowhere can still be deleted; "not installed" is an error (exit 1), and
/// so is a model a running engine or session holds.
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
