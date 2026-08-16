//! `gitai` - the single binary.
//!
//! Four verbs: set up a config, check it, serve webhooks, or run one task by
//! hand. The last one needs no forge and no network, which makes it the way to
//! try the pipeline before wiring anything up.

use std::path::PathBuf;
use std::sync::Arc;

use clap::{Args, Parser, Subcommand};
use gitai_core::config::Config;
use gitai_core::model::TaskId;
use gitai_core::store::Store;
use gitai_forge::ForgeRegistry;
use gitai_pipeline::Engine;
use gitai_server::AppState;
use gitai_store::SqliteStore;

mod init;
mod local;

#[derive(Debug, Parser)]
#[command(
    name = "gitai",
    version,
    about = "Issue in, reviewed pull request out. Self-hosted, model-agnostic."
)]
struct Cli {
    #[arg(short, long, default_value = "gitai.toml", env = "GITAI_CONFIG", global = true)]
    config: PathBuf,

    /// Debug logging.
    #[arg(short, long, global = true)]
    verbose: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Write a starting gitai.toml and the prompt templates.
    Init {
        #[arg(default_value = ".")]
        dir: PathBuf,
        /// Overwrite files that already exist.
        #[arg(long)]
        force: bool,
    },

    /// Check the config, the sandbox, the prompts and every configured model.
    Doctor,

    /// Serve webhooks and run the queue.
    Serve,

    /// Run one task now, against a checkout on disk.
    Run(RunArgs),

    /// Run a fast, token-efficient web search.
    Search {
        /// Search query
        query: String,

        /// Search provider: duckduckgo, tavily, brave, searxng
        #[arg(long)]
        provider: Option<String>,

        /// Maximum results to return
        #[arg(long, default_value = "5")]
        max_results: usize,
    },
}

#[derive(Debug, Args)]
struct RunArgs {
    /// Path to a git checkout.
    #[arg(long)]
    repo: PathBuf,

    /// What needs doing. This becomes the issue title.
    #[arg(long)]
    title: String,

    /// The details. Everything the planner gets beyond the title.
    #[arg(long, conflicts_with = "body_file")]
    body: Option<String>,

    /// Read the details from a file instead.
    #[arg(long)]
    body_file: Option<PathBuf>,

    /// Branch to work from. Defaults to whatever the checkout is on.
    #[arg(long)]
    base: Option<String>,

    #[arg(long)]
    rounds: Option<u32>,
    #[arg(long)]
    attempts: Option<u32>,
    #[arg(long)]
    iterations: Option<u32>,
    #[arg(long)]
    max_cost: Option<f64>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let _ = dotenvy::dotenv();
    init_tracing(cli.verbose);

    match cli.command {
        Command::Init { dir, force } => {
            let written = init::init(&dir, force)?;
            if written.is_empty() {
                println!("Nothing to write; everything is already there.");
            } else {
                println!("Wrote:");
                for f in &written {
                    println!("  {f}");
                }
            }
            println!(
                "\nNext: fill in [gate] with your build and test commands, point [models] at \
                 something real, then run `gitai doctor`."
            );
            Ok(())
        }

        Command::Doctor => {
            let cfg = Config::load(&cli.config)?;
            let problems = init::doctor(&cfg).await?;
            if problems > 0 {
                std::process::exit(1);
            }
            Ok(())
        }

        Command::Serve => serve(&cli.config).await,

        Command::Run(args) => run_once(&cli.config, args).await,

        Command::Search {
            query,
            provider,
            max_results,
        } => {
            let mut cfg = Config::load(&cli.config).unwrap_or_default();
            if let Some(p) = provider {
                cfg.web_search.provider = p;
            }
            cfg.web_search.max_results = max_results;

            let started = std::time::Instant::now();
            let engine = gitai_pipeline::WebSearchEngine::new(cfg.web_search);
            let results = engine.search(&query).await?;
            let elapsed = started.elapsed();

            let formatted = gitai_pipeline::web_search::format_results_compact(
                &query,
                &results,
                max_results,
                220,
            );
            println!("{formatted}");
            println!("---");
            println!(
                "Fetched {} result(s) in {:.2}s (~{} tokens)",
                results.len(),
                elapsed.as_secs_f64(),
                gitai_llm::tokens::estimate(&formatted)
            );
            Ok(())
        }
    }
}

/// Everything the pipeline needs, wired together once.
async fn build_state(cfg: Config) -> anyhow::Result<AppState> {
    let cfg = Arc::new(cfg);

    let store = Arc::new(SqliteStore::connect(&cfg.storage.url).await?);
    store.migrate().await?;
    let store: Arc<dyn Store> = store;

    let forges = Arc::new(ForgeRegistry::build(&cfg)?);
    let sandbox = gitai_sandbox::build_sandbox(&cfg.sandbox);
    sandbox.preflight().await?;

    let engine = Arc::new(Engine::new(
        cfg.clone(),
        store.clone(),
        sandbox,
        forges.clone(),
    )?);

    Ok(AppState {
        cfg,
        store,
        forges,
        engine,
    })
}

async fn serve(config_path: &PathBuf) -> anyhow::Result<()> {
    let cfg = Config::load(config_path)?;

    if cfg.gate.is_empty() {
        tracing::warn!(
            "no [gate] commands are configured. Nothing can objectively reject a patch, \
             so reviews will be models grading models. Run `gitai doctor` for the details."
        );
    }

    let concurrency = cfg.server.concurrency;
    let state = build_state(cfg).await?;

    let workers = gitai_server::spawn_workers(state.clone(), concurrency);
    tracing::info!(runners = workers.len() - 1, "queue running");

    gitai_server::serve(state, shutdown_signal()).await?;

    for handle in workers {
        handle.abort();
    }
    tracing::info!("stopped");
    Ok(())
}

async fn run_once(config_path: &PathBuf, args: RunArgs) -> anyhow::Result<()> {
    let cfg = Config::load(config_path)?;

    let mut budget = cfg.budget;
    if let Some(v) = args.rounds {
        budget.max_rounds = v;
    }
    if let Some(v) = args.attempts {
        budget.attempts_per_round = v;
    }
    if let Some(v) = args.iterations {
        budget.max_iterations = v;
    }
    if let Some(v) = args.max_cost {
        budget.max_cost_usd = v;
    }

    let body = match (&args.body, &args.body_file) {
        (Some(b), _) => b.clone(),
        (None, Some(path)) => std::fs::read_to_string(path)?,
        (None, None) => String::new(),
    };

    let task = local::local_task(&args.repo, args.title, body, args.base, budget)?;
    let task_id = task.id;

    let state = build_state(cfg).await?;
    state.store.create_task(&task).await?;

    println!("task {task_id}");
    println!(
        "repo {} on {}",
        args.repo.display(),
        task.base_branch.as_deref().unwrap_or("?")
    );
    println!(
        "budget: {} round(s) x {} attempt(s), {} iteration(s) each, ${:.2} ceiling\n",
        budget.max_rounds, budget.attempts_per_round, budget.max_iterations, budget.max_cost_usd
    );

    // Tail the event log while the engine works, so a long run is watchable.
    let tail = tokio::spawn(tail_events(state.store.clone(), task_id));
    let cancel = shutdown_signal();
    let final_state = tokio::select! {
        res = state.engine.clone().run_task(task_id) => res?,
        _ = cancel => {
            tail.abort();
            println!("\n[interrupted by signal]");
            return Ok(());
        }
    };
    tail.abort();

    // One last read, to catch events written between the last poll and the end.
    let task = state.store.get_task(task_id).await?;
    println!("\n---");
    println!("state:  {final_state}");
    println!(
        "spend:  {} model calls, {} tokens, ${:.3}, {}s",
        task.spend.llm_calls,
        task.spend.tokens(),
        task.spend.cost_usd,
        task.spend.wall_secs
    );

    let attempts = state.store.list_attempts(task_id).await?;
    println!("attempts: {}", attempts.len());
    for a in &attempts {
        println!(
            "  round {} #{} [{}] model `{}`, {} iteration(s){}",
            a.round,
            a.index,
            a.state,
            a.model,
            a.iterations,
            a.review
                .as_ref()
                .map(|r| format!(", score {}", r.score))
                .unwrap_or_default()
        );
    }

    if let Some(err) = &task.last_error {
        println!("\nerror: {err}");
        std::process::exit(1);
    }
    Ok(())
}

/// Prints new events as they land. Errors are ignored: this is a view, and a
/// hiccup in it must not disturb the run.
async fn tail_events(store: Arc<dyn Store>, task_id: TaskId) {
    let mut cursor = 0i64;
    loop {
        if let Ok(events) = store.list_events(task_id, cursor, 100).await {
            for e in &events {
                println!("[{}] {}", e.kind, e.message);
                cursor = e.seq;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    }
}

fn init_tracing(verbose: bool) {
    use tracing_subscriber::EnvFilter;

    let default = if verbose {
        "gitai=debug,info"
    } else {
        "gitai=info,warn"
    };
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_env("GITAI_LOG").unwrap_or_else(|_| default.into()))
        .with_target(false)
        .init();
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to install SIGTERM handler");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {
            tracing::info!("received SIGINT (Ctrl+C), shutting down");
        }
        _ = terminate => {
            tracing::info!("received SIGTERM, shutting down");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_cli_definition_is_internally_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn body_and_body_file_cannot_both_be_given() {
        let err = Cli::try_parse_from([
            "gitai",
            "run",
            "--repo",
            ".",
            "--title",
            "t",
            "--body",
            "b",
            "--body-file",
            "f",
        ])
        .unwrap_err();
        assert!(err.to_string().contains("cannot be used with"), "{err}");
    }

    #[test]
    fn budget_flags_override_the_config() {
        let cli = Cli::try_parse_from([
            "gitai",
            "run",
            "--repo",
            ".",
            "--title",
            "t",
            "--rounds",
            "1",
            "--attempts",
            "5",
        ])
        .unwrap();
        match cli.command {
            Command::Run(args) => {
                assert_eq!(args.rounds, Some(1));
                assert_eq!(args.attempts, Some(5));
                assert_eq!(args.iterations, None);
            }
            _ => panic!("expected run"),
        }
    }

    #[test]
    fn the_config_path_has_a_default_and_is_global() {
        let cli = Cli::try_parse_from(["gitai", "doctor"]).unwrap();
        assert_eq!(cli.config, PathBuf::from("gitai.toml"));

        let cli = Cli::try_parse_from(["gitai", "doctor", "--config", "other.toml"]).unwrap();
        assert_eq!(cli.config, PathBuf::from("other.toml"));
    }
}
