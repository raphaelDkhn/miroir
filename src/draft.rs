//! `miroir draft` (Appendix B1), on the host: a draft `MIROIR_SPEC` from the evidence of one or
//! more reference runs of a task. Scaffold: WP14 implements it.

use std::path::PathBuf;

/// What to draft from.
#[derive(Debug, Clone)]
pub struct DraftArgs {
    /// Harbor trial directories of one task, or `evidence.db` files.
    pub inputs: Vec<PathBuf>,
}

/// Print a draft spec to `out`. `Ok(true)` when a draft was printed, `Ok(false)` when there is
/// nothing to draft; an error is a usage or I/O error.
pub fn run(args: &DraftArgs, out: &mut impl std::io::Write) -> anyhow::Result<bool> {
    let _ = (args, out);
    Err(anyhow::anyhow!("WP14: not implemented"))
}
