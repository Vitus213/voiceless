//! Text processing modes: builtin catalog plus a JSON-persisted repository
//! compatible with the Python `modes.json` layout.
//!
//! A mode decides what happens to the final transcript between `[text]`
//! cleanup and injection. `quick` types the text as is; every other mode is
//! a prompt template (`{text}` = the cleaned transcript) sent to the
//! `[processing]` chat provider. The builtin catalog is `quick`,
//! `voice-polish`, `prompt-optimize` and `translate-en`; users add their
//! own through `voiceless mode add|update|remove` (`mode_cmd.rs`) or the
//! console's `/api/modes` handlers (`console/settings.rs`).
//!
//! Who reads it: `stream::build_pipeline` opens the repository once per
//! session, resolves the requested mode (`stream --mode`, `ctl start
//! --mode`, the daemon's default mode, the console picker), builds the
//! `prompt_table` and expands the chosen template with `render_template`;
//! `coordinator` skips processing for `quick` by id. The mode id is stored
//! in history as `processing_mode`. `[processing].prompt` overrides the
//! prompt-optimize template at table-build time without touching the file.
//!
//! File: `<config>/voiceless/modes.json` (`stream::modes_path`,
//! `models::default_config_dir`), a JSON array of `Mode` objects, pretty
//! printed, written atomically through a `modes.tmp` sibling and rename.
//! A missing file is served from the builtins in memory and only created
//! on the first mutation. Unknown keys and a file lacking any builtin id
//! are read errors; nothing is repaired silently.
//!
//! Invariants: the four builtins are always present and immutable; ids are
//! unique; names are unique after trimming, case-insensitively; the list
//! is kept sorted by `sort_order`, then name, then id. No file locking:
//! CLI and console writers can race, last write wins.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One processing mode as stored in `modes.json` and returned by the CLI
/// and `/api/modes`. `deny_unknown_fields` makes a file with extra keys a
/// read error, in line with the strict config contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mode {
    /// Fixed ids for builtins (`quick`, ...), a hyphenated UUID v4 for user
    /// modes. The value of `--mode`, of the console `mode` field and of the
    /// history `processing_mode` column.
    pub id: String,
    /// Display name; also accepted by `resolve` (case-insensitive, trimmed).
    /// Unique across the repository.
    pub name: String,
    /// Prompt template; `{text}` is replaced by the transcript. Empty for
    /// `quick`. Note that only `quick` bypasses the provider: a user mode
    /// with an empty prompt still sends an (empty) prompt.
    pub prompt: String,
    /// Short status label shown while the provider works ("润色中").
    /// Stored and echoed by CLI/console; the Rust pipeline itself does not
    /// display it.
    pub processing_label: String,
    /// `true` for the catalog modes; `update` and `remove` refuse them.
    pub builtin: bool,
    /// Primary list order. Builtins use 0..3; a new user mode gets the
    /// current maximum plus one.
    pub sort_order: i64,
}

/// Failures of the modes repository; `Display` strings are user-facing.
#[derive(Debug, thiserror::Error)]
pub enum ModesError {
    /// No mode with that id (or, for `resolve`, that id or name).
    #[error("mode not found: {0}")]
    NotFound(String),
    /// `update`/`remove` on a mode with `builtin = true`.
    #[error("builtin mode cannot be modified: {0}")]
    BuiltinImmutable(String),
    /// Another mode already has this name (trimmed, case-insensitive).
    #[error("mode name already exists: {0}")]
    DuplicateName(String),
    /// The name was empty or whitespace only.
    #[error("mode name must not be empty")]
    EmptyName,
    /// `modes.json` cannot be read, parsed or fails validation (duplicate
    /// or empty id, missing builtin). First field is the path.
    #[error("cannot read modes file {0}: {1}")]
    Read(String, String),
    /// `modes.json` cannot be serialized or written (directory creation,
    /// temp file, rename). First field is the path.
    #[error("cannot write modes file {0}: {1}")]
    Write(String, String),
}

/// The four catalog modes in `sort_order` 0..3. `quick` has an empty
/// prompt and never calls a provider; the other prompts are Chinese
/// instructions that ask for the result only, no explanation, with the
/// transcript substituted for `{text}`.
pub fn builtin_modes() -> Vec<Mode> {
    vec![
        Mode {
            id: "quick".to_owned(),
            name: "快速输入".to_owned(),
            prompt: String::new(),
            processing_label: "处理中".to_owned(),
            builtin: true,
            sort_order: 0,
        },
        Mode {
            id: "voice-polish".to_owned(),
            name: "语音润色".to_owned(),
            prompt: "在不改变原意、不编造事实的前提下，删除口头语并修正语病，输出简体中文；英文与代码原样保留，数字一律使用阿拉伯数字。只输出处理后的文本，不要解释。原文：{text}".to_owned(),
            processing_label: "润色中".to_owned(),
            builtin: true,
            sort_order: 1,
        },
        Mode {
            id: "prompt-optimize".to_owned(),
            name: "提示词优化".to_owned(),
            prompt: "将以下需求改写为清晰、可执行的提示词。保留所有事实和约束；只输出提示词，不要解释。原文：{text}".to_owned(),
            processing_label: "优化中".to_owned(),
            builtin: true,
            sort_order: 2,
        },
        Mode {
            id: "translate-en".to_owned(),
            name: "翻译为英文".to_owned(),
            prompt: "将以下文本准确翻译为自然英文。只输出译文，不要解释。原文：{text}".to_owned(),
            processing_label: "翻译中".to_owned(),
            builtin: true,
            sort_order: 3,
        },
    ]
}

/// One-pass template expansion; only known placeholders are substituted and
/// inserted content is never re-parsed.
///
/// Placeholders: `{text}`, `{selected}`, `{clipboard}`. Unknown
/// placeholders (`{unknown}`) and a `{` without a closing `}` are copied
/// verbatim, so a prompt can contain literal braces. The current pipeline
/// passes empty strings for `selected` and `clipboard` (kept for the
/// Python template format), so those expand to nothing. Single pass means
/// a transcript containing `{clipboard}` is inserted literally, never
/// expanded: user speech cannot inject template directives.
pub fn render_template(template: &str, text: &str, selected: &str, clipboard: &str) -> String {
    let mut output = String::with_capacity(template.len());
    let mut cursor = 0;
    let bytes = template.as_bytes();
    while cursor < bytes.len() {
        let Some(opening) = template[cursor..].find('{') else {
            output.push_str(&template[cursor..]);
            break;
        };
        let opening = cursor + opening;
        output.push_str(&template[cursor..opening]);
        // Unterminated brace: the rest of the template is literal.
        let Some(closing) = template[opening + 1..].find('}') else {
            output.push_str(&template[opening..]);
            break;
        };
        let closing = opening + 1 + closing;
        let field = &template[opening + 1..closing];
        match field {
            "text" => output.push_str(text),
            "selected" => output.push_str(selected),
            "clipboard" => output.push_str(clipboard),
            _ => output.push_str(&template[opening..closing + 1]),
        }
        cursor = closing + 1;
    }
    output
}

/// Mode id of the builtin prompt-optimize mode; `[processing].prompt`
/// overrides its template.
pub const PROMPT_OPTIMIZE_ID: &str = "prompt-optimize";

/// The builtin prompt-optimize template (the type4me design). This is the
/// effective organize prompt when `[processing].prompt` is empty; the
/// console's `GET /api/prompt` returns it as the default.
pub fn builtin_prompt_optimize_prompt() -> String {
    builtin_modes()
        .into_iter()
        .find(|mode| mode.id == PROMPT_OPTIMIZE_ID)
        .map(|mode| mode.prompt)
        .unwrap_or_default()
}

/// Build the (mode id, prompt template) table the pipeline consults.
/// A non-empty `organize_prompt` replaces the builtin prompt-optimize
/// template, keeping the dictation-organizing prompt editable without a
/// rebuild.
///
/// Whitespace-only `organize_prompt` counts as empty. Every other mode
/// keeps its stored prompt; the override is applied at build time and is
/// never written back to `modes.json`. Called once per session by
/// `stream::build_pipeline` with `[processing].prompt`.
pub fn prompt_table(modes: &[Mode], organize_prompt: &str) -> Vec<(String, String)> {
    modes
        .iter()
        .map(|mode| {
            let prompt = if mode.id == PROMPT_OPTIMIZE_ID && !organize_prompt.trim().is_empty() {
                organize_prompt.to_owned()
            } else {
                mode.prompt.clone()
            };
            (mode.id.clone(), prompt)
        })
        .collect()
}

/// In-memory copy of `modes.json` bound to its path. Reads serve from
/// memory; every mutation validates, sorts, rewrites the whole file and
/// updates memory, so the struct and the file agree after each call. Not
/// shared between processes: open a fresh repository (or `reload`) to see
/// another writer's changes.
pub struct ModesRepository {
    /// Location of `modes.json`; may not exist yet.
    path: PathBuf,
    /// Always sorted (see `sort_modes`) and validated.
    modes: Vec<Mode>,
}

impl ModesRepository {
    /// Open the repository. A missing file loads the builtin modes into
    /// memory without touching disk; the file is only created on the first
    /// mutation so read-only commands work anywhere.
    ///
    /// An existing file that is unreadable, not a JSON array of `Mode`,
    /// has unknown keys, duplicate or empty ids, or lacks a builtin id is a
    /// `Read` error; the repository never repairs or overwrites it.
    pub fn open(path: PathBuf) -> Result<Self, ModesError> {
        let modes = if path.exists() {
            Self::load(&path)?
        } else {
            sort_modes(builtin_modes())
        };
        Ok(Self { path, modes })
    }

    /// Read, parse, validate and sort the file at `path`.
    fn load(path: &Path) -> Result<Vec<Mode>, ModesError> {
        let raw = fs::read_to_string(path)
            .map_err(|error| ModesError::Read(path.display().to_string(), error.to_string()))?;
        let modes: Vec<Mode> = serde_json::from_str(&raw)
            .map_err(|error| ModesError::Read(path.display().to_string(), error.to_string()))?;
        validate(&modes)
            .map_err(|message| ModesError::Read(path.display().to_string(), message))?;
        Ok(sort_modes(modes))
    }

    /// The `modes.json` path this repository reads and writes.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// All modes in display order (sorted by `sort_order`, name, id).
    pub fn list(&self) -> &[Mode] {
        &self.modes
    }

    /// Re-read the file from disk (`voiceless mode reload`, picking up
    /// edits by another process). On error the in-memory list is left
    /// unchanged, so a later mutation can still persist a good file. A
    /// missing file is a `Read` error here, unlike `open`.
    pub fn reload(&mut self) -> Result<&[Mode], ModesError> {
        self.modes = Self::load(&self.path)?;
        Ok(&self.modes)
    }

    /// Look up a mode by exact id.
    pub fn get(&self, mode_id: &str) -> Result<&Mode, ModesError> {
        self.modes
            .iter()
            .find(|mode| mode.id == mode_id)
            .ok_or_else(|| ModesError::NotFound(mode_id.to_owned()))
    }

    /// Resolve by id first, then by case-insensitive name; empty input maps
    /// to quick mode.
    ///
    /// This is what `--mode` and the console `mode` field go through, so a
    /// user can type either `translate-en` or `翻译为英文`. The name match
    /// trims and lower-cases both sides; the id match is exact.
    pub fn resolve(&self, identifier: Option<&str>) -> Result<&Mode, ModesError> {
        match identifier {
            None => self.get("quick"),
            Some(identifier) if identifier.trim().is_empty() => self.get("quick"),
            Some(identifier) => {
                if let Some(mode) = self.modes.iter().find(|mode| mode.id == identifier) {
                    return Ok(mode);
                }
                let key = normalized_name(identifier);
                self.modes
                    .iter()
                    .find(|mode| normalized_name(&mode.name) == key)
                    .ok_or_else(|| ModesError::NotFound(identifier.to_owned()))
            }
        }
    }

    /// Create a user mode and persist the file. The name is trimmed and
    /// must be non-empty and unique; `prompt` and `processing_label` are
    /// stored verbatim (an empty prompt is allowed). The new mode gets a
    /// fresh UUID id, `builtin = false` and the next `sort_order`, and is
    /// returned as stored. Errors: `EmptyName`, `DuplicateName`, `Write`.
    pub fn add(
        &mut self,
        name: &str,
        prompt: &str,
        processing_label: &str,
    ) -> Result<Mode, ModesError> {
        let name = clean_name(name)?;
        self.ensure_unique_name(&name, None)?;
        let sort_order = self.next_sort_order();
        let mode = Mode {
            id: new_mode_id(),
            name,
            prompt: prompt.to_owned(),
            processing_label: processing_label.to_owned(),
            builtin: false,
            sort_order,
        };
        self.modes.push(mode.clone());
        self.persist()?;
        Ok(mode)
    }

    /// Change any of name, prompt and label of a user mode; `None` leaves a
    /// field untouched. The id and `sort_order` never change. Builtins are
    /// refused with `BuiltinImmutable` before anything is modified; a new
    /// name goes through the same trimming and uniqueness rules as `add`
    /// (the mode's own current name does not count as a duplicate). The
    /// file is rewritten even when every argument is `None`. Returns the
    /// updated mode.
    pub fn update(
        &mut self,
        mode_id: &str,
        name: Option<&str>,
        prompt: Option<&str>,
        processing_label: Option<&str>,
    ) -> Result<Mode, ModesError> {
        let index = self
            .modes
            .iter()
            .position(|mode| mode.id == mode_id)
            .ok_or_else(|| ModesError::NotFound(mode_id.to_owned()))?;
        if self.modes[index].builtin {
            return Err(ModesError::BuiltinImmutable(mode_id.to_owned()));
        }
        if let Some(name) = name {
            let cleaned = clean_name(name)?;
            self.ensure_unique_name(&cleaned, Some(mode_id))?;
            self.modes[index].name = cleaned;
        }
        if let Some(prompt) = prompt {
            self.modes[index].prompt = prompt.to_owned();
        }
        if let Some(label) = processing_label {
            self.modes[index].processing_label = label.to_owned();
        }
        let mode = self.modes[index].clone();
        self.persist()?;
        Ok(mode)
    }

    /// Delete a user mode and persist the file; returns the removed mode.
    /// Builtins are refused with `BuiltinImmutable`. Sessions already
    /// running with the mode are unaffected (they hold their own copy of
    /// the prompt table).
    pub fn remove(&mut self, mode_id: &str) -> Result<Mode, ModesError> {
        let index = self
            .modes
            .iter()
            .position(|mode| mode.id == mode_id)
            .ok_or_else(|| ModesError::NotFound(mode_id.to_owned()))?;
        if self.modes[index].builtin {
            return Err(ModesError::BuiltinImmutable(mode_id.to_owned()));
        }
        let mode = self.modes.remove(index);
        self.persist()?;
        Ok(mode)
    }

    /// Sort, validate and write the in-memory list. Validation before the
    /// write guarantees the file on disk is always loadable by `open`.
    fn persist(&mut self) -> Result<(), ModesError> {
        self.modes = sort_modes(std::mem::take(&mut self.modes));
        validate(&self.modes)
            .map_err(|message| ModesError::Write(self.path.display().to_string(), message))?;
        store(&self.path, &self.modes)
    }

    /// Highest existing `sort_order` plus one (0 for an empty list), so new
    /// modes append to the end of the display order.
    fn next_sort_order(&self) -> i64 {
        self.modes
            .iter()
            .map(|mode| mode.sort_order)
            .max()
            .unwrap_or(-1)
            + 1
    }

    /// Reject `name` if another mode (ignoring the id in `excluding`)
    /// already uses it after trim + lower-case normalisation.
    fn ensure_unique_name(&self, name: &str, excluding: Option<&str>) -> Result<(), ModesError> {
        let key = normalized_name(name);
        if self
            .modes
            .iter()
            .any(|mode| Some(mode.id.as_str()) != excluding && normalized_name(&mode.name) == key)
        {
            return Err(ModesError::DuplicateName(name.to_owned()));
        }
        Ok(())
    }
}

/// Write the list as pretty JSON (trailing newline) atomically: the
/// content goes to a `<stem>.tmp` sibling (`modes.tmp`) and is renamed
/// over the target, so a crash mid-write leaves the old file intact and a
/// concurrent reader never sees a truncated one. Creates the parent
/// directory (`~/.config/voiceless`) on first use.
fn store(path: &Path, modes: &[Mode]) -> Result<(), ModesError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| ModesError::Write(path.display().to_string(), error.to_string()))?;
    }
    let content = serde_json::to_string_pretty(modes)
        .map_err(|error| ModesError::Write(path.display().to_string(), error.to_string()))?
        + "\n";
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, content)
        .map_err(|error| ModesError::Write(path.display().to_string(), error.to_string()))?;
    fs::rename(&temporary, path)
        .map_err(|error| ModesError::Write(path.display().to_string(), error.to_string()))?;
    Ok(())
}

/// Display order: `sort_order`, then name, then id (a total order, so the
/// file and the listing are deterministic).
fn sort_modes(mut modes: Vec<Mode>) -> Vec<Mode> {
    modes.sort_by(|left, right| {
        left.sort_order
            .cmp(&right.sort_order)
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.id.cmp(&right.id))
    });
    modes
}

/// Structural rules shared by load and persist: non-empty unique ids and
/// every builtin id present. Names are not checked here (uniqueness is
/// enforced at mutation time only, so a hand-edited file with duplicate
/// names still loads).
fn validate(modes: &[Mode]) -> Result<(), String> {
    let mut ids = std::collections::HashSet::new();
    for mode in modes {
        if mode.id.trim().is_empty() {
            return Err("mode id must not be empty".to_owned());
        }
        if !ids.insert(mode.id.clone()) {
            return Err(format!("duplicate mode id: {}", mode.id));
        }
    }
    for builtin in builtin_modes() {
        if !modes.iter().any(|mode| mode.id == builtin.id) {
            return Err(format!("missing builtin mode: {}", builtin.id));
        }
    }
    Ok(())
}

/// Trimmed name, or `EmptyName` when nothing is left.
fn clean_name(name: &str) -> Result<String, ModesError> {
    let cleaned = name.trim();
    if cleaned.is_empty() {
        return Err(ModesError::EmptyName);
    }
    Ok(cleaned.to_owned())
}

/// Comparison key for names: trimmed and Unicode lower-cased.
fn normalized_name(value: &str) -> String {
    value.trim().to_lowercase()
}

/// UUID-v4 style identifier sourced from the OS CSPRNG, formatted with
/// hyphens for compatibility with the Python repository.
fn new_mode_id() -> String {
    let mut bytes = [0_u8; 16];
    getrandom::getrandom(&mut bytes).expect("OS random source available");
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}
