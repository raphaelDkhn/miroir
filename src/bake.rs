//! `miroir bake` (§3.4), the last `RUN` of a world Dockerfile: seed the world, install change
//! capture, and record the bake. Scaffold: WP3 implements it.

use serde::{Deserialize, Serialize};

use crate::manifest::Manifest;
use crate::paths::Paths;
use crate::pg::Shift;

/// `/miroir/bake.json`: when the bake started, and which Miroir baked.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BakeInfo {
    pub baked_at: jiff::Timestamp,
    pub miroir: String,
}

/// The shift that moves data captured at `captured` to `started`; an error when `captured` is
/// later.
pub fn captured_shift(
    captured: jiff::Timestamp,
    started: jiff::Timestamp,
) -> Result<Shift, String> {
    let _ = (captured, started);
    Err("WP3: not implemented".into())
}

/// Bake the world into the image.
#[expect(clippy::unused_async, reason = "scaffold: WP3 awaits once it implements this")]
pub async fn bake(paths: &Paths, manifest: &Manifest) -> anyhow::Result<()> {
    let _ = (paths, manifest);
    Err(anyhow::anyhow!("WP3: not implemented"))
}
