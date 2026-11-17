//! The manifest, `/miroir/miroir.yaml` (§4.1): how to run the product, bake its world, and
//! which ports the agent may reach. Scaffold: WP1 implements it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// A world's manifest.
#[derive(Debug, Clone)]
pub struct Manifest {
    /// The only TCP ports that may listen off loopback: the agent's door.
    pub expose: Vec<u16>,
    /// Datastores first.
    pub processes: Vec<Process>,
    /// Layered over the container's environment for every process and command.
    pub env: BTreeMap<String, String>,
    /// Commands run at build with only the datastores up.
    pub bake: Vec<Vec<String>>,
    /// Commands run at build with every process up.
    pub seed: Vec<Vec<String>>,
    pub database: Database,
    pub sinks: Option<Sinks>,
    /// A command that exits 0 when background work is done.
    pub quiet: Option<Vec<String>>,
}

/// A process of the world.
#[derive(Debug, Clone)]
pub struct Process {
    pub name: String,
    pub command: Vec<String>,
    /// The user to run as; `None` runs as Miroir's own user (root).
    pub user: Option<String>,
    pub cwd: Option<PathBuf>,
    /// Overrides the manifest `env` for this process.
    pub env: BTreeMap<String, String>,
    /// Started before the date shift and before every other process.
    pub datastore: bool,
    pub ready: Option<Ready>,
}

/// How to tell that a process is ready, on 127.0.0.1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ready {
    Tcp(u16),
    Http(String),
}

/// The graded database.
#[derive(Debug, Clone)]
pub struct Database {
    /// `postgres://…/<db>`, reached as a superuser.
    pub url: String,
    /// `table` or `table.column` entries left out of capture.
    pub ignore: Vec<String>,
    pub time: DbTime,
}

/// How the date shift treats the database's time columns.
#[derive(Debug, Clone)]
pub struct DbTime {
    /// `table.column` entries never shifted.
    pub exclude: Vec<String>,
    /// `table.column` globs of integer columns holding Unix time.
    pub epoch: BTreeMap<String, EpochUnit>,
    /// When the data the `bake` commands restore was captured.
    pub captured: Option<jiff::Timestamp>,
}

/// The unit of a Unix-time column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EpochUnit {
    S,
    Ms,
}

impl EpochUnit {
    /// Units per second: 1 or 1000.
    pub fn per_second(self) -> i64 {
        match self {
            EpochUnit::S => 1,
            EpochUnit::Ms => 1000,
        }
    }
}

/// The loopback recorders of outbound email and HTTP.
#[derive(Debug, Clone)]
pub struct Sinks {
    pub smtp: Option<u16>,
    pub http: Option<u16>,
    /// Canned HTTP answers by `"[METHOD ]/path[*]"`.
    pub responses: BTreeMap<String, CannedResponse>,
}

/// A canned HTTP answer of the HTTP sink.
#[derive(Debug, Clone)]
pub struct CannedResponse {
    pub status: u16,
    pub json: Option<Value>,
    pub body: Option<String>,
    pub headers: BTreeMap<String, String>,
}

/// A manifest error: the field path and what to change.
#[derive(Debug, thiserror::Error)]
#[error("{field}: {message}")]
pub struct ManifestError {
    pub field: String,
    pub message: String,
}

impl Manifest {
    /// Load and validate the manifest at `path`.
    pub fn load(path: &Path) -> Result<Manifest, ManifestError> {
        Err(ManifestError {
            field: path.display().to_string(),
            message: "WP1: not implemented".into(),
        })
    }

    /// The datastore processes, in manifest order.
    pub fn datastores(&self) -> Vec<&Process> {
        Vec::new()
    }

    /// The other processes, in manifest order.
    pub fn apps(&self) -> Vec<&Process> {
        Vec::new()
    }
}
