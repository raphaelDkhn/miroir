//! Postgres: catalog reads, trigger capture into `miroir.log`, time shifts and snapshots.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

use sqlx::{AssertSqlSafe, Connection, PgConnection, Row};

use super::{
    CONNECT_LIMIT, Column, ColumnKind, DbError, Dialect, LogRow, Move, NEVER_FROM, Snapshot, Table,
    check_sql, collided, over_budget, quote, unix_shift,
};

const DEFAULT_SCHEMA: &str = "public";

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
  AND ($1::text[] IS NULL OR n.nspname = ANY($1))
ORDER BY n.nspname, c.relname, a.attnum";

const CAPTURE: &str = r"
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

pub async fn ping(url: &str) -> Result<(), sqlx::Error> {
    let mut conn = PgConnection::connect(url).await?;
    sqlx::query("SELECT 1").execute(&mut conn).await?;
    conn.close().await
}

async fn connect(url: &str) -> Result<PgConnection, DbError> {
    tokio::time::timeout(CONNECT_LIMIT, PgConnection::connect(url))
        .await
        .map_err(|_| DbError(format!("no connection after {} s", CONNECT_LIMIT.as_secs())))?
        .map_err(|e| DbError(format!("cannot connect to the database: {e}")))
}

pub async fn tables(url: &str, schemas: Option<&[String]>) -> Result<Vec<Table>, DbError> {
    let mut conn = connect(url).await?;
    let tables = read_tables(&mut conn, schemas).await?;
    conn.close().await?;
    Ok(tables)
}

async fn read_tables(
    conn: &mut PgConnection,
    schemas: Option<&[String]>,
) -> Result<Vec<Table>, DbError> {
    let rows = sqlx::query(TABLES).bind(schemas).fetch_all(&mut *conn).await?;
    let mut tables: Vec<Table> = Vec::new();
    for row in rows {
        let (schema, name): (String, String) = (row.try_get(0)?, row.try_get(1)?);
        let tbl = if schema == DEFAULT_SCHEMA { name.clone() } else { format!("{schema}.{name}") };
        if tables.last().is_none_or(|t| t.name != tbl) {
            let qualified = format!(
                "{}.{}",
                quote(Dialect::Postgres, &schema),
                quote(Dialect::Postgres, &name)
            );
            let partitioned = row.try_get::<String, _>(2)? == "p";
            let (columns, schema) = (Vec::new(), schema.clone());
            let (id, captured) = (row.try_get(7)?, row.try_get(9)?);
            tables.push(Table { name: tbl, schema, qualified, columns, partitioned, id, captured });
        }
        let kind = match row.try_get::<String, _>(4)?.as_str() {
            "date" => ColumnKind::Date,
            "timestamp without time zone" | "timestamp with time zone" => ColumnKind::Timestamp,
            _ => ColumnKind::Other,
        };
        let column = Column {
            name: row.try_get(3)?,
            kind,
            generated: !row.try_get::<String, _>(5)?.is_empty(),
            pk: row.try_get(6)?,
            sql_type: row.try_get(8)?,
        };
        if let Some(table) = tables.last_mut() {
            table.columns.push(column);
        }
    }
    Ok(tables)
}

pub async fn shift(
    url: &str,
    tables: &[Table],
    exclude: &BTreeSet<(String, String)>,
    by: Move,
    live: bool,
) -> Result<(), DbError> {
    let moving: Vec<(&Table, Vec<&Column>)> = tables
        .iter()
        .map(|table| (table, table.moving(exclude, by)))
        .filter(|(_, columns)| !columns.is_empty())
        .collect();
    let mut conn = connect(url).await?;
    let wait = if live { "SET lock_timeout = '10s'; " } else { "" };
    let setup = format!("SET session_replication_role = replica; SET TIME ZONE 'UTC'; {wait}BEGIN");
    sqlx::raw_sql(AssertSqlSafe(setup)).execute(&mut conn).await?;
    let mut logged = BTreeSet::new();
    if live && !moving.is_empty() {
        let locked: Vec<&str> = moving.iter().map(|(table, _)| table.qualified.as_str()).collect();
        let lock = format!("LOCK TABLE {} IN SHARE ROW EXCLUSIVE MODE", locked.join(", "));
        sqlx::raw_sql(AssertSqlSafe(lock)).execute(&mut conn).await?;
        let names = sqlx::query_scalar("SELECT DISTINCT tbl FROM miroir.log");
        logged = names.fetch_all(&mut conn).await?.into_iter().collect::<BTreeSet<String>>();
    }
    for (table, columns) in &moving {
        sqlx::raw_sql("SAVEPOINT miroir_shift").execute(&mut conn).await?;
        let moved = shift_sql(table, columns, by);
        match sqlx::raw_sql(AssertSqlSafe(moved)).execute(&mut conn).await {
            Err(e) if collided(&e) => {
                sqlx::raw_sql("ROLLBACK TO SAVEPOINT miroir_shift").execute(&mut conn).await?;
                let in_order = shift_in_order_sql(table, columns, by);
                sqlx::raw_sql(AssertSqlSafe(in_order)).execute(&mut conn).await?;
            }
            moved => {
                moved?;
            }
        }
        if logged.contains(&table.name) {
            for column in columns {
                for image in ["old", "new"] {
                    let sql = log_sql(table, column, image, by);
                    sqlx::raw_sql(AssertSqlSafe(sql)).execute(&mut conn).await?;
                }
            }
        }
    }
    sqlx::raw_sql("COMMIT").execute(&mut conn).await?;
    if live {
        sqlx::raw_sql("VACUUM miroir.log").execute(&mut conn).await?;
    }
    conn.close().await?;
    Ok(())
}

fn log_sql(table: &Table, column: &Column, image: &str, by: Move) -> String {
    let name = text(&column.name);
    let value = format!("({image}->>{name})::{}", column.sql_type);
    let (guard, moved) = moved(&value, column.kind, by);
    format!(
        "UPDATE miroir.log SET {image} = jsonb_set({image}, ARRAY[{name}], to_jsonb({moved})) \
         WHERE tbl = {} AND {guard}",
        text(&table.name)
    )
}

fn text(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn shift_sql(table: &Table, columns: &[&Column], by: Move) -> String {
    let sets: Vec<String> = columns
        .iter()
        .map(|c| {
            let q = quote(Dialect::Postgres, &c.name);
            let (guard, moved) = moved(&q, c.kind, by);
            format!("{q} = CASE WHEN {guard} THEN {moved} ELSE {q} END")
        })
        .collect();
    let only = if table.partitioned { "" } else { "ONLY " };
    format!("UPDATE {only}{} SET {}", table.qualified, sets.join(", "))
}

fn moved(value: &str, kind: ColumnKind, by: Move) -> (String, String) {
    let never = format!("{value} < '{NEVER_FROM}'");
    match kind {
        ColumnKind::UnixTime { per_second, max } => unix_shift(value, by.seconds, per_second, max),
        ColumnKind::Date => (never, format!("{value} + ({})", by.dates)),
        ColumnKind::Timestamp | ColumnKind::EpochSeconds | ColumnKind::Other => {
            (never, format!("{value} + make_interval(secs => {})", by.seconds))
        }
    }
}

fn shift_in_order_sql(table: &Table, columns: &[&Column], by: Move) -> String {
    let only = if table.partitioned { "" } else { "ONLY " };
    let order: Vec<String> = columns
        .iter()
        .map(|c| format!("{} {}", quote(Dialect::Postgres, &c.name), by.order()))
        .collect();
    format!(
        "DO $miroir$ DECLARE r record; BEGIN \
         FOR r IN SELECT tableoid AS tbl, ctid AS id FROM {only}{} ORDER BY {} LOOP \
         {} WHERE tableoid = r.tbl AND ctid = r.id; \
         END LOOP; END $miroir$",
        table.qualified,
        order.join(", "),
        shift_sql(table, columns, by)
    )
}

pub async fn execute(url: &str, sql: &str) -> Result<(), DbError> {
    let mut conn = connect(url).await?;
    sqlx::raw_sql(AssertSqlSafe(sql)).execute(&mut conn).await?;
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
    let c = quote(Dialect::Postgres, column);
    format!("SELECT DISTINCT {c}::text FROM {} WHERE {c} IS NOT NULL", table.qualified)
}

pub async fn rewrite(
    url: &str,
    table: &Table,
    column: &str,
    pairs: Option<&[(String, String)]>,
) -> Result<u64, DbError> {
    let mut conn = connect(url).await?;
    sqlx::raw_sql("SET session_replication_role = replica; BEGIN").execute(&mut conn).await?;
    let changed = match pairs {
        None => {
            let sql = blank_sql(table, column);
            sqlx::raw_sql(AssertSqlSafe(sql)).execute(&mut conn).await?.rows_affected()
        }
        Some(pairs) => {
            let base: String = sqlx::query_scalar(
                "SELECT format_type(atttypid, -1) FROM pg_attribute \
                 WHERE attrelid = $1::regclass AND attname = $2",
            )
            .bind(&table.qualified)
            .bind(column)
            .fetch_one(&mut conn)
            .await?;
            let map = "CREATE TEMP TABLE miroir_map (old text PRIMARY KEY, new text NOT NULL) \
                       ON COMMIT DROP";
            sqlx::raw_sql(map).execute(&mut conn).await?;
            for batch in super::map_batches(pairs)? {
                let fill = "INSERT INTO miroir_map SELECT j->>0, j->>1 FROM json_array_elements($1::json) j";
                sqlx::query(fill).bind(batch).execute(&mut conn).await?;
            }
            sqlx::raw_sql("ANALYZE miroir_map").execute(&mut conn).await?;
            let sql = rewrite_sql(table, column, &base);
            sqlx::raw_sql(AssertSqlSafe(sql)).execute(&mut conn).await?.rows_affected()
        }
    };
    sqlx::raw_sql("COMMIT").execute(&mut conn).await?;
    conn.close().await?;
    Ok(changed)
}

fn only(table: &Table) -> &'static str {
    if table.partitioned { "" } else { "ONLY " }
}

fn blank_sql(table: &Table, column: &str) -> String {
    let c = quote(Dialect::Postgres, column);
    format!("UPDATE {}{} SET {c} = NULL WHERE {c} IS NOT NULL", only(table), table.qualified)
}

fn rewrite_sql(table: &Table, column: &str, base: &str) -> String {
    let c = quote(Dialect::Postgres, column);
    format!(
        "UPDATE {}{} AS t SET {c} = CAST(m.new AS {base}) FROM miroir_map m WHERE t.{c}::text = m.old",
        only(table),
        table.qualified
    )
}

pub async fn install_capture(url: &str, tables: &[Table]) -> Result<Vec<String>, DbError> {
    let mut conn = connect(url).await?;
    let mut script = String::from(CAPTURE);
    for table in tables {
        let tbl = table.name.replace('\'', "''");
        let t = &table.qualified;
        let _ = write!(
            script,
            "\nCREATE TRIGGER miroir_capture AFTER INSERT OR UPDATE OR DELETE ON {t} \
             FOR EACH ROW EXECUTE FUNCTION miroir.capture('{tbl}');\
             \nCREATE TRIGGER miroir_truncate AFTER TRUNCATE ON {t} \
             FOR EACH STATEMENT EXECUTE FUNCTION miroir.capture('{tbl}');"
        );
    }
    sqlx::raw_sql(AssertSqlSafe(script)).execute(&mut conn).await?;
    conn.close().await?;
    Ok(Vec::new())
}

pub async fn clear_log(url: &str) -> Result<(), DbError> {
    let mut conn = connect(url).await?;
    sqlx::raw_sql("SET lock_timeout = '10s'; TRUNCATE miroir.log RESTART IDENTITY")
        .execute(&mut conn)
        .await?;
    conn.close().await?;
    Ok(())
}

pub async fn value(url: &str, sql: &str) -> Result<Option<String>, DbError> {
    let mut conn = connect(url).await?;
    sqlx::raw_sql("BEGIN READ ONLY; SET LOCAL statement_timeout = '10s'")
        .execute(&mut conn)
        .await?;
    let query =
        sqlx::query_scalar::<_, Option<String>>(AssertSqlSafe(check_sql(Dialect::Postgres, sql)));
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
    sqlx::raw_sql("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY").execute(&mut conn).await?;
    let tables = read_tables(&mut conn, schemas).await?;
    let size: i64 = sqlx::query_scalar("SELECT pg_total_relation_size('miroir.log')")
        .fetch_one(&mut conn)
        .await?;
    if size > log_budget {
        sqlx::raw_sql("ROLLBACK").execute(&mut conn).await?;
        conn.close().await?;
        let log = Err(over_budget(size, log_budget));
        return Ok(Snapshot { tables, log, scores: Vec::new() });
    }
    let rows = sqlx::query("SELECT tbl, op, old::text, new::text FROM miroir.log ORDER BY seq")
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
    sqlx::raw_sql("SET LOCAL statement_timeout = '10s'").execute(&mut conn).await?;
    let mut scores = Vec::with_capacity(checks.len());
    for sql in checks {
        sqlx::raw_sql("SAVEPOINT miroir_check").execute(&mut conn).await?;
        let query = sqlx::query_scalar::<_, Option<String>>(AssertSqlSafe(check_sql(
            Dialect::Postgres,
            sql,
        )));
        match query.fetch_optional(&mut conn).await {
            Ok(value) => {
                sqlx::raw_sql("RELEASE SAVEPOINT miroir_check").execute(&mut conn).await?;
                scores.push(Ok(value.flatten()));
            }
            Err(e) => {
                sqlx::raw_sql("ROLLBACK TO SAVEPOINT miroir_check").execute(&mut conn).await?;
                scores.push(Err(e.to_string()));
            }
        }
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
        let only = if table.partitioned { "" } else { "ONLY " };
        let sql = if columns.is_empty() {
            format!("SELECT count(*)::text FROM {only}{}", table.qualified)
        } else {
            let row: Vec<String> = columns.iter().map(|c| quote(Dialect::Postgres, c)).collect();
            format!(
                "SELECT count(*)::text || ':' || coalesce(md5(string_agg(h, '' ORDER BY h)), '') \
                 FROM (SELECT md5(ROW({})::text) AS h FROM {only}{}) rows",
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

    fn column(name: &str, kind: ColumnKind) -> Column {
        Column { name: name.into(), kind, generated: false, pk: false, sql_type: String::new() }
    }

    #[test]
    fn a_rewrite_casts_to_the_columns_type_without_its_modifier() {
        let (qualified, partitioned) = ("\"public\".\"contacts\"".to_string(), false);
        let (name, schema) = ("contacts".to_string(), "public".to_string());
        let columns = Vec::new();
        let table = Table { name, schema, qualified, columns, partitioned, id: 0, captured: false };
        assert_eq!(
            distinct_sql(&table, "email"),
            "SELECT DISTINCT \"email\"::text FROM \"public\".\"contacts\" WHERE \"email\" IS NOT NULL"
        );
        assert_eq!(
            rewrite_sql(&table, "code", "bpchar"),
            "UPDATE ONLY \"public\".\"contacts\" AS t SET \"code\" = CAST(m.new AS bpchar) \
             FROM miroir_map m WHERE t.\"code\"::text = m.old"
        );
        let parent = Table { partitioned: true, ..table };
        assert_eq!(
            blank_sql(&parent, "birthday"),
            "UPDATE \"public\".\"contacts\" SET \"birthday\" = NULL WHERE \"birthday\" IS NOT NULL",
            "a partitioned table updates its partitions too"
        );
    }

    #[test]
    fn shift_sql_moves_each_kind_of_time_its_own_way() {
        let unix = ColumnKind::UnixTime { per_second: 1, max: i64::MAX.unsigned_abs() };
        let columns = vec![
            column("due", ColumnKind::Date),
            column("at", ColumnKind::Timestamp),
            column("created_unix", unix),
        ];
        let (qualified, partitioned) = ("\"public\".\"issue\"".to_string(), false);
        let (name, schema) = ("issue".to_string(), "public".to_string());
        let table = Table { name, schema, qualified, columns, partitioned, id: 0, captured: true };
        let moving: Vec<&Column> = table.columns.iter().collect();
        let sql = shift_sql(&table, &moving, Move { seconds: 2 * 86_400, dates: 3 });
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
    fn the_log_moves_each_copy_of_a_column_as_the_table_moved() {
        let column = column("created_at", ColumnKind::Timestamp);
        let column = Column { sql_type: "timestamp(3) with time zone".into(), ..column };
        let (qualified, partitioned) = ("\"public\".\"it's\"".to_string(), false);
        let (name, schema) = ("it's".to_string(), "public".to_string());
        let table = Table {
            name,
            schema,
            qualified,
            columns: Vec::new(),
            partitioned,
            id: 0,
            captured: true,
        };
        let value = "(old->>'created_at')::timestamp(3) with time zone";
        assert_eq!(
            log_sql(&table, &column, "old", Move { seconds: -1_800, dates: -1 }),
            format!(
                "UPDATE miroir.log SET old = jsonb_set(old, ARRAY['created_at'], \
                 to_jsonb({value} + make_interval(secs => -1800))) \
                 WHERE tbl = 'it''s' AND {value} < '9000-01-01'"
            )
        );
    }

    #[test]
    fn a_move_by_minutes_takes_any_value_back() {
        let by = Move { seconds: -1_800, dates: -1 };
        let unix = ColumnKind::UnixTime { per_second: 1000, max: i64::MAX.unsigned_abs() };
        assert_eq!(
            moved("r.\"at\"", ColumnKind::Timestamp, by),
            (
                "r.\"at\" < '9000-01-01'".to_string(),
                "r.\"at\" + make_interval(secs => -1800)".to_string()
            )
        );
        assert_eq!(moved("d", ColumnKind::Date, by).1, "d + (-1)");
        assert_eq!(moved("u", unix, by).1, "u + (-1800000)");
    }
}
