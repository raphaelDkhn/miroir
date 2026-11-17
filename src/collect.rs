//! `miroir collect` (§3.6), Harbor's collect hook in the world service: write the episode's
//! evidence once `main` has stopped. Scaffold: WP4 implements it.

use crate::evidence::Fault;
use crate::manifest::Manifest;
use crate::paths::Paths;

/// What collect wrote.
#[derive(Debug, Clone)]
pub struct Summary {
    pub changes: usize,
    pub effects: usize,
    pub faults: Vec<Fault>,
    /// The size of the evidence file.
    pub bytes: u64,
}

/// Write `/miroir/evidence/evidence.db`. Faults are recorded, never returned as errors; an
/// error is Miroir's own failure.
#[expect(clippy::unused_async, reason = "scaffold: WP4 awaits once it implements this")]
pub async fn collect(paths: &Paths, manifest: &Manifest) -> anyhow::Result<Summary> {
    let _ = (paths, manifest);
    Err(anyhow::anyhow!("WP4: not implemented"))
}
