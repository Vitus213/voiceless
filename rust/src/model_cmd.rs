//! `voiceless model` subcommands backed by the integrity-checked manager.

use crate::models::{default_cache_dir, default_data_dir, HttpDownloader, ModelManager};

#[derive(Debug, Clone)]
pub enum ModelCommand {
    List,
    Install { id: String },
    Check { id: String },
    Remove { id: String },
}

pub fn run(command: ModelCommand, json: bool) -> i32 {
    let manager = ModelManager::new(&default_data_dir(), &default_cache_dir());
    match command {
        ModelCommand::List => list(json),
        ModelCommand::Install { id } => install(&manager, &id),
        ModelCommand::Check { id } => check(&manager, &id),
        ModelCommand::Remove { id } => remove(&manager, &id),
    }
}

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

fn find_spec(id: &str) -> Option<crate::models::ModelSpec> {
    crate::models::catalog()
        .into_iter()
        .find(|spec| spec.id == id)
}
