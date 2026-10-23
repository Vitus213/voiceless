//! The `voiceless` binary: a clap front end over the `voiceless` library
//! crate (see `src/lib.rs` for the pipeline and module map).
//!
//! Every subcommand maps to one library module that returns the process
//! exit code, which `main` hands to `std::process::exit`:
//!
//! - `0`: success (for `doctor`: every required check passed).
//! - `1`: failure (configuration, I/O, backend or injection error, a failed
//!   quality gate, a required `doctor` check missing).
//! - `2`: `benchmark asr|latency` skipped because credentials, corpus or
//!   audio were missing; a skip is never reported as a pass.
//! - `130`: a `stream` session cancelled by the second Ctrl-C or SIGTERM.
//!
//! `stream` is special-cased: when the configured streaming backend needs a
//! local model (`local-offline`, `local-streaming`) and a running daemon owns
//! `dev.voiceless.Daemon` on the session bus, the session runs inside the
//! daemon (which already holds the model; a second copy would not fit in
//! VRAM) and this process only relays events and Ctrl-C through
//! `ctl::stream_via_daemon`.
//!
//! `///` comments on the clap items below are their `--help` text: keep each
//! to one short sentence. Doc comments are deliberately absent from the
//! `Subcommand` enums themselves, because clap would apply an enum-level doc
//! comment as the `about` of the parent command.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// Top-level command line: one global `--config` option and one subcommand.
#[derive(Debug, Parser)]
#[command(
    name = "voiceless",
    version,
    about = "Fast realtime voice input for Linux"
)]
struct Cli {
    /// Config file for stream and daemon (default: ~/.config/voiceless/config.toml).
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// The subcommand to run.
    #[command(subcommand)]
    command: Command,
}

// Variant docs are the `about` lines in `voiceless --help`; keep them in
// step with the command table in README.md.
#[derive(Debug, Subcommand)]
enum Command {
    /// Record and transcribe in realtime, then type the final text.
    Stream(StreamArgs),
    /// Transcribe one WAV file with the batch backend.
    Transcribe(TranscribeArgs),
    /// Record for N seconds, then transcribe.
    Record(RecordArgs),
    /// Manage the model catalog: list, install, check, remove.
    Model(ModelArgs),
    /// Check dependencies and the data directory.
    Doctor,
    /// Manage text processing modes.
    Mode(ModeArgs),
    /// Query the recognition history or serve the web console.
    History(HistoryArgs),
    /// Run the resident daemon (warm engine, D-Bus control, hotkey, console).
    Daemon(DaemonArgs),
    /// Control the resident daemon (one D-Bus call; for hotkey bindings).
    Ctl(CtlArgs),
    /// Run the CER and stop-to-inject latency gates.
    Benchmark(BenchmarkArgs),
}

/// Arguments of `voiceless ctl`.
#[derive(Debug, Args)]
struct CtlArgs {
    /// Which daemon call to make.
    #[command(subcommand)]
    action: CtlCommand,
}

// One variant per `dev.voiceless.Daemon.Controller` method (`Watch`
// subscribes to the `Event` signal instead); see `ctl::CtlAction`.
#[derive(Debug, Subcommand)]
enum CtlCommand {
    /// Start a session when idle, stop it when recording.
    Toggle {
        /// Processing mode; empty uses the daemon default.
        #[arg(long, default_value = "")]
        mode: String,
    },
    /// Start a session (hold-to-talk press).
    Start {
        /// Processing mode; empty uses the daemon default.
        #[arg(long, default_value = "")]
        mode: String,
    },
    /// Stop and finalize the session (hold-to-talk release).
    Stop,
    /// Cancel the session without typing anything.
    Cancel,
    /// Print idle, recording or stopping.
    Status,
    /// Print every daemon event as a JSON line.
    Watch,
}

/// Arguments of `voiceless benchmark`.
#[derive(Debug, Args)]
struct BenchmarkArgs {
    /// Which gate to run.
    #[command(subcommand)]
    action: BenchmarkAction,
}

// Both gates exit 2 ("skipped, unverified") without credentials, corpus or
// audio, so a missing environment can never look like a pass.
#[derive(Debug, Subcommand)]
enum BenchmarkAction {
    /// Replay the corpus through a realtime backend and report the CER.
    Asr {
        /// Corpus split to replay (the `<split>.jsonl` corpus file).
        #[arg(long, default_value = "test")]
        split: String,
        /// Streaming backend to benchmark.
        #[arg(long, default_value = "cloud-realtime")]
        backend: String,
        /// Exit 1 when the cloud-realtime CER exceeds 0.02.
        #[arg(long)]
        enforce: bool,
    },
    /// Run cloud sessions and measure stop-to-inject latency.
    Latency {
        /// Number of trials to run.
        #[arg(long, default_value_t = 100)]
        trials: usize,
        /// Inject each final text with the configured [inject] method; without it the stop-to-inject gate is unverified.
        #[arg(long)]
        inject: bool,
        /// Exit 1 when a latency budget is exceeded (otherwise report only).
        #[arg(long)]
        enforce: bool,
    },
}

/// Arguments of `voiceless stream`.
#[derive(Debug, Args)]
struct StreamArgs {
    /// Streaming backend for this run (overrides [asr] streaming_backend).
    #[arg(long)]
    backend: Option<String>,
    /// Print every session event as a JSON line.
    #[arg(long)]
    json: bool,
    /// Recognize without typing the result.
    #[arg(long)]
    no_inject: bool,
    /// Processing mode (quick types the text as recognized).
    #[arg(long, default_value = "quick")]
    mode: String,
}

/// Arguments of `voiceless transcribe`.
#[derive(Debug, Args)]
struct TranscribeArgs {
    /// WAV file to transcribe.
    wav: PathBuf,
    /// Batch backend for this run (overrides [asr] batch_backend).
    #[arg(long)]
    backend: Option<String>,
    /// Type the result into the focused window.
    #[arg(long)]
    inject: bool,
    /// Print the result as JSON.
    #[arg(long)]
    json: bool,
}

/// Arguments of `voiceless record`.
#[derive(Debug, Args)]
struct RecordArgs {
    /// Recording length in seconds.
    #[arg(long, default_value_t = 5.0)]
    seconds: f64,
    /// Batch backend for this run (overrides [asr] batch_backend).
    #[arg(long)]
    backend: Option<String>,
    /// Recognize without typing the result.
    #[arg(long)]
    no_inject: bool,
    /// Print the result as JSON.
    #[arg(long)]
    json: bool,
}

/// Arguments of `voiceless model`.
#[derive(Debug, Args)]
struct ModelArgs {
    /// Catalog operation.
    #[command(subcommand)]
    action: ModelAction,
    /// Print the model list as JSON lines.
    #[arg(long, global = true)]
    json: bool,
}

// Mapped one-to-one onto `model_cmd::ModelCommand`.
#[derive(Debug, Subcommand)]
enum ModelAction {
    /// List catalog entries with their install state and supply-chain fields.
    List,
    /// Download, verify and activate a catalog model.
    Install {
        /// Catalog id (see model list).
        id: String,
    },
    /// Verify an installed model against the catalog.
    Check {
        /// Catalog id (see model list).
        id: String,
    },
    /// Remove an installed model.
    Remove {
        /// Catalog id (see model list).
        id: String,
    },
}

/// Arguments of `voiceless mode`. The options are global so they may follow
/// the action (`mode add --name ...`).
#[derive(Debug, Args)]
struct ModeArgs {
    /// Mode operation.
    #[command(subcommand)]
    action: ModeAction,
    /// Mode id (required by update and remove).
    #[arg(long, global = true)]
    id: Option<String>,
    /// Display name (required by add).
    #[arg(long, global = true)]
    name: Option<String>,
    /// Prompt template; {text} is replaced by the transcript.
    #[arg(long, global = true)]
    prompt: Option<String>,
    /// Label shown while the mode processes text.
    #[arg(long, global = true)]
    processing_label: Option<String>,
}
// Every action prints its result as JSON; see `mode_cmd::run`.
#[derive(Debug, Subcommand)]
enum ModeAction {
    /// Print all modes as a JSON array (builtins first).
    List,
    /// Re-read the modes file and print the modes.
    Reload,
    /// Add a custom mode (--name, --prompt, --processing-label).
    Add,
    /// Update a custom mode by --id.
    Update,
    /// Remove a custom mode by --id (builtins are rejected).
    Remove,
}

/// Arguments of `voiceless daemon`.
#[derive(Debug, Args)]
struct DaemonArgs {
    /// Default processing mode for sessions that do not name one.
    #[arg(long, default_value = "quick")]
    mode: String,
    /// Recognize without typing the result (debugging).
    #[arg(long)]
    no_inject: bool,
}

/// Arguments of `voiceless history`. The options are global so they may
/// follow the action; each action reads only the ones it needs.
#[derive(Debug, Args)]
struct HistoryArgs {
    /// History operation.
    #[command(subcommand)]
    action: HistoryAction,
    /// Record ids to delete (repeat the flag for several).
    #[arg(long, global = true)]
    ids: Vec<String>,
    /// Delete every record and recording.
    #[arg(long, global = true)]
    all: bool,
    /// Page size for list.
    #[arg(long, global = true, default_value_t = 50)]
    limit: i64,
    /// Continue list from a previous next_cursor.
    #[arg(long, global = true)]
    cursor: Option<String>,
    /// CSV file written by export.
    #[arg(long, global = true)]
    destination: Option<PathBuf>,
    /// Usage window in days: 1, 7 or 30.
    #[arg(long, global = true, default_value_t = 7)]
    days: i64,
    /// Address the console binds to (serve).
    #[arg(long, global = true, default_value = "127.0.0.1")]
    host: String,
    /// Port the console binds to (serve).
    #[arg(long, global = true, default_value_t = 8790)]
    port: u16,
}

// `Serve` is dispatched to `console::serve_standalone`; the others go
// through `history_cmd::run` by name.
#[derive(Debug, Subcommand)]
enum HistoryAction {
    /// Print a page of records as JSON (--limit, --cursor).
    List,
    /// Delete records by --ids or --all, together with their recordings.
    Delete,
    /// Export all records to a CSV file (--destination).
    Export,
    /// Print aggregate counts.
    Totals,
    /// Print usage over the last --days days.
    Usage,
    /// Open the local web console for browsing records and recordings.
    Serve,
}

/// Parse the command line, run one subcommand and exit with its code.
#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let code = match cli.command {
        // A daemon that already holds the local model runs the session; the
        // relay returns `None` when no daemon owns the bus name.
        Command::Stream(args) if uses_local_model(&cli.config, &args) => {
            match voiceless::ctl::stream_via_daemon(&args.mode, !args.no_inject, args.json).await {
                Some(Ok(code)) => code,
                Some(Err(error)) => {
                    eprintln!("Voiceless: {error}");
                    1
                }
                None => run_stream(cli.config, args).await,
            }
        }
        Command::Stream(args) => run_stream(cli.config, args).await,
        Command::Transcribe(args) => {
            voiceless::batch_cmd::transcribe(voiceless::batch_cmd::TranscribeArgs {
                wav: args.wav,
                backend: args.backend,
                inject: args.inject,
                json: args.json,
            })
            .await
        }
        Command::Record(args) => {
            voiceless::batch_cmd::record(voiceless::batch_cmd::RecordArgs {
                seconds: args.seconds,
                backend: args.backend,
                no_inject: args.no_inject,
                json: args.json,
            })
            .await
        }
        Command::Model(args) => voiceless::model_cmd::run(
            match args.action {
                ModelAction::List => voiceless::model_cmd::ModelCommand::List,
                ModelAction::Install { id } => voiceless::model_cmd::ModelCommand::Install { id },
                ModelAction::Check { id } => voiceless::model_cmd::ModelCommand::Check { id },
                ModelAction::Remove { id } => voiceless::model_cmd::ModelCommand::Remove { id },
            },
            args.json,
        ),
        Command::Mode(args) => {
            let action = match args.action {
                ModeAction::List => "list",
                ModeAction::Reload => "reload",
                ModeAction::Add => "add",
                ModeAction::Update => "update",
                ModeAction::Remove => "remove",
            };
            voiceless::mode_cmd::run(
                action,
                voiceless::mode_cmd::ModeArgs {
                    id: args.id,
                    name: args.name,
                    prompt: args.prompt,
                    processing_label: args.processing_label,
                },
            )
        }
        Command::History(args) if matches!(args.action, HistoryAction::Serve) => {
            match voiceless::console::serve_standalone(
                voiceless::models::default_data_dir().join("history.sqlite3"),
                &args.host,
                args.port,
            )
            .await
            {
                Ok(code) => code,
                Err(error) => {
                    eprintln!("Voiceless: {error}");
                    1
                }
            }
        }
        Command::History(args) => {
            let action = match args.action {
                HistoryAction::List => "list",
                HistoryAction::Delete => "delete",
                HistoryAction::Export => "export",
                HistoryAction::Totals => "totals",
                HistoryAction::Usage => "usage",
                HistoryAction::Serve => unreachable!("handled above"),
            };
            voiceless::history_cmd::run(
                action,
                voiceless::history_cmd::HistoryArgs {
                    ids: args.ids,
                    all: args.all,
                    limit: args.limit,
                    cursor: args.cursor,
                    destination: args.destination,
                    days: args.days,
                },
            )
        }
        Command::Doctor => {
            println!("Voiceless doctor");
            // The fcitx5 bridge check uses zbus::blocking, whose block_on
            // panics on a tokio runtime thread; run the checks on a blocking
            // worker instead (commit 156f6a4).
            let checks = tokio::task::spawn_blocking(voiceless::doctor::run_checks)
                .await
                .expect("doctor checks panicked");
            let mut all_ok = true;
            for check in &checks {
                if !check.pass() {
                    all_ok = false;
                }
                println!(
                    "{} {}: {}",
                    if check.pass() { "ok" } else { "FAIL" },
                    check.name,
                    check.detail
                );
            }
            // Optional tools that are missing still pass; see `Check::pass`.
            i32::from(!all_ok)
        }
        Command::Ctl(args) => {
            use voiceless::ctl::CtlAction;
            voiceless::ctl::run(match args.action {
                CtlCommand::Toggle { mode } => CtlAction::Toggle(mode),
                CtlCommand::Start { mode } => CtlAction::Start(mode),
                CtlCommand::Stop => CtlAction::Stop,
                CtlCommand::Cancel => CtlAction::Cancel,
                CtlCommand::Status => CtlAction::Status,
                CtlCommand::Watch => CtlAction::Watch,
            })
            .await
        }
        Command::Daemon(args) => {
            let options = voiceless::stream::StreamOptions {
                config_path: cli.config,
                backend: None,
                json: false,
                inject: !args.no_inject,
                mode: args.mode,
            };
            match voiceless::daemon::serve(options).await {
                Ok(()) => 0,
                Err(error) => {
                    eprintln!("Voiceless: {error}");
                    1
                }
            }
        }
        Command::Benchmark(args) => match args.action {
            BenchmarkAction::Asr {
                split,
                backend,
                enforce,
            } => {
                let mut bench_args =
                    voiceless::benchmark_cmd::AsrBenchmarkArgs::new(split, backend);
                bench_args.enforce = enforce;
                voiceless::benchmark_cmd::run_asr(bench_args).await
            }
            BenchmarkAction::Latency {
                trials,
                inject,
                enforce,
            } => {
                let mut latency_args = voiceless::latency_cmd::LatencyBenchmarkArgs::new();
                latency_args.trials = trials;
                latency_args.inject = inject;
                latency_args.enforce = enforce;
                voiceless::latency_cmd::run_latency(latency_args).await
            }
        },
    };
    // Never returns; the subcommand's code becomes the process status.
    std::process::exit(code);
}

/// Whether `stream` would load the local model that a running daemon already
/// holds; such sessions run in the daemon instead (see `ctl::stream_via_daemon`).
/// An explicit `--config` or `--backend` asks for something the daemon's own
/// configuration may not be, so those always run standalone.
fn uses_local_model(config: &Option<PathBuf>, args: &StreamArgs) -> bool {
    if config.is_some() || args.backend.is_some() {
        return false;
    }
    voiceless::config::AppConfig::load_optional(None).is_ok_and(|config| {
        matches!(
            config.asr.streaming_backend.as_str(),
            "local-offline" | "local-streaming"
        )
    })
}

/// Run `stream` in this process (SIGINT/SIGTERM control) and map a setup
/// error to exit code 1 after printing it.
async fn run_stream(config_path: Option<PathBuf>, args: StreamArgs) -> i32 {
    match voiceless::stream::run(voiceless::stream::StreamOptions {
        config_path,
        backend: args.backend,
        json: args.json,
        inject: !args.no_inject,
        mode: args.mode,
    })
    .await
    {
        Ok(code) => code,
        Err(error) => {
            eprintln!("Voiceless: {error}");
            1
        }
    }
}
