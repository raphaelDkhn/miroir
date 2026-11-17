//! The world's processes: start in order until ready, wait for quiet, record crashes, stop in
//! reverse order (§3.4, §3.5). Scaffold: WP1 implements it.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use crate::manifest::Process;

/// How long a process's ready probe may take to pass.
pub const READY_LIMIT: Duration = Duration::from_secs(60);
/// How long two passes of the `quiet` command in a row may take.
pub const QUIET_LIMIT: Duration = Duration::from_secs(30);

/// A process that exited while the world ran.
#[derive(Debug, Clone)]
pub struct Crash {
    pub process: String,
    pub status: String,
}

/// A supervisor failure. Each message names the process or command and the fix.
#[derive(Debug, thiserror::Error)]
pub enum SupervisorError {
    #[error("{0}")]
    Spawn(String),
    #[error("{0}")]
    NotReady(String),
    #[error("{0}")]
    Exited(String),
    #[error("{0}")]
    Command(String),
    #[error("{0}")]
    QuietTimeout(String),
    #[error("{0}")]
    Stop(String),
}

/// Starts, watches and stops the world's processes, and runs its commands.
#[derive(Debug)]
pub struct Supervisor {
    env: BTreeMap<String, String>,
    faults: Option<PathBuf>,
}

impl Supervisor {
    /// `env` is layered over this process's environment for every child and command; a
    /// process's own `env` goes on top. Crashes are appended to `faults` when it is set.
    pub fn new(env: BTreeMap<String, String>, faults: Option<PathBuf>) -> Supervisor {
        Supervisor { env, faults }
    }

    /// Start `processes` in order, each once the previous one is ready.
    #[expect(
        clippy::unused_async,
        clippy::unused_async_trait_impl,
        reason = "scaffold: WP1 awaits once it implements this"
    )]
    pub async fn start(
        &self,
        processes: &[&Process],
        ready_within: Duration,
    ) -> Result<(), SupervisorError> {
        let _ = (&self.env, &self.faults, processes, ready_within);
        Err(SupervisorError::Spawn("WP1: not implemented".into()))
    }

    /// Run one command to completion within `limit`.
    #[expect(
        clippy::unused_async,
        clippy::unused_async_trait_impl,
        reason = "scaffold: WP1 awaits once it implements this"
    )]
    pub async fn run_within(
        &self,
        argv: &[String],
        limit: Duration,
    ) -> Result<(), SupervisorError> {
        let _ = (argv, limit);
        Err(SupervisorError::Command("WP1: not implemented".into()))
    }

    /// Wait until `quiet` passes twice in a row, within `within`.
    #[expect(
        clippy::unused_async,
        clippy::unused_async_trait_impl,
        reason = "scaffold: WP1 awaits once it implements this"
    )]
    pub async fn wait_quiet(
        &self,
        quiet: Option<&[String]>,
        within: Duration,
    ) -> Result<(), SupervisorError> {
        let _ = (quiet, within);
        Err(SupervisorError::QuietTimeout("WP1: not implemented".into()))
    }

    /// The first process that exited unexpectedly, if any.
    pub fn crash(&self) -> Option<Crash> {
        None
    }

    /// Stop every process in reverse order: SIGTERM, then SIGKILL past `limit`.
    #[expect(
        clippy::unused_async,
        clippy::unused_async_trait_impl,
        reason = "scaffold: WP1 awaits once it implements this"
    )]
    pub async fn stop(&self, limit: Duration) -> Result<(), SupervisorError> {
        let _ = limit;
        Err(SupervisorError::Stop("WP1: not implemented".into()))
    }
}
