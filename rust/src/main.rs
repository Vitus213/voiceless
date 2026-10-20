use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "voiceless",
    version,
    about = "Fast realtime voice input for Linux"
)]
struct Cli {
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Stream(StreamArgs),
    Transcribe(TranscribeArgs),
    Record(RecordArgs),
    Model(ModelArgs),
    Doctor,
    Mode(ModeArgs),
    History(HistoryArgs),
    Daemon(DaemonArgs),
    Benchmark(BenchmarkArgs),
}

#[derive(Debug, Args)]
struct BenchmarkArgs {
    #[command(subcommand)]
    action: BenchmarkAction,
}

#[derive(Debug, Subcommand)]
enum BenchmarkAction {
    Asr {
        #[arg(long, default_value = "test")]
        split: String,
        #[arg(long, default_value = "cloud-realtime")]
        backend: String,
        #[arg(long)]
        enforce: bool,
    },
    Latency {
        #[arg(long, default_value_t = 100)]
        trials: usize,
        #[arg(long)]
        inject: bool,
        #[arg(long)]
        enforce: bool,
    },
}

#[derive(Debug, Args)]
struct StreamArgs {
    #[arg(long)]
    backend: Option<String>,
    #[arg(long)]
    json: bool,
    #[arg(long)]
    no_inject: bool,
    #[arg(long, default_value = "quick")]
    mode: String,
}

#[derive(Debug, Args)]
struct TranscribeArgs {
    wav: PathBuf,
    #[arg(long)]
    backend: Option<String>,
    #[arg(long)]
    inject: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct RecordArgs {
    #[arg(long, default_value_t = 5.0)]
    seconds: f64,
    #[arg(long)]
    backend: Option<String>,
    #[arg(long)]
    no_inject: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ModelArgs {
    #[command(subcommand)]
    action: ModelAction,
    #[arg(long, global = true)]
    json: bool,
}

#[derive(Debug, Subcommand)]
enum ModelAction {
    List,
    Install { id: String },
    Check { id: String },
    Remove { id: String },
}

#[derive(Debug, Args)]
struct ModeArgs {
    #[command(subcommand)]
    action: ModeAction,
    #[arg(long, global = true)]
    id: Option<String>,
    #[arg(long, global = true)]
    name: Option<String>,
    #[arg(long, global = true)]
    prompt: Option<String>,
    #[arg(long, global = true)]
    processing_label: Option<String>,
}
#[derive(Debug, Subcommand)]
enum ModeAction {
    List,
    Reload,
    Add,
    Update,
    Remove,
}

#[derive(Debug, Args)]
struct DaemonArgs {
    #[arg(long, default_value = "quick")]
    mode: String,
}

#[derive(Debug, Args)]
struct HistoryArgs {
    #[command(subcommand)]
    action: HistoryAction,
    #[arg(long, global = true)]
    ids: Vec<String>,
    #[arg(long, global = true)]
    all: bool,
    #[arg(long, global = true, default_value_t = 50)]
    limit: i64,
    #[arg(long, global = true)]
    cursor: Option<String>,
    #[arg(long, global = true)]
    destination: Option<PathBuf>,
    #[arg(long, global = true, default_value_t = 7)]
    days: i64,
    #[arg(long, global = true, default_value = "127.0.0.1")]
    host: String,
    #[arg(long, global = true, default_value_t = 8790)]
    port: u16,
}

#[derive(Debug, Subcommand)]
enum HistoryAction {
    List,
    Delete,
    Export,
    Totals,
    Usage,
    /// Open the local web console for browsing records and recordings.
    Serve,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let code = match cli.command {
        Command::Stream(args) => match voiceless::stream::run(voiceless::stream::StreamOptions {
            config_path: cli.config,
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
        },
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
            match voiceless::history_web::serve(
                voiceless::models::default_data_dir().join("history.sqlite3"),
                voiceless::history_web::ServeOptions {
                    host: args.host,
                    port: args.port,
                },
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
            let checks = voiceless::doctor::run_checks();
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
            i32::from(!all_ok)
        }
        Command::Daemon(args) => {
            let options = voiceless::stream::StreamOptions {
                config_path: cli.config,
                backend: None,
                json: false,
                inject: true,
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
    std::process::exit(code);
}
