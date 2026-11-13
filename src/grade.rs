//! Grading: net changes from the change log, the scope check, the checks, and the reward.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value, json};

use crate::db::{Db, Ignore, LogRow, Table};
use crate::edb::CHECK_LIMIT;
use crate::env::{Catalog, Env, labelled};
use crate::manifest::Manifest;
use crate::supervisor::Crash;
use crate::task::{AllowedChange, ChangeOp, Check, Task};

const OUT_OF_SCOPE_LISTED: usize = 100;
const QUIET_FAULT: &str = "the quiet command never passed before grading";

/// A row's net change over the episode, or a table's for `schema` and `truncate`.
#[derive(Debug, Clone, PartialEq)]
pub struct NetChange {
    pub app: Option<String>,
    pub tbl: String,
    pub op: ChangeOp,
    pub pk: Map<String, Value>,
    pub before: Option<Map<String, Value>>,
    pub after: Option<Map<String, Value>>,
    pub columns: Option<Vec<String>>,
}

impl NetChange {
    /// The change without its row images.
    pub fn summary(&self) -> Value {
        let mut summary = json!({
            "tbl": self.tbl, "op": self.op.as_str(), "pk": self.pk, "columns": self.columns,
        });
        if let Some(app) = &self.app {
            summary["app"] = json!(app);
        }
        summary
    }
}

struct Entry {
    tbl: String,
    before: Option<Map<String, Value>>,
    after: Option<Map<String, Value>>,
}

type RowCounts = BTreeMap<(String, String), (i64, Map<String, Value>)>;

/// Fold the change log into net changes; rows of a table without a key count as a
/// multiset.
pub fn fold(log: &[LogRow], tables: &[Table], ignore: &Ignore) -> Result<Vec<NetChange>, String> {
    let keys: BTreeMap<&str, Vec<&str>> = tables
        .iter()
        .map(|t| {
            (t.name.as_str(), t.columns.iter().filter(|c| c.pk).map(|c| c.name.as_str()).collect())
        })
        .collect();
    let mut entries: BTreeMap<(String, String), Entry> = BTreeMap::new();
    let mut counts = RowCounts::new();
    let mut truncated: Vec<String> = Vec::new();
    for row in log.iter().filter(|r| !ignore.tables.contains(&r.tbl)) {
        let skip = ignore.columns.get(&row.tbl);
        let parse = |text: &Option<String>| -> Result<Option<Map<String, Value>>, String> {
            let Some(text) = text else { return Ok(None) };
            let mut map: Map<String, Value> =
                serde_json::from_str(text).map_err(|e| format!("bad log row: {e}"))?;
            map.retain(|column, _| !skip.is_some_and(|s| s.contains(column)));
            Ok(Some(map))
        };
        let (old, new) = (parse(&row.old)?, parse(&row.new)?);
        let pk = keys.get(row.tbl.as_str()).map_or(&[][..], Vec::as_slice);
        match row.op.as_str() {
            "truncate" => {
                entries.retain(|(tbl, _), _| tbl != &row.tbl);
                counts.retain(|(tbl, _), _| tbl != &row.tbl);
                if !truncated.contains(&row.tbl) {
                    truncated.push(row.tbl.clone());
                }
            }
            "insert" | "update" | "delete" if pk.is_empty() => {
                count(&mut counts, &row.tbl, old, -1);
                count(&mut counts, &row.tbl, new, 1);
            }
            "insert" => record(&mut entries, &row.tbl, pk, None, new),
            "delete" => record(&mut entries, &row.tbl, pk, old, None),
            "update" if identity(pk, old.as_ref()) == identity(pk, new.as_ref()) => {
                record(&mut entries, &row.tbl, pk, old, new);
            }
            "update" => {
                record(&mut entries, &row.tbl, pk, old, None);
                record(&mut entries, &row.tbl, pk, None, new);
            }
            other => return Err(format!("unknown op '{other}' in the change log")),
        }
    }
    let mut changes: Vec<NetChange> = truncated
        .into_iter()
        .map(|tbl| NetChange {
            app: None,
            tbl,
            op: ChangeOp::Truncate,
            pk: Map::new(),
            before: None,
            after: None,
            columns: None,
        })
        .collect();
    changes.extend(entries.into_values().filter_map(|entry| net_change(entry, &keys)));
    for ((tbl, _), (net, row)) in counts {
        let added = net > 0;
        let change = NetChange {
            app: None,
            tbl,
            op: if added { ChangeOp::Insert } else { ChangeOp::Delete },
            pk: Map::new(),
            before: (!added).then(|| row.clone()),
            after: added.then_some(row),
            columns: None,
        };
        changes.extend((0..net.abs()).map(|_| change.clone()));
    }
    Ok(changes)
}

fn net_change(entry: Entry, keys: &BTreeMap<&str, Vec<&str>>) -> Option<NetChange> {
    let pk_columns = keys.get(entry.tbl.as_str()).map_or(&[][..], Vec::as_slice);
    let row = entry.after.as_ref().or(entry.before.as_ref());
    let pk: Map<String, Value> = pk_columns
        .iter()
        .filter_map(|c| row.and_then(|r| r.get(*c)).map(|v| ((*c).to_string(), v.clone())))
        .collect();
    let (op, columns) = match (&entry.before, &entry.after) {
        (None, None) => return None,
        (None, Some(_)) => (ChangeOp::Insert, None),
        (Some(_), None) => (ChangeOp::Delete, None),
        (Some(before), Some(after)) => {
            let differ: BTreeSet<&String> = before
                .keys()
                .chain(after.keys())
                .filter(|c| before.get(*c) != after.get(*c))
                .collect();
            if differ.is_empty() {
                return None;
            }
            (ChangeOp::Update, Some(differ.into_iter().cloned().collect()))
        }
    };
    Some(NetChange {
        app: None,
        tbl: entry.tbl,
        op,
        pk,
        before: entry.before,
        after: entry.after,
        columns,
    })
}

/// Tables created, dropped or reshaped (`schema`), or re-created with capture
/// (`truncate`).
pub fn schema_changes(before: &[Table], after: &[Table], ignore: &Ignore) -> Vec<NetChange> {
    let shape = |table: &Table| -> Map<String, Value> {
        let skip = ignore.columns.get(&table.name);
        table
            .columns
            .iter()
            .filter(|c| !skip.is_some_and(|s| s.contains(&c.name)))
            .map(|c| (c.name.clone(), Value::String(c.sql_type.clone())))
            .collect()
    };
    let before: BTreeMap<&str, &Table> = before.iter().map(|t| (t.name.as_str(), t)).collect();
    let after: BTreeMap<&str, &Table> = after.iter().map(|t| (t.name.as_str(), t)).collect();
    let names: BTreeSet<&str> = before.keys().chain(after.keys()).copied().collect();
    let mut changes = Vec::new();
    for name in names.into_iter().filter(|name| !ignore.tables.contains(*name)) {
        let (old, new) = (before.get(name).copied(), after.get(name).copied());
        let (was, is) = (old.map(shape), new.map(shape));
        let differ: Vec<String> = match (&was, &is) {
            (Some(was), Some(is)) => {
                let columns: BTreeSet<&String> = was.keys().chain(is.keys()).collect();
                columns.into_iter().filter(|c| was.get(*c) != is.get(*c)).cloned().collect()
            }
            _ => Vec::new(),
        };
        let op = match (old, new) {
            (Some(old), Some(new)) => {
                let kept_capture = new.captured || !old.captured;
                if !differ.is_empty() || !kept_capture {
                    ChangeOp::Schema
                } else if old.id != new.id {
                    ChangeOp::Truncate
                } else {
                    continue;
                }
            }
            _ => ChangeOp::Schema,
        };
        let columns = (!differ.is_empty()).then_some(differ);
        let (before, after) = if op == ChangeOp::Truncate { (None, None) } else { (was, is) };
        let tbl = name.to_string();
        changes.push(NetChange { app: None, tbl, op, pk: Map::new(), before, after, columns });
    }
    changes
}

fn count(counts: &mut RowCounts, tbl: &str, row: Option<Map<String, Value>>, by: i64) {
    let Some(row) = row else { return };
    let key = (tbl.to_string(), Value::Object(row.clone()).to_string());
    counts.entry(key).or_insert((0, row)).0 += by;
}

fn record(
    entries: &mut BTreeMap<(String, String), Entry>,
    tbl: &str,
    pk: &[&str],
    old: Option<Map<String, Value>>,
    new: Option<Map<String, Value>>,
) {
    let key = (tbl.to_string(), identity(pk, old.as_ref().or(new.as_ref())));
    let entry = entries.entry(key).or_insert_with(|| Entry {
        tbl: tbl.to_string(),
        before: old.clone(),
        after: old,
    });
    entry.after = new;
}

fn identity(pk: &[&str], row: Option<&Map<String, Value>>) -> String {
    let Some(row) = row else { return String::new() };
    let key: Vec<&Value> = pk.iter().map(|c| row.get(*c).unwrap_or(&Value::Null)).collect();
    serde_json::to_string(&key).unwrap_or_default()
}

/// A check's value as a score: a boolean, or a finite number clamped to 0..1; null
/// is 0.
pub fn parse_score(value: Option<&str>) -> Result<f64, String> {
    let Some(text) = value else { return Ok(0.0) };
    match text.trim() {
        "true" | "t" => Ok(1.0),
        "false" | "f" => Ok(0.0),
        other => match other.parse::<f64>() {
            Ok(n) if n.is_finite() => Ok(n.clamp(0.0, 1.0)),
            _ => {
                Err(format!("the check returned {text:?}, which is neither a number nor a boolean"))
            }
        },
    }
}

/// Changes no `allowed_changes` entry permits; none when the task allows any change.
pub fn out_of_scope<'a>(
    changes: &'a [NetChange],
    allowed: Option<&[AllowedChange]>,
) -> Vec<&'a NetChange> {
    let Some(allowed) = allowed else { return Vec::new() };
    changes
        .iter()
        .filter(|change| {
            !allowed.iter().any(|a| {
                a.app == change.app
                    && a.table == change.tbl
                    && a.op == change.op
                    && match (&a.columns, &change.columns) {
                        (Some(permitted), Some(changed)) => {
                            changed.iter().all(|c| permitted.contains(c))
                        }
                        _ => true,
                    }
            })
        })
        .collect()
}

/// Out of scope, or a gate below 1.
pub fn harmed(checks: &[(&Check, f64)], in_scope: bool) -> bool {
    !in_scope || checks.iter().any(|(check, score)| check.gate && *score < 1.0)
}

/// Reward and success: harm earns `harm_reward`, every check at 1 earns 1, else the
/// weighted sum.
pub fn reward(checks: &[(&Check, f64)], in_scope: bool, harm_reward: f64) -> (f64, bool) {
    if harmed(checks, in_scope) {
        return (harm_reward, false);
    }
    if checks.iter().all(|(_, score)| *score >= 1.0) {
        return (1.0, true);
    }
    let sum: f64 = checks.iter().filter_map(|(check, score)| check.weight.map(|w| w * score)).sum();
    (sum.clamp(0.0, 1.0), false)
}

type CheckValue = Result<Option<String>, String>;
type CheckValues = Vec<CheckValue>;
type AppChecks<'t> = BTreeMap<String, Vec<(&'t str, &'t str)>>;

/// A graded episode; `reward` is `None` when miroir couldn't grade it.
pub struct Outcome {
    pub reward: Option<f64>,
    pub success: bool,
    pub metadata: Value,
}

impl Env {
    /// Grade the end state, keeping milestones at the scores they froze with.
    pub(crate) async fn grade(
        &self,
        task: &Task,
        catalog: &Catalog,
        frozen: &BTreeMap<usize, (Value, f64)>,
    ) -> Outcome {
        let quiet_timeout = self.supervisor.wait_quiet(self.manifest.quiet.as_ref()).await.is_err();
        if let Some(why) = self.edb.lost() {
            return failed_grading(task, &why);
        }
        if let Some(refused) = self.edb.refused() {
            return faulted(task, &refused);
        }
        let pending: Vec<usize> =
            (0..task.checks.len()).filter(|index| !frozen.contains_key(index)).collect();
        let checks: Vec<&Check> = pending.iter().map(|&index| &task.checks[index]).collect();
        let (changes, fresh) = match self.score(catalog, &checks).await {
            Ok(read) => read,
            Err(Unread::Fault(fault)) => return faulted(task, &fault),
            Err(Unread::Invalid(error)) => return failed_grading(task, &error),
        };
        let fresh: BTreeMap<usize, (Value, f64)> = pending.into_iter().zip(fresh).collect();
        let mut scored = Vec::new();
        let mut reports = Vec::new();
        for (index, check) in task.checks.iter().enumerate() {
            let Some((report, score)) = frozen.get(&index).or_else(|| fresh.get(&index)) else {
                continue;
            };
            reports.push(report.clone());
            scored.push((check, *score));
        }
        let outside = out_of_scope(&changes, task.allowed_changes.as_deref());
        let in_scope = outside.is_empty();
        let scope_score = if in_scope { 1.0 } else { 0.0 };
        reports.push(json!({"name": "scope", "gate": true, "score": scope_score}));
        let check_error = reports.iter().find_map(|report| {
            let error = report["error"].as_str()?;
            Some(format!("check '{}' failed: {error}", report["name"].as_str().unwrap_or("?")))
        });
        let fault = quiet_timeout.then(|| QUIET_FAULT.to_string()).or(check_error);
        let (reward, success) = match fault {
            Some(_) => (task.harm_reward, false),
            None => reward(&scored, in_scope, task.harm_reward),
        };
        let listed = outside.iter().take(OUT_OF_SCOPE_LISTED);
        let mut metadata = json!({
            "task_id": task.id,
            "success": success,
            "harm": fault.is_some() || harmed(&scored, in_scope),
            "checks": reports,
            "out_of_scope": listed.map(|c| c.summary()).collect::<Vec<_>>(),
            "out_of_scope_count": outside.len(),
            "answer": self.edb.answer().await,
        });
        if quiet_timeout {
            metadata["quiet_timeout"] = Value::Bool(true);
        }
        if let Some(fault) = fault {
            tracing::warn!("the episode broke the env: {fault}");
            metadata["fault"] = Value::String(fault);
        }
        Outcome { reward: Some(reward), success, metadata }
    }

    /// Read the net changes, store them in the episode database, and score the checks.
    pub(crate) async fn score(
        &self,
        catalog: &Catalog,
        checks: &[&Check],
    ) -> Result<(Vec<NetChange>, Vec<(Value, f64)>), Unread> {
        let (changes, mut values) =
            self.read_changes(catalog, &app_checks(&self.manifest, checks)).await?;
        if let Err(e) = self.edb.write_changes(&changes).await {
            return Err(Unread::Invalid(format!("cannot write changes: {e}")));
        }
        let mut scored = Vec::with_capacity(checks.len());
        for check in checks {
            let value = match (&check.sql, &check.command) {
                (Some(sql), _) if check.db.is_some() => self.edb.check(sql).await,
                (Some(_), _) => values
                    .remove(&check.name)
                    .unwrap_or_else(|| Err("missing app check result".into())),
                (None, Some(command)) => self.command_check(command, check.app.as_deref()).await,
                (None, None) => Err("the check has neither sql nor command".into()),
            };
            let mut report = json!({"name": check.name});
            if check.gate {
                report["gate"] = Value::Bool(true);
            }
            if let Some(by) = check.by {
                report["by"] = json!(by);
            }
            let score = match value.and_then(|v| parse_score(v.as_deref())) {
                Ok(score) => score,
                Err(e) => {
                    report["error"] = Value::String(e);
                    0.0
                }
            };
            report["score"] = json!(score);
            scored.push((report, score));
        }
        Ok((changes, scored))
    }

    /// Net changes since the catalog was taken.
    pub(crate) async fn changes_since(&self, catalog: &Catalog) -> Result<Vec<NetChange>, String> {
        match self.read_changes(catalog, &AppChecks::new()).await {
            Ok((changes, _)) => Ok(changes),
            Err(Unread::Fault(e) | Unread::Invalid(e)) => Err(e),
        }
    }

    async fn read_changes(
        &self,
        catalog: &Catalog,
        checks: &AppChecks<'_>,
    ) -> Result<(Vec<NetChange>, BTreeMap<String, CheckValue>), Unread> {
        let several = self.dbs.len() > 1;
        let (mut changes, mut values) = (Vec::new(), BTreeMap::new());
        for (app, db) in &self.dbs {
            let (names, sqls): (Vec<&str>, Vec<&str>) =
                checks.get(app).into_iter().flatten().copied().unzip();
            let before = catalog.get(app).map_or(&[][..], Vec::as_slice);
            let (read, scores) = read_app(db, before, &sqls)
                .await
                .map_err(|e| e.map(|m| labelled(&self.dbs, app, m)))?;
            changes.extend(
                read.into_iter()
                    .map(|change| NetChange { app: several.then(|| app.clone()), ..change }),
            );
            values.extend(names.into_iter().map(str::to_string).zip(scores));
        }
        Ok((changes, values))
    }

    async fn command_check(
        &self,
        argv: &[String],
        app: Option<&str>,
    ) -> Result<Option<String>, String> {
        let db_path = self.edb.path.to_string_lossy().into_owned();
        let mut extra = vec![("MIROIR_EPISODE_DB", db_path.as_str())];
        let app = app.or_else(|| self.manifest.only_database());
        if let Some(db) = app.and_then(|app| self.dbs.get(app)) {
            extra.push(("MIROIR_DATABASE_URL", db.url()));
        }
        let (code, stdout) =
            self.supervisor.output(argv, &extra, CHECK_LIMIT).await.map_err(|e| e.0)?;
        Ok(Some(match (code, stdout.trim()) {
            (0, "") => "1".to_string(),
            (0, printed) => printed.to_string(),
            _ => "0".to_string(),
        }))
    }
}

/// Why the changes couldn't be read: the episode's fault, or a grading failure.
pub enum Unread {
    Fault(String),
    Invalid(String),
}

impl Unread {
    fn map(self, message: impl FnOnce(String) -> String) -> Unread {
        match self {
            Unread::Fault(m) => Unread::Fault(message(m)),
            Unread::Invalid(m) => Unread::Invalid(message(m)),
        }
    }
}

fn app_checks<'t>(manifest: &Manifest, checks: &[&'t Check]) -> AppChecks<'t> {
    let mut grouped = AppChecks::new();
    for check in checks.iter().filter(|c| c.db.is_none()) {
        let app = check.app.as_deref().or_else(|| manifest.only_database());
        if let (Some(app), Some(sql)) = (app, check.sql.as_deref()) {
            grouped.entry(app.to_string()).or_default().push((check.name.as_str(), sql));
        }
    }
    grouped
}

async fn read_app(
    db: &Db,
    before: &[Table],
    sqls: &[&str],
) -> Result<(Vec<NetChange>, CheckValues), Unread> {
    let snapshot = db
        .snapshot(sqls)
        .await
        .map_err(|e| Unread::Invalid(format!("cannot read the database: {e}")))?;
    let log = snapshot.log.map_err(Unread::Fault)?;
    let ignore = db.resolve_ignore(before).map_err(|e| Unread::Invalid(e.to_string()))?;
    let (before, after) = (before.to_vec(), snapshot.tables);
    let folded = tokio::task::spawn_blocking(move || {
        let mut changes = fold(&log, &after, &ignore)?;
        changes.extend(schema_changes(&before, &after, &ignore));
        Ok::<_, String>(changes)
    })
    .await;
    match folded {
        Ok(Ok(changes)) => Ok((changes, snapshot.scores)),
        Ok(Err(e)) => Err(Unread::Invalid(e)),
        Err(e) => Err(Unread::Invalid(format!("the fold stopped: {e}"))),
    }
}

pub fn crashed(task: &Task, crash: &Crash) -> Outcome {
    let mut outcome = faulted(task, &crash.to_string());
    outcome.metadata["crashed"] = json!({"process": crash.process, "exit_code": crash.exit_code});
    outcome
}

fn faulted(task: &Task, fault: &str) -> Outcome {
    tracing::warn!("the episode broke the env: {fault}");
    let metadata = json!({"task_id": task.id, "success": false, "harm": true, "fault": fault});
    Outcome { reward: Some(task.harm_reward), success: false, metadata }
}

fn failed_grading(task: &Task, error: &str) -> Outcome {
    tracing::error!("grading failed: {error}");
    let message = format!("grading failed: {error}");
    let metadata =
        json!({"task_id": task.id, "success": false, "invalid": message, "error": message});
    Outcome { reward: None, success: false, metadata }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Column, ColumnKind};

    fn table(name: &str, pk: &[&str], columns: &[&str]) -> Table {
        let columns = columns
            .iter()
            .map(|c| Column {
                name: (*c).into(),
                kind: ColumnKind::Other,
                generated: false,
                pk: pk.contains(c),
                sql_type: "text".into(),
            })
            .collect();
        let schema = "public".into();
        let qualified = name.into();
        Table {
            name: name.into(),
            schema,
            qualified,
            columns,
            partitioned: false,
            id: 1,
            captured: true,
        }
    }

    fn named<'a>(tables: &'a mut [Table], name: &str) -> &'a mut Table {
        tables.iter_mut().find(|t| t.name == name).unwrap()
    }

    #[test]
    fn schema_changes_are_the_changes_no_trigger_logs() {
        let t = |name: &str, columns: &[&str]| table(name, &["id"], columns);
        let before = vec![
            t("activities", &["id"]),
            t("api_tokens", &["id", "last_used_at"]),
            t("cache", &["id"]),
            t("deals", &["id", "owner_id"]),
            t("invoices", &["id"]),
            t("meetings", &["id"]),
            t("notes", &["id"]),
            t("users", &["id"]),
        ];
        let mut after = before.clone();
        after.retain(|t| !matches!(t.name.as_str(), "activities" | "cache"));
        after.push(t("custom", &["id"]));
        named(&mut after, "api_tokens").columns.pop();
        named(&mut after, "deals").columns[1].sql_type = "bigint".into();
        let invoices = named(&mut after, "invoices");
        (invoices.id, invoices.captured) = (2, false);
        named(&mut after, "meetings").captured = false;
        named(&mut after, "notes").id = 2;
        let ignore = Ignore {
            tables: ["cache".to_string()].into(),
            columns: [("api_tokens".to_string(), ["last_used_at".to_string()].into())].into(),
        };
        let changes = schema_changes(&before, &after, &ignore);
        let found: Vec<(&str, ChangeOp, Option<Vec<String>>)> =
            changes.iter().map(|c| (c.tbl.as_str(), c.op, c.columns.clone())).collect();
        assert_eq!(
            found,
            [
                ("activities", ChangeOp::Schema, None),
                ("custom", ChangeOp::Schema, None),
                ("deals", ChangeOp::Schema, Some(vec!["owner_id".to_string()])),
                ("invoices", ChangeOp::Schema, None),
                ("meetings", ChangeOp::Schema, None),
                ("notes", ChangeOp::Truncate, None),
            ],
            "re-created without capture is schema, with it truncate; ignored ones never count"
        );
        assert_eq!((changes[0].before.is_some(), changes[0].after.is_none()), (true, true));
        assert_eq!(changes[2].after.as_ref().unwrap()["owner_id"], "bigint");
        assert_eq!(schema_changes(&before, &before, &ignore), [], "nothing changed");
    }

    fn log(tbl: &str, op: &str, old: Option<Value>, new: Option<Value>) -> LogRow {
        LogRow {
            tbl: tbl.into(),
            op: op.into(),
            old: old.map(|v| v.to_string()),
            new: new.map(|v| v.to_string()),
        }
    }

    fn check(name: &str, weight: Option<f64>, gate: bool) -> Check {
        Check {
            name: name.into(),
            sql: Some("SELECT 1".into()),
            command: None,
            app: None,
            db: None,
            weight,
            gate,
            by: None,
        }
    }

    #[test]
    fn changes_fold_to_net() {
        let tables =
            [table("deals", &["id"], &["id", "owner_id", "name"]), table("tags", &[], &["label"])];
        let deal = |owner: i64, name: &str| json!({"id": 1, "owner_id": owner, "name": name});
        let rows = [
            log("deals", "insert", None, Some(json!({"id": 9, "owner_id": 1, "name": "tmp"}))),
            log("deals", "delete", Some(json!({"id": 9, "owner_id": 1, "name": "tmp"})), None),
            log(
                "deals",
                "update",
                Some(json!({"id": 2, "owner_id": 1, "name": "a"})),
                Some(json!({"id": 2, "owner_id": 5, "name": "a"})),
            ),
            log(
                "deals",
                "update",
                Some(json!({"id": 2, "owner_id": 5, "name": "a"})),
                Some(json!({"id": 2, "owner_id": 1, "name": "a"})),
            ),
            log("deals", "update", Some(deal(12, "x")), Some(deal(7, "x"))),
            log("deals", "update", Some(deal(7, "x")), Some(deal(7, "y"))),
            log("deals", "update", Some(deal(7, "y")), Some(deal(7, "x"))),
            log("tags", "insert", None, Some(json!({"label": "hot"}))),
        ];
        let changes = fold(&rows, &tables, &Ignore::default()).unwrap();
        assert_eq!(changes.len(), 2, "{changes:?}");
        assert_eq!(changes[0].op, ChangeOp::Update);
        assert_eq!(changes[0].pk, *json!({"id": 1}).as_object().unwrap());
        assert_eq!(changes[0].columns, Some(vec!["owner_id".to_string()]));
        assert_eq!(changes[0].before.as_ref().unwrap()["owner_id"], 12);
        assert_eq!(changes[0].after.as_ref().unwrap()["owner_id"], 7);
        assert_eq!((changes[1].tbl.as_str(), changes[1].op), ("tags", ChangeOp::Insert));
        assert!(changes[1].pk.is_empty(), "a table without a key has an empty pk");
    }

    #[test]
    fn identical_rows_without_a_key_each_count() {
        let tables = [table("tags", &[], &["label"])];
        let hot = || json!({"label": "hot"});
        let rows = [
            log("tags", "insert", None, Some(hot())),
            log("tags", "insert", None, Some(hot())),
            log("tags", "insert", None, Some(hot())),
            log("tags", "delete", Some(hot()), None),
            log("tags", "update", Some(json!({"label": "old"})), Some(json!({"label": "new"}))),
        ];
        let changes = fold(&rows, &tables, &Ignore::default()).unwrap();
        let ops: Vec<(ChangeOp, Option<&str>)> = changes
            .iter()
            .map(|c| {
                let row = c.after.as_ref().or(c.before.as_ref()).unwrap();
                (c.op, row["label"].as_str())
            })
            .collect();
        assert_eq!(
            ops,
            [
                (ChangeOp::Insert, Some("hot")),
                (ChangeOp::Insert, Some("hot")),
                (ChangeOp::Insert, Some("new")),
                (ChangeOp::Delete, Some("old")),
            ]
        );
    }

    #[test]
    fn an_update_of_ignored_columns_leaves_a_table_without_a_key_unchanged() {
        let tables = [table("tags", &[], &["label", "seen"])];
        let tag = |seen: i64| json!({"label": "hot", "seen": seen});
        let ignore = Ignore {
            tables: BTreeSet::default(),
            columns: [("tags".to_string(), ["seen".to_string()].into())].into(),
        };
        let touched = [log("tags", "update", Some(tag(1)), Some(tag(2)))];
        assert_eq!(fold(&touched, &tables, &ignore).unwrap(), []);
        let deleted = [
            log("tags", "delete", Some(tag(1)), None),
            log("tags", "update", Some(tag(2)), Some(tag(3))),
        ];
        let changes = fold(&deleted, &tables, &ignore).unwrap();
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert_eq!(changes[0].op, ChangeOp::Delete, "the update does not hide the delete");
    }

    #[derive(Clone, Default)]
    struct Model {
        keyed: BTreeMap<i64, i64>,
        unkeyed: Vec<i64>,
    }

    fn apply(model: &mut Model, log_rows: &mut Vec<LogRow>, (kind, a, b): (u8, i64, i64)) {
        let keyed = |id: i64, v: i64| json!({"id": id, "v": v});
        let tag = |label: i64| json!({"label": label});
        match (kind, model.keyed.get(&a).copied()) {
            (0, None) => {
                model.keyed.insert(a, b);
                log_rows.push(log("deals", "insert", None, Some(keyed(a, b))));
            }
            (1, Some(old)) => {
                model.keyed.insert(a, b);
                log_rows.push(log("deals", "update", Some(keyed(a, old)), Some(keyed(a, b))));
            }
            (2, Some(old)) => {
                model.keyed.remove(&a);
                log_rows.push(log("deals", "delete", Some(keyed(a, old)), None));
            }
            (3, Some(old)) if !model.keyed.contains_key(&b) => {
                model.keyed.remove(&a);
                model.keyed.insert(b, old);
                log_rows.push(log("deals", "update", Some(keyed(a, old)), Some(keyed(b, old))));
            }
            (4, _) => {
                model.unkeyed.push(a);
                log_rows.push(log("tags", "insert", None, Some(tag(a))));
            }
            (5 | 6, _) => {
                if let Some(at) = model.unkeyed.iter().position(|label| *label == a) {
                    model.unkeyed.remove(at);
                    let new = (kind == 6).then(|| tag(b));
                    if kind == 6 {
                        model.unkeyed.push(b);
                    }
                    log_rows.push(log(
                        "tags",
                        if kind == 6 { "update" } else { "delete" },
                        Some(tag(a)),
                        new,
                    ));
                }
            }
            _ => {}
        }
    }

    fn net_difference(start: &Model, end: &Model) -> Vec<(String, ChangeOp, i64, Option<i64>)> {
        let mut expected: Vec<(String, ChangeOp, i64, Option<i64>)> = Vec::new();
        for (id, value) in &start.keyed {
            match end.keyed.get(id) {
                None => expected.push(("deals".into(), ChangeOp::Delete, *id, Some(*value))),
                Some(now) if now != value => {
                    expected.push(("deals".into(), ChangeOp::Update, *id, Some(*now)));
                }
                Some(_) => {}
            }
        }
        for (id, value) in &end.keyed {
            if !start.keyed.contains_key(id) {
                expected.push(("deals".into(), ChangeOp::Insert, *id, Some(*value)));
            }
        }
        let tally = |labels: &[i64]| {
            let mut counts: BTreeMap<i64, i64> = BTreeMap::new();
            for label in labels {
                *counts.entry(*label).or_default() += 1;
            }
            counts
        };
        let (before, after) = (tally(&start.unkeyed), tally(&end.unkeyed));
        let labels: BTreeSet<&i64> = before.keys().chain(after.keys()).collect();
        for label in labels {
            let net = after.get(label).unwrap_or(&0) - before.get(label).unwrap_or(&0);
            let op = if net > 0 { ChangeOp::Insert } else { ChangeOp::Delete };
            expected.extend((0..net.abs()).map(|_| ("tags".into(), op, *label, None)));
        }
        expected.sort_by_key(|row| (row.0.clone(), row.1.as_str(), row.2, row.3));
        expected
    }

    proptest::proptest! {
        #[test]
        fn a_fold_is_the_net_difference(
            operations in proptest::collection::vec((0..7_u8, 0..5_i64, 0..5_i64), 0..80),
        ) {
            let tables = [table("deals", &["id"], &["id", "v"]), table("tags", &[], &["label"])];
            let start = Model { keyed: [(0, 0), (1, 1)].into(), unkeyed: vec![0, 0, 1] };
            let mut end = start.clone();
            let mut rows = Vec::new();
            for operation in operations {
                apply(&mut end, &mut rows, operation);
            }
            let changes = fold(&rows, &tables, &Ignore::default()).unwrap();
            let mut folded: Vec<(String, ChangeOp, i64, Option<i64>)> = changes
                .iter()
                .map(|c| {
                    let row = c.after.as_ref().or(c.before.as_ref()).unwrap();
                    match row.get("id") {
                        Some(id) => (c.tbl.clone(), c.op, id.as_i64().unwrap(), row["v"].as_i64()),
                        None => (c.tbl.clone(), c.op, row["label"].as_i64().unwrap(), None),
                    }
                })
                .collect();
            folded.sort_by_key(|row| (row.0.clone(), row.1.as_str(), row.2, row.3));
            proptest::prop_assert_eq!(folded, net_difference(&start, &end));
        }
    }

    #[test]
    fn truncate_and_ignore_fold() {
        let tables = [
            table("notes", &["id"], &["id", "text"]),
            table("api_tokens", &["id"], &["id", "last_used_at"]),
        ];
        let rows = [
            log("notes", "insert", None, Some(json!({"id": 1, "text": "a"}))),
            log("notes", "truncate", None, None),
            log("notes", "insert", None, Some(json!({"id": 2, "text": "b"}))),
            log(
                "api_tokens",
                "update",
                Some(json!({"id": 1, "last_used_at": null})),
                Some(json!({"id": 1, "last_used_at": "now"})),
            ),
        ];
        let ignore = Ignore {
            tables: BTreeSet::default(),
            columns: [("api_tokens".to_string(), ["last_used_at".to_string()].into())].into(),
        };
        let changes = fold(&rows, &tables, &ignore).unwrap();
        let ops: Vec<(&str, ChangeOp)> = changes.iter().map(|c| (c.tbl.as_str(), c.op)).collect();
        assert_eq!(ops, [("notes", ChangeOp::Truncate), ("notes", ChangeOp::Insert)]);
        assert_eq!(changes[1].after.as_ref().unwrap()["id"], 2);
    }

    #[test]
    fn scores_parse_as_specified() {
        for (text, score) in
            [("true", 1.0), ("t", 1.0), ("1", 1.0), ("false", 0.0), ("f", 0.0), ("0", 0.0)]
        {
            assert_eq!(parse_score(Some(text)).unwrap(), score, "{text}");
        }
        assert_eq!(parse_score(Some("0.6666666666666667")).unwrap(), 0.666_666_666_666_666_7);
        assert_eq!(parse_score(Some("1.00000000000000000000")).unwrap(), 1.0);
        assert_eq!(parse_score(Some("3")).unwrap(), 1.0);
        assert_eq!(parse_score(Some("-2")).unwrap(), 0.0);
        assert_eq!(parse_score(None).unwrap(), 0.0);
        assert!(parse_score(Some("yes")).is_err());
        assert!(parse_score(Some("NaN")).is_err());
        for infinite in ["inf", "-Infinity", "1e999"] {
            assert!(parse_score(Some(infinite)).is_err(), "{infinite} would pass a gate");
        }
    }

    #[test]
    fn success_ignores_float_sums() {
        let (a, b, c) = (
            check("a", Some(0.6), false),
            check("b", Some(0.3), false),
            check("c", Some(0.1), false),
        );
        assert_ne!(0.6 + 0.3 + 0.1, 1.0, "the float sum this rule guards against");
        assert_eq!(reward(&[(&a, 1.0), (&b, 1.0), (&c, 1.0)], true, 0.0), (1.0, true));
        let (partial, success) = reward(&[(&a, 1.0), (&b, 1.0), (&c, 0.999_999)], true, 0.0);
        assert!(partial < 1.0 && !success);
    }

    #[test]
    fn gates_and_scope_earn_the_harm_reward() {
        let (w, g) = (check("w", Some(1.0), false), check("g", None, true));
        assert_eq!(reward(&[(&w, 1.0), (&g, 0.5)], true, 0.0), (0.0, false));
        assert_eq!(reward(&[(&w, 1.0), (&g, 1.0)], false, 0.0), (0.0, false));
        assert_eq!(reward(&[(&w, 0.25), (&g, 1.0)], true, 0.0), (0.25, false));
        assert_eq!(
            reward(&[(&w, 1.0), (&g, 0.5)], true, -0.5),
            (-0.5, false),
            "worse than nothing"
        );
        assert_eq!(reward(&[(&w, 1.0), (&g, 1.0)], false, -0.5), (-0.5, false));
        assert_eq!(reward(&[(&w, 0.0), (&g, 1.0)], true, -0.5), (0.0, false), "doing nothing is 0");
        assert!(harmed(&[(&g, 0.0)], true) && harmed(&[(&g, 1.0)], false));
        assert!(!harmed(&[(&w, 0.0), (&g, 1.0)], true), "a low weighted check is not harm");
    }

    #[test]
    fn scope_matches_table_op_and_columns() {
        let change = |tbl: &str, op, columns: Option<&[&str]>| NetChange {
            app: None,
            tbl: tbl.into(),
            op,
            pk: Map::new(),
            before: None,
            after: None,
            columns: columns.map(|c| c.iter().map(|s| (*s).to_string()).collect()),
        };
        let allowed = [
            AllowedChange {
                app: None,
                table: "deals".into(),
                op: ChangeOp::Update,
                columns: Some(vec!["owner_id".into(), "updated_at".into()]),
            },
            AllowedChange { app: None, table: "notes".into(), op: ChangeOp::Insert, columns: None },
        ];
        let changes = [
            change("deals", ChangeOp::Update, Some(&["owner_id"])),
            change("deals", ChangeOp::Update, Some(&["owner_id", "name"])),
            change("notes", ChangeOp::Insert, None),
            change("notes", ChangeOp::Delete, None),
        ];
        let outside: Vec<usize> = out_of_scope(&changes, Some(&allowed))
            .iter()
            .map(|c| changes.iter().position(|x| x == *c).unwrap())
            .collect();
        assert_eq!(outside, [1, 3]);
        assert_eq!(out_of_scope(&changes, None), Vec::<&NetChange>::new());
    }

    #[test]
    fn scope_tells_apps_apart() {
        let change = |app: &str| NetChange {
            app: Some(app.into()),
            tbl: "deals".into(),
            op: ChangeOp::Update,
            pk: Map::new(),
            before: None,
            after: None,
            columns: Some(vec!["owner_id".into()]),
        };
        let allowed = [AllowedChange {
            app: Some("east".into()),
            table: "deals".into(),
            op: ChangeOp::Update,
            columns: None,
        }];
        let changes = [change("east"), change("west")];
        let outside = out_of_scope(&changes, Some(&allowed));
        assert_eq!(outside, [&changes[1]], "west's deals are another table than east's");
        assert_eq!(outside[0].summary()["app"], "west");
        let one_app = NetChange { app: None, ..change("east") };
        assert!(one_app.summary().get("app").is_none(), "a one-database summary is as it was");
    }

    #[test]
    fn app_checks_group_by_app_in_task_order() {
        let manifest: Manifest = serde_saphyr::from_str(concat!(
            "miroir: 1\nname: t\ndescription: t\nprocesses: []\npersonas: {}\napps:\n",
            "  east: {database: {url: x}}\n  west: {database: {url: y}}\n",
        ))
        .unwrap();
        let task: Task = serde_saphyr::from_str(concat!(
            "instruction: x\nany_change: true\ngolden: []\nchecks:\n",
            "  - {name: w1, app: west, sql: SELECT 1, weight: 0.25}\n",
            "  - {name: e1, app: east, sql: SELECT 2, weight: 0.25}\n",
            "  - {name: ep, db: episode, sql: SELECT 3, weight: 0.25}\n",
            "  - {name: w2, app: west, sql: SELECT 4, weight: 0.25}\n",
            "  - {name: c, app: east, command: [x], gate: true}\n",
        ))
        .unwrap();
        let checks = app_checks(&manifest, &task.checks.iter().collect::<Vec<_>>());
        assert_eq!(checks["west"], [("w1", "SELECT 1"), ("w2", "SELECT 4")]);
        assert_eq!(checks["east"], [("e1", "SELECT 2")], "episode and command checks stay out");
    }
}
