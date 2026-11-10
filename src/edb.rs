//! The episode database, a `SQLite` file: actions, effects, net changes and the answer,
//! which episode checks query.

use std::path::{Path, PathBuf};
use std::sync::PoisonError;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{Map, Value, json};
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteSynchronous,
};
use sqlx::{AssertSqlSafe, Connection, Row, SqliteConnection};

use crate::grade::NetChange;

/// How long a check may run.
pub const CHECK_LIMIT: Duration = Duration::from_secs(10);
const EFFECT_BUDGET: u64 = 1024 * 1024 * 1024;
const CODE_BUDGET: u64 = 64 * 1024 * 1024;

const EPISODE_SCHEMA: &str = "
CREATE TABLE actions (seq INTEGER PRIMARY KEY, tool TEXT NOT NULL, app TEXT, method TEXT,
                      path TEXT, status INTEGER, latency_ms INTEGER NOT NULL,
                      arguments TEXT NOT NULL, result TEXT, via TEXT);
CREATE TABLE effects (seq INTEGER PRIMARY KEY, kind TEXT NOT NULL, data TEXT NOT NULL);
CREATE TABLE changes (app TEXT, tbl TEXT NOT NULL, op TEXT NOT NULL, pk TEXT NOT NULL,
                      before TEXT, after TEXT, columns TEXT);
CREATE TABLE answer (value TEXT);";

/// A call to record; `via` says what made it when the agent didn't call it directly.
pub struct ActionRow<'a> {
    pub tool: &'a str,
    pub app: Option<&'a str>,
    pub method: Option<&'a str>,
    pub path: Option<&'a str>,
    pub status: Option<u64>,
    pub latency_ms: i64,
    pub arguments: &'a Value,
    pub result: &'a Value,
    pub via: Option<&'a str>,
}

/// A recorded call, as read back.
pub struct StoredAction {
    pub tool: String,
    pub app: Option<String>,
    pub method: Option<String>,
    pub path: Option<String>,
    pub status: Option<i64>,
    pub latency_ms: i64,
    pub arguments: String,
    pub result: Option<String>,
    pub via: Option<String>,
}

/// A stored net change, as read back.
pub struct StoredChange {
    pub app: Option<String>,
    pub tbl: String,
    pub op: String,
    pub pk: String,
    pub before: Option<String>,
    pub after: Option<String>,
    pub columns: Option<String>,
}

/// The episode database, and whether the episode lost part of its record or broke a
/// budget.
pub struct EpisodeDb {
    pool: SqlitePool,
    pub path: PathBuf,
    lost: std::sync::Mutex<Option<String>>,
    refused: std::sync::Mutex<Option<String>>,
    effect_bytes: AtomicU64,
    code_bytes: AtomicU64,
}

impl EpisodeDb {
    /// Create the database afresh, readable by its owner only.
    pub async fn create(path: &Path) -> Result<EpisodeDb, sqlx::Error> {
        for suffix in ["", "-journal", "-wal", "-shm"] {
            match tokio::fs::remove_file(format!("{}{suffix}", path.display())).await {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    return Err(sqlx::Error::Io(e));
                }
                _ => {}
            }
        }
        tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .await
            .map_err(sqlx::Error::Io)?;
        let options = SqliteConnectOptions::new()
            .filename(path)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(CHECK_LIMIT);
        let pool = SqlitePoolOptions::new().max_connections(1).connect_with(options).await?;
        sqlx::raw_sql(EPISODE_SCHEMA).execute(&pool).await?;
        Ok(EpisodeDb {
            pool,
            path: path.to_path_buf(),
            lost: std::sync::Mutex::new(None),
            refused: std::sync::Mutex::new(None),
            effect_bytes: AtomicU64::new(0),
            code_bytes: AtomicU64::new(0),
        })
    }

    /// Empty it for the next episode.
    pub async fn clear(&self) -> Result<(), sqlx::Error> {
        sqlx::raw_sql(
            "DELETE FROM actions; DELETE FROM effects; DELETE FROM changes; DELETE FROM answer; \
             VACUUM; PRAGMA wal_checkpoint(TRUNCATE)",
        )
        .execute(&self.pool)
        .await?;
        *self.lost.lock().unwrap_or_else(PoisonError::into_inner) = None;
        *self.refused.lock().unwrap_or_else(PoisonError::into_inner) = None;
        self.effect_bytes.store(0, Ordering::SeqCst);
        self.code_bytes.store(0, Ordering::SeqCst);
        Ok(())
    }

    /// Why miroir lost part of the record, which voids the grade.
    pub fn lost(&self) -> Option<String> {
        self.lost.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Note that miroir lost part of the record.
    pub fn lose(&self, what: &str) {
        tracing::error!("{what}");
        self.lost.lock().unwrap_or_else(PoisonError::into_inner).get_or_insert_with(|| what.into());
    }

    /// Note that the episode broke a budget, which faults its grade.
    pub fn refuse(&self, why: String) {
        tracing::warn!("{why}");
        self.refused.lock().unwrap_or_else(PoisonError::into_inner).get_or_insert(why);
    }

    /// Why the episode broke a budget.
    pub fn refused(&self) -> Option<String> {
        self.refused.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Record a call; calls made `via` `run_code` count toward the code's budget.
    pub async fn add_action(&self, row: &ActionRow<'_>) {
        let (arguments, result) = (row.arguments.to_string(), row.result.to_string());
        if row.via == Some("run_code") {
            let size = u64::try_from(arguments.len() + result.len()).unwrap_or(u64::MAX);
            self.code_bytes.fetch_add(size, Ordering::SeqCst);
        }
        let sql = "INSERT INTO actions (tool, app, method, path, status, latency_ms, arguments, \
                   result, via) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)";
        let insert = sqlx::query(sql)
            .bind(row.tool)
            .bind(row.app)
            .bind(row.method)
            .bind(row.path)
            .bind(row.status.and_then(|s| i64::try_from(s).ok()))
            .bind(row.latency_ms)
            .bind(arguments)
            .bind(result)
            .bind(row.via);
        if let Err(e) = insert.execute(&self.pool).await {
            self.lose(&format!("cannot record an action: {e}"));
        }
    }

    /// Whether the episode's code is still within its 64 MiB of requests.
    pub fn code_allowed(&self) -> bool {
        if self.code_bytes.load(Ordering::SeqCst) < CODE_BUDGET {
            return true;
        }
        self.refuse("the episode's code made more than 64 MiB of requests".into());
        false
    }

    /// Record an email or HTTP call, within the 1 GiB budget.
    pub async fn add_effect(&self, kind: &str, data: &Value) {
        let data = data.to_string();
        let size = u64::try_from(data.len()).unwrap_or(u64::MAX);
        let spent = self.effect_bytes.fetch_add(size, Ordering::SeqCst).saturating_add(size);
        if spent > EFFECT_BUDGET {
            self.refuse("the episode's emails and HTTP calls took more than 1 GiB".into());
            return;
        }
        let insert =
            sqlx::query("INSERT INTO effects (kind, data) VALUES (?, ?)").bind(kind).bind(data);
        if let Err(e) = insert.execute(&self.pool).await {
            self.lose(&format!("cannot record an effect: {e}"));
        }
    }

    pub async fn set_answer(&self, answer: &str) -> Result<(), String> {
        let insert = sqlx::query("INSERT INTO answer (value) VALUES (?)").bind(answer);
        insert
            .execute(&self.pool)
            .await
            .map(|_| ())
            .map_err(|e| format!("cannot store the answer: {e}"))
    }

    /// The last submitted answer.
    pub async fn answer(&self) -> Option<String> {
        let query = sqlx::query_scalar::<_, Option<String>>(
            "SELECT value FROM answer ORDER BY rowid DESC LIMIT 1",
        );
        query.fetch_optional(&self.pool).await.ok().flatten().flatten()
    }

    /// Replace the stored net changes.
    pub async fn write_changes(&self, changes: &[NetChange]) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM changes").execute(&mut *tx).await?;
        for change in changes {
            let object = |row: &Option<Map<String, Value>>| {
                row.as_ref().map(|r| Value::Object(r.clone()).to_string())
            };
            sqlx::query(
                "INSERT INTO changes (app, tbl, op, pk, before, after, columns) \
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(change.app.as_deref())
            .bind(&change.tbl)
            .bind(change.op.as_str())
            .bind(Value::Object(change.pk.clone()).to_string())
            .bind(object(&change.before))
            .bind(object(&change.after))
            .bind(change.columns.as_ref().map(|c| json!(c).to_string()))
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await
    }

    pub async fn effect_count(&self) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar("SELECT count(*) FROM effects").fetch_one(&self.pool).await
    }

    /// Sequence number of the last effect; 0 when there is none.
    pub async fn last_effect(&self) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar("SELECT coalesce(max(seq), 0) FROM effects").fetch_one(&self.pool).await
    }

    pub async fn actions(&self) -> Result<Vec<StoredAction>, sqlx::Error> {
        let sql = "SELECT tool, app, method, path, status, latency_ms, arguments, result, via \
                   FROM actions ORDER BY seq";
        let rows = sqlx::query(sql).fetch_all(&self.pool).await?;
        rows.iter()
            .map(|row| {
                Ok(StoredAction {
                    tool: row.try_get(0)?,
                    app: row.try_get(1)?,
                    method: row.try_get(2)?,
                    path: row.try_get(3)?,
                    status: row.try_get(4)?,
                    latency_ms: row.try_get(5)?,
                    arguments: row.try_get(6)?,
                    result: row.try_get(7)?,
                    via: row.try_get(8)?,
                })
            })
            .collect()
    }

    pub async fn changes(&self) -> Result<Vec<StoredChange>, sqlx::Error> {
        let sql = "SELECT app, tbl, op, pk, before, after, columns FROM changes ORDER BY rowid";
        let rows = sqlx::query(sql).fetch_all(&self.pool).await?;
        rows.iter()
            .map(|row| {
                Ok(StoredChange {
                    app: row.try_get(0)?,
                    tbl: row.try_get(1)?,
                    op: row.try_get(2)?,
                    pk: row.try_get(3)?,
                    before: row.try_get(4)?,
                    after: row.try_get(5)?,
                    columns: row.try_get(6)?,
                })
            })
            .collect()
    }

    /// Effects after `after`, up to `last`, 64 at a time.
    pub async fn effects(
        &self,
        after: i64,
        last: i64,
    ) -> Result<Vec<(i64, String, String)>, sqlx::Error> {
        let sql = "SELECT seq, kind, data FROM effects WHERE seq > ? AND seq <= ? ORDER BY seq \
                   LIMIT 64";
        sqlx::query_as(sql).bind(after).bind(last).fetch_all(&self.pool).await
    }

    /// Run a check's scalar SQL read-only, within `CHECK_LIMIT`.
    pub async fn check(&self, sql: &str) -> Result<Option<String>, String> {
        self.check_within(sql, CHECK_LIMIT).await
    }

    async fn check_within(&self, sql: &str, limit: Duration) -> Result<Option<String>, String> {
        let options =
            SqliteConnectOptions::new().filename(&self.path).read_only(true).busy_timeout(limit);
        let mut conn = SqliteConnection::connect_with(&options).await.map_err(|e| e.to_string())?;
        let deadline = std::time::Instant::now() + limit;
        let mut handle = conn.lock_handle().await.map_err(|e| e.to_string())?;
        handle.set_progress_handler(1000, move || std::time::Instant::now() < deadline);
        drop(handle);
        let sql = format!("SELECT CAST((\n{}\n) AS TEXT)", sql.trim().trim_end_matches(';'));
        let query = sqlx::query_scalar::<_, Option<String>>(AssertSqlSafe(sql));
        let value = query.fetch_optional(&mut conn).await.map_err(|e| match e {
            _ if std::time::Instant::now() >= deadline => {
                format!("the check took longer than {} s", limit.as_secs())
            }
            e => e.to_string(),
        });
        if let Err(e) = conn.close().await {
            tracing::debug!("cannot close a check's connection: {e}");
        }
        value.map(Option::flatten)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn writing_the_changes_again_puts_them_in_place_of_the_last() {
        let path = std::env::temp_dir().join(format!("miroir-rewrite-{}.db", std::process::id()));
        let db = EpisodeDb::create(&path).await.unwrap();
        let change = NetChange {
            app: None,
            tbl: "deals".into(),
            op: crate::task::ChangeOp::Delete,
            pk: [("id".to_string(), json!(1))].into_iter().collect(),
            before: None,
            after: None,
            columns: None,
        };
        for _ in 0..2 {
            db.write_changes(std::slice::from_ref(&change)).await.unwrap();
        }
        assert_eq!(db.changes().await.unwrap().len(), 1, "a milestone wrote them first");
        drop(db);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[tokio::test]
    async fn changes_keep_their_app() {
        let path = std::env::temp_dir().join(format!("miroir-apps-{}.db", std::process::id()));
        let db = EpisodeDb::create(&path).await.unwrap();
        let change = |app: Option<&str>| NetChange {
            app: app.map(str::to_string),
            tbl: "deals".into(),
            op: crate::task::ChangeOp::Delete,
            pk: [("id".to_string(), json!(1))].into_iter().collect(),
            before: None,
            after: None,
            columns: None,
        };
        db.write_changes(&[change(Some("east")), change(Some("west")), change(None)])
            .await
            .unwrap();
        let apps: Vec<Option<String>> =
            db.changes().await.unwrap().into_iter().map(|c| c.app).collect();
        assert_eq!(apps, [Some("east".into()), Some("west".into()), None]);
        let east =
            db.check("SELECT count(*) FROM changes WHERE app = 'east' AND tbl = 'deals'").await;
        assert_eq!(east, Ok(Some("1".into())), "an episode check tells same-named tables apart");
        drop(db);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[tokio::test]
    async fn only_its_owner_reads_the_database() {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!("miroir-closed-{}.db", std::process::id()));
        let db = EpisodeDb::create(&path).await.unwrap();
        db.add_effect("email", &json!({"subject": "Reset your password"})).await;
        for suffix in ["", "-wal", "-shm"] {
            let file = format!("{}{suffix}", path.display());
            let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{file}");
        }
        drop(db);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[tokio::test]
    async fn an_action_records_its_latency() {
        let path = std::env::temp_dir().join(format!("miroir-action-{}.db", std::process::id()));
        let db = EpisodeDb::create(&path).await.unwrap();
        db.add_action(&ActionRow {
            tool: "api_request",
            app: None,
            method: Some("GET"),
            path: Some("/deals"),
            status: Some(200),
            latency_ms: 42,
            arguments: &json!({}),
            result: &json!({}),
            via: None,
        })
        .await;
        assert_eq!(db.lost(), None);
        assert_eq!(db.check("SELECT latency_ms FROM actions").await, Ok(Some("42".into())));
        drop(db);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[tokio::test]
    async fn the_codes_requests_are_refused_past_their_budget() {
        let path = std::env::temp_dir().join(format!("miroir-code-{}.db", std::process::id()));
        let db = EpisodeDb::create(&path).await.unwrap();
        let (arguments, result) =
            (json!({"method": "GET", "path": "/deals"}), json!({"status": 200}));
        let row = |via| ActionRow {
            tool: "api_request",
            app: None,
            method: Some("GET"),
            path: Some("/deals"),
            status: Some(200),
            latency_ms: 1,
            arguments: &arguments,
            result: &result,
            via,
        };
        db.code_bytes.store(CODE_BUDGET - 1, Ordering::SeqCst);
        db.add_action(&row(None)).await;
        assert!(db.code_allowed(), "the env's own calls don't count toward it");
        db.add_action(&row(Some("run_code"))).await;
        let kept = db.check("SELECT count(*) FROM actions WHERE via = 'run_code'").await;
        assert_eq!(kept, Ok(Some("1".into())));
        assert!(!db.code_allowed(), "past the budget");
        assert!(
            db.refused().unwrap().contains("64 MiB"),
            "the episode's fault, which grading sees"
        );
        assert_eq!(db.lost(), None, "miroir lost nothing");
        db.clear().await.unwrap();
        assert!(db.code_allowed(), "a reset starts clean");
        assert_eq!(db.refused(), None);
        drop(db);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[tokio::test]
    async fn a_lost_record_is_noticed_until_the_next_episode() {
        let path = std::env::temp_dir().join(format!("miroir-lost-{}.db", std::process::id()));
        let db = EpisodeDb::create(&path).await.unwrap();
        assert_eq!(db.lost(), None);
        sqlx::raw_sql("DROP TABLE effects").execute(&db.pool).await.unwrap();
        db.add_effect("email", &json!({})).await;
        assert!(db.lost().unwrap().contains("effects"), "the insert failed");
        sqlx::raw_sql(EPISODE_SCHEMA.split(';').nth(1).unwrap()).execute(&db.pool).await.unwrap();
        db.clear().await.unwrap();
        assert_eq!(db.lost(), None, "a reset starts clean");
        db.add_effect("http", &json!({"body": "x".repeat(5_000_000)})).await;
        assert_eq!(db.effect_count().await.unwrap(), 1);
        db.clear().await.unwrap();
        assert!(std::fs::metadata(&path).unwrap().len() < 1_000_000, "the space came back");
        db.effect_bytes.store(EFFECT_BUDGET, Ordering::SeqCst);
        db.add_effect("http", &json!({})).await;
        assert_eq!(db.effect_count().await.unwrap(), 0, "over budget, so not stored");
        assert_eq!(db.lost(), None, "miroir lost nothing");
        assert!(db.refused().unwrap().contains("1 GiB"), "the episode's fault, which grading sees");
        db.clear().await.unwrap();
        assert_eq!(db.refused(), None, "a reset starts clean");
        drop(db);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[tokio::test]
    async fn episode_checks_stop_at_the_limit_and_never_block_writes() {
        let path = std::env::temp_dir().join(format!("miroir-edb-{}.db", std::process::id()));
        let db = EpisodeDb::create(&path).await.unwrap();
        db.add_effect("email", &json!({"to": ["a@b.test"]})).await;
        let trailing_comment = "SELECT count(*) = 1 FROM effects -- one email";
        assert_eq!(db.check(trailing_comment).await, Ok(Some("1".into())));
        let endless = "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n) \
                       SELECT count(*) FROM n";
        let started = std::time::Instant::now();
        let effect = json!({"path": "/x"});
        let (result, ()) = tokio::join!(
            db.check_within(endless, Duration::from_millis(500)),
            db.add_effect("http", &effect)
        );
        assert!(result.unwrap_err().contains("took longer"), "the check was interrupted");
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(db.effect_count().await.unwrap(), 2);
        drop(db);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }
}
