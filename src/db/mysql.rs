//! `MySQL`: catalog reads, trigger capture into `miroir.log`, time shifts and snapshots.
//! `MySQL` can't turn triggers off, so a shift mutes capture through `miroir.mute`.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write;

use reqwest::Url;
use sqlx::{AssertSqlSafe, Connection, MySqlConnection, Row};

use super::{
    CONNECT_LIMIT, Column, ColumnKind, DbError, Dialect, LogRow, Move, NEVER_FROM, Snapshot, Table,
    check_sql, collided, over_budget, quote, unix_shift,
};

const TABLES: &str = r"
SELECT CAST(c.TABLE_SCHEMA AS CHAR), CAST(c.TABLE_NAME AS CHAR), CAST(c.COLUMN_NAME AS CHAR),
       CAST(c.DATA_TYPE AS CHAR), CAST(c.EXTRA AS CHAR), CAST(c.COLUMN_KEY AS CHAR),
       CAST(c.COLUMN_TYPE AS CHAR)
FROM information_schema.COLUMNS c
JOIN information_schema.TABLES t ON t.TABLE_SCHEMA = c.TABLE_SCHEMA AND t.TABLE_NAME = c.TABLE_NAME
WHERE t.TABLE_TYPE = 'BASE TABLE'
  AND c.TABLE_SCHEMA NOT IN ('mysql', 'sys', 'information_schema', 'performance_schema', 'miroir')
ORDER BY c.TABLE_SCHEMA, c.TABLE_NAME, c.ORDINAL_POSITION";

const TABLE_IDS: &str =
    "SELECT CAST(NAME AS CHAR), CAST(TABLE_ID AS SIGNED) FROM information_schema.INNODB_TABLES";

const CAPTURED: &str = r"
SELECT CAST(EVENT_OBJECT_SCHEMA AS CHAR), CAST(EVENT_OBJECT_TABLE AS CHAR)
FROM information_schema.TRIGGERS
WHERE TRIGGER_NAME LIKE 'miroir\_%'
GROUP BY EVENT_OBJECT_SCHEMA, EVENT_OBJECT_TABLE
HAVING COUNT(*) = 3";

const MUTED: &str = "NOT EXISTS (SELECT 1 FROM miroir.mute WHERE id = CONNECTION_ID())";

const UPDATE_TRIGGERS: &str = r"
SELECT CAST(TRIGGER_NAME AS CHAR), CAST(EVENT_OBJECT_TABLE AS CHAR)
FROM information_schema.TRIGGERS
WHERE EVENT_MANIPULATION = 'UPDATE' AND TRIGGER_NAME NOT LIKE 'miroir\_%'
  AND TRIGGER_SCHEMA NOT IN ('mysql', 'sys', 'information_schema', 'performance_schema', 'miroir')
ORDER BY TRIGGER_SCHEMA, EVENT_OBJECT_TABLE, TRIGGER_NAME";

const CASCADES: &str = r"
SELECT CAST(TABLE_NAME AS CHAR), CAST(CONSTRAINT_NAME AS CHAR), CAST(REFERENCED_TABLE_NAME AS CHAR),
       CAST(DELETE_RULE AS CHAR), CAST(UPDATE_RULE AS CHAR), CAST(CONSTRAINT_SCHEMA AS CHAR)
FROM information_schema.REFERENTIAL_CONSTRAINTS
WHERE (DELETE_RULE <> 'RESTRICT' AND DELETE_RULE <> 'NO ACTION')
   OR (UPDATE_RULE <> 'RESTRICT' AND UPDATE_RULE <> 'NO ACTION')
ORDER BY CONSTRAINT_SCHEMA, TABLE_NAME, CONSTRAINT_NAME";

pub async fn ping(url: &str) -> Result<(), sqlx::Error> {
    let mut conn = MySqlConnection::connect(url).await?;
    sqlx::query("SELECT 1").execute(&mut conn).await?;
    conn.close().await
}

async fn connect(url: &str) -> Result<MySqlConnection, DbError> {
    tokio::time::timeout(CONNECT_LIMIT, MySqlConnection::connect(url))
        .await
        .map_err(|_| DbError(format!("no connection after {} s", CONNECT_LIMIT.as_secs())))?
        .map_err(|e| DbError(format!("cannot connect to the database: {e}")))
}

fn default_schema(url: &str) -> String {
    Url::parse(url).map(|u| u.path().trim_start_matches('/').to_string()).unwrap_or_default()
}

pub async fn tables(url: &str, schemas: Option<&[String]>) -> Result<Vec<Table>, DbError> {
    let mut conn = connect(url).await?;
    let tables = read_tables(&mut conn, url, schemas).await?;
    conn.close().await?;
    Ok(tables)
}

async fn read_tables(
    conn: &mut MySqlConnection,
    url: &str,
    schemas: Option<&[String]>,
) -> Result<Vec<Table>, DbError> {
    let home = default_schema(url);
    let mut ids = HashMap::new();
    for row in sqlx::query(TABLE_IDS).fetch_all(&mut *conn).await? {
        ids.insert(row.try_get::<String, _>(0)?, row.try_get::<i64, _>(1)?);
    }
    let mut captured = HashSet::new();
    for row in sqlx::query(CAPTURED).fetch_all(&mut *conn).await? {
        captured.insert((row.try_get::<String, _>(0)?, row.try_get::<String, _>(1)?));
    }
    let rows = sqlx::query(TABLES).fetch_all(&mut *conn).await?;
    let mut tables: Vec<Table> = Vec::new();
    for row in rows {
        let (schema, name): (String, String) = (row.try_get(0)?, row.try_get(1)?);
        if schemas.is_some_and(|wanted| !wanted.contains(&schema)) {
            continue;
        }
        let tbl = if schema == home { name.clone() } else { format!("{schema}.{name}") };
        if tables.last().is_none_or(|t| t.name != tbl) {
            let qualified =
                format!("{}.{}", quote(Dialect::MySql, &schema), quote(Dialect::MySql, &name));
            let id = innodb_name(&schema, &name).and_then(|n| ids.get(&n).copied()).unwrap_or(0);
            let captured = captured.contains(&(schema.clone(), name.clone()));
            let (columns, schema) = (Vec::new(), schema.clone());
            let partitioned = false;
            tables.push(Table { name: tbl, schema, qualified, columns, partitioned, id, captured });
        }
        let kind = match row.try_get::<String, _>(3)?.as_str() {
            "date" => ColumnKind::Date,
            "datetime" => ColumnKind::Timestamp,
            "timestamp" => ColumnKind::EpochSeconds,
            _ => ColumnKind::Other,
        };
        let extra: String = row.try_get(4)?;
        let column = Column {
            name: row.try_get(2)?,
            kind,
            generated: extra.contains("VIRTUAL GENERATED") || extra.contains("STORED GENERATED"),
            pk: row.try_get::<String, _>(5)? == "PRI",
            sql_type: row.try_get(6)?,
        };
        if let Some(table) = tables.last_mut() {
            table.columns.push(column);
        }
    }
    Ok(tables)
}

fn innodb_name(schema: &str, table: &str) -> Option<String> {
    let spell = |part: &str| {
        let mut spelled = String::new();
        for c in part.chars() {
            match c {
                'a'..='z' | 'A'..='Z' | '0'..='9' | '_' => spelled.push(c),
                _ if c.is_ascii() => {
                    let _ = write!(spelled, "@{:04x}", u32::from(c));
                }
                _ => return None,
            }
        }
        Some(spelled)
    };
    Some(format!("{}/{}", spell(schema)?, spell(table)?))
}

pub async fn shift(
    url: &str,
    tables: &[Table],
    exclude: &BTreeSet<(String, String)>,
    by: Move,
    live: bool,
) -> Result<(), DbError> {
    let mut conn = connect(url).await?;
    let wait = if live {
        "SET SESSION innodb_lock_wait_timeout = 10; \
         SET SESSION TRANSACTION ISOLATION LEVEL REPEATABLE READ; "
    } else {
        ""
    };
    sqlx::raw_sql(AssertSqlSafe(shift_setup(wait, muted(tables)))).execute(&mut conn).await?;
    for table in tables {
        let moving = table.moving(exclude, by);
        if moving.is_empty() {
            continue;
        }
        let at_once = shift_sql(table, &moving, by);
        match sqlx::raw_sql(AssertSqlSafe(at_once)).execute(&mut conn).await {
            Err(e) if collided(&e) => {
                let order: Vec<String> = moving
                    .iter()
                    .map(|c| format!("{} {}", quote(Dialect::MySql, &c.name), by.order()))
                    .collect();
                let sql =
                    format!("{} ORDER BY {}", shift_sql(table, &moving, by), order.join(", "));
                sqlx::raw_sql(AssertSqlSafe(sql)).execute(&mut conn).await?;
            }
            moved => {
                moved?;
            }
        }
        for column in moving.iter().filter(|_| live) {
            for image in ["old", "new"] {
                let sql = log_sql(table, column, image, by);
                sqlx::raw_sql(AssertSqlSafe(sql)).execute(&mut conn).await?;
            }
        }
    }
    let end = if muted(tables) {
        "DELETE FROM miroir.mute WHERE id = CONNECTION_ID(); COMMIT"
    } else {
        "COMMIT"
    };
    sqlx::raw_sql(end).execute(&mut conn).await?;
    conn.close().await?;
    Ok(())
}

pub async fn distinct(url: &str, table: &Table, column: &str) -> Result<Vec<String>, DbError> {
    let mut conn = connect(url).await?;
    let values =
        sqlx::query_scalar(AssertSqlSafe(distinct_sql(table, column))).fetch_all(&mut conn).await?;
    conn.close().await?;
    Ok(values)
}

fn distinct_sql(table: &Table, column: &str) -> String {
    let c = quote(Dialect::MySql, column);
    format!(
        "SELECT DISTINCT CONVERT({c} USING utf8mb4) COLLATE utf8mb4_bin FROM {} WHERE {c} IS NOT NULL",
        table.qualified
    )
}

pub async fn rewrite(
    url: &str,
    table: &Table,
    column: &str,
    pairs: Option<&[(String, String)]>,
) -> Result<u64, DbError> {
    let mut conn = connect(url).await?;
    let setup = "SET time_zone = '+00:00', foreign_key_checks = 0, sql_mode = CONCAT_WS(',', \
                 NULLIF(REPLACE(REPLACE(@@sql_mode, 'NO_ZERO_DATE', ''), 'NO_ZERO_IN_DATE', ''), ''), \
                 'STRICT_ALL_TABLES')";
    sqlx::raw_sql(setup).execute(&mut conn).await?;
    let changed = match pairs {
        None => {
            let sql = blank_sql(table, column);
            sqlx::raw_sql(AssertSqlSafe(sql)).execute(&mut conn).await?.rows_affected()
        }
        Some(pairs) => {
            let map = "CREATE TEMPORARY TABLE miroir_map (old LONGTEXT CHARACTER SET utf8mb4 \
                       COLLATE utf8mb4_bin NOT NULL, new LONGTEXT CHARACTER SET utf8mb4 NOT NULL, \
                       KEY (old(255)))";
            sqlx::raw_sql(map).execute(&mut conn).await?;
            for batch in super::map_batches(pairs)? {
                let fill = "INSERT INTO miroir_map (old, new) SELECT j.old, j.new FROM JSON_TABLE(?, \
                            '$[*]' COLUMNS (old LONGTEXT PATH '$[0]', new LONGTEXT PATH '$[1]')) AS j";
                sqlx::query(fill).bind(batch).execute(&mut conn).await?;
            }
            let sql = rewrite_sql(table, column);
            let changed = sqlx::raw_sql(AssertSqlSafe(sql)).execute(&mut conn).await?;
            sqlx::raw_sql("DROP TEMPORARY TABLE miroir_map").execute(&mut conn).await?;
            changed.rows_affected()
        }
    };
    conn.close().await?;
    Ok(changed)
}

fn still_times(table: &Table, column: &str) -> String {
    let mut out = String::new();
    for c in table.time_columns().filter(|c| c.name != column) {
        let q = quote(Dialect::MySql, &c.name);
        let _ = write!(out, ", t.{q} = t.{q}");
    }
    out
}

fn blank_sql(table: &Table, column: &str) -> String {
    let c = quote(Dialect::MySql, column);
    let still = still_times(table, column);
    format!("UPDATE {} AS t SET t.{c} = NULL{still} WHERE t.{c} IS NOT NULL", table.qualified)
}

fn rewrite_sql(table: &Table, column: &str) -> String {
    let c = quote(Dialect::MySql, column);
    let still = still_times(table, column);
    format!(
        "UPDATE {} AS t STRAIGHT_JOIN miroir_map AS m \
         ON CONVERT(t.{c} USING utf8mb4) COLLATE utf8mb4_bin = m.old SET t.{c} = m.new{still}",
        table.qualified
    )
}

fn muted(tables: &[Table]) -> bool {
    tables.iter().any(|table| table.captured)
}

fn shift_setup(wait: &str, muted: bool) -> String {
    let mute = if muted { "; INSERT INTO miroir.mute (id) VALUES (CONNECTION_ID())" } else { "" };
    format!(
        "SET time_zone = '+00:00', foreign_key_checks = 0, \
         sql_mode = REPLACE(REPLACE(@@sql_mode, 'NO_ZERO_DATE', ''), 'NO_ZERO_IN_DATE', ''); \
         {wait}START TRANSACTION{mute}"
    )
}

fn shift_sql(table: &Table, moving: &[&Column], by: Move) -> String {
    let sets: Vec<String> = table
        .time_columns()
        .map(|c| {
            let q = quote(Dialect::MySql, &c.name);
            if !moving.iter().any(|m| m.name == c.name) {
                return format!("{q} = {q}");
            }
            let (guard, moved) = moved(&q, c.kind, by);
            format!("{q} = IF({guard}, {moved}, {q})")
        })
        .collect();
    format!("UPDATE {} SET {}", table.qualified, sets.join(", "))
}

fn moved(value: &str, kind: ColumnKind, by: Move) -> (String, String) {
    let seconds = by.seconds;
    let (low, high, step) = match kind {
        ColumnKind::UnixTime { per_second, max } => {
            return unix_shift(value, seconds, per_second, max);
        }
        ColumnKind::EpochSeconds => (
            format!("'1970-01-01 00:00:01' + INTERVAL {} SECOND", (-seconds).max(0)),
            format!("'2038-01-19 03:14:07' - INTERVAL {} SECOND", seconds.max(0)),
            format!("INTERVAL {seconds} SECOND"),
        ),
        ColumnKind::Date => (
            "'1000-01-01'".to_string(),
            format!("'{NEVER_FROM}'"),
            format!("INTERVAL {} DAY", by.dates),
        ),
        ColumnKind::Timestamp | ColumnKind::Other => (
            "'1000-01-01'".to_string(),
            format!("'{NEVER_FROM}'"),
            format!("INTERVAL {seconds} SECOND"),
        ),
    };
    (format!("{value} >= {low} AND {value} < {high}"), format!("{value} + {step}"))
}

fn log_sql(table: &Table, column: &Column, image: &str, by: Move) -> String {
    let member = column.name.replace('\\', "\\\\").replace('"', "\\\"");
    let path = format!("'{}'", literal(&format!("$.\"{member}\"")));
    let found = format!("JSON_EXTRACT({image}, {path})");
    let typed = match column.kind {
        ColumnKind::Date => format!("CAST(JSON_UNQUOTE({found}) AS DATE)"),
        ColumnKind::UnixTime { .. } if column.sql_type.contains("unsigned") => {
            format!("CAST({found} AS UNSIGNED)")
        }
        ColumnKind::UnixTime { .. } => format!("CAST({found} AS SIGNED)"),
        ColumnKind::Timestamp | ColumnKind::EpochSeconds | ColumnKind::Other => {
            format!("CAST(JSON_UNQUOTE({found}) AS DATETIME(6))")
        }
    };
    let value = format!("IF(JSON_TYPE({found}) = 'NULL', NULL, {typed})");
    let (guard, moved) = moved(&value, column.kind, by);
    format!(
        "UPDATE miroir.log SET {image} = JSON_SET({image}, {path}, {moved}) \
         WHERE tbl = '{}' AND {guard}",
        literal(&table.name)
    )
}

pub async fn execute(url: &str, sql: &str) -> Result<(), DbError> {
    let mut conn = connect(url).await?;
    sqlx::raw_sql(AssertSqlSafe(sql)).execute(&mut conn).await?;
    conn.close().await?;
    Ok(())
}

pub async fn install_capture(url: &str, tables: &[Table]) -> Result<Vec<String>, DbError> {
    let mut conn = connect(url).await?;
    sqlx::raw_sql(
        "CREATE DATABASE IF NOT EXISTS miroir; \
         CREATE TABLE IF NOT EXISTS miroir.log (seq BIGINT AUTO_INCREMENT PRIMARY KEY, \
         tbl VARCHAR(255) NOT NULL, op VARCHAR(16) NOT NULL, old JSON, new JSON); \
         CREATE TABLE IF NOT EXISTS miroir.mute (id BIGINT UNSIGNED PRIMARY KEY)",
    )
    .execute(&mut conn)
    .await?;
    for (n, table) in tables.iter().enumerate() {
        let tbl = literal(&table.name);
        let row = |alias: &str| {
            let pairs: Vec<String> = table
                .columns
                .iter()
                .map(|c| {
                    let column = format!("{alias}.{}", quote(Dialect::MySql, &c.name));
                    let value = match c.kind {
                        ColumnKind::EpochSeconds => {
                            format!("CONVERT_TZ({column}, @@session.time_zone, '+00:00')")
                        }
                        _ => column,
                    };
                    format!("'{}', {value}", literal(&c.name))
                })
                .collect();
            format!("JSON_OBJECT({})", pairs.join(", "))
        };
        let schema = quote(Dialect::MySql, &table.schema);
        for (event, op, old, new) in [
            ("INSERT", "insert", "NULL".to_string(), row("NEW")),
            ("UPDATE", "update", row("OLD"), row("NEW")),
            ("DELETE", "delete", row("OLD"), "NULL".to_string()),
        ] {
            let sql = format!(
                "CREATE TRIGGER {schema}.miroir_{op}_{n} AFTER {event} ON {} FOR EACH ROW \
                 INSERT INTO miroir.log (tbl, op, old, new) SELECT '{tbl}', '{op}', {old}, {new} \
                 FROM DUAL WHERE {MUTED}",
                table.qualified
            );
            sqlx::raw_sql(AssertSqlSafe(sql)).execute(&mut conn).await?;
        }
    }
    let mut warnings = Vec::new();
    for row in sqlx::query(UPDATE_TRIGGERS).fetch_all(&mut conn).await? {
        let (name, table): (String, String) = (row.try_get(0)?, row.try_get(1)?);
        warnings.push(format!(
            "trigger {name} on {table} also fires when a reset or a wait moves dates, because MySQL cannot \
             turn triggers off"
        ));
    }
    for row in sqlx::query(CASCADES).fetch_all(&mut conn).await? {
        let (child, name, parent): (String, String, String) =
            (row.try_get(0)?, row.try_get(1)?, row.try_get(2)?);
        let (on_delete, on_update): (String, String) = (row.try_get(3)?, row.try_get(4)?);
        warnings.push(format!(
            "foreign key {name} on {child} references {parent} with ON DELETE {on_delete} \
             ON UPDATE {on_update}: MySQL fires no triggers for cascaded changes, so check \
             {child}'s final state rather than its changes"
        ));
    }
    conn.close().await?;
    Ok(warnings)
}

fn literal(text: &str) -> String {
    text.replace('\\', "\\\\").replace('\'', "''")
}

pub async fn clear_log(url: &str) -> Result<(), DbError> {
    let mut conn = connect(url).await?;
    sqlx::raw_sql("SET SESSION lock_wait_timeout = 10; TRUNCATE TABLE miroir.log")
        .execute(&mut conn)
        .await?;
    conn.close().await?;
    Ok(())
}

pub async fn value(url: &str, sql: &str) -> Result<Option<String>, DbError> {
    let mut conn = connect(url).await?;
    sqlx::raw_sql("SET SESSION max_execution_time = 10000; START TRANSACTION READ ONLY")
        .execute(&mut conn)
        .await?;
    let query =
        sqlx::query_scalar::<_, Option<String>>(AssertSqlSafe(check_sql(Dialect::MySql, sql)));
    let value = query.fetch_optional(&mut conn).await;
    sqlx::raw_sql("ROLLBACK").execute(&mut conn).await?;
    conn.close().await?;
    Ok(value?.flatten())
}

pub async fn snapshot(
    url: &str,
    schemas: Option<&[String]>,
    checks: &[&str],
    log_budget: i64,
) -> Result<Snapshot, DbError> {
    let mut conn = connect(url).await?;
    sqlx::raw_sql(
        "SET SESSION TRANSACTION ISOLATION LEVEL REPEATABLE READ; \
         START TRANSACTION READ ONLY, WITH CONSISTENT SNAPSHOT",
    )
    .execute(&mut conn)
    .await?;
    let tables = read_tables(&mut conn, url, schemas).await?;
    let size: i64 = sqlx::query_scalar(
        "SELECT CAST(COALESCE(SUM(COALESCE(LENGTH(CAST(old AS CHAR)), 0) + \
                                  COALESCE(LENGTH(CAST(new AS CHAR)), 0)), 0) AS SIGNED) \
         FROM miroir.log",
    )
    .fetch_one(&mut conn)
    .await?;
    if size > log_budget {
        sqlx::raw_sql("ROLLBACK").execute(&mut conn).await?;
        conn.close().await?;
        let log = Err(over_budget(size, log_budget));
        return Ok(Snapshot { tables, log, scores: Vec::new() });
    }
    let rows = sqlx::query(
        "SELECT tbl, op, CAST(old AS CHAR), CAST(new AS CHAR) FROM miroir.log ORDER BY seq",
    )
    .fetch_all(&mut conn)
    .await?;
    let mut log = Vec::with_capacity(rows.len());
    for row in rows {
        log.push(LogRow {
            tbl: row.try_get(0)?,
            op: row.try_get(1)?,
            old: row.try_get(2)?,
            new: row.try_get(3)?,
        });
    }
    sqlx::raw_sql("SET SESSION max_execution_time = 10000").execute(&mut conn).await?;
    let mut scores = Vec::with_capacity(checks.len());
    for sql in checks {
        let query =
            sqlx::query_scalar::<_, Option<String>>(AssertSqlSafe(check_sql(Dialect::MySql, sql)));
        scores.push(
            query.fetch_optional(&mut conn).await.map(Option::flatten).map_err(|e| e.to_string()),
        );
    }
    sqlx::raw_sql("ROLLBACK").execute(&mut conn).await?;
    conn.close().await?;
    Ok(Snapshot { tables, log: Ok(log), scores })
}

pub async fn fingerprint(
    url: &str,
    tables: &[(Table, Vec<String>)],
) -> Result<BTreeMap<String, String>, DbError> {
    let mut conn = connect(url).await?;
    let mut prints = BTreeMap::new();
    for (table, columns) in tables {
        let sql = if columns.is_empty() {
            format!("SELECT CAST(COUNT(*) AS CHAR) FROM {}", table.qualified)
        } else {
            let row: Vec<String> = columns.iter().map(|c| quote(Dialect::MySql, c)).collect();
            format!(
                "SELECT CONCAT(COUNT(*), ':', COALESCE(SUM(CAST(CONV(\
                 LEFT(MD5(JSON_ARRAY({})), 16), 16, 10) AS UNSIGNED)), 0)) FROM {}",
                row.join(", "),
                table.qualified
            )
        };
        let print: String = sqlx::query_scalar(AssertSqlSafe(sql)).fetch_one(&mut conn).await?;
        prints.insert(table.name.clone(), print);
    }
    conn.close().await?;
    Ok(prints)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rewrite_compares_bytes_and_keeps_every_time_column() {
        let column = |name: &str, kind| Column {
            name: name.into(),
            kind,
            generated: false,
            pk: false,
            sql_type: String::new(),
        };
        let columns = vec![
            column("email", ColumnKind::Other),
            column("created_at", ColumnKind::Timestamp),
            column("updated_at", ColumnKind::EpochSeconds),
        ];
        let table = Table {
            name: "customers".into(),
            schema: "shop".into(),
            qualified: "`shop`.`customers`".into(),
            columns,
            partitioned: false,
            id: 0,
            captured: false,
        };
        assert_eq!(
            distinct_sql(&table, "email"),
            "SELECT DISTINCT CONVERT(`email` USING utf8mb4) COLLATE utf8mb4_bin \
             FROM `shop`.`customers` WHERE `email` IS NOT NULL"
        );
        assert_eq!(
            rewrite_sql(&table, "email"),
            "UPDATE `shop`.`customers` AS t STRAIGHT_JOIN miroir_map AS m \
             ON CONVERT(t.`email` USING utf8mb4) COLLATE utf8mb4_bin = m.old \
             SET t.`email` = m.new, t.`created_at` = t.`created_at`, t.`updated_at` = t.`updated_at`"
        );
        assert_eq!(
            blank_sql(&table, "email"),
            "UPDATE `shop`.`customers` AS t SET t.`email` = NULL, \
             t.`created_at` = t.`created_at`, t.`updated_at` = t.`updated_at` WHERE t.`email` IS NOT NULL"
        );
    }

    #[test]
    fn a_shift_mutes_capture_only_once_it_is_installed() {
        let table = |captured| Table {
            name: "orders".into(),
            schema: "shop".into(),
            qualified: "`shop`.`orders`".into(),
            columns: Vec::new(),
            partitioned: false,
            id: 0,
            captured,
        };
        assert!(muted(&[table(false), table(true)]));
        assert!(!muted(&[table(false)]));
        let muting = shift_setup("", true);
        assert!(
            muting.ends_with(
                "START TRANSACTION; INSERT INTO miroir.mute (id) VALUES (CONNECTION_ID())"
            )
        );
        assert!(shift_setup("", false).ends_with("START TRANSACTION"), "{muting}");
        assert!(!shift_setup("", false).contains("miroir"));
    }

    #[test]
    fn shift_sql_moves_unix_time_and_sets_still_columns_to_themselves() {
        let column = |name: &str, kind| Column {
            name: name.into(),
            kind,
            generated: false,
            pk: false,
            sql_type: String::new(),
        };
        let unix = ColumnKind::UnixTime { per_second: 1, max: u64::from(u32::MAX) };
        let columns =
            vec![column("paid_unix", unix), column("updated_at", ColumnKind::EpochSeconds)];
        let (qualified, partitioned) = ("`shop`.`orders`".to_string(), false);
        let (name, schema) = ("orders".to_string(), "shop".to_string());
        let table = Table { name, schema, qualified, columns, partitioned, id: 0, captured: true };
        let sql = shift_sql(&table, &[&table.columns[0]], Move { seconds: 2 * 86_400, dates: 2 });
        assert_eq!(
            sql,
            "UPDATE `shop`.`orders` SET `paid_unix` = IF(`paid_unix` > 0 \
             AND `paid_unix` < 221845392000 AND `paid_unix` <= 4294794495, \
             `paid_unix` + (172800), `paid_unix`), `updated_at` = `updated_at`"
        );
    }

    #[test]
    fn the_log_moves_each_copy_of_a_column_as_the_table_moved() {
        let column = |name: &str, kind, sql_type: &str| Column {
            name: name.into(),
            kind,
            generated: false,
            pk: false,
            sql_type: sql_type.into(),
        };
        let (qualified, partitioned) = ("`shop`.`orders`".to_string(), false);
        let (name, schema) = ("orders".to_string(), "shop".to_string());
        let table = Table {
            name,
            schema,
            qualified,
            columns: Vec::new(),
            partitioned,
            id: 0,
            captured: true,
        };
        let by = Move { seconds: -1_800, dates: -1 };
        let placed = column("placed_at", ColumnKind::Timestamp, "datetime");
        let found = "JSON_EXTRACT(old, '$.\"placed_at\"')";
        let value = format!(
            "IF(JSON_TYPE({found}) = 'NULL', NULL, CAST(JSON_UNQUOTE({found}) AS DATETIME(6)))"
        );
        assert_eq!(
            log_sql(&table, &placed, "old", by),
            format!(
                "UPDATE miroir.log SET old = JSON_SET(old, '$.\"placed_at\"', \
                 {value} + INTERVAL -1800 SECOND) WHERE tbl = 'orders' \
                 AND {value} >= '1000-01-01' AND {value} < '9000-01-01'"
            )
        );
        let unix = ColumnKind::UnixTime { per_second: 1, max: u64::from(u32::MAX) };
        let paid = column("it's \"paid\"", unix, "int(10) unsigned");
        let sql = log_sql(&table, &paid, "new", by);
        assert!(sql.contains("JSON_EXTRACT(new, '$.\"it''s \\\\\"paid\\\\\"\"')"), "{sql}");
        assert!(sql.contains("AS UNSIGNED)) + (-1800)"), "{sql}");
    }

    #[test]
    fn a_move_by_minutes_keeps_each_kind_of_time_in_its_range() {
        let by = Move { seconds: -1_800, dates: -1 };
        assert_eq!(
            moved("v", ColumnKind::EpochSeconds, by),
            (
                "v >= '1970-01-01 00:00:01' + INTERVAL 1800 SECOND \
                 AND v < '2038-01-19 03:14:07' - INTERVAL 0 SECOND"
                    .to_string(),
                "v + INTERVAL -1800 SECOND".to_string()
            )
        );
        assert_eq!(
            moved("v", ColumnKind::Timestamp, by),
            (
                "v >= '1000-01-01' AND v < '9000-01-01'".to_string(),
                "v + INTERVAL -1800 SECOND".to_string()
            )
        );
        assert_eq!(moved("v", ColumnKind::Date, by).1, "v + INTERVAL -1 DAY");
    }

    #[test]
    fn innodb_names_are_spelled_as_mysql_lists_them() {
        let spelled = |schema, table| innodb_name(schema, table);
        assert_eq!(spelled("shop", "Mixed_Case9").as_deref(), Some("shop/Mixed_Case9"));
        assert_eq!(
            spelled("my-db", "odd-name $x").as_deref(),
            Some("my@002ddb/odd@002dname@0020@0024x")
        );
        assert_eq!(spelled("s", "a.b@c#d").as_deref(), Some("s/a@002eb@0040c@0023d"));
        assert_eq!(spelled("s", "café"), None, "MySQL spells it caf@0p, a code of its own");
    }
}
