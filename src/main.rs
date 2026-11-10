//! Miroir runs inside a product's container and turns it into an RL environment: it bakes a
//! seed snapshot at image build, restores it before each episode, serves the agent's tools over
//! `OpenEnv` and MCP, and grades the end state from the changes and outbound calls it captured.

mod bake;
mod check;
mod clock;
mod code;
mod coworker;
mod db;
mod edb;
mod env;
mod episode;
mod grade;
mod import;
mod manifest;
mod mcp;
mod record;
mod sandbox;
mod scrub;
mod server;
mod sinks;
mod state;
mod supervisor;
mod task;
mod tools;

use std::io::{IsTerminal, Write};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand};
use tokio::signal::unix::{SignalKind, signal};

use crate::env::{Env, EnvError, Paths};

/// Turn a product's container into an RL environment.
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve the env: restore the seed state, then serve episodes over WebSocket and MCP.
    Run,
    /// Seed every world and keep its state as the pristine copy, when building the image.
    Bake,
    /// Check that the env and its tasks grade reliably.
    Check {
        /// Print the full report as JSON.
        #[arg(long)]
        json: bool,
        /// Also replay each golden run once without each of its calls.
        #[arg(long)]
        mutate: bool,
        /// Check only this world's tasks.
        #[arg(long)]
        world: Option<String>,
    },
    /// Serve the sandbox that runs the agent's code, in a container of its own.
    Sandbox,
    /// Scrub production database dumps, and agent traces, into seed data.
    Import(import::Args),
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .with_target(false)
        .init();
    match dispatch(Cli::parse().command).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn dispatch(command: Command) -> anyhow::Result<()> {
    supervisor::ensure_entrypoint()?;
    match command {
        Command::Run => run().await,
        Command::Bake => bake().await,
        Command::Check { json, mutate, world } => check(json, mutate, world.as_deref()).await,
        Command::Sandbox => sandbox::serve(port()?).await,
        Command::Import(args) => {
            import::run(&args).await?;
            tracing::info!(out = %args.out.display(), "import finished");
            Ok(())
        }
    }
}

async fn bake() -> anyhow::Result<()> {
    let paths = Paths::default();
    let (manifest, _) = env::load(&paths)?;
    sinks::start(&manifest.sinks, None).await.context("cannot start the sinks")?;
    bake::run(&paths, &manifest).await?;
    tracing::info!("bake finished");
    Ok(())
}

async fn check(json: bool, mutate: bool, world: Option<&str>) -> anyhow::Result<()> {
    let report = match Env::open(Paths::default(), sandbox()?).await.map(Arc::new) {
        Ok(env) => {
            let worlds = &env.manifest.worlds;
            if let Some(world) = world.filter(|world| !worlds.iter().any(|w| w == world)) {
                anyhow::bail!("unknown world '{world}'; the manifest has {}", worlds.join(", "));
            }
            sinks::start(&env.manifest.sinks, Some(env.edb.clone()))
                .await
                .context("cannot start the sinks")?;
            serve_proxy(&env).await?;
            let report = check::run(&env, mutate, world).await;
            env.shutdown().await;
            report
        }
        Err(e) => check::static_failure(&e.to_string()),
    };
    let text = if json { serde_json::to_string_pretty(&report)? } else { check::summary(&report) };
    writeln!(std::io::stdout().lock(), "{text}")?;
    if report["passed"] == true { Ok(()) } else { Err(anyhow::anyhow!("miroir check failed")) }
}

fn sandbox() -> anyhow::Result<Option<code::Sandbox>> {
    match std::env::var("MIROIR_SANDBOX") {
        Ok(url) => Ok(Some(code::Sandbox::new(&url).map_err(anyhow::Error::msg)?)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(e) => Err(anyhow::anyhow!("MIROIR_SANDBOX: {e}")),
    }
}

async fn serve_proxy(env: &Arc<Env>) -> anyhow::Result<()> {
    if env.sandbox.is_none() {
        return Ok(());
    }
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", code::PROXY_PORT))
        .await
        .with_context(|| format!("cannot listen on port {} for the sandbox", code::PROXY_PORT))?;
    let proxy = code::proxy(env.clone());
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, proxy).await {
            tracing::error!("the sandbox's proxy stopped: {e}");
        }
    });
    Ok(())
}

fn port() -> anyhow::Result<u16> {
    match std::env::var("MIROIR_PORT") {
        Ok(port) => port.parse().context("MIROIR_PORT must be a port number"),
        Err(_) => Ok(8000),
    }
}

fn bearer_token() -> anyhow::Result<Option<String>> {
    match std::env::var("MIROIR_TOKEN") {
        Ok(token) if !token.is_empty() => Ok(Some(token)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        _ => Err(anyhow::anyhow!(
            "MIROIR_TOKEN must not be empty; set it to a token, or unset it to serve without one"
        )),
    }
}

async fn warn_if_online() {
    if reachable(("1.1.1.1", 443)).await {
        tracing::warn!(
            "this env can reach the internet, so calls the product makes past the sinks are \
             neither recorded nor blocked; run it on an internal network"
        );
    }
}

async fn reachable(address: impl tokio::net::ToSocketAddrs) -> bool {
    let connect = tokio::net::TcpStream::connect(address);
    matches!(tokio::time::timeout(Duration::from_secs(1), connect).await, Ok(Ok(_)))
}

async fn run() -> anyhow::Result<()> {
    if rustix::process::getpid().is_init() {
        tracing::warn!("miroir is PID 1, so the orphans a reset leaves are never reaped; use tini");
    }
    let env = Arc::new(Env::open(Paths::default(), sandbox()?).await?);
    sinks::start(&env.manifest.sinks, Some(env.edb.clone()))
        .await
        .context("cannot start the sinks")?;
    serve_proxy(&env).await?;
    let port = port()?;
    let token = bearer_token()?;
    if token.is_none() {
        tracing::warn!(port, "MIROIR_TOKEN is not set, so anyone who can reach this port can act");
    }
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .with_context(|| format!("cannot listen on port {port}"))?;
    let router = server::router(env.clone(), token);
    let mut serving = tokio::spawn(server::serve(listener, router));
    tracing::info!(port, "serving OpenEnv");
    tokio::spawn(warn_if_online());

    let mut terminate = signal(SignalKind::terminate()).context("cannot watch SIGTERM")?;
    let boot = async {
        env.boot().await?;
        if let Some(sandbox) = &env.sandbox {
            sandbox.probe(&[code::PROXY_PORT, port]).await.map_err(EnvError::Other)?;
        }
        Ok::<(), EnvError>(())
    };
    tokio::pin!(boot);
    let mut booted = false;
    let outcome = loop {
        tokio::select! {
            result = &mut boot, if !booted => match result {
                Ok(()) => {
                    booted = true;
                    tracing::info!("env ready");
                }
                Err(e) => break Err(anyhow::Error::new(e).context("the env could not start")),
            },
            result = &mut serving => break Err(anyhow::anyhow!("the server stopped: {result:?}")),
            _ = terminate.recv() => break Ok(()),
            result = tokio::signal::ctrl_c() => break result.context("cannot watch Ctrl-C"),
        }
    };
    tracing::info!("stopping");
    env.shutdown().await;
    tracing::info!("stopped");
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_open_port_is_reachable_and_a_closed_one_is_not() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(reachable(("127.0.0.1", port)).await);
        drop(listener);
        assert!(!reachable(("127.0.0.1", 1)).await, "nothing listens on port 1");
    }
}
