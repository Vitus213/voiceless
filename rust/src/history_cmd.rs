//! `voiceless history` subcommands backed by the SQLite store.
//!
//! Entry point for `history list|delete|export|totals|usage`, called from
//! `main.rs` after clap parsing. `history serve` is dispatched there
//! straight to `console::serve_standalone` and never reaches this module.
//! The store is `<XDG data dir>/voiceless/history.sqlite3`
//! (`models::default_data_dir`); opening it creates or migrates the file,
//! so even a read-only command needs a writable data directory.
//!
//! Output contract: exactly one JSON document on stdout and exit code 0 on
//! success; on any failure one `Voiceless: …` line on stderr, nothing on
//! stdout and exit code 1. Shapes: `list` is `{"records": [HistoryRecord],
//! "next_cursor"}` (the console's `/api/records` shape) over the rows the
//! filter flags select (`--search`, `--from`, `--to`, `--mode`, `--app`,
//! `--model`, `--audio`, what the console's search takes); `delete` is
//! `{"deleted": n}`; `export` is `{"exported": n, "path"}`; `totals` is
//! `HistoryTotals`; `usage` is the `HistoryStore::usage` object.

use std::path::PathBuf;

use crate::history::{HistoryFilter, HistoryStore};
use crate::models::default_data_dir;

/// Flags shared by every history action (clap declares them global; each
/// action reads only the ones it needs).
pub struct HistoryArgs {
    /// `--ids <id>` (repeatable): rows `delete` targets.
    pub ids: Vec<String>,
    /// `--all`: `delete` every row and recording instead of `--ids`.
    pub all: bool,
    /// `--limit`: page size of `list` (default 50; the store accepts
    /// 1..=1000).
    pub limit: i64,
    /// `--cursor`: `next_cursor` of a previous `list` page.
    pub cursor: Option<String>,
    /// `--destination`: CSV path, required by `export`.
    pub destination: Option<PathBuf>,
    /// `--days`: window of `usage` (default 7; 1, 7 or 30).
    pub days: i64,
}

/// The rows `list` and `export` select, as the command line spells them
/// (clap declares them global; the other actions ignore them). Every flag
/// is optional and blank means "any"; `history::HistoryFilter` says what
/// the values mean. `main.rs` hands them to `run_with`.
#[derive(Debug, Clone, Default)]
pub struct HistoryFlags {
    /// `--search <text>`: the raw or final text contains it.
    pub search: Option<String>,
    /// `--from <time>`: created at or after this RFC 3339 time.
    pub from: Option<String>,
    /// `--to <time>`: created before this RFC 3339 time.
    pub to: Option<String>,
    /// `--mode <id>`: this processing mode.
    pub mode: Option<String>,
    /// `--app <id>`: dictated into this application.
    pub app: Option<String>,
    /// `--model <id>`: recognized with this model.
    pub model: Option<String>,
    /// `--audio <with|without>`: keeps a saved recording, or none.
    pub audio: Option<String>,
}

impl HistoryFlags {
    /// The same filter as the store checks it.
    fn filter(&self) -> HistoryFilter {
        HistoryFilter {
            text: self.search.clone().unwrap_or_default(),
            from: self.from.clone(),
            to: self.to.clone(),
            mode: self.mode.clone(),
            app: self.app.clone(),
            model: self.model.clone(),
            audio: self.audio.clone(),
        }
    }
}

/// Run one action and return the process exit code, `list` and `export`
/// over the rows `flags` selects. Blocking (SQLite and file I/O).
/// `delete` with neither `--all` nor `--ids` deletes nothing and prints
/// `{"deleted": 0}`; an unknown `action` is a usage error and a filter
/// value the store rejects fails with its message (both exit 1).
pub fn run_with(action: &str, args: HistoryArgs, flags: HistoryFlags) -> i32 {
    let store = match HistoryStore::open(default_data_dir().join("history.sqlite3")) {
        Ok(store) => store,
        Err(error) => {
            eprintln!("Voiceless: {error}");
            return 1;
        }
    };
    let value = match action {
        "list" => match store.search(&flags.filter(), args.limit, args.cursor.as_deref()) {
            Ok(page) => serde_json::json!({
                "records": page.records,
                "next_cursor": page.next_cursor,
            }),
            Err(error) => {
                eprintln!("Voiceless: {error}");
                return 1;
            }
        },
        "delete" => {
            let deleted = if args.all {
                store.delete_all()
            } else {
                store.delete(&args.ids)
            };
            match deleted {
                Ok(count) => serde_json::json!({ "deleted": count }),
                Err(error) => {
                    eprintln!("Voiceless: {error}");
                    return 1;
                }
            }
        }
        "export" => {
            let Some(destination) = args.destination.as_ref() else {
                eprintln!("Voiceless: history export requires --destination");
                return 1;
            };
            match store.export_csv(destination, &flags.filter()) {
                Ok(count) => {
                    serde_json::json!({ "exported": count, "path": destination.display().to_string() })
                }
                Err(error) => {
                    eprintln!("Voiceless: {error}");
                    return 1;
                }
            }
        }
        "totals" => match store.totals() {
            Ok(totals) => serde_json::to_value(&totals).expect("serialize totals"),
            Err(error) => {
                eprintln!("Voiceless: {error}");
                return 1;
            }
        },
        "usage" => match store.usage(args.days) {
            Ok(summary) => summary,
            Err(error) => {
                eprintln!("Voiceless: {error}");
                return 1;
            }
        },
        other => {
            eprintln!("Voiceless: unknown history action: {other}");
            return 1;
        }
    };
    println!("{}", serde_json::to_string(&value).expect("serialize JSON"));
    0
}
