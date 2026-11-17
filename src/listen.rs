//! The boot's listener audit (§3.5 step 8): only the manifest's `expose` ports may listen off
//! loopback, and each of them must. Scaffold: WP1 implements it.

use std::net::IpAddr;

use crate::manifest::Manifest;

/// A TCP socket in the LISTEN state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listener {
    pub addr: IpAddr,
    pub port: u16,
}

/// The LISTEN rows of one `/proc/net/tcp` or `/proc/net/tcp6` text.
pub fn parse(table: &str) -> Vec<Listener> {
    let _ = table;
    Vec::new()
}

/// The listeners of this network namespace, from both files.
pub fn current() -> std::io::Result<Vec<Listener>> {
    Err(std::io::Error::other("WP1: not implemented"))
}

/// Whether `addr` is loopback: 127.0.0.0/8, `::1` or `::ffff:127.0.0.0/104`.
pub fn is_loopback(addr: IpAddr) -> bool {
    let _ = addr;
    false
}

/// The two rules of §3.5 step 8. The error holds every violation's message, one per line, each
/// naming the manifest process whose ready probe uses the port, when one does.
pub fn audit(listeners: &[Listener], manifest: &Manifest) -> Result<(), String> {
    let _ = (listeners, manifest);
    Err("WP1: not implemented".into())
}
