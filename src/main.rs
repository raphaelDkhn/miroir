//! The `miroir` command line (§5): it parses the subcommand, calls the library, and maps the
//! result to an exit status.

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use miroir::manifest::Manifest;
use miroir::paths::{self, Paths};
use miroir::{bake, check, collect, draft, grade, run};

/// Turn a product's container image into graded Harbor environments.
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Bake the world into the image: the last RUN of a world Dockerfile.
    Bake,
    /// Boot the world and supervise it: the world image's CMD.
    Run,
    /// Write the episode's evidence: Harbor's collect hook in the world service.
    Collect,
    /// Grade the evidence with the task's grading spec: the verifier's /tests/test.sh.
    Grade,
    /// Check task directories and Harbor job results, on the host.
    Check {
        /// A directory of task directories, searched recursively.
        #[arg(long, value_name = "DIR")]
        tasks: Vec<PathBuf>,
        /// Print the report as JSON.
        #[arg(long)]
        json: bool,
        /// A Harbor job directory.
        #[arg(value_name = "JOB_DIR")]
        jobs: Vec<PathBuf>,
    },
    /// Draft a grading spec from the evidence of reference runs, on the host.
    Draft {
        /// A Harbor trial directory, or an evidence.db file.
        #[arg(required = true, value_name = "INPUT")]
        inputs: Vec<PathBuf>,
    },
}

/// The exit status of a void grade, which Harbor and verifiers record as an invalid trial.
const VOID: u8 = 75;
/// The exit status of `check` and `draft` on a usage or I/O error.
const USAGE: u8 = 2;

#[tokio::main]
async fn main() -> ExitCode {
    let paths = Paths::default();
    match Cli::parse().command {
        Command::Bake => {
            logs();
            world(bake(&paths).await)
        }
        Command::Run => {
            logs();
            world(run(&paths).await)
        }
        Command::Collect => {
            logs();
            world(collect(&paths).await)
        }
        Command::Grade => grade(&paths).await,
        Command::Check { tasks, json, jobs } => {
            logs();
            verdict(check::run(&check::Args { tasks, jobs, json }, &mut std::io::stdout()))
        }
        Command::Draft { inputs } => {
            logs();
            verdict(draft::run(&draft::DraftArgs { inputs }, &mut std::io::stdout()).await)
        }
    }
}

/// Log to stderr; `grade` installs no logger, so nothing precedes its result.
fn logs() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .with_target(false)
        .init();
}

async fn bake(paths: &Paths) -> anyhow::Result<()> {
    let manifest = Manifest::load(&paths.manifest())?;
    bake::bake(paths, &manifest).await
}

async fn run(paths: &Paths) -> anyhow::Result<()> {
    let manifest = Manifest::load(&paths.manifest())?;
    run::run(paths, &manifest).await
}

async fn collect(paths: &Paths) -> anyhow::Result<()> {
    let manifest = Manifest::load(&paths.manifest())?;
    let summary = collect::collect(paths, &manifest).await?;
    let faults: Vec<&str> = summary.faults.iter().map(|fault| fault.kind.as_str()).collect();
    writeln!(
        std::io::stdout(),
        "evidence: {} changes, {} effects, {} bytes; faults: [{}]",
        summary.changes,
        summary.effects,
        summary.bytes,
        faults.join(", "),
    )?;
    Ok(())
}

async fn grade(paths: &Paths) -> ExitCode {
    let inputs = grade::Inputs {
        spec: std::env::var(paths::SPEC_VAR).ok(),
        evidence: paths.evidence(),
        answer: PathBuf::from(paths::ANSWER),
        out_dir: PathBuf::from(paths::VERIFIER_DIR),
    };
    match grade::grade(&inputs, &mut std::io::stdout()).await {
        grade::Outcome::Scored { .. } => ExitCode::SUCCESS,
        grade::Outcome::Void(_) => ExitCode::from(VOID),
    }
}

/// `bake`, `run` and `collect`: 0, or 1 with the error logged.
fn world(result: anyhow::Result<()>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

/// `check` and `draft`: 0 on `Ok(true)`, 1 on `Ok(false)`, 2 with the error logged.
fn verdict(result: anyhow::Result<bool>) -> ExitCode {
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            tracing::error!("{e:#}");
            ExitCode::from(USAGE)
        }
    }
}
