//! `voiceless mode` subcommands backed by the modes repository.
//!
//! Entry point for `mode list|reload|add|update|remove`, called from
//! `main.rs` after clap parsing. The repository file is
//! `<XDG config dir>/voiceless/modes.json` (`models::default_config_dir`,
//! the same path `stream::modes_path` resolves for sessions). A missing
//! file yields the builtin modes (`quick`, `voice-polish`,
//! `prompt-optimize`, `translate-en`) without creating it; the first
//! `add`/`update`/`remove` writes it. Builtin modes cannot be updated or
//! removed (`ModesError::BuiltinImmutable`) and names must be unique
//! (`modes::ModesRepository` enforces both). Sessions open the repository
//! themselves, so a change made here is picked up by the next session.
//!
//! Output contract: one JSON document on stdout and exit code 0 on success
//! (`list` and `reload` print the whole `[Mode]` array, the other actions
//! the affected `Mode`); on failure one `Voiceless: …` line on stderr and
//! exit code 1.

use crate::models::default_config_dir;
use crate::modes::ModesRepository;

/// Run one action and return the process exit code. `add` needs `--name`
/// (prompt and label default to empty); `update` and `remove` need `--id`
/// and `update` changes only the flags given. Blocking file I/O.
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

/// Flags shared by every mode action (clap declares them global).
pub struct ModeArgs {
    /// `--id`: the mode `update`/`remove` target (builtin id or user UUID).
    pub id: Option<String>,
    /// `--name`: display name, required by `add`, optional new name for
    /// `update`; must be unique in the repository.
    pub name: Option<String>,
    /// `--prompt`: template sent to the `[processing]` provider; `{text}`
    /// is replaced by the cleaned transcript (`modes::render_template`).
    pub prompt: Option<String>,
    /// `--processing-label`: short status label stored with the mode and
    /// echoed by the CLI and console; the pipeline does not display it.
    pub processing_label: Option<String>,
}
