//! The loopback SMTP and HTTP sinks: they record the email and requests the product sends, as
//! `effects.jsonl` lines (§11.1 journal shapes), and answer HTTP with canned responses. A `sink`
//! fault line is appended when the HTTP sink cannot read a request (its body is over 25 MiB)
//! and when an email over 25 MiB is recorded truncated. Scaffold: WP1 implements it.

use std::path::PathBuf;

use crate::manifest::Sinks;

/// The journals the sinks record to.
#[derive(Debug, Clone)]
pub struct Recording {
    pub effects: PathBuf,
    pub faults: PathBuf,
}

/// Start the sinks the manifest lists, recording to `recording` when it is set.
#[expect(clippy::unused_async, reason = "scaffold: WP1 awaits once it implements this")]
pub async fn start(sinks: &Sinks, recording: Option<Recording>) -> std::io::Result<()> {
    let _ = (sinks, recording);
    Err(std::io::Error::other("WP1: not implemented"))
}
