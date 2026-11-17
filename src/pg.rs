//! The graded Postgres database: its catalog, change capture, the date shift and the change log
//! (§3.4, §3.5, §3.6). Scaffold: WP2 implements it.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::manifest::Database;

/// The most bytes of change-log row text (`old::text` plus `new::text`) collect reads.
pub const LOG_BUDGET: u64 = 64 * 1024 * 1024;

/// A table of the catalog.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Table {
    pub name: String,
    pub schema: String,
    pub qualified: String,
    pub columns: Vec<Column>,
    pub partitioned: bool,
    pub id: i64,
    pub captured: bool,
}

impl Table {
    /// The name in the evidence, `changes.tbl`, `allow` and `database.ignore`: `name` in schema
    /// `public`, `schema.name` elsewhere.
    pub fn evidence_name(&self) -> String {
        String::new()
    }
}

/// A column of a catalog table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Column {
    pub name: String,
    pub kind: ColumnKind,
    pub generated: bool,
    pub pk: bool,
    pub sql_type: String,
}

/// How the date shift moves a column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ColumnKind {
    Date,
    Timestamp,
    UnixTime { per_second: i64, max: u64 },
    Other,
}

/// The date shift: whole elapsed days for timestamps and Unix times, UTC calendar days for
/// dates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shift {
    pub timestamps: i64,
    pub dates: i64,
}

/// One row of `miroir.log`.
#[derive(Debug, Clone)]
pub struct LogRow {
    pub tbl: String,
    pub op: String,
    pub old: Option<String>,
    pub new: Option<String>,
}

/// The tables and columns `database.ignore` leaves out of capture.
#[derive(Debug, Default)]
pub struct Ignore {
    pub tables: BTreeSet<String>,
    pub columns: BTreeMap<String, BTreeSet<String>>,
}

/// The live catalog and the change log, read in one transaction.
#[derive(Debug)]
pub struct Snapshot {
    pub tables: Vec<Table>,
    pub log: Vec<LogRow>,
}

/// A database failure.
#[derive(Debug, thiserror::Error)]
pub enum PgError {
    #[error("the database does not answer: {0}")]
    Unreachable(String),
    #[error("the change log's text passed its {budget}-byte budget after {rows} rows")]
    OverBudget { rows: u64, budget: u64 },
    #[error("{0}")]
    Sql(String),
}

/// The graded database, reached as a superuser.
#[derive(Debug)]
pub struct Pg {
    database: Database,
}

impl Pg {
    /// The database at `database.url`.
    pub fn new(database: &Database) -> Pg {
        Pg { database: database.clone() }
    }

    /// Wait until the database answers, within `limit`.
    #[expect(
        clippy::unused_async,
        clippy::unused_async_trait_impl,
        reason = "scaffold: WP2 awaits once it implements this"
    )]
    pub async fn wait_answering(&self, limit: Duration) -> Result<(), PgError> {
        let _ = (&self.database, limit);
        Err(PgError::Unreachable("WP2: not implemented".into()))
    }

    /// The catalog, with epoch columns marked.
    #[expect(
        clippy::unused_async,
        clippy::unused_async_trait_impl,
        reason = "scaffold: WP2 awaits once it implements this"
    )]
    pub async fn catalog(&self) -> Result<Vec<Table>, PgError> {
        Err(PgError::Sql("WP2: not implemented".into()))
    }

    /// Resolve `database.ignore` against `tables`; an unknown table or column is an error.
    pub fn ignore(&self, tables: &[Table]) -> Result<Ignore, PgError> {
        let _ = tables;
        Err(PgError::Sql("WP2: not implemented".into()))
    }

    /// Install change capture on every table not ignored.
    #[expect(
        clippy::unused_async,
        clippy::unused_async_trait_impl,
        reason = "scaffold: WP2 awaits once it implements this"
    )]
    pub async fn install_capture(&self, tables: &[Table], ignore: &Ignore) -> Result<(), PgError> {
        let _ = (tables, ignore);
        Err(PgError::Sql("WP2: not implemented".into()))
    }

    /// Truncate `miroir.log`.
    #[expect(
        clippy::unused_async,
        clippy::unused_async_trait_impl,
        reason = "scaffold: WP2 awaits once it implements this"
    )]
    pub async fn clear_log(&self) -> Result<(), PgError> {
        Err(PgError::Sql("WP2: not implemented".into()))
    }

    /// Shift the time columns of `tables` by `shift`, with no trigger firing.
    #[expect(
        clippy::unused_async,
        clippy::unused_async_trait_impl,
        reason = "scaffold: WP2 awaits once it implements this"
    )]
    pub async fn shift(&self, tables: &[Table], shift: Shift) -> Result<(), PgError> {
        let _ = (tables, shift);
        Err(PgError::Sql("WP2: not implemented".into()))
    }

    /// The live catalog, then `SELECT tbl, op, old::text, new::text FROM miroir.log ORDER BY
    /// seq` streamed, in one REPEATABLE READ READ ONLY transaction. Stops with `OverBudget` as
    /// soon as the text read passes `budget`, so memory stays within the budget plus one row.
    #[expect(
        clippy::unused_async,
        clippy::unused_async_trait_impl,
        reason = "scaffold: WP2 awaits once it implements this"
    )]
    pub async fn snapshot(&self, budget: u64) -> Result<Snapshot, PgError> {
        let _ = budget;
        Err(PgError::Sql("WP2: not implemented".into()))
    }
}

/// The shift that moves data from `from` to `to`.
pub fn shift_between(from: jiff::Timestamp, to: jiff::Timestamp) -> Shift {
    let _ = (from, to);
    Shift { timestamps: 0, dates: 0 }
}
