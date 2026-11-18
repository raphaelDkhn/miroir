//! The graded Postgres database: its catalog, change capture into `miroir.log`, the date shift,
//! and the one consistent read of the catalog and the change log that collect folds.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sqlx::postgres::PgRow;
use sqlx::{AssertSqlSafe, Connection, PgConnection, Row};
use tokio::time::{Instant, sleep, timeout};

use crate::manifest::{Database, EpochUnit};

/// The most change log collect reads, in bytes: each row's text (`old::text` plus `new::text`)
/// plus `LOG_ROW_COST`.
pub const LOG_BUDGET: u64 = 64 * 1024 * 1024;
/// Read and folded, a row takes a kilobyte or more however little text it has, and a truncate
/// rescans the rows folded so far, so no row reads for free.
const LOG_ROW_COST: u64 = 1024;

const PING_LIMIT: Duration = Duration::from_secs(5);
const CONNECT_LIMIT: Duration = Duration::from_secs(30);
const DAY_SECONDS: i64 = 86_400;
/// Values on or after this date, such as `9999-12-31` for "never", are never shifted.
const NEVER_FROM: &str = "9000-01-01";
/// [`NEVER_FROM`] in Unix seconds.
const NEVER_FROM_UNIX: i64 = 221_845_392_000;

const TABLES: &str = r"
SELECT n.nspname::text, c.relname::text, c.relkind::text, a.attname::text,
       format_type(a.atttypid, NULL), a.attgenerated::text,
       COALESCE(a.attnum = ANY(i.indkey::int2[]), false),
       c.oid::bigint, format_type(a.atttypid, a.atttypmod),
       (SELECT count(*) FROM pg_trigger t WHERE t.tgrelid = c.oid AND t.tgenabled IN ('O', 'A')
          AND t.tgname IN ('miroir_capture', 'miroir_truncate')) = 2
FROM pg_class c
JOIN pg_namespace n ON n.oid = c.relnamespace
JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped
LEFT JOIN pg_index i ON i.indrelid = c.oid AND i.indisprimary
WHERE c.relkind IN ('r', 'p') AND NOT c.relispartition
  AND n.nspname NOT IN ('miroir', 'information_schema') AND n.nspname NOT LIKE 'pg\_%'
ORDER BY n.nspname, c.relname, a.attnum";

const CAPTURE: &str = r"
DO $$ BEGIN
  IF NOT (SELECT rolsuper FROM pg_roles WHERE rolname = current_user) THEN
    RAISE EXCEPTION 'role % is not a superuser', current_user;
  END IF;
END $$;
CREATE SCHEMA IF NOT EXISTS miroir;
CREATE TABLE IF NOT EXISTS miroir.log (
  seq bigserial PRIMARY KEY, tbl text NOT NULL, op text NOT NULL, old jsonb, new jsonb);
CREATE OR REPLACE FUNCTION miroir.capture() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog SET timezone = 'UTC' AS $$
BEGIN
  IF TG_OP = 'TRUNCATE' THEN
    INSERT INTO miroir.log (tbl, op) VALUES (TG_ARGV[0], 'truncate');
  ELSIF TG_OP = 'INSERT' THEN
    INSERT INTO miroir.log (tbl, op, new) VALUES (TG_ARGV[0], 'insert', to_jsonb(NEW));
  ELSIF TG_OP = 'UPDATE' THEN
    INSERT INTO miroir.log (tbl, op, old, new)
    VALUES (TG_ARGV[0], 'update', to_jsonb(OLD), to_jsonb(NEW));
  ELSE
    INSERT INTO miroir.log (tbl, op, old) VALUES (TG_ARGV[0], 'delete', to_jsonb(OLD));
  END IF;
  RETURN NULL;
END $$;";

const LOG: &str = "SELECT tbl, op, old::text, new::text FROM miroir.log ORDER BY seq";

/// A table of the catalog.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Table {
    /// The name in its schema.
    pub name: String,
    pub schema: String,
    /// `"schema"."name"`, quoted for SQL.
    pub qualified: String,
    /// In table order.
    pub columns: Vec<Column>,
    /// A partitioned table, whose rows live in its partitions.
    pub partitioned: bool,
    /// The catalog id, which changes when the table is re-created.
    pub id: i64,
    /// Whether both capture triggers are on it and enabled.
    pub captured: bool,
}

impl Table {
    /// The name in the evidence, `changes.tbl`, `allow` and `database.ignore`: `name` in schema
    /// `public`, `schema.name` elsewhere.
    pub fn evidence_name(&self) -> String {
        if self.schema == "public" {
            self.name.clone()
        } else {
            format!("{}.{}", self.schema, self.name)
        }
    }
}

/// A column of a catalog table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Column {
    pub name: String,
    pub kind: ColumnKind,
    /// A generated column, which the date shift never writes.
    pub generated: bool,
    /// Part of the primary key.
    pub pk: bool,
    /// The type as Postgres formats it, with its modifier: `timestamp(3) with time zone`.
    pub sql_type: String,
}

/// How the date shift moves a column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ColumnKind {
    /// By UTC calendar days.
    Date,
    /// By whole elapsed days.
    Timestamp,
    /// An integer counting `per_second` units a second, up to `max`, by whole elapsed days.
    UnixTime { per_second: i64, max: u64 },
    /// Never.
    Other,
}

/// The date shift: whole elapsed days for timestamps and Unix times, UTC calendar days for
/// dates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shift {
    pub timestamps: i64,
    pub dates: i64,
}

/// One row of `miroir.log`: the table as the evidence names it, the op, and the row images as
/// JSON text.
#[derive(Debug, Clone)]
pub struct LogRow {
    pub tbl: String,
    pub op: String,
    pub old: Option<String>,
    pub new: Option<String>,
}

/// The tables and columns `database.ignore` leaves out of capture, as the evidence names them.
#[derive(Debug, Default)]
pub struct Ignore {
    pub tables: BTreeSet<String>,
    pub columns: BTreeMap<String, BTreeSet<String>>,
}

/// The live catalog and the change log, read in one transaction.
#[derive(Debug)]
pub struct Snapshot {
    /// Column kinds come from the types alone: epoch columns are not marked.
    pub tables: Vec<Table>,
    pub log: Vec<LogRow>,
}

/// A database failure.
#[derive(Debug, thiserror::Error)]
pub enum PgError {
    #[error("the database does not answer: {0}")]
    Unreachable(String),
    #[error("the change log passed its {budget}-byte budget after {rows} rows")]
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

    /// Wait until the server at `database.url` answers, within `limit`. A database that does not
    /// exist yet counts as an answer: a `bake` command may create it.
    pub async fn wait_answering(&self, limit: Duration) -> Result<(), PgError> {
        let deadline = Instant::now() + limit;
        loop {
            let error = match timeout(PING_LIMIT, ping(&self.database.url)).await {
                Ok(Ok(())) => return Ok(()),
                Ok(Err(error)) => error.to_string(),
                Err(_) => format!("a connection attempt hung for {} s", PING_LIMIT.as_secs()),
            };
            if Instant::now() >= deadline {
                return Err(PgError::Unreachable(format!(
                    "{error}, still after {} s: check the host, port and user of database.url \
                     against the datastore process",
                    limit.as_secs()
                )));
            }
            sleep(Duration::from_millis(100)).await;
        }
    }

    /// The catalog: every table outside the `miroir`, `information_schema` and `pg_*` schemas,
    /// partitions left out, with the integer columns that `database.time.epoch` names marked as
    /// Unix time. An epoch glob matching no `integer` or `bigint` column, or a
    /// `database.time.exclude` entry naming no column, is an error.
    pub async fn catalog(&self) -> Result<Vec<Table>, PgError> {
        let mut conn = connect(&self.database.url).await?;
        let mut tables = read_tables(&mut conn).await.map_err(failed("cannot read the catalog"))?;
        conn.close().await.map_err(failed("cannot close the connection"))?;
        mark_epoch(&mut tables, &self.database.time.epoch)?;
        excluded(&tables, &self.database.time.exclude)?;
        Ok(tables)
    }

    /// Resolve `database.ignore` against `tables`. An entry naming a table, as the evidence
    /// names it, ignores the table; otherwise its last dot splits table and column. An unknown
    /// table or column, or a primary-key column, is an error.
    pub fn ignore(&self, tables: &[Table]) -> Result<Ignore, PgError> {
        let mut ignore = Ignore::default();
        for entry in &self.database.ignore {
            if tables.iter().any(|table| table.evidence_name() == *entry) {
                ignore.tables.insert(entry.clone());
                continue;
            }
            let Some((table, column)) = find_column(tables, entry) else {
                return Err(PgError::Sql(format!(
                    "database.ignore: no table or column '{entry}': name a table as the evidence \
                     does (schema.table outside public), or a column as table.column"
                )));
            };
            if column.pk {
                return Err(PgError::Sql(format!(
                    "database.ignore: '{entry}' is in its table's primary key, which identifies \
                     the rows: ignore the whole table instead"
                )));
            }
            ignore.columns.entry(table.evidence_name()).or_default().insert(column.name.clone());
        }
        Ok(ignore)
    }

    /// Install change capture on every table not ignored: the `miroir` schema, `miroir.log`,
    /// `miroir.capture()` (`SECURITY DEFINER`, in UTC), and on each table the row trigger
    /// `miroir_capture` and the statement trigger `miroir_truncate`, in one transaction. A role
    /// that is not a superuser is refused, since a table owner could install capture but never
    /// run the date shift a later boot needs.
    pub async fn install_capture(&self, tables: &[Table], ignore: &Ignore) -> Result<(), PgError> {
        let mut conn = connect(&self.database.url).await?;
        sqlx::raw_sql(AssertSqlSafe(capture_sql(tables, ignore)))
            .execute(&mut conn)
            .await
            .map_err(failed("cannot install change capture"))?;
        conn.close().await.map_err(failed("cannot close the connection"))
    }

    /// Truncate `miroir.log`.
    pub async fn clear_log(&self) -> Result<(), PgError> {
        let mut conn = connect(&self.database.url).await?;
        sqlx::raw_sql("SET lock_timeout = '10s'; TRUNCATE miroir.log RESTART IDENTITY")
            .execute(&mut conn)
            .await
            .map_err(failed(
                "cannot truncate miroir.log (if a process holds a write transaction open, make \
                 the quiet command wait for it)",
            ))?;
        conn.close().await.map_err(failed("cannot close the connection"))
    }

    /// Shift the time columns of `tables` by `shift`, in one transaction with no trigger
    /// firing. Generated columns, `database.time.exclude` columns and values on or after
    /// `9000-01-01` stay; an exclude entry naming no column of `tables` is an error. A shift
    /// that moves nothing does not connect.
    pub async fn shift(&self, tables: &[Table], shift: Shift) -> Result<(), PgError> {
        let excluded = excluded(tables, &self.database.time.exclude)?;
        let mut moving = Vec::new();
        for table in tables {
            let columns = moving_columns(table, excluded.get(&table.evidence_name()), shift);
            if !columns.is_empty() {
                moving.push((table, columns));
            }
        }
        if moving.is_empty() {
            return Ok(());
        }
        let mut conn = connect(&self.database.url).await?;
        let setup = "SET session_replication_role = replica; SET TIME ZONE 'UTC'; BEGIN";
        sqlx::raw_sql(setup).execute(&mut conn).await.map_err(failed("cannot start the shift"))?;
        for (table, columns) in &moving {
            let context = format!("cannot shift the dates of {}", table.evidence_name());
            shift_table(&mut conn, table, columns, shift).await.map_err(failed(&context))?;
        }
        sqlx::raw_sql("COMMIT")
            .execute(&mut conn)
            .await
            .map_err(failed("cannot commit the shift"))?;
        conn.close().await.map_err(failed("cannot close the connection"))
    }

    /// The live catalog, then `SELECT tbl, op, old::text, new::text FROM miroir.log ORDER BY
    /// seq` streamed, in one REPEATABLE READ READ ONLY transaction. Stops with `OverBudget` as
    /// soon as the rows read pass `budget`, each counted as [`LOG_BUDGET`] says.
    pub async fn snapshot(&self, budget: u64) -> Result<Snapshot, PgError> {
        let mut conn = connect(&self.database.url).await?;
        sqlx::raw_sql("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut conn)
            .await
            .map_err(failed("cannot start the read"))?;
        let tables = read_tables(&mut conn).await.map_err(failed("cannot read the catalog"))?;
        let log = read_log(&mut conn, budget).await?;
        conn.close().await.map_err(failed("cannot close the connection"))?;
        Ok(Snapshot { tables, log })
    }
}

/// The shift that moves data from `from` to `to`: whole elapsed days for timestamps, UTC
/// calendar days for dates.
pub fn shift_between(from: jiff::Timestamp, to: jiff::Timestamp) -> Shift {
    Shift { timestamps: days_between(from, to), dates: calendar_days_between(from, to) }
}

/// Whole days from `from` to `to`, rounded down.
fn days_between(from: jiff::Timestamp, to: jiff::Timestamp) -> i64 {
    let nanos = to.duration_since(from).as_nanos();
    i64::try_from(nanos.div_euclid(i128::from(DAY_SECONDS) * 1_000_000_000)).unwrap_or(0)
}

/// UTC calendar days from `from`'s date to `to`'s.
fn calendar_days_between(from: jiff::Timestamp, to: jiff::Timestamp) -> i64 {
    to.as_second().div_euclid(DAY_SECONDS) - from.as_second().div_euclid(DAY_SECONDS)
}

/// The error of a failed database call, prefixed with `what`: `Unreachable` when the
/// connection broke, else `Sql`.
fn failed(what: &str) -> impl FnOnce(sqlx::Error) -> PgError + '_ {
    move |error| {
        let message = format!("{what}: {error}");
        if let sqlx::Error::Io(_) = error {
            PgError::Unreachable(message)
        } else {
            PgError::Sql(message)
        }
    }
}

/// The SQLSTATE of an error the server returned.
fn code(error: &sqlx::Error) -> Option<Cow<'_, str>> {
    error.as_database_error()?.code()
}

/// Whether the server refused a connection only because the database does not exist.
fn missing_database(error: &sqlx::Error) -> bool {
    code(error).is_some_and(|code| code == "3D000")
}

/// Whether a statement broke a unique or exclusion constraint.
fn collided(error: &sqlx::Error) -> bool {
    code(error).is_some_and(|code| code == "23505" || code == "23P01")
}

async fn connect(url: &str) -> Result<PgConnection, PgError> {
    match timeout(CONNECT_LIMIT, PgConnection::connect(url)).await {
        Ok(Ok(conn)) => Ok(conn),
        Ok(Err(error)) => Err(PgError::Unreachable(error.to_string())),
        Err(_) => {
            Err(PgError::Unreachable(format!("no connection after {} s", CONNECT_LIMIT.as_secs())))
        }
    }
}

/// Connect once and run `SELECT 1`; a missing database counts as an answer.
async fn ping(url: &str) -> Result<(), sqlx::Error> {
    let mut conn = match PgConnection::connect(url).await {
        Ok(conn) => conn,
        Err(error) if missing_database(&error) => return Ok(()),
        Err(error) => return Err(error),
    };
    sqlx::query("SELECT 1").execute(&mut conn).await?;
    conn.close().await
}

async fn read_tables(conn: &mut PgConnection) -> Result<Vec<Table>, sqlx::Error> {
    let rows = sqlx::query(TABLES).fetch_all(&mut *conn).await?;
    let mut tables: Vec<Table> = Vec::new();
    for row in &rows {
        let id: i64 = row.try_get(7)?;
        if tables.last().is_none_or(|table| table.id != id) {
            tables.push(table_of(row, id)?);
        }
        let column = column_of(row)?;
        if let Some(table) = tables.last_mut() {
            table.columns.push(column);
        }
    }
    Ok(tables)
}

/// The table of a catalog row, without its columns.
fn table_of(row: &PgRow, id: i64) -> Result<Table, sqlx::Error> {
    let schema: String = row.try_get(0)?;
    let name: String = row.try_get(1)?;
    Ok(Table {
        qualified: format!("{}.{}", quote(&schema), quote(&name)),
        partitioned: row.try_get::<String, _>(2)? == "p",
        columns: Vec::new(),
        captured: row.try_get(9)?,
        name,
        schema,
        id,
    })
}

/// The column of a catalog row; its kind from its type alone.
fn column_of(row: &PgRow) -> Result<Column, sqlx::Error> {
    let kind = match row.try_get::<String, _>(4)?.as_str() {
        "date" => ColumnKind::Date,
        "timestamp without time zone" | "timestamp with time zone" => ColumnKind::Timestamp,
        _ => ColumnKind::Other,
    };
    Ok(Column {
        name: row.try_get(3)?,
        kind,
        generated: !row.try_get::<String, _>(5)?.is_empty(),
        pk: row.try_get(6)?,
        sql_type: row.try_get(8)?,
    })
}

/// Mark the columns each `database.time.epoch` glob names as Unix time.
fn mark_epoch(tables: &mut [Table], epoch: &BTreeMap<String, EpochUnit>) -> Result<(), PgError> {
    for (pattern, unit) in epoch {
        let (table_glob, column_glob) = pattern.rsplit_once('.').unwrap_or(("", pattern));
        let mut found = false;
        for table in tables.iter_mut().filter(|table| glob(table_glob, &table.evidence_name())) {
            for column in &mut table.columns {
                if let (true, Some(max)) =
                    (glob(column_glob, &column.name), integer_max(&column.sql_type))
                {
                    column.kind = ColumnKind::UnixTime { per_second: unit.per_second(), max };
                    found = true;
                }
            }
        }
        if !found {
            return Err(PgError::Sql(format!(
                "database.time.epoch: '{pattern}' matches no integer or bigint column: write \
                 table.column, the table as the evidence names it, with * for any run of \
                 characters"
            )));
        }
    }
    Ok(())
}

/// The `database.time.exclude` columns by table; an entry naming no column is an error.
fn excluded(
    tables: &[Table],
    entries: &[String],
) -> Result<BTreeMap<String, BTreeSet<String>>, PgError> {
    let mut excluded: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for entry in entries {
        let Some((table, column)) = find_column(tables, entry) else {
            return Err(PgError::Sql(format!(
                "database.time.exclude: no column '{entry}': write table.column, the table as \
                 the evidence names it (schema.table outside public)"
            )));
        };
        excluded.entry(table.evidence_name()).or_default().insert(column.name.clone());
    }
    Ok(excluded)
}

/// The table and column a `table.column` entry names; its last dot splits them.
fn find_column<'t>(tables: &'t [Table], entry: &str) -> Option<(&'t Table, &'t Column)> {
    let (table, column) = entry.rsplit_once('.')?;
    let table = tables.iter().find(|t| t.evidence_name() == table)?;
    let column = table.columns.iter().find(|c| c.name == column)?;
    Some((table, column))
}

/// Whether `text` matches `pattern`, where `*` stands for any run of characters.
fn glob(pattern: &str, text: &str) -> bool {
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

/// The largest value of the integer types that can hold Unix time, by their name.
fn integer_max(sql_type: &str) -> Option<u64> {
    match sql_type {
        "integer" => Some(i32::MAX.unsigned_abs().into()),
        "bigint" => Some(i64::MAX.unsigned_abs()),
        _ => None,
    }
}

fn capture_sql(tables: &[Table], ignore: &Ignore) -> String {
    let mut script = String::from(CAPTURE);
    for table in tables {
        let name = table.evidence_name();
        if ignore.tables.contains(&name) {
            continue;
        }
        let (on, tbl) = (&table.qualified, literal(&name));
        let _ = write!(
            script,
            "\nCREATE TRIGGER miroir_capture AFTER INSERT OR UPDATE OR DELETE ON {on} \
             FOR EACH ROW EXECUTE FUNCTION miroir.capture({tbl});\
             \nCREATE TRIGGER miroir_truncate AFTER TRUNCATE ON {on} \
             FOR EACH STATEMENT EXECUTE FUNCTION miroir.capture({tbl});"
        );
    }
    script
}

/// The days `shift` moves a column of `kind`.
fn days(shift: Shift, kind: ColumnKind) -> i64 {
    match kind {
        ColumnKind::Date => shift.dates,
        ColumnKind::Timestamp | ColumnKind::UnixTime { .. } => shift.timestamps,
        ColumnKind::Other => 0,
    }
}

/// The columns of `table` that `shift` moves.
fn moving_columns<'t>(
    table: &'t Table,
    excluded: Option<&BTreeSet<String>>,
    shift: Shift,
) -> Vec<&'t Column> {
    let mut moving = Vec::new();
    for column in &table.columns {
        let kept = column.generated || excluded.is_some_and(|names| names.contains(&column.name));
        if !kept && days(shift, column.kind) != 0 {
            moving.push(column);
        }
    }
    moving
}

/// Shift one table's columns, row by row in order when moving them all at once breaks a
/// unique or exclusion constraint.
async fn shift_table(
    conn: &mut PgConnection,
    table: &Table,
    columns: &[&Column],
    shift: Shift,
) -> Result<(), sqlx::Error> {
    sqlx::raw_sql("SAVEPOINT miroir_shift").execute(&mut *conn).await?;
    let moved = sqlx::raw_sql(AssertSqlSafe(shift_sql(table, columns, shift)));
    match moved.execute(&mut *conn).await {
        Err(error) if collided(&error) => {
            sqlx::raw_sql("ROLLBACK TO SAVEPOINT miroir_shift").execute(&mut *conn).await?;
            let in_order = shift_in_order_sql(table, columns, shift);
            sqlx::raw_sql(AssertSqlSafe(in_order)).execute(&mut *conn).await?;
        }
        moved => {
            moved?;
        }
    }
    Ok(())
}

fn shift_sql(table: &Table, columns: &[&Column], shift: Shift) -> String {
    let mut sets = Vec::new();
    for column in columns {
        let q = quote(&column.name);
        let (guard, moved) = moved(&q, column.kind, shift);
        sets.push(format!("{q} = CASE WHEN {guard} THEN {moved} ELSE {q} END"));
    }
    format!("UPDATE {}{} SET {}", only(table), table.qualified, sets.join(", "))
}

/// The rows of `table` one at a time, latest first when moving forward, so that no row moves
/// onto a value another row still holds.
fn shift_in_order_sql(table: &Table, columns: &[&Column], shift: Shift) -> String {
    let direction = if shift.dates > 0 || shift.timestamps > 0 { "DESC" } else { "ASC" };
    let order: Vec<String> =
        columns.iter().map(|c| format!("{} {direction}", quote(&c.name))).collect();
    format!(
        "DO $miroir$ DECLARE r record; BEGIN \
         FOR r IN SELECT tableoid AS tbl, ctid AS id FROM {}{} ORDER BY {} LOOP \
         {} WHERE tableoid = r.tbl AND ctid = r.id; \
         END LOOP; END $miroir$",
        only(table),
        table.qualified,
        order.join(", "),
        shift_sql(table, columns, shift)
    )
}

/// The condition under which `value`, of `kind`, moves, and its moved value.
fn moved(value: &str, kind: ColumnKind, shift: Shift) -> (String, String) {
    let seconds = shift.timestamps.saturating_mul(DAY_SECONDS);
    let never = format!("{value} < '{NEVER_FROM}'");
    match kind {
        ColumnKind::UnixTime { per_second, max } => unix_shift(value, seconds, per_second, max),
        ColumnKind::Date => (never, format!("{value} + ({})", shift.dates)),
        ColumnKind::Timestamp | ColumnKind::Other => {
            (never, format!("{value} + make_interval(secs => {seconds})"))
        }
    }
}

/// A Unix time moves when it is set (above 0), before [`NEVER_FROM`], and has room in its type.
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

/// `ONLY`, unless the table is partitioned: inheritance children are tables of their own.
fn only(table: &Table) -> &'static str {
    if table.partitioned { "" } else { "ONLY " }
}

/// A quoted identifier.
fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// A string literal.
fn literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// The change log in order; `OverBudget` once its rows pass `budget`, the rest left unread
/// (the connection is then dropped, never drained).
async fn read_log(conn: &mut PgConnection, budget: u64) -> Result<Vec<LogRow>, PgError> {
    let mut read = Read::default();
    let rows = sqlx::query(LOG)
        .try_map(|row: PgRow| read.take(log_row(&row)?, budget))
        .fetch_all(&mut *conn)
        .await;
    match rows {
        Ok(log) => Ok(log),
        Err(_) if read.bytes > budget => Err(PgError::OverBudget { rows: read.rows, budget }),
        Err(error) => Err(failed("cannot read miroir.log")(error)),
    }
}

fn log_row(row: &PgRow) -> Result<LogRow, sqlx::Error> {
    Ok(LogRow {
        tbl: row.try_get(0)?,
        op: row.try_get(1)?,
        old: row.try_get(2)?,
        new: row.try_get(3)?,
    })
}

/// What a change-log read has taken so far.
#[derive(Debug, Default)]
struct Read {
    rows: u64,
    bytes: u64,
}

impl Read {
    /// Count `row`; an error, which stops the read, once the rows read pass `budget`.
    fn take(&mut self, row: LogRow, budget: u64) -> Result<LogRow, sqlx::Error> {
        self.rows += 1;
        self.bytes = self.bytes.saturating_add(LOG_ROW_COST);
        for text in [&row.old, &row.new].into_iter().flatten() {
            self.bytes = self.bytes.saturating_add(u64::try_from(text.len()).unwrap_or(u64::MAX));
        }
        if self.bytes > budget {
            return Err(sqlx::Error::Protocol(format!("the change log passed {budget} bytes")));
        }
        Ok(row)
    }
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use super::*;
    use crate::manifest::DbTime;

    fn column(name: &str, kind: ColumnKind) -> Column {
        Column { name: name.into(), kind, generated: false, pk: false, sql_type: String::new() }
    }

    fn typed(name: &str, sql_type: &str) -> Column {
        Column { sql_type: sql_type.into(), ..column(name, ColumnKind::Other) }
    }

    fn key(name: &str) -> Column {
        Column { pk: true, ..typed(name, "bigint") }
    }

    fn table(schema: &str, name: &str, columns: Vec<Column>) -> Table {
        Table {
            name: name.into(),
            schema: schema.into(),
            qualified: format!("{}.{}", quote(schema), quote(name)),
            columns,
            partitioned: false,
            id: 1,
            captured: true,
        }
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_string()).collect()
    }

    fn database(url: &str) -> Database {
        let time = DbTime { exclude: Vec::new(), epoch: BTreeMap::new(), captured: None };
        Database { url: url.into(), ignore: Vec::new(), time }
    }

    fn ignoring(entries: &[&str]) -> Pg {
        Pg::new(&Database { ignore: strings(entries), ..database("postgres://x@127.0.0.1/db") })
    }

    /// The catalog of the tests: a public table, a table outside `public`, and a public table
    /// named like that schema.
    fn crm() -> Vec<Table> {
        vec![
            table("public", "deals", vec![key("id"), typed("notes", "text")]),
            table("audit", "events", vec![key("id"), typed("payload", "jsonb")]),
            table("public", "audit", vec![key("id"), typed("events", "integer")]),
        ]
    }

    /// A URL on which nothing listens.
    fn closed_url() -> String {
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        format!("postgres://postgres@127.0.0.1:{port}/crm")
    }

    #[derive(Debug)]
    struct ServerError(&'static str);

    impl std::fmt::Display for ServerError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "SQLSTATE {}", self.0)
        }
    }

    impl std::error::Error for ServerError {}

    impl sqlx::error::DatabaseError for ServerError {
        fn message(&self) -> &str {
            self.0
        }

        fn code(&self) -> Option<Cow<'_, str>> {
            Some(Cow::Borrowed(self.0))
        }

        fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
            self
        }

        fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
            self
        }

        fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
            self
        }

        fn kind(&self) -> sqlx::error::ErrorKind {
            sqlx::error::ErrorKind::Other
        }
    }

    fn server(code: &'static str) -> sqlx::Error {
        sqlx::Error::Database(Box::new(ServerError(code)))
    }

    #[test]
    fn the_evidence_names_tables_bare_in_public_and_by_schema_elsewhere() {
        assert_eq!(table("public", "deals", Vec::new()).evidence_name(), "deals");
        assert_eq!(table("audit", "events", Vec::new()).evidence_name(), "audit.events");
        assert_eq!(table("Public", "deals", Vec::new()).evidence_name(), "Public.deals");
    }

    #[test]
    fn never_from_unix_is_never_from() {
        let never: jiff::Timestamp = format!("{NEVER_FROM}T00:00:00Z").parse().unwrap();
        assert_eq!(never.as_second(), NEVER_FROM_UNIX);
    }

    #[test]
    fn days_round_down_in_whole_days() {
        let anchor: jiff::Timestamp = "2026-10-01T12:00:00Z".parse().unwrap();
        let at = |s: &str| days_between(anchor, s.parse().unwrap());
        assert_eq!(at("2026-10-02T11:59:59Z"), 0);
        assert_eq!(at("2026-10-02T12:00:00Z"), 1);
        assert_eq!(at("2026-10-27T14:03:11Z"), 26);
        assert_eq!(at("2026-10-01T11:59:59Z"), -1);
    }

    #[test]
    fn dates_move_by_calendar_days_however_late_the_boot() {
        let anchor: jiff::Timestamp = "2026-10-01T18:00:00Z".parse().unwrap();
        let at = |s: &str| {
            let Shift { timestamps, dates } = shift_between(anchor, s.parse().unwrap());
            (timestamps, dates)
        };
        assert_eq!(at("2026-10-01T23:59:59Z"), (0, 0));
        assert_eq!(at("2026-10-02T00:00:00Z"), (0, 1), "the calendar moved, 6 hours after bake");
        assert_eq!(at("2026-10-02T17:59:59Z"), (0, 1));
        assert_eq!(at("2026-10-02T18:00:00Z"), (1, 1));
        assert_eq!(at("2026-10-03T00:00:00Z"), (1, 2));
        assert_eq!(at("2026-09-30T23:00:00Z"), (-1, -1));
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
            ("", "", true),
            ("", "a", false),
        ] {
            assert_eq!(glob(pattern, text), matches, "{pattern} ~ {text}");
        }
    }

    #[test]
    fn only_32_and_64_bit_integers_hold_unix_time() {
        assert_eq!(integer_max("integer"), Some(2_147_483_647));
        assert_eq!(integer_max("bigint"), Some(9_223_372_036_854_775_807));
        for other in ["smallint", "numeric(12,2)", "text", "timestamp without time zone"] {
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
        let (guard, _) = unix_shift("t", 0, 1, 2_147_483_647);
        assert_eq!(
            guard, "t > 0 AND t < 221845392000 AND t <= 2147483647",
            "no move, no room needed"
        );
    }

    #[test]
    fn a_shift_moves_timestamps_by_whole_days_and_dates_by_calendar_days() {
        let shift = Shift { timestamps: -1, dates: -2 };
        let unix = ColumnKind::UnixTime { per_second: 1000, max: i64::MAX.unsigned_abs() };
        assert_eq!(
            moved("r.\"at\"", ColumnKind::Timestamp, shift),
            (
                "r.\"at\" < '9000-01-01'".to_string(),
                "r.\"at\" + make_interval(secs => -86400)".to_string()
            )
        );
        assert_eq!(
            moved("d", ColumnKind::Date, shift),
            ("d < '9000-01-01'".into(), "d + (-2)".into())
        );
        assert_eq!(moved("u", unix, shift).1, "u + (-86400000)");
        let kinds = [ColumnKind::Date, ColumnKind::Timestamp, unix, ColumnKind::Other];
        assert_eq!(kinds.map(|kind| days(shift, kind)), [-2, -1, -1, 0]);
    }

    #[test]
    fn shift_sql_moves_each_kind_of_time_its_own_way() {
        let unix = ColumnKind::UnixTime { per_second: 1, max: i64::MAX.unsigned_abs() };
        let columns = vec![
            column("due", ColumnKind::Date),
            column("at", ColumnKind::Timestamp),
            column("created_unix", unix),
        ];
        let table = table("public", "issue", columns);
        let moving: Vec<&Column> = table.columns.iter().collect();
        let sql = shift_sql(&table, &moving, Shift { timestamps: 2, dates: 3 });
        assert_eq!(
            sql,
            "UPDATE ONLY \"public\".\"issue\" SET \
             \"due\" = CASE WHEN \"due\" < '9000-01-01' THEN \"due\" + (3) ELSE \"due\" END, \
             \"at\" = CASE WHEN \"at\" < '9000-01-01' THEN \"at\" + make_interval(secs => 172800) \
             ELSE \"at\" END, \
             \"created_unix\" = CASE WHEN \"created_unix\" > 0 AND \"created_unix\" < 221845392000 \
             AND \"created_unix\" <= 9223372036854603007 THEN \"created_unix\" + (172800) \
             ELSE \"created_unix\" END"
        );
    }

    #[test]
    fn a_colliding_shift_moves_rows_one_at_a_time_from_the_far_end() {
        let table = table("public", "rates", vec![column("day", ColumnKind::Date)]);
        let moving: Vec<&Column> = table.columns.iter().collect();
        let forward = shift_in_order_sql(&table, &moving, Shift { timestamps: 0, dates: 1 });
        assert_eq!(
            forward,
            "DO $miroir$ DECLARE r record; BEGIN \
             FOR r IN SELECT tableoid AS tbl, ctid AS id FROM ONLY \"public\".\"rates\" \
             ORDER BY \"day\" DESC LOOP \
             UPDATE ONLY \"public\".\"rates\" SET \"day\" = CASE WHEN \"day\" < '9000-01-01' \
             THEN \"day\" + (1) ELSE \"day\" END WHERE tableoid = r.tbl AND ctid = r.id; \
             END LOOP; END $miroir$"
        );
        let back = shift_in_order_sql(&table, &moving, Shift { timestamps: -1, dates: -1 });
        assert!(back.contains("ORDER BY \"day\" ASC"), "{back}");
        let forward_by_timestamps =
            shift_in_order_sql(&table, &moving, Shift { timestamps: 1, dates: 0 });
        assert!(forward_by_timestamps.contains("ORDER BY \"day\" DESC"), "{forward_by_timestamps}");
        let parent = Table { partitioned: true, ..table.clone() };
        let sql = shift_sql(&parent, &moving, Shift { timestamps: 0, dates: 1 });
        assert!(
            sql.starts_with("UPDATE \"public\".\"rates\" SET"),
            "partitions move with it: {sql}"
        );
    }

    #[test]
    fn only_set_time_columns_that_are_not_excluded_move() {
        let unix = ColumnKind::UnixTime { per_second: 1, max: 10 };
        let generated = Column { generated: true, ..column("age_at", ColumnKind::Timestamp) };
        let table = table(
            "public",
            "contacts",
            vec![
                column("birthday", ColumnKind::Date),
                column("due", ColumnKind::Date),
                column("seen_at", ColumnKind::Timestamp),
                column("seen_unix", unix),
                generated,
                column("name", ColumnKind::Other),
            ],
        );
        let excluded: BTreeSet<String> = ["birthday".to_string()].into();
        let names = |shift| {
            let moving = moving_columns(&table, Some(&excluded), shift);
            moving.iter().map(|c| c.name.as_str()).collect::<Vec<_>>()
        };
        assert_eq!(names(Shift { timestamps: 1, dates: 1 }), ["due", "seen_at", "seen_unix"]);
        assert_eq!(names(Shift { timestamps: 0, dates: 1 }), ["due"], "six hours after bake");
        assert_eq!(names(Shift { timestamps: 0, dates: 0 }), Vec::<&str>::new());
        let all = moving_columns(&table, None, Shift { timestamps: 0, dates: 1 });
        assert_eq!(all.len(), 2, "with no exclusion the birthday moves too");
    }

    #[test]
    fn exclusions_name_columns_as_the_evidence_does() {
        let tables = crm();
        let found = excluded(&tables, &strings(&["deals.notes", "audit.events.payload"])).unwrap();
        let expected: BTreeMap<String, BTreeSet<String>> = [
            ("deals".to_string(), ["notes".to_string()].into()),
            ("audit.events".to_string(), ["payload".to_string()].into()),
        ]
        .into();
        assert_eq!(found, expected);
        for entry in ["deals", "deals.missing", "events.payload", "nowhere.id"] {
            let Err(PgError::Sql(error)) = excluded(&tables, &strings(&[entry])) else {
                panic!("{entry} names no column");
            };
            assert!(error.starts_with(&format!("database.time.exclude: no column '{entry}'")));
        }
    }

    #[test]
    fn ignore_resolves_tables_columns_and_schema_qualified_tables() {
        let tables = crm();
        let ignore = ignoring(&["deals.notes", "audit.events", "audit.events.payload"]);
        let found = ignore.ignore(&tables).unwrap();
        assert_eq!(found.tables, ["audit.events".to_string()].into(), "a table name wins");
        let columns: BTreeMap<String, BTreeSet<String>> = [
            ("audit.events".to_string(), ["payload".to_string()].into()),
            ("deals".to_string(), ["notes".to_string()].into()),
        ]
        .into();
        assert_eq!(found.columns, columns);
        let column = ignoring(&["audit.id"]).ignore(&tables).unwrap_err().to_string();
        assert!(column.contains("'audit.id' is in its table's primary key"), "{column}");
        for entry in ["contacts", "deals.missing", "events.payload", "notes"] {
            let error = ignoring(&[entry]).ignore(&tables).unwrap_err().to_string();
            let expected = format!("database.ignore: no table or column '{entry}'");
            assert!(error.starts_with(&expected), "{error}");
        }
        let none = ignoring(&[]).ignore(&tables).unwrap();
        assert!(none.tables.is_empty() && none.columns.is_empty());
    }

    #[test]
    fn epoch_globs_mark_integer_columns_as_unix_time() {
        let mut tables = vec![
            table(
                "public",
                "deals",
                vec![typed("synced_unix", "integer"), typed("note_unix", "text")],
            ),
            table("audit", "events", vec![typed("at_ms", "bigint"), typed("id", "bigint")]),
            table("public", "issue", vec![typed("closed_ms", "bigint")]),
        ];
        let epoch: BTreeMap<String, EpochUnit> =
            [("deals.*_unix".to_string(), EpochUnit::S), ("*.*_ms".to_string(), EpochUnit::Ms)]
                .into();
        mark_epoch(&mut tables, &epoch).unwrap();
        let kinds: Vec<(&str, ColumnKind)> = tables
            .iter()
            .flat_map(|t| t.columns.iter().map(|c| (c.name.as_str(), c.kind)))
            .collect();
        let int = ColumnKind::UnixTime { per_second: 1, max: 2_147_483_647 };
        let ms = ColumnKind::UnixTime { per_second: 1000, max: i64::MAX.unsigned_abs() };
        assert_eq!(
            kinds,
            [
                ("synced_unix", int),
                ("note_unix", ColumnKind::Other),
                ("at_ms", ms),
                ("id", ColumnKind::Other),
                ("closed_ms", ms),
            ]
        );
        for pattern in ["deals.note_unix", "deals.missing", "nowhere.synced_unix", "synced_unix"] {
            let epoch: BTreeMap<String, EpochUnit> = [(pattern.to_string(), EpochUnit::S)].into();
            let error = mark_epoch(&mut tables, &epoch).unwrap_err().to_string();
            let expected = format!("database.time.epoch: '{pattern}' matches no integer");
            assert!(error.starts_with(&expected), "{error}");
        }
    }

    #[test]
    fn capture_triggers_every_table_not_ignored_under_its_evidence_name() {
        let tables = [
            table("public", "deals", Vec::new()),
            table("audit", "events", Vec::new()),
            table("public", "it's", Vec::new()),
            table("public", "cache", Vec::new()),
        ];
        let ignore = Ignore { tables: ["cache".to_string()].into(), columns: BTreeMap::new() };
        let sql = capture_sql(&tables, &ignore);
        let triggers = sql.strip_prefix(CAPTURE).unwrap();
        let on = |on: &str, tbl: &str| {
            format!(
                "\nCREATE TRIGGER miroir_capture AFTER INSERT OR UPDATE OR DELETE ON {on} \
                 FOR EACH ROW EXECUTE FUNCTION miroir.capture({tbl});\
                 \nCREATE TRIGGER miroir_truncate AFTER TRUNCATE ON {on} \
                 FOR EACH STATEMENT EXECUTE FUNCTION miroir.capture({tbl});"
            )
        };
        let expected = [
            on("\"public\".\"deals\"", "'deals'"),
            on("\"audit\".\"events\"", "'audit.events'"),
            on("\"public\".\"it's\"", "'it''s'"),
        ];
        assert_eq!(triggers, expected.concat());
        assert!(
            CAPTURE.contains("SECURITY DEFINER SET search_path = pg_catalog SET timezone = 'UTC'")
        );
        let (refusal, _) = CAPTURE.split_once("CREATE SCHEMA").unwrap();
        let refused = "RAISE EXCEPTION 'role % is not a superuser', current_user;";
        assert!(refusal.contains(refused), "a role that is not a superuser installs nothing");
        assert_eq!(capture_sql(&[], &ignore), CAPTURE, "the log exists even with no table");
    }

    #[test]
    fn names_and_values_are_quoted_for_sql() {
        assert_eq!(quote("we\"ird"), "\"we\"\"ird\"");
        assert_eq!(literal("it's"), "'it''s'");
    }

    #[test]
    fn a_log_read_stops_once_its_rows_pass_the_budget() {
        let row = |old: Option<&str>, new: Option<&str>| LogRow {
            tbl: "notes".into(),
            op: "update".into(),
            old: old.map(str::to_string),
            new: new.map(str::to_string),
        };
        let budget = 3 * LOG_ROW_COST + 20;
        let mut read = Read::default();
        let first = read.take(row(Some("{\"a\":1}"), Some("{\"a\":2}")), budget).unwrap();
        assert_eq!(first.new.as_deref(), Some("{\"a\":2}"));
        assert_eq!((read.rows, read.bytes), (1, LOG_ROW_COST + 14));
        read.take(row(None, None), budget).unwrap();
        assert_eq!((read.rows, read.bytes), (2, 2 * LOG_ROW_COST + 14), "a truncate costs a row");
        read.take(row(None, Some("{\"bb\"}")), budget).unwrap();
        assert_eq!((read.rows, read.bytes), (3, budget), "exactly the budget still fits");
        assert!(read.take(row(None, None), budget).is_err(), "a row without text passes it too");
        assert_eq!((read.rows, read.bytes), (4, budget + LOG_ROW_COST));
    }

    #[test]
    fn server_errors_are_told_apart_by_their_sqlstate() {
        assert!(missing_database(&server("3D000")));
        assert!(collided(&server("23505")) && collided(&server("23P01")));
        for other in ["23503", "57P03", "42P01"] {
            assert!(!missing_database(&server(other)) && !collided(&server(other)), "{other}");
        }
        let protocol = sqlx::Error::Protocol("bad".into());
        assert!(!missing_database(&protocol) && !collided(&protocol));
    }

    #[test]
    fn a_broken_connection_does_not_answer_and_other_failures_are_sql() {
        let io = sqlx::Error::Io(std::io::Error::other("reset by peer"));
        let PgError::Unreachable(message) = failed("cannot read miroir.log")(io) else {
            panic!("an I/O error is a database that stopped answering");
        };
        assert_eq!(
            message,
            "cannot read miroir.log: error communicating with database: reset by peer"
        );
        let PgError::Sql(message) = failed("cannot read the catalog")(server("42P01")) else {
            panic!("a server error is SQL");
        };
        assert_eq!(
            message,
            "cannot read the catalog: error returned from database: SQLSTATE 42P01"
        );
    }

    /// The message of an `Unreachable` error.
    fn unreachable<T: std::fmt::Debug>(result: Result<T, PgError>) -> String {
        match result {
            Err(PgError::Unreachable(message)) => message,
            other => panic!("expected a database that does not answer, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_database_nobody_listens_on_does_not_answer() {
        let pg = Pg::new(&database(&closed_url()));
        let started = Instant::now();
        let message = unreachable(pg.wait_answering(Duration::from_millis(300)).await);
        assert!(started.elapsed() >= Duration::from_millis(300), "it kept trying until the limit");
        assert!(message.contains(", still after "), "{message}");
        assert!(message.ends_with(
            "s: check the host, port and user of database.url against the datastore process"
        ));
        let refused = unreachable(pg.catalog().await);
        assert!(refused.contains("Connection refused"), "{refused}");
        unreachable(pg.snapshot(LOG_BUDGET).await);
        unreachable(pg.clear_log().await);
        unreachable(pg.install_capture(&[], &Ignore::default()).await);
        let tables = [table("public", "deals", vec![column("due", ColumnKind::Date)])];
        unreachable(pg.shift(&tables, Shift { timestamps: 0, dates: 1 }).await);
    }

    #[tokio::test]
    async fn a_shift_that_moves_nothing_never_connects() {
        let pg = Pg::new(&database(&closed_url()));
        let tables = [table("public", "deals", vec![column("due", ColumnKind::Date)])];
        pg.shift(&tables, Shift { timestamps: 0, dates: 0 }).await.unwrap();
        pg.shift(&[], Shift { timestamps: 3, dates: 3 }).await.unwrap();
        let unknown = DbTime { exclude: strings(&["deals.missing"]), ..database("").time };
        let pg = Pg::new(&Database { time: unknown, ..database(&closed_url()) });
        let error = pg.shift(&tables, Shift { timestamps: 0, dates: 0 }).await.unwrap_err();
        assert!(error.to_string().starts_with("database.time.exclude: no column"), "{error}");
    }

    #[tokio::test]
    async fn a_server_that_never_answers_fails_the_wait() {
        let silent = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = silent.local_addr().unwrap().port();
        let pg = Pg::new(&database(&format!("postgres://postgres@127.0.0.1:{port}/crm")));
        let message = unreachable(pg.wait_answering(Duration::ZERO).await);
        assert!(message.starts_with("a connection attempt hung for 5 s"), "{message}");
    }
}
