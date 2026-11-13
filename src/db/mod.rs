//! App databases, Postgres or `MySQL`, behind one interface: catalog reads, change capture,
//! time shifts, snapshots and fingerprints.

mod mysql;
mod postgres;

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use reqwest::Url;
use tokio::time::{Instant, sleep, timeout};

const PING_LIMIT: Duration = Duration::from_secs(5);
const CONNECT_LIMIT: Duration = Duration::from_secs(30);
const DEFAULT_LOG_BUDGET_MB: i64 = 1024;
const MAP_BATCH_BYTES: usize = 8 * 1024 * 1024;
const NEVER_FROM: &str = "9000-01-01";
const NEVER_FROM_UNIX: i64 = 221_845_392_000;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct DbError(pub String);

impl From<sqlx::Error> for DbError {
    fn from(e: sqlx::Error) -> Self {
        DbError(e.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    Postgres,
    MySql,
}

#[derive(Debug, Clone)]
pub struct Table {
    /// Name as checks see it, schema-qualified outside the default schema.
    pub name: String,
    pub schema: String,
    /// Quoted `schema.table` for SQL.
    pub qualified: String,
    pub columns: Vec<Column>,
    /// A partitioned Postgres table, which updates reach through its partitions.
    pub partitioned: bool,
    /// Catalog id, which changes when the table is re-created.
    pub id: i64,
    /// Whether the capture triggers are on it.
    pub captured: bool,
}

/// A column: how it holds time, and whether it is generated or part of the key.
#[derive(Debug, Clone)]
pub struct Column {
    pub name: String,
    pub kind: ColumnKind,
    pub generated: bool,
    pub pk: bool,
    pub sql_type: String,
}

/// How a column holds time, which says how a shift moves it.
///
/// `EpochSeconds` is a `MySQL` `TIMESTAMP`, 1970 to 2038; `UnixTime` is an integer
/// counting `per_second` units a second, up to `max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnKind {
    Date,
    Timestamp,
    EpochSeconds,
    UnixTime { per_second: i64, max: u64 },
    Other,
}

/// Days a reset moves the data: timestamps by whole days, dates by calendar days.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shift {
    pub timestamps: i64,
    pub dates: i64,
}

/// How far data moves: seconds for timestamps, days for dates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Move {
    pub seconds: i64,
    pub dates: i64,
}

impl From<Shift> for Move {
    fn from(shift: Shift) -> Move {
        Move { seconds: shift.timestamps.saturating_mul(86_400), dates: shift.dates }
    }
}

impl Move {
    fn of(self, kind: ColumnKind) -> i64 {
        if kind == ColumnKind::Date { self.dates } else { self.seconds }
    }

    fn order(self) -> &'static str {
        if self.dates > 0 || self.seconds > 0 { "DESC" } else { "ASC" }
    }
}

/// A change-log row, with the row's images as JSON.
#[derive(Debug, Clone)]
pub struct LogRow {
    pub tbl: String,
    pub op: String,
    pub old: Option<String>,
    pub new: Option<String>,
}

/// One consistent read: the tables, the change log, and the checks' values.
pub struct Snapshot {
    pub tables: Vec<Table>,
    pub log: Result<Vec<LogRow>, String>,
    pub scores: Vec<Result<Option<String>, String>>,
}

/// Tables, and columns by table, whose changes don't count.
#[derive(Debug, Default)]
pub struct Ignore {
    pub tables: BTreeSet<String>,
    pub columns: BTreeMap<String, BTreeSet<String>>,
}

/// An app's database.
pub struct Db {
    pub dialect: Dialect,
    url: String,
    schemas: Option<Vec<String>>,
    ignore: Vec<String>,
    exclude: Vec<String>,
    epoch: Vec<(String, i64)>,
    log_budget: i64,
}

impl Table {
    /// Columns that hold time and aren't generated.
    pub fn time_columns(&self) -> impl Iterator<Item = &Column> {
        self.columns.iter().filter(|c| c.kind != ColumnKind::Other && !c.generated)
    }

    fn moving(&self, exclude: &BTreeSet<(String, String)>, by: Move) -> Vec<&Column> {
        self.time_columns()
            .filter(|c| by.of(c.kind) != 0)
            .filter(|c| !exclude.contains(&(self.name.clone(), c.name.clone())))
            .collect()
    }
}

impl Db {
    /// Each app's database, splitting `MIROIR_LOG_BUDGET_MB` (1024 by default) between
    /// them.
    pub fn for_apps(manifest: &crate::manifest::Manifest) -> Result<BTreeMap<String, Db>, DbError> {
        let budget_mb = match std::env::var("MIROIR_LOG_BUDGET_MB") {
            Ok(text) => text.parse::<i64>().map_err(|_| {
                DbError(format!("MIROIR_LOG_BUDGET_MB must be a whole number, not '{text}'"))
            })?,
            Err(_) => DEFAULT_LOG_BUDGET_MB,
        };
        let count = i64::try_from(manifest.databases().count()).unwrap_or(i64::MAX).max(1);
        let budget = budget_mb.saturating_mul(1024 * 1024) / count;
        Ok(manifest
            .databases()
            .map(|(app, database)| (app.to_string(), Db::new(database, budget)))
            .collect())
    }

    fn new(database: &crate::manifest::Database, log_budget: i64) -> Db {
        let dialect =
            if database.url.starts_with("mysql") { Dialect::MySql } else { Dialect::Postgres };
        Db {
            dialect,
            url: database.url.clone(),
            schemas: database.schemas.clone(),
            ignore: database.ignore.clone(),
            exclude: database.time.exclude.clone(),
            epoch: database
                .time
                .epoch
                .iter()
                .map(|(pattern, unit)| (pattern.clone(), unit.per_second()))
                .collect(),
            log_budget,
        }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// Wait until the database answers, or with `server_only` only its server.
    pub async fn wait_answering(&self, server_only: bool, limit: Duration) -> Result<(), DbError> {
        let url = if server_only { self.server_url()? } else { self.url.clone() };
        let deadline = Instant::now() + limit;
        loop {
            let ping = async {
                match self.dialect {
                    Dialect::Postgres => postgres::ping(&url).await,
                    Dialect::MySql => mysql::ping(&url).await,
                }
            };
            let result = match timeout(PING_LIMIT, ping).await {
                Ok(result) => result.map_err(|e| e.to_string()),
                Err(_) => Err("a connection attempt hung".to_string()),
            };
            match result {
                Ok(()) => return Ok(()),
                Err(e) if Instant::now() >= deadline => {
                    return Err(DbError(format!(
                        "the database did not answer within {} s: {e}",
                        limit.as_secs()
                    )));
                }
                Err(_) => sleep(Duration::from_millis(100)).await,
            }
        }
    }

    fn server_url(&self) -> Result<String, DbError> {
        let mut url = Url::parse(&self.url).map_err(|e| DbError(e.to_string()))?;
        url.set_path(match self.dialect {
            Dialect::Postgres => "/postgres",
            Dialect::MySql => "",
        });
        Ok(url.to_string())
    }

    pub async fn tables(&self) -> Result<Vec<Table>, DbError> {
        match self.dialect {
            Dialect::Postgres => postgres::tables(&self.url, self.schemas.as_deref()).await,
            Dialect::MySql => mysql::tables(&self.url, self.schemas.as_deref()).await,
        }
    }

    /// Move every time column by a reset's shift.
    pub async fn shift(&self, shift: Shift) -> Result<(), DbError> {
        self.move_by(Move::from(shift), false).await
    }

    /// Move every time column during an episode, and the change log's copies with it.
    pub async fn age(&self, by: Move) -> Result<(), DbError> {
        self.move_by(by, true).await
    }

    async fn move_by(&self, by: Move, live: bool) -> Result<(), DbError> {
        let mut tables = self.tables().await?;
        self.mark_epoch(&mut tables)?;
        let exclude = self.resolve_exclude(&tables)?;
        match self.dialect {
            Dialect::Postgres => postgres::shift(&self.url, &tables, &exclude, by, live).await,
            Dialect::MySql => mysql::shift(&self.url, &tables, &exclude, by, live).await,
        }
    }

    /// Reclaim space before the state is saved; Postgres only.
    pub async fn compact(&self) -> Result<(), DbError> {
        match self.dialect {
            Dialect::Postgres => postgres::execute(&self.url, "VACUUM (FULL, ANALYZE)").await,
            Dialect::MySql => Ok(()),
        }
    }

    pub async fn execute(&self, sql: &str) -> Result<(), DbError> {
        match self.dialect {
            Dialect::Postgres => postgres::execute(&self.url, sql).await,
            Dialect::MySql => mysql::execute(&self.url, sql).await,
        }
    }

    /// Install the triggers that log every change; returns warnings about what they miss.
    pub async fn install_capture(&self) -> Result<Vec<String>, DbError> {
        let mut tables = self.tables().await?;
        let ignore = self.resolve_ignore(&tables)?;
        self.resolve_exclude(&tables)?;
        self.mark_epoch(&mut tables.clone())?;
        tables.retain(|table| !ignore.tables.contains(&table.name));
        match self.dialect {
            Dialect::Postgres => postgres::install_capture(&self.url, &tables).await,
            Dialect::MySql => mysql::install_capture(&self.url, &tables).await,
        }
    }

    /// Empty the change log.
    pub async fn clear_log(&self) -> Result<(), DbError> {
        let cleared = match self.dialect {
            Dialect::Postgres => postgres::clear_log(&self.url).await,
            Dialect::MySql => mysql::clear_log(&self.url).await,
        };
        cleared.map_err(|e| {
            DbError(format!("{e}; if an app holds a transaction open, make quiet wait for it"))
        })
    }

    /// Run a scalar query read-only.
    pub async fn value(&self, sql: &str) -> Result<Option<String>, DbError> {
        match self.dialect {
            Dialect::Postgres => postgres::value(&self.url, sql).await,
            Dialect::MySql => mysql::value(&self.url, sql).await,
        }
    }

    /// Read the tables, the change log and each check's value in one read-only snapshot.
    pub async fn snapshot(&self, checks: &[&str]) -> Result<Snapshot, DbError> {
        let schemas = self.schemas.as_deref();
        match self.dialect {
            Dialect::Postgres => {
                postgres::snapshot(&self.url, schemas, checks, self.log_budget).await
            }
            Dialect::MySql => mysql::snapshot(&self.url, schemas, checks, self.log_budget).await,
        }
    }

    /// Hash each table's rows over the columns that don't hold time.
    pub async fn fingerprint(&self) -> Result<BTreeMap<String, String>, DbError> {
        let mut tables = self.tables().await?;
        self.mark_epoch(&mut tables)?;
        let ignore = self.resolve_ignore(&tables)?;
        let mut hashed = Vec::new();
        for table in tables {
            if ignore.tables.contains(&table.name) {
                continue;
            }
            let skip = ignore.columns.get(&table.name);
            let columns: Vec<String> = table
                .columns
                .iter()
                .filter(|c| {
                    c.kind == ColumnKind::Other && !skip.is_some_and(|s| s.contains(&c.name))
                })
                .map(|c| c.name.clone())
                .collect();
            hashed.push((table, columns));
        }
        match self.dialect {
            Dialect::Postgres => postgres::fingerprint(&self.url, &hashed).await,
            Dialect::MySql => mysql::fingerprint(&self.url, &hashed).await,
        }
    }

    /// Create an empty database `name` on the same server.
    pub async fn scratch(&self, name: &str) -> Result<Db, DbError> {
        let mut url = Url::parse(&self.url).map_err(|e| DbError(e.to_string()))?;
        url.set_path(&format!("/{name}"));
        let quoted = quote(self.dialect, name);
        let server = self.server_url()?;
        for sql in
            [format!("DROP DATABASE IF EXISTS {quoted}"), format!("CREATE DATABASE {quoted}")]
        {
            match self.dialect {
                Dialect::Postgres => postgres::execute(&server, &sql).await?,
                Dialect::MySql => mysql::execute(&server, &sql).await?,
            }
        }
        Ok(Db {
            dialect: self.dialect,
            url: url.to_string(),
            schemas: (self.dialect == Dialect::MySql).then(|| vec![name.to_string()]),
            ignore: Vec::new(),
            exclude: Vec::new(),
            epoch: Vec::new(),
            log_budget: self.log_budget,
        })
    }

    /// A column's distinct non-null values, as text.
    pub async fn distinct(&self, table: &Table, column: &str) -> Result<Vec<String>, DbError> {
        match self.dialect {
            Dialect::Postgres => postgres::distinct(&self.url, table, column).await,
            Dialect::MySql => mysql::distinct(&self.url, table, column).await,
        }
    }

    /// Replace a column's values by `(old, new)` pairs, or blank it with `None`; returns
    /// the rows changed.
    pub async fn rewrite(
        &self,
        table: &Table,
        column: &str,
        pairs: Option<&[(String, String)]>,
    ) -> Result<u64, DbError> {
        match self.dialect {
            Dialect::Postgres => postgres::rewrite(&self.url, table, column, pairs).await,
            Dialect::MySql => mysql::rewrite(&self.url, table, column, pairs).await,
        }
    }

    /// Resolve `database.ignore` against the tables; a key column can't be ignored.
    pub fn resolve_ignore(&self, tables: &[Table]) -> Result<Ignore, DbError> {
        let mut ignore = Ignore::default();
        for entry in &self.ignore {
            if tables.iter().any(|t| &t.name == entry) {
                ignore.tables.insert(entry.clone());
                continue;
            }
            let (table, column) = split_column(tables, entry)
                .ok_or_else(|| DbError(format!("database.ignore: no table or column '{entry}'")))?;
            let known = tables.iter().find(|t| t.name == table);
            if known.is_some_and(|t| t.columns.iter().any(|c| c.pk && c.name == column)) {
                let message = format!("database.ignore: '{entry}' identifies the table's rows");
                return Err(DbError(message));
            }
            ignore.columns.entry(table).or_default().insert(column);
        }
        Ok(ignore)
    }

    fn mark_epoch(&self, tables: &mut [Table]) -> Result<(), DbError> {
        for (pattern, per_second) in &self.epoch {
            let (table_glob, column_glob) = pattern.rsplit_once('.').unwrap_or(("", pattern));
            let mut found = false;
            for table in tables.iter_mut().filter(|t| glob(table_glob, &t.name)) {
                for column in table.columns.iter_mut().filter(|c| glob(column_glob, &c.name)) {
                    let unmarked = match column.kind {
                        ColumnKind::Other | ColumnKind::UnixTime { .. } => true,
                        ColumnKind::Date | ColumnKind::Timestamp | ColumnKind::EpochSeconds => {
                            false
                        }
                    };
                    if let (true, Some(max)) = (unmarked, integer_max(&column.sql_type)) {
                        column.kind = ColumnKind::UnixTime { per_second: *per_second, max };
                        found = true;
                    }
                }
            }
            if !found {
                let message = format!(
                    "database.time.epoch: '{pattern}' matches no 32- or 64-bit integer column"
                );
                return Err(DbError(message));
            }
        }
        Ok(())
    }

    fn resolve_exclude(&self, tables: &[Table]) -> Result<BTreeSet<(String, String)>, DbError> {
        let mut exclude = BTreeSet::new();
        for entry in &self.exclude {
            let found = split_column(tables, entry)
                .ok_or_else(|| DbError(format!("database.time.exclude: no column '{entry}'")))?;
            exclude.insert(found);
        }
        Ok(exclude)
    }
}

fn map_batches(pairs: &[(String, String)]) -> Result<Vec<String>, DbError> {
    let mut batches = Vec::new();
    let mut batch: Vec<[&str; 2]> = Vec::new();
    let mut size = 0;
    for (old, new) in pairs {
        size += old.len() + new.len() + 8;
        batch.push([old, new]);
        if size >= MAP_BATCH_BYTES {
            batches.push(serde_json::to_string(&batch).map_err(|e| DbError(e.to_string()))?);
            (batch, size) = (Vec::new(), 0);
        }
    }
    if !batch.is_empty() {
        batches.push(serde_json::to_string(&batch).map_err(|e| DbError(e.to_string()))?);
    }
    Ok(batches)
}

/// Whether `text` matches `pattern`, where `*` stands for any run of characters.
pub fn glob(pattern: &str, text: &str) -> bool {
    let mut parts = pattern.split('*');
    let Some(mut rest) = text.strip_prefix(parts.next().unwrap_or_default()) else {
        return false;
    };
    let mut parts: Vec<&str> = parts.collect();
    let Some(last) = parts.pop() else { return rest.is_empty() };
    for part in parts {
        let Some(at) = rest.find(part) else { return false };
        rest = &rest[at + part.len()..];
    }
    rest.ends_with(last)
}

fn integer_max(sql_type: &str) -> Option<u64> {
    let sql_type = sql_type.to_ascii_lowercase();
    let base = sql_type.split(['(', ' ']).next().unwrap_or_default();
    let unsigned = sql_type.contains("unsigned");
    match (base, unsigned) {
        ("integer" | "int", false) => Some(u64::from(i32::MAX.unsigned_abs())),
        ("integer" | "int", true) => Some(u64::from(u32::MAX)),
        ("bigint", false) => Some(i64::MAX.unsigned_abs()),
        ("bigint", true) => Some(u64::MAX),
        _ => None,
    }
}

fn unix_shift(q: &str, seconds: i64, per_second: i64, max: u64) -> (String, String) {
    let delta = seconds.saturating_mul(per_second);
    let never = NEVER_FROM_UNIX.saturating_mul(per_second);
    let room = if delta >= 0 {
        format!("{q} <= {}", max.saturating_sub(delta.unsigned_abs()))
    } else {
        format!("{q} > {}", delta.unsigned_abs())
    };
    (format!("{q} > 0 AND {q} < {never} AND {room}"), format!("{q} + ({delta})"))
}

fn split_column(tables: &[Table], entry: &str) -> Option<(String, String)> {
    let (table, column) = entry.rsplit_once('.')?;
    let known = tables.iter().find(|t| t.name == table)?;
    known.columns.iter().any(|c| c.name == column).then(|| (table.to_string(), column.to_string()))
}

fn collided(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .is_some_and(|e| e.is_unique_violation() || e.code().as_deref() == Some("23P01"))
}

fn over_budget(size: i64, budget: i64) -> String {
    let mb = |bytes: i64| bytes / (1024 * 1024);
    format!(
        "the change log takes {} MiB, over the {} MiB budget (MIROIR_LOG_BUDGET_MB)",
        mb(size),
        mb(budget)
    )
}

fn quote(dialect: Dialect, name: &str) -> String {
    match dialect {
        Dialect::Postgres => format!("\"{}\"", name.replace('"', "\"\"")),
        Dialect::MySql => format!("`{}`", name.replace('`', "``")),
    }
}

fn check_sql(dialect: Dialect, sql: &str) -> String {
    let sql = sql.trim().trim_end_matches(';');
    match dialect {
        Dialect::Postgres => format!("SELECT CAST((\n{sql}\n) AS TEXT)"),
        Dialect::MySql => format!("SELECT CAST((\n{sql}\n) AS CHAR)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn never_from_unix_is_never_from() {
        let never: jiff::Timestamp = format!("{NEVER_FROM}T00:00:00Z").parse().unwrap();
        assert_eq!(never.as_second(), NEVER_FROM_UNIX);
    }

    #[test]
    fn glob_stars_stand_for_any_run_of_characters() {
        for (pattern, text, matches) in [
            ("*_unix", "created_unix", true),
            ("*_unix", "unix_created", false),
            ("*", "anything", true),
            ("branch", "branch", true),
            ("branch", "branches", false),
            ("a*b*c", "a-b-b-c", true),
            ("a*b*c", "a-c", false),
            ("ab*", "a", false),
            ("*.*", "public.issue", true),
        ] {
            assert_eq!(glob(pattern, text), matches, "{pattern} ~ {text}");
        }
    }

    #[test]
    fn only_32_and_64_bit_integers_hold_unix_time() {
        assert_eq!(integer_max("integer"), Some(2_147_483_647));
        assert_eq!(integer_max("bigint"), Some(9_223_372_036_854_775_807));
        assert_eq!(integer_max("int(11)"), Some(2_147_483_647));
        assert_eq!(integer_max("int(10) unsigned"), Some(4_294_967_295));
        assert_eq!(integer_max("bigint unsigned"), Some(u64::MAX));
        for other in ["smallint", "tinyint(1)", "numeric(12,2)", "text", "timestamp"] {
            assert_eq!(integer_max(other), None, "{other}");
        }
    }

    #[test]
    fn unix_shift_stops_at_the_type_and_at_zero() {
        let (guard, moved) = unix_shift("t", 864_000, 1, 2_147_483_647);
        assert_eq!(guard, "t > 0 AND t < 221845392000 AND t <= 2146619647");
        assert_eq!(moved, "t + (864000)");
        let (guard, moved) = unix_shift("t", -86_400, 1000, u64::MAX);
        assert_eq!(guard, "t > 0 AND t < 221845392000000 AND t > 86400000");
        assert_eq!(moved, "t + (-86400000)");
        let (guard, moved) = unix_shift("t", -1_800, 1000, u64::MAX);
        assert_eq!(guard, "t > 0 AND t < 221845392000000 AND t > 1800000", "half an hour back");
        assert_eq!(moved, "t + (-1800000)");
    }

    #[test]
    fn a_shift_moves_timestamps_by_its_days_in_seconds() {
        let by = Move::from(Shift { timestamps: -2, dates: -3 });
        assert_eq!(by, Move { seconds: -172_800, dates: -3 });
        assert_eq!((by.of(ColumnKind::Date), by.of(ColumnKind::Timestamp)), (-3, -172_800));
        assert_eq!(by.order(), "ASC");
        assert_eq!(Move { seconds: 60, dates: 0 }.order(), "DESC");
    }
}
