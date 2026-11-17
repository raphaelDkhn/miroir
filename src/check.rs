//! `miroir check` (§5.5), on the host: static task checks, grader validation over Harbor job
//! results, and per-task calibration stats. Scaffold: WP6 implements it.

use std::path::PathBuf;

/// What to check.
#[derive(Debug, Clone)]
pub struct Args {
    /// Directories of task directories, searched recursively.
    pub tasks: Vec<PathBuf>,
    /// Harbor job directories.
    pub jobs: Vec<PathBuf>,
    /// Print JSON instead of text.
    pub json: bool,
}

/// Check, printing the report to `out`. `Ok(true)` when there is no violation; an error is a
/// usage or I/O error.
pub fn run(args: &Args, out: &mut impl std::io::Write) -> anyhow::Result<bool> {
    let _ = (args, out);
    Err(anyhow::anyhow!("WP6: not implemented"))
}
