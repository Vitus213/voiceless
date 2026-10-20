//! `voiceless mode` subcommands backed by the modes repository.

use crate::models::default_config_dir;
use crate::modes::ModesRepository;

pub fn run(action: &str, args: ModeArgs) -> i32 {
    let mut repository = match ModesRepository::open(default_config_dir().join("modes.json")) {
        Ok(repository) => repository,
        Err(error) => {
            eprintln!("Voiceless: {error}");
            return 1;
        }
    };
    let value = match action {
        "list" => serde_json::to_value(repository.list()).expect("serialize modes"),
        "reload" => match repository.reload() {
            Ok(modes) => serde_json::to_value(modes).expect("serialize modes"),
            Err(error) => {
                eprintln!("Voiceless: {error}");
                return 1;
            }
        },
        "add" => {
            let Some(name) = args.name.as_deref() else {
                eprintln!("Voiceless: mode add requires --name");
                return 1;
            };
            match repository.add(
                name,
                args.prompt.as_deref().unwrap_or_default(),
                args.processing_label.as_deref().unwrap_or_default(),
            ) {
                Ok(mode) => serde_json::to_value(&mode).expect("serialize mode"),
                Err(error) => {
                    eprintln!("Voiceless: {error}");
                    return 1;
                }
            }
        }
        "update" => {
            let Some(id) = args.id.as_deref() else {
                eprintln!("Voiceless: mode update requires --id");
                return 1;
            };
            match repository.update(
                id,
                args.name.as_deref(),
                args.prompt.as_deref(),
                args.processing_label.as_deref(),
            ) {
                Ok(mode) => serde_json::to_value(&mode).expect("serialize mode"),
                Err(error) => {
                    eprintln!("Voiceless: {error}");
                    return 1;
                }
            }
        }
        "remove" => {
            let Some(id) = args.id.as_deref() else {
                eprintln!("Voiceless: mode remove requires --id");
                return 1;
            };
            match repository.remove(id) {
                Ok(mode) => serde_json::to_value(&mode).expect("serialize mode"),
                Err(error) => {
                    eprintln!("Voiceless: {error}");
                    return 1;
                }
            }
        }
        other => {
            eprintln!("Voiceless: unknown mode action: {other}");
            return 1;
        }
    };
    println!("{}", serde_json::to_string(&value).expect("serialize JSON"));
    0
}

pub struct ModeArgs {
    pub id: Option<String>,
    pub name: Option<String>,
    pub prompt: Option<String>,
    pub processing_label: Option<String>,
}
