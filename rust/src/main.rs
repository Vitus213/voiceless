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
//! When a daemon owns the default data directory, it runs `stream` using the
//! active configuration. This process relays events and forwards signals through
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
    /// Config file for dictation, transcription and the console (defaults to the user config).
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
    /// Manage the model catalog: list, install, import, check, remove.
    Model(ModelArgs),
    /// Install the verified standalone release archive into your user directory.
    #[cfg(target_os = "linux")]
    Install(InstallArgs),
    /// Update through the installing channel, or save and restore a data snapshot.
    #[cfg(target_os = "linux")]
    Update(UpdateArgs),
    /// Check dependencies and the data directory.
    Doctor,
    /// Manage text processing modes.
    Mode(ModeArgs),
    /// Query the recognition history or serve the web console.
    History(HistoryArgs),
    /// Open the web console, serving it locally when needed.
    Web(WebArgs),
    /// Apply a JSON settings patch from standard input.
    Config(ConfigArgs),
    /// Run the resident daemon (warm engine, local control, hotkey, console).
    Daemon(DaemonArgs),
    /// Control the resident daemon from the command line.
    Ctl(CtlArgs),
    /// Run the CER and stop-to-inject latency gates.
    Benchmark(BenchmarkArgs),
}

#[cfg(target_os = "linux")]
#[derive(Debug, Args)]
struct InstallArgs {
    /// Downloaded release archive for this platform.
    archive: PathBuf,
    /// Expected SHA-256 of the archive from GitHub Releases.
    #[arg(long)]
    sha256: String,
}

#[cfg(target_os = "linux")]
#[derive(Debug, Args)]
struct UpdateArgs {
    /// Switch a standalone installation to its previous local version.
    #[arg(long, conflicts_with_all = ["snapshot", "list", "restore"])]
    rollback: bool,
    /// Save a private snapshot of the configuration and history now.
    #[arg(long, conflicts_with_all = ["list", "restore"])]
    snapshot: bool,
    /// List the saved data snapshots.
    #[arg(long, conflicts_with_all = ["restore"])]
    list: bool,
    /// Return the configuration and history to the snapshot with this id.
    #[arg(long, value_name = "ID")]
    restore: Option<String>,
}

#[derive(Debug, Args)]
struct WebArgs {
    /// Override the configured console port. Use 0 for an available port.
    #[arg(long)]
    port: Option<u16>,
    /// Print the URL without launching a browser.
    #[arg(long)]
    no_open: bool,
}

#[derive(Debug, Args)]
struct ConfigArgs {
    #[command(subcommand)]
    action: ConfigAction,
}

#[derive(Debug, Subcommand)]
enum ConfigAction {
    /// Validate and save a JSON object of settings read from stdin.
    Patch,
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
    /// Paste the last completed dictation into the focused window again.
    Repaste,
    /// Refuse every start (hotkey included) until `resume`; a session in progress finishes.
    Pause,
    /// Accept starts again after `pause`.
    Resume,
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
        /// Realtime backend to measure. The latency gate supports cloud-realtime.
        #[arg(long, default_value = "cloud-realtime")]
        backend: String,
        /// Processing mode to measure. The latency gate supports quick.
        #[arg(long, default_value = "quick")]
        mode: String,
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
    /// Override application rules with a specific processing mode.
    #[arg(long, default_value = "")]
    mode: String,
    /// Hostname supplied by an authorized caller for this session.
    #[arg(long)]
    site: Option<String>,
    /// Project vocabulary to apply to this session.
    #[arg(long)]
    project: Option<String>,
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
    /// Print the model list, or the result of an import, as JSON.
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
    /// Import a local GGUF model file after verifying it.
    Import {
        /// GGUF model file to verify and add to the model list.
        file: PathBuf,
    },
    /// Verify an installed model.
    Check {
        /// Model id (see model list).
        id: String,
    },
    /// Remove an installed model.
    Remove {
        /// Model id (see model list).
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
    /// Render sample text through a mode without capture or delivery.
    Preview {
        /// Sample text. Read from standard input when omitted.
        #[arg(long)]
        text: Option<String>,
        /// Mode name or id. If omitted, use the matching rule or default mode.
        #[arg(long, default_value = "")]
        mode: String,
        /// Application id for the preview's rule match.
        #[arg(long)]
        app_id: Option<String>,
        /// Hostname for the preview's authorized site rule.
        #[arg(long)]
        site: Option<String>,
        /// Project vocabulary to apply to the sample.
        #[arg(long)]
        project: Option<String>,
        /// Send this sample to the configured model when the mode permits it.
        #[arg(long)]
        send_model: bool,
        /// Print a structured result with each output step.
        #[arg(long)]
        json: bool,
    },
    /// Serve local output previews to an opted-in OpenCode client over stdio.
    Mcp,
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
    /// Search the raw and current text.
    #[arg(long, global = true)]
    search: Option<String>,
    /// Include records created at or after this RFC 3339 time.
    #[arg(long, global = true)]
    from: Option<String>,
    /// Include records created before this RFC 3339 time.
    #[arg(long, global = true)]
    to: Option<String>,
    /// Limit the query to a processing mode id.
    #[arg(long, global = true)]
    mode: Option<String>,
    /// Limit the query to an application id.
    #[arg(long, global = true)]
    app: Option<String>,
    /// Limit the query to a recognition model id.
    #[arg(long, global = true)]
    model: Option<String>,
    /// Include records with or without a saved recording.
    #[arg(long, global = true, value_parser = ["with", "without"])]
    audio: Option<String>,
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
    /// Export matching records to a CSV file (--destination).
    Export,
    /// Print aggregate counts.
    Totals,
    /// Print usage over the last --days days.
    Usage,
    /// Open the local web console for browsing records and recordings.
    Serve,
}

fn main() {
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == voiceless::output::SCRIPT_WORKER_ARG)
    {
        std::process::exit(exit_with_result(voiceless::output::run_script_worker()));
    }
    run();
}

/// Parse the command line, run one subcommand and exit with its code.
#[tokio::main]
async fn run() {
    let cli = Cli::parse();
    let code = match cli.command {
        Command::Stream(args) if cli.config.is_none() && args.backend.is_none() => {
            let context = voiceless::stream::DictationContext {
                site: args.site.clone(),
                project: args.project.clone(),
            };
            match voiceless::ctl::stream_via_daemon(
                &args.mode,
                !args.no_inject,
                args.json,
                &context,
            )
            .await
            {
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
                config_path: cli.config,
                wav: args.wav,
                backend: args.backend,
                inject: args.inject,
                json: args.json,
            })
            .await
        }
        Command::Record(args) => {
            voiceless::batch_cmd::record(voiceless::batch_cmd::RecordArgs {
                config_path: cli.config,
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
                ModelAction::Import { file } => voiceless::model_cmd::ModelCommand::Import { file },
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
                ModeAction::Preview {
                    text,
                    mode,
                    app_id,
                    site,
                    project,
                    send_model,
                    json,
                } => {
                    std::process::exit(
                        voiceless::mode_cmd::preview(
                            cli.config.as_deref(),
                            voiceless::mode_cmd::PreviewArgs {
                                text,
                                mode,
                                app_id,
                                site,
                                project,
                                send_model,
                                json,
                            },
                        )
                        .await,
                    );
                }
                ModeAction::Mcp => {
                    std::process::exit(exit_with_result(
                        voiceless::mode_cmd::serve_mcp(cli.config.as_deref()).await,
                    ));
                }
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
            voiceless::history_cmd::run_with(
                action,
                voiceless::history_cmd::HistoryArgs {
                    ids: args.ids,
                    all: args.all,
                    limit: args.limit,
                    cursor: args.cursor,
                    destination: args.destination,
                    days: args.days,
                },
                voiceless::history_cmd::HistoryFlags {
                    search: args.search,
                    from: args.from,
                    to: args.to,
                    mode: args.mode,
                    app: args.app,
                    model: args.model,
                    audio: args.audio,
                },
            )
        }
        Command::Web(args) => exit_with_result(run_web(cli.config, args).await),
        #[cfg(target_os = "linux")]
        Command::Install(args) => {
            exit_with_result(voiceless::update::install(&args.archive, &args.sha256))
        }
        #[cfg(target_os = "linux")]
        Command::Update(args) => exit_with_result(
            voiceless::update::run(voiceless::update::Update {
                rollback: args.rollback,
                snapshot: args.snapshot,
                list: args.list,
                restore: args.restore,
            })
            .await,
        ),
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
        Command::Config(args) => match args.action {
            ConfigAction::Patch => exit_with_result(patch_config(cli.config)),
        },
        Command::Ctl(args) => {
            use voiceless::ctl::CtlAction;
            voiceless::ctl::run(match args.action {
                CtlCommand::Toggle { mode } => CtlAction::Toggle(mode),
                CtlCommand::Start { mode } => CtlAction::Start(mode),
                CtlCommand::Stop => CtlAction::Stop,
                CtlCommand::Cancel => CtlAction::Cancel,
                CtlCommand::Repaste => CtlAction::Repaste,
                CtlCommand::Status => CtlAction::Status,
                CtlCommand::Watch => CtlAction::Watch,
                CtlCommand::Pause => CtlAction::Pause,
                CtlCommand::Resume => CtlAction::Resume,
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
                context: Default::default(),
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
                backend,
                mode,
                trials,
                inject,
                enforce,
            } => {
                voiceless::latency_cmd::run_latency(voiceless::latency_cmd::LatencyBenchmarkArgs {
                    backend,
                    mode,
                    trials,
                    inject,
                    enforce,
                    ..Default::default()
                })
                .await
            }
        },
    };
    // Never returns; the subcommand's code becomes the process status.
    std::process::exit(code);
}

fn exit_with_result(result: Result<(), String>) -> i32 {
    match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("Voiceless: {error}");
            1
        }
    }
}

fn patch_config(path: Option<PathBuf>) -> Result<(), String> {
    let path = path.unwrap_or_else(voiceless::config::default_config_path);
    let body = serde_json::from_reader(std::io::stdin().lock()).map_err(|error| {
        format!(
            "invalid JSON settings patch at line {}, column {}",
            error.line(),
            error.column()
        )
    })?;
    voiceless::config::patch(&path, &body)
}

async fn run_web(config_path: Option<PathBuf>, args: WebArgs) -> Result<(), String> {
    use voiceless::config::{default_config_path, AppConfig};

    let config =
        AppConfig::load_optional(config_path.as_deref()).map_err(|error| error.to_string())?;
    voiceless::console::open_web(
        config_path.unwrap_or_else(default_config_path),
        &config.console.host,
        args.port.unwrap_or(config.console.port),
        !args.no_open,
    )
    .await
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
        context: voiceless::stream::DictationContext {
            site: args.site,
            project: args.project,
        },
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
