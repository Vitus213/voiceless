//! `voiceless history` subcommands backed by the SQLite store.

use std::path::PathBuf;

use crate::history::HistoryStore;
use crate::models::default_data_dir;

pub struct HistoryArgs {
    pub ids: Vec<String>,
    pub all: bool,
    pub limit: i64,
    pub cursor: Option<String>,
    pub destination: Option<PathBuf>,
    pub days: i64,
}

pub fn run(action: &str, args: HistoryArgs) -> i32 {
    let store = match HistoryStore::open(default_data_dir().join("history.sqlite3")) {
        Ok(store) => store,
        Err(error) => {
            eprintln!("Voiceless: {error}");
            return 1;
        }
    };
    let value = match action {
        "list" => match store.query(args.limit, args.cursor.as_deref()) {
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
            match store.export_csv(destination) {
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
