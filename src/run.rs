//! `miroir run` (§3.5), the world image's `CMD`: boot the world, audit its listeners, write the
//! ready marker, and supervise until SIGTERM. Scaffold: WP3 implements it.

use crate::manifest::Manifest;
use crate::paths::Paths;

/// Boot the world and supervise it until SIGTERM.
#[expect(clippy::unused_async, reason = "scaffold: WP3 awaits once it implements this")]
pub async fn run(paths: &Paths, manifest: &Manifest) -> anyhow::Result<()> {
    let _ = (paths, manifest);
    Err(anyhow::anyhow!("WP3: not implemented"))
}
