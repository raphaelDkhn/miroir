//! Folding the change log into net changes against the boot catalog, and diffing the boot
//! catalog against the live one (§3.6 step 4). Scaffold: WP2 implements it.

use crate::evidence::NetChange;
use crate::pg::{Ignore, LogRow, Table};

/// The net changes of `log` against `tables`, the boot catalog (`catalog.json`); each
/// `NetChange::tbl` is `Table::evidence_name`. An error is Miroir's own failure.
pub fn fold(log: &[LogRow], tables: &[Table], ignore: &Ignore) -> Result<Vec<NetChange>, String> {
    let _ = (log, tables, ignore);
    Err("WP2: not implemented".into())
}

/// A `schema` change for every table created, dropped or reshaped since boot, or whose capture
/// trigger is gone, and a `truncate` change for every table re-created.
pub fn schema_changes(boot: &[Table], now: &[Table], ignore: &Ignore) -> Vec<NetChange> {
    let _ = (boot, now, ignore);
    Vec::new()
}
