//! The evidence file, `/miroir/evidence/evidence.db` (§4.4): what an episode changed and sent.
//! `miroir collect` writes it once; `miroir grade` and `miroir draft` read it.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};
use sqlx::{ConnectOptions, Connection, SqliteConnection};

/// The evidence format, `meta.format`. A grader voids evidence of any other format.
pub const FORMAT: &str = "1";
/// The largest evidence file collect keeps whole. Past it, collect writes `meta`, `tables` and
/// `faults` only, with an `evidence_budget` fault.
pub const BUDGET: u64 = 16 * 1024 * 1024;

/// The evidence schema.
pub const SCHEMA: &str = "\
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
-- exactly two rows: format = '1' and miroir = '<the collecting binary's version>'

CREATE TABLE tables (name TEXT PRIMARY KEY, columns TEXT NOT NULL, pk TEXT NOT NULL);
-- every table captured at boot: JSON arrays of column names (table order, ignored columns
-- left out) and primary-key columns ([] for a keyless table)

CREATE TABLE changes (
  id      INTEGER PRIMARY KEY,
  tbl     TEXT NOT NULL,   -- table; schema.table outside public
  op      TEXT NOT NULL,   -- insert | update | delete | truncate | schema
  pk      TEXT NOT NULL,   -- JSON object of the primary key; {} for keyless rows, truncate, schema
  before  TEXT,            -- JSON row as the episode found it; NULL for insert
  after   TEXT,            -- JSON row as the episode left it; NULL for delete
  columns TEXT             -- JSON array of changed columns (update, schema); NULL otherwise
);
-- one row per net-changed row (keyless tables: one per net instance), or per table for
-- truncate and schema; rows absent from changes are as baked

CREATE TABLE sent_emails (
  seq INTEGER PRIMARY KEY,   -- position in the effects journal (shared with sent_requests)
  sender TEXT, recipients TEXT NOT NULL, cc TEXT NOT NULL, bcc TEXT NOT NULL,
  subject TEXT, text TEXT, html TEXT
);
-- recipients, cc, bcc: JSON arrays of addresses; bcc = envelope recipients missing from headers

CREATE TABLE sent_requests (
  seq INTEGER PRIMARY KEY,
  method TEXT NOT NULL, path TEXT NOT NULL,
  query TEXT NOT NULL,     -- JSON object of query parameters
  headers TEXT NOT NULL,   -- JSON object, lower-case names, authorization dropped
  body TEXT NOT NULL       -- always valid JSON: the parsed body, a JSON string of the text, or null
);

CREATE TABLE faults (seq INTEGER PRIMARY KEY, kind TEXT NOT NULL, detail TEXT NOT NULL);
-- kind: crash | sink | quiet_timeout | database | log_budget | evidence_budget
-- sink: the HTTP sink could not read a request (a body over 25 MiB), or an email over 25 MiB
-- was recorded truncated
";

/// The kinds of `faults.kind`. Every fault is harm.
pub mod fault {
    /// A manifest process exited while the world ran.
    pub const CRASH: &str = "crash";
    /// The HTTP sink could not read a request, or an email over 25 MiB was recorded truncated.
    pub const SINK: &str = "sink";
    /// The world's background work did not become quiet at collect.
    pub const QUIET_TIMEOUT: &str = "quiet_timeout";
    /// The database did not answer at collect.
    pub const DATABASE: &str = "database";
    /// The change log's text passed `pg::LOG_BUDGET`, so its changes were left out.
    pub const LOG_BUDGET: &str = "log_budget";
    /// The evidence passed `evidence::BUDGET`, so its changes and effects were left out.
    pub const EVIDENCE_BUDGET: &str = "evidence_budget";
}

/// The kind of a net change, `changes.op`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeOp {
    Insert,
    Update,
    Delete,
    Truncate,
    Schema,
}

impl ChangeOp {
    /// The op as `changes.op` spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            ChangeOp::Insert => "insert",
            ChangeOp::Update => "update",
            ChangeOp::Delete => "delete",
            ChangeOp::Truncate => "truncate",
            ChangeOp::Schema => "schema",
        }
    }
}

/// One row of `changes`: a row the episode changed, net, or a table it truncated or reshaped.
#[derive(Debug, Clone, PartialEq)]
pub struct NetChange {
    /// The table, as `pg::Table::evidence_name` names it.
    pub tbl: String,
    pub op: ChangeOp,
    /// The primary key; empty for keyless rows, truncate and schema.
    pub pk: Map<String, Value>,
    /// The row as the episode found it; `None` for an insert.
    pub before: Option<Map<String, Value>>,
    /// The row as the episode left it; `None` for a delete.
    pub after: Option<Map<String, Value>>,
    /// The changed columns of an update or a schema change.
    pub columns: Option<Vec<String>>,
}

/// One row of `faults`, and one line of `faults.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fault {
    /// One of the [`fault`] kinds.
    pub kind: String,
    pub detail: String,
}

/// One row of `tables`: a table captured at boot.
#[derive(Debug, Clone)]
pub struct CapturedTable {
    /// As `pg::Table::evidence_name` names it.
    pub name: String,
    /// In table order, ignored columns left out.
    pub columns: Vec<String>,
    /// The primary-key columns; empty for a keyless table.
    pub pk: Vec<String>,
}

/// Everything an evidence file holds.
#[derive(Debug)]
pub struct Evidence {
    /// The collecting binary's version, `meta.miroir`.
    pub miroir: String,
    pub tables: Vec<CapturedTable>,
    pub changes: Vec<NetChange>,
    /// The lines of `effects.jsonl`, in journal order.
    pub effects: Vec<Value>,
    /// The lines of `faults.jsonl`, then collect's own faults.
    pub faults: Vec<Fault>,
}

/// An `effects.jsonl` line, in the shape `journal.rs` states.
#[derive(Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "lowercase", deny_unknown_fields)]
enum Effect {
    Email(Email),
    Http(Request),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Email {
    from: Option<String>,
    to: Vec<String>,
    cc: Vec<String>,
    bcc: Vec<String>,
    subject: Option<String>,
    text: Option<String>,
    html: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    method: String,
    path: String,
    query: BTreeMap<String, String>,
    headers: BTreeMap<String, String>,
    body: Value,
}

/// Write a new evidence file at `path`, which must not exist, and return its size in bytes.
/// Each effect line becomes a row of `sent_emails` or `sent_requests` whose `seq` is the line's
/// 1-based position. A line of another kind or shape is an error: a Miroir bug.
pub async fn write(path: &Path, evidence: &Evidence) -> anyhow::Result<u64> {
    tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await
        .with_context(|| format!("cannot create the evidence file {}", path.display()))?;
    fill(path, evidence)
        .await
        .with_context(|| format!("cannot write the evidence file {}", path.display()))?;
    Ok(tokio::fs::metadata(path).await?.len())
}

async fn fill(path: &Path, evidence: &Evidence) -> anyhow::Result<()> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .journal_mode(SqliteJournalMode::Delete)
        .disable_statement_logging();
    let mut conn = SqliteConnection::connect_with(&options).await?;
    sqlx::raw_sql(SCHEMA).execute(&mut conn).await?;
    let mut tx = conn.begin().await?;
    insert_meta(&mut tx, evidence).await?;
    insert_changes(&mut tx, &evidence.changes).await?;
    insert_effects(&mut tx, &evidence.effects).await?;
    insert_faults(&mut tx, &evidence.faults).await?;
    tx.commit().await?;
    conn.close().await?;
    Ok(())
}

/// Open an evidence file to read it: read-only, with statement logging off, once its
/// `meta.format` is [`FORMAT`]. A missing file is an error and is never created; another
/// format is an error naming both formats and the version that wrote the file.
pub async fn open(path: &Path) -> anyhow::Result<SqliteConnection> {
    let options =
        SqliteConnectOptions::new().filename(path).read_only(true).disable_statement_logging();
    let mut conn = SqliteConnection::connect_with(&options)
        .await
        .with_context(|| format!("cannot open {}", path.display()))?;
    let meta: BTreeMap<String, String> = sqlx::query_as("SELECT key, value FROM meta")
        .fetch_all(&mut conn)
        .await
        .with_context(|| format!("{} is not Miroir evidence", path.display()))?
        .into_iter()
        .collect();
    let format = meta.get("format").map_or("none", String::as_str);
    if format != FORMAT {
        let writer = meta.get("miroir").map_or("unknown", String::as_str);
        anyhow::bail!(
            "{} is evidence format {format}, written by miroir {writer}, but this miroir ({}) \
             reads format {FORMAT}: grade it with the miroir image of the release that \
             collected it",
            path.display(),
            env!("CARGO_PKG_VERSION"),
        );
    }
    Ok(conn)
}

async fn insert_meta(conn: &mut SqliteConnection, evidence: &Evidence) -> anyhow::Result<()> {
    for (key, value) in [("format", FORMAT), ("miroir", evidence.miroir.as_str())] {
        sqlx::query("INSERT INTO meta (key, value) VALUES (?, ?)")
            .bind(key)
            .bind(value)
            .execute(&mut *conn)
            .await?;
    }
    for table in &evidence.tables {
        sqlx::query("INSERT INTO tables (name, columns, pk) VALUES (?, ?, ?)")
            .bind(&table.name)
            .bind(serde_json::to_string(&table.columns)?)
            .bind(serde_json::to_string(&table.pk)?)
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

async fn insert_changes(conn: &mut SqliteConnection, changes: &[NetChange]) -> anyhow::Result<()> {
    for (id, change) in (1_i64..).zip(changes) {
        sqlx::query(
            "INSERT INTO changes (id, tbl, op, pk, before, after, columns) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(&change.tbl)
        .bind(change.op.as_str())
        .bind(serde_json::to_string(&change.pk)?)
        .bind(json(change.before.as_ref())?)
        .bind(json(change.after.as_ref())?)
        .bind(json(change.columns.as_ref())?)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

async fn insert_effects(conn: &mut SqliteConnection, effects: &[Value]) -> anyhow::Result<()> {
    for (seq, line) in (1_i64..).zip(effects) {
        let effect = Effect::deserialize(line)
            .with_context(|| format!("effects.jsonl line {seq} is not an email or a request"))?;
        let insert = match effect {
            Effect::Email(email) => sqlx::query(
                "INSERT INTO sent_emails (seq, sender, recipients, cc, bcc, subject, text, html) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(seq)
            .bind(email.from)
            .bind(serde_json::to_string(&email.to)?)
            .bind(serde_json::to_string(&email.cc)?)
            .bind(serde_json::to_string(&email.bcc)?)
            .bind(email.subject)
            .bind(email.text)
            .bind(email.html),
            Effect::Http(request) => sqlx::query(
                "INSERT INTO sent_requests (seq, method, path, query, headers, body) \
                 VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(seq)
            .bind(request.method)
            .bind(request.path)
            .bind(serde_json::to_string(&request.query)?)
            .bind(serde_json::to_string(&request.headers)?)
            .bind(serde_json::to_string(&request.body)?),
        };
        insert.execute(&mut *conn).await?;
    }
    Ok(())
}

async fn insert_faults(conn: &mut SqliteConnection, faults: &[Fault]) -> anyhow::Result<()> {
    for (seq, fault) in (1_i64..).zip(faults) {
        sqlx::query("INSERT INTO faults (seq, kind, detail) VALUES (?, ?, ?)")
            .bind(seq)
            .bind(&fault.kind)
            .bind(&fault.detail)
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

/// The JSON text of an optional value; `None` stays NULL.
fn json<T: Serialize>(value: Option<&T>) -> serde_json::Result<Option<String>> {
    value.map(serde_json::to_string).transpose()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::json;
    use sqlx::FromRow;
    use sqlx::sqlite::SqliteRow;

    use super::*;

    type ChangeRow = (i64, String, String, String, Option<String>, Option<String>, Option<String>);
    type EmailRow = (i64, Option<String>, String, String, String, Option<String>);
    type RequestRow = (String, String, String, i64, String);

    /// A path in the temp directory that no file has yet.
    fn new_path(name: &str) -> PathBuf {
        let nanos =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let file = format!("miroir-evidence-{name}-{}-{nanos}.db", std::process::id());
        std::env::temp_dir().join(file)
    }

    fn object(value: Value) -> Map<String, Value> {
        let Value::Object(map) = value else { panic!("not a JSON object: {value}") };
        map
    }

    fn evidence(effects: Vec<Value>) -> Evidence {
        Evidence {
            miroir: "0.1.0".into(),
            tables: Vec::new(),
            changes: Vec::new(),
            effects,
            faults: Vec::new(),
        }
    }

    fn email(to: &str) -> Value {
        json!({"kind": "email", "data": {
            "from": "crm@acme.test", "to": [to], "cc": ["dana@acme.test"],
            "bcc": ["audit@acme.test"], "subject": "Deal 2001 is now yours",
            "text": "It is yours.", "html": null}})
    }

    fn request(path: &str, body: &Value) -> Value {
        json!({"kind": "http", "data": {
            "method": "POST", "path": path, "query": {"expand": "charges"},
            "headers": {"content-type": "application/json"}, "body": body}})
    }

    /// Write `evidence` to a new file and open it to read.
    async fn written(name: &str, evidence: &Evidence) -> (PathBuf, SqliteConnection) {
        let path = new_path(name);
        write(&path, evidence).await.unwrap();
        let conn = open(&path).await.unwrap();
        (path, conn)
    }

    async fn rows<T>(conn: &mut SqliteConnection, sql: &'static str) -> Vec<T>
    where
        T: for<'r> FromRow<'r, SqliteRow> + Send + Unpin,
    {
        sqlx::query_as(sql).fetch_all(conn).await.unwrap()
    }

    #[tokio::test]
    async fn meta_tables_and_faults_read_back_as_written() {
        let mut evidence = evidence(Vec::new());
        evidence.tables = vec![
            CapturedTable {
                name: "invoices".into(),
                columns: vec!["id".into(), "status".into()],
                pk: vec!["id".into()],
            },
            CapturedTable { name: "audit.log".into(), columns: vec!["line".into()], pk: vec![] },
        ];
        evidence.faults = vec![
            Fault { kind: fault::CRASH.into(), detail: "web exited with status 1".into() },
            Fault { kind: fault::QUIET_TIMEOUT.into(), detail: "quiet failed twice".into() },
        ];
        let path = new_path("tables");
        let size = write(&path, &evidence).await.unwrap();
        assert_eq!(size, std::fs::metadata(&path).unwrap().len());
        let mut conn = open(&path).await.unwrap();

        let meta: Vec<(String, String)> =
            rows(&mut conn, "SELECT key, value FROM meta ORDER BY key").await;
        assert_eq!(meta, [("format".into(), FORMAT.into()), ("miroir".into(), "0.1.0".into())]);
        let tables: Vec<(String, String, String)> =
            rows(&mut conn, "SELECT name, columns, pk FROM tables ORDER BY rowid").await;
        assert_eq!(
            tables,
            [
                ("invoices".into(), r#"["id","status"]"#.into(), r#"["id"]"#.into()),
                ("audit.log".into(), r#"["line"]"#.into(), "[]".into()),
            ]
        );
        let faults: Vec<(i64, String, String)> =
            rows(&mut conn, "SELECT seq, kind, detail FROM faults ORDER BY seq").await;
        assert_eq!(
            faults,
            [
                (1, "crash".into(), "web exited with status 1".into()),
                (2, "quiet_timeout".into(), "quiet failed twice".into()),
            ]
        );
        let effects: Vec<(i64,)> = rows(
            &mut conn,
            "SELECT (SELECT count(*) FROM sent_emails) + (SELECT count(*) FROM sent_requests)",
        )
        .await;
        assert_eq!(effects, [(0,)]);
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn changes_read_back_as_written_with_ids_in_order() {
        let mut evidence = evidence(Vec::new());
        let change =
            |tbl: &str, op, pk, before: Option<Value>, after: Option<Value>, columns| NetChange {
                tbl: String::from(tbl),
                op,
                pk: object(pk),
                before: before.map(object),
                after: after.map(object),
                columns,
            };
        evidence.changes = vec![
            change(
                "invoices",
                ChangeOp::Update,
                json!({"id": 4001}),
                Some(json!({"id": 4001, "status": "open"})),
                Some(json!({"id": 4001, "status": "paid"})),
                Some(vec!["status".into()]),
            ),
            change("audit.log", ChangeOp::Insert, json!({}), None, Some(json!({"n": 1})), None),
            change(
                "invoices",
                ChangeOp::Delete,
                json!({"id": 9}),
                Some(json!({"id": 9})),
                None,
                None,
            ),
            change("deals", ChangeOp::Truncate, json!({}), None, None, None),
            change("notes", ChangeOp::Schema, json!({}), None, None, Some(vec!["memo".into()])),
        ];
        let (path, mut conn) = written("changes", &evidence).await;
        let changes: Vec<ChangeRow> = rows(
            &mut conn,
            "SELECT id, tbl, op, pk, before, after, columns FROM changes ORDER BY id",
        )
        .await;
        let text = |s: &str| Some(s.to_string());
        let row = |id, tbl: &str, op: &str, pk: &str, before, after, columns| {
            (id, tbl.to_string(), op.to_string(), pk.to_string(), before, after, columns)
        };
        assert_eq!(
            changes,
            [
                row(
                    1,
                    "invoices",
                    "update",
                    r#"{"id":4001}"#,
                    text(r#"{"id":4001,"status":"open"}"#),
                    text(r#"{"id":4001,"status":"paid"}"#),
                    text(r#"["status"]"#),
                ),
                row(2, "audit.log", "insert", "{}", None, text(r#"{"n":1}"#), None),
                row(3, "invoices", "delete", r#"{"id":9}"#, text(r#"{"id":9}"#), None, None),
                row(4, "deals", "truncate", "{}", None, None, None),
                row(5, "notes", "schema", "{}", None, None, text(r#"["memo"]"#)),
            ]
        );
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn an_email_line_maps_to_its_columns() {
        let unparsed = json!({"kind": "email", "data": {
            "from": null, "to": [], "cc": [], "bcc": ["ops@acme.test"],
            "subject": null, "text": null, "html": "<p>hi</p>"}});
        let (path, mut conn) =
            written("email", &evidence(vec![email("marcus@acme.test"), unparsed])).await;
        let emails: Vec<EmailRow> = rows(
            &mut conn,
            "SELECT seq, sender, recipients, cc, bcc, subject FROM sent_emails ORDER BY seq",
        )
        .await;
        assert_eq!(
            emails,
            [
                (
                    1,
                    Some("crm@acme.test".into()),
                    r#"["marcus@acme.test"]"#.into(),
                    r#"["dana@acme.test"]"#.into(),
                    r#"["audit@acme.test"]"#.into(),
                    Some("Deal 2001 is now yours".into()),
                ),
                (2, None, "[]".into(), "[]".into(), r#"["ops@acme.test"]"#.into(), None),
            ]
        );
        let bodies: Vec<(Option<String>, Option<String>)> =
            rows(&mut conn, "SELECT text, html FROM sent_emails ORDER BY seq").await;
        assert_eq!(bodies, [(Some("It is yours.".into()), None), (None, Some("<p>hi</p>".into()))]);
        let first: Vec<(String,)> =
            rows(&mut conn, "SELECT recipients ->> '$[0]' FROM sent_emails WHERE seq = 1").await;
        assert_eq!(first, [("marcus@acme.test".into(),)]);
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn request_bodies_are_valid_json_encoded_once() {
        let effects = vec![
            request("/json", &json!({"amount": 125_000})),
            request("/text", &json!("amount=125000")),
            request("/empty", &Value::Null),
        ];
        let (path, mut conn) = written("bodies", &evidence(effects)).await;
        let requests: Vec<RequestRow> = rows(
            &mut conn,
            "SELECT path, query, headers, json_valid(body), json_type(body) \
             FROM sent_requests ORDER BY seq",
        )
        .await;
        let row = |path: &str, kind: &str| {
            let query = r#"{"expand":"charges"}"#.to_string();
            let headers = r#"{"content-type":"application/json"}"#.to_string();
            (path.to_string(), query, headers, 1, kind.to_string())
        };
        assert_eq!(requests, [row("/json", "object"), row("/text", "text"), row("/empty", "null")]);
        let read: Vec<(Option<i64>, Option<String>)> = rows(
            &mut conn,
            "SELECT body ->> '$.amount', CASE WHEN json_type(body) = 'text' THEN body ->> '$' END \
             FROM sent_requests ORDER BY seq",
        )
        .await;
        assert_eq!(
            read,
            [(Some(125_000), None), (None, Some("amount=125000".into())), (None, None)]
        );
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn emails_and_requests_share_the_journal_order() {
        let effects = vec![
            request("/first", &Value::Null),
            email("marcus@acme.test"),
            request("/third", &Value::Null),
            email("dana@acme.test"),
        ];
        let (path, mut conn) = written("order", &evidence(effects)).await;
        let requests: Vec<(i64, String)> =
            rows(&mut conn, "SELECT seq, path FROM sent_requests ORDER BY seq").await;
        assert_eq!(requests, [(1, "/first".into()), (3, "/third".into())]);
        let emails: Vec<(i64, String)> =
            rows(&mut conn, "SELECT seq, recipients ->> '$[0]' FROM sent_emails ORDER BY seq")
                .await;
        assert_eq!(emails, [(2, "marcus@acme.test".into()), (4, "dana@acme.test".into())]);
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn an_unknown_effect_kind_is_an_error() {
        let path = new_path("kind");
        let sms = json!({"kind": "sms", "data": {"to": "+15550100"}});
        let error =
            write(&path, &evidence(vec![email("marcus@acme.test"), sms])).await.unwrap_err();
        let error = format!("{error:#}");
        assert!(error.starts_with("cannot write the evidence file "), "{error}");
        assert!(error.contains(": effects.jsonl line 2 is not an email or a request: "), "{error}");
        assert!(error.contains("unknown variant `sms`"), "{error}");
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn an_effect_line_of_another_shape_is_an_error() {
        let shapes = [
            json!({"kind": "http", "data": {
                "path": "/x", "query": {}, "headers": {}, "body": null}}),
            json!({"kind": "http", "data": {
                "method": "GET", "path": "/x", "query": {}, "headers": {}}}),
            json!({"kind": "http", "data": {
                "method": "GET", "path": "/x", "query": {"page": 2}, "headers": {}, "body": null}}),
            json!({"kind": "email", "data": {"to": "marcus@acme.test", "cc": [], "bcc": []}}),
            json!({"kind": "email", "data": {"to": [], "cc": [], "bcc": [], "attachments": []}}),
            json!({"kind": "email", "data": {"to": [], "cc": [], "bcc": []}, "at": 3}),
            json!(["email"]),
        ];
        for shape in shapes {
            let path = new_path("shape");
            let error = write(&path, &evidence(vec![shape.clone()])).await.unwrap_err();
            assert!(format!("{error:#}").contains(": effects.jsonl line 1 is not"), "{shape}");
            std::fs::remove_file(path).unwrap();
        }
    }

    #[tokio::test]
    async fn an_existing_file_is_never_overwritten() {
        let path = new_path("exists");
        std::fs::write(&path, "kept").unwrap();
        let error = write(&path, &evidence(Vec::new())).await.unwrap_err();
        assert!(format!("{error:#}").starts_with("cannot create the evidence file"), "{error:#}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "kept");
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn open_refuses_another_format_naming_both() {
        let path = new_path("format");
        write(&path, &evidence(Vec::new())).await.unwrap();
        let options = SqliteConnectOptions::new().filename(&path);
        let mut conn = SqliteConnection::connect_with(&options).await.unwrap();
        sqlx::raw_sql("UPDATE meta SET value = '2' WHERE key = 'format'")
            .execute(&mut conn)
            .await
            .unwrap();
        conn.close().await.unwrap();
        let error = open(&path).await.unwrap_err().to_string();
        assert!(error.contains("is evidence format 2, written by miroir 0.1.0"), "{error}");
        assert!(error.contains(&format!("reads format {FORMAT}")), "{error}");
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn open_refuses_a_file_that_is_not_evidence() {
        let path = new_path("other");
        let options = SqliteConnectOptions::new().filename(&path).create_if_missing(true);
        let mut conn = SqliteConnection::connect_with(&options).await.unwrap();
        sqlx::raw_sql("CREATE TABLE notes (text TEXT)").execute(&mut conn).await.unwrap();
        conn.close().await.unwrap();
        let error = open(&path).await.unwrap_err().to_string();
        assert!(error.ends_with("is not Miroir evidence"), "{error}");
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn open_never_creates_a_missing_file() {
        let path = new_path("missing");
        let error = open(&path).await.unwrap_err().to_string();
        assert!(error.starts_with("cannot open"), "{error}");
        assert!(!path.exists());
    }
}
