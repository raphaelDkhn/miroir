//! `miroir collect` (§3.6), Harbor's collect hook in the world service: once `main` has
//! stopped, wait for quiet, read the change log and the journals, and write the episode's
//! evidence. A fault goes into the evidence, as harm; collect fails only for Miroir's own
//! failures, which void the trial.

use std::io::ErrorKind;
use std::path::Path;

use anyhow::Context;
use serde_json::Value;

use crate::evidence::{self, CapturedTable, Evidence, Fault, fault};
use crate::manifest::Manifest;
use crate::paths::Paths;
use crate::pg::{self, Ignore, Pg, PgError, Snapshot, Table};
use crate::supervisor::{self, Supervisor, SupervisorError};
use crate::{fold, journal};

/// What collect wrote.
#[derive(Debug, Clone)]
pub struct Summary {
    /// The rows of `changes`: none after the `evidence_budget` fallback.
    pub changes: usize,
    /// The rows of `sent_emails` and `sent_requests`: none after the fallback.
    pub effects: usize,
    /// The rows of `faults`, in order.
    pub faults: Vec<Fault>,
    /// The size of the evidence file.
    pub bytes: u64,
}

/// Write `/miroir/evidence/evidence.db`. Faults are recorded, never returned as errors; an
/// error is Miroir's own failure: the world never became ready, its boot catalog or a journal
/// is unreadable, the fold failed, or no evidence file could be written.
pub async fn collect(paths: &Paths, manifest: &Manifest) -> anyhow::Result<Summary> {
    let ready = paths.ready();
    anyhow::ensure!(
        ready.exists(),
        "the world never finished booting: {} is missing, so `miroir run` stopped before the \
         world was ready; the world container's log says why",
        ready.display()
    );
    let boot = boot_catalog(&paths.catalog()).await?;
    let pg = Pg::new(&manifest.database);
    let ignore =
        pg.ignore(&boot).context("cannot resolve database.ignore against the boot catalog")?;
    let quiet = Supervisor::new(manifest.env.clone(), None)
        .wait_quiet(manifest.quiet.as_deref(), supervisor::QUIET_LIMIT)
        .await;
    let snapshot = pg.snapshot(pg::LOG_BUDGET).await;
    let (effects, oversized) = read_effects(&paths.effects())?;
    let faults = journal::read(&paths.faults())
        .with_context(|| format!("cannot read {}", paths.faults().display()))?;
    let evidence = assemble(Reads { boot, ignore, quiet, snapshot, effects, oversized, faults })?;
    persist(&paths.evidence(), evidence).await
}

/// The lines of the effects journal; none, and an `evidence_budget` fault, when the file is over
/// `evidence::BUDGET`: no evidence file could hold its effects, and reading it whole could
/// exhaust the world's memory, which would void the trial instead of scoring it.
fn read_effects(path: &Path) -> anyhow::Result<(Vec<Value>, Option<Fault>)> {
    let cannot = || format!("cannot read {}", path.display());
    match std::fs::metadata(path) {
        Ok(meta) if meta.len() > evidence::BUDGET => {
            let detail = format!(
                "{} took {} bytes, over the evidence's {}-byte budget, so its effects were left \
                 out",
                path.display(),
                meta.len(),
                evidence::BUDGET
            );
            return Ok((Vec::new(), Some(Fault { kind: fault::EVIDENCE_BUDGET.into(), detail })));
        }
        Err(e) if e.kind() != ErrorKind::NotFound => return Err(e).with_context(cannot),
        _ => {}
    }
    Ok((journal::read(path).with_context(cannot)?, None))
}

/// The tables `miroir run` read when the world became ready.
async fn boot_catalog(path: &Path) -> anyhow::Result<Vec<Table>> {
    let text = tokio::fs::read(path).await.with_context(|| {
        format!(
            "cannot read the boot catalog {}, which `miroir run` writes before the world is \
             ready",
            path.display()
        )
    })?;
    serde_json::from_slice(&text).with_context(|| {
        format!("the boot catalog {} is not a JSON array of tables", path.display())
    })
}

/// What collect read from the world, before it becomes evidence.
struct Reads {
    /// The boot catalog, `catalog.json`.
    boot: Vec<Table>,
    ignore: Ignore,
    /// The wait for quiet.
    quiet: Result<(), SupervisorError>,
    snapshot: Result<Snapshot, PgError>,
    /// The lines of `effects.jsonl`.
    effects: Vec<Value>,
    /// The fault that left `effects.jsonl` unread, when it was over the evidence budget.
    oversized: Option<Fault>,
    /// The lines of `faults.jsonl`.
    faults: Vec<Value>,
}

/// The evidence of `reads`. Its faults are the journal's, then collect's own: a failed quiet, a
/// snapshot that failed, which leaves no changes, then an effects journal left unread. A fold
/// error is Miroir's own failure.
fn assemble(reads: Reads) -> anyhow::Result<Evidence> {
    let Reads { boot, ignore, quiet, snapshot, effects, oversized, faults: lines } = reads;
    let mut faults = Vec::new();
    for (line, value) in (1_usize..).zip(lines) {
        let fault = serde_json::from_value(value)
            .with_context(|| format!("faults.jsonl line {line} is not a fault"))?;
        faults.push(fault);
    }
    if let Err(error) = quiet {
        faults.push(Fault { kind: fault::QUIET_TIMEOUT.into(), detail: error.to_string() });
    }
    let changes = match snapshot {
        Ok(snapshot) => {
            let mut changes = fold::fold(&snapshot.log, &boot, &ignore).map_err(|e| {
                anyhow::anyhow!("cannot fold the change log into net changes, a Miroir bug: {e}")
            })?;
            changes.extend(fold::schema_changes(&boot, &snapshot.tables, &ignore));
            changes
        }
        Err(error) => {
            let kind = match error {
                PgError::OverBudget { .. } => fault::LOG_BUDGET,
                PgError::Unreachable(_) | PgError::Sql(_) => fault::DATABASE,
            };
            faults.push(Fault { kind: kind.into(), detail: error.to_string() });
            Vec::new()
        }
    };
    faults.extend(oversized);
    let tables = captured(&boot, &ignore);
    Ok(Evidence { miroir: env!("CARGO_PKG_VERSION").into(), tables, changes, effects, faults })
}

/// The tables captured at boot, in catalog order, with their ignored columns left out.
fn captured(boot: &[Table], ignore: &Ignore) -> Vec<CapturedTable> {
    let mut tables = Vec::new();
    for table in boot.iter().filter(|table| table.captured) {
        let name = table.evidence_name();
        let ignored = ignore.columns.get(&name);
        let (mut columns, mut pk) = (Vec::new(), Vec::new());
        for column in &table.columns {
            if ignored.is_some_and(|ignored| ignored.contains(&column.name)) {
                continue;
            }
            if column.pk {
                pk.push(column.name.clone());
            }
            columns.push(column.name.clone());
        }
        tables.push(CapturedTable { name, columns, pk });
    }
    tables
}

/// Write `evidence` to `target` through `<target>.tmp`, fsynced before the rename, so
/// `target` is whole or absent. Past `evidence::BUDGET`, the file holds `meta`, `tables` and
/// `faults` only, with an `evidence_budget` fault counting what it left out.
async fn persist(target: &Path, mut evidence: Evidence) -> anyhow::Result<Summary> {
    let tmp = target.with_extension("db.tmp");
    if let Err(e) = tokio::fs::remove_file(&tmp).await
        && e.kind() != ErrorKind::NotFound
    {
        return Err(e).with_context(|| format!("cannot remove the stale {}", tmp.display()));
    }
    let mut bytes = evidence::write(&tmp, &evidence).await?;
    if bytes > evidence::BUDGET {
        tokio::fs::remove_file(&tmp)
            .await
            .with_context(|| format!("cannot remove the oversized {}", tmp.display()))?;
        let detail = format!(
            "the evidence took {bytes} bytes, over its {}-byte budget, so its {} changes and {} \
             effects were left out",
            evidence::BUDGET,
            evidence.changes.len(),
            evidence.effects.len(),
        );
        evidence.changes.clear();
        evidence.effects.clear();
        evidence.faults.push(Fault { kind: fault::EVIDENCE_BUDGET.into(), detail });
        bytes = evidence::write(&tmp, &evidence).await?;
    }
    let file = tokio::fs::File::open(&tmp)
        .await
        .with_context(|| format!("cannot open {} to fsync it", tmp.display()))?;
    file.sync_all().await.with_context(|| format!("cannot fsync {}", tmp.display()))?;
    tokio::fs::rename(&tmp, target)
        .await
        .with_context(|| format!("cannot rename {} to {}", tmp.display(), target.display()))?;
    let Evidence { changes, effects, faults, .. } = evidence;
    Ok(Summary { changes: changes.len(), effects: effects.len(), faults, bytes })
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::PathBuf;

    use serde_json::{Map, json};
    use sqlx::Connection;

    use super::*;
    use crate::evidence::{ChangeOp, NetChange};
    use crate::manifest::{Database, DbTime};
    use crate::pg::{Column, ColumnKind, LogRow};

    /// A new, empty directory in the temp directory.
    fn new_dir(name: &str) -> PathBuf {
        let nanos =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = format!("miroir-collect-{name}-{}-{nanos}", std::process::id());
        let dir = std::env::temp_dir().join(dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn manifest() -> Manifest {
        Manifest {
            expose: vec![3000],
            processes: Vec::new(),
            env: BTreeMap::new(),
            bake: Vec::new(),
            seed: Vec::new(),
            database: Database {
                url: "postgres://postgres@127.0.0.1:5432/crm".into(),
                ignore: Vec::new(),
                time: DbTime { exclude: Vec::new(), epoch: BTreeMap::new(), captured: None },
            },
            sinks: None,
            quiet: None,
        }
    }

    /// A boot-catalog table whose `columns` are (name, primary key) pairs.
    fn table(schema: &str, name: &str, columns: &[(&str, bool)], captured: bool) -> Table {
        let mut kept = Vec::new();
        for &(column, pk) in columns {
            let (kind, sql_type) = (ColumnKind::Other, "text".into());
            kept.push(Column { name: column.into(), kind, generated: false, pk, sql_type });
        }
        Table {
            name: name.into(),
            schema: schema.into(),
            qualified: format!("\"{schema}\".\"{name}\""),
            columns: kept,
            partitioned: false,
            id: 1,
            captured,
        }
    }

    /// Two captured tables around one that bake left uncaptured.
    fn boot() -> Vec<Table> {
        let tokens = [("id", true), ("token", false), ("last_used_at", false)];
        vec![
            table("public", "api_tokens", &tokens, true),
            table("public", "sessions", &[("sid", true)], false),
            table("audit", "log", &[("line", false)], true),
        ]
    }

    fn reads(quiet: Result<(), SupervisorError>, snapshot: Result<Snapshot, PgError>) -> Reads {
        Reads {
            boot: boot(),
            ignore: Ignore::default(),
            quiet,
            snapshot,
            effects: Vec::new(),
            oversized: None,
            faults: Vec::new(),
        }
    }

    /// The snapshot of a database that does not answer, which never reaches the fold.
    fn dead_database() -> Result<Snapshot, PgError> {
        Err(PgError::Unreachable("connection refused".into()))
    }

    fn pairs(faults: &[Fault]) -> Vec<(&str, &str)> {
        faults.iter().map(|fault| (fault.kind.as_str(), fault.detail.as_str())).collect()
    }

    fn object(value: Value) -> Map<String, Value> {
        let Value::Object(map) = value else { panic!("not a JSON object: {value}") };
        map
    }

    /// An insert into `notes`.
    fn note(id: i64, text: &str) -> NetChange {
        NetChange {
            tbl: "notes".into(),
            op: ChangeOp::Insert,
            pk: object(json!({"id": id})),
            before: None,
            after: Some(object(json!({"id": id, "text": text}))),
            columns: None,
        }
    }

    fn request(path: &str) -> Value {
        json!({"kind": "http", "data": {
            "method": "POST", "path": path, "query": {}, "headers": {}, "body": null}})
    }

    /// Two changes, one effect and one fault.
    fn sample() -> Evidence {
        let notes = CapturedTable {
            name: "notes".into(),
            columns: vec!["id".into(), "text".into()],
            pk: vec!["id".into()],
        };
        Evidence {
            miroir: "0.1.0".into(),
            tables: vec![notes],
            changes: vec![note(1, "hello"), note(2, "world")],
            effects: vec![request("/stripe/v1/payment_intents")],
            faults: vec![Fault { kind: fault::CRASH.into(), detail: "worker exited".into() }],
        }
    }

    /// The rows of `meta`, `tables`, `changes`, `sent_requests` and `faults` in an evidence file.
    async fn counts(path: &Path) -> (i64, i64, i64, i64, i64) {
        let mut conn = evidence::open(path, false).await.unwrap();
        let counts = sqlx::query_as(
            "SELECT (SELECT count(*) FROM meta), (SELECT count(*) FROM tables), \
             (SELECT count(*) FROM changes), (SELECT count(*) FROM sent_requests), \
             (SELECT count(*) FROM faults)",
        )
        .fetch_one(&mut conn)
        .await
        .unwrap();
        conn.close().await.unwrap();
        counts
    }

    #[tokio::test]
    async fn collect_fails_when_the_world_never_became_ready() {
        let dir = new_dir("unready");
        let paths = Paths::new(&dir);
        std::fs::create_dir_all(paths.evidence_dir()).unwrap();
        let error = collect(&paths, &manifest()).await.unwrap_err().to_string();
        assert!(error.starts_with("the world never finished booting: "), "{error}");
        assert!(!paths.evidence().exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn collect_fails_without_a_readable_boot_catalog() {
        let dir = new_dir("catalog");
        let paths = Paths::new(&dir);
        std::fs::create_dir_all(paths.run_dir()).unwrap();
        std::fs::write(paths.ready(), "").unwrap();
        let error = format!("{:#}", collect(&paths, &manifest()).await.unwrap_err());
        assert!(error.starts_with("cannot read the boot catalog "), "{error}");
        std::fs::write(paths.catalog(), r#"{"tables": []}"#).unwrap();
        let error = format!("{:#}", collect(&paths, &manifest()).await.unwrap_err());
        assert!(error.contains("catalog.json is not a JSON array of tables: "), "{error}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_failed_snapshot_is_a_fault_and_leaves_no_changes() {
        let failures = [
            (PgError::Unreachable("connection refused".into()), fault::DATABASE),
            (PgError::Sql("canceling statement due to conflict".into()), fault::DATABASE),
            (PgError::OverBudget { rows: 3, budget: pg::LOG_BUDGET }, fault::LOG_BUDGET),
        ];
        for (error, kind) in failures {
            let detail = error.to_string();
            let evidence = assemble(reads(Ok(()), Err(error))).unwrap();
            assert_eq!(evidence.changes, []);
            assert_eq!(pairs(&evidence.faults), [(kind, detail.as_str())]);
        }
    }

    #[test]
    fn any_failed_quiet_is_a_quiet_timeout_fault() {
        let failures = [
            SupervisorError::QuietTimeout("quiet did not pass twice in a row within 30 s".into()),
            SupervisorError::Command("quiet [redis-cli] exited with status 127".into()),
            SupervisorError::Spawn("cannot start quiet [redis-cli]: not found".into()),
        ];
        for error in failures {
            let detail = error.to_string();
            let evidence = assemble(reads(Err(error), dead_database())).unwrap();
            assert_eq!(pairs(&evidence.faults)[0], (fault::QUIET_TIMEOUT, detail.as_str()));
        }
    }

    #[test]
    fn the_journals_faults_come_first_then_collects_own_in_step_order() {
        let quiet = Err(SupervisorError::QuietTimeout("quiet timed out".into()));
        let mut reads = reads(quiet, Err(PgError::OverBudget { rows: 9, budget: pg::LOG_BUDGET }));
        reads.faults = vec![
            json!({"kind": "crash", "detail": "worker exited with status 1"}),
            json!({"kind": "sink", "detail": "POST /hooks: a body over 25 MiB"}),
        ];
        let unread = "effects.jsonl took 16777217 bytes".to_string();
        reads.oversized = Some(Fault { kind: fault::EVIDENCE_BUDGET.into(), detail: unread });
        let evidence = assemble(reads).unwrap();
        let kinds: Vec<&str> = evidence.faults.iter().map(|fault| fault.kind.as_str()).collect();
        assert_eq!(
            kinds,
            [
                fault::CRASH,
                fault::SINK,
                fault::QUIET_TIMEOUT,
                fault::LOG_BUDGET,
                fault::EVIDENCE_BUDGET
            ]
        );
        assert_eq!(evidence.faults[1].detail, "POST /hooks: a body over 25 MiB");
        assert_eq!(evidence.faults[4].detail, "effects.jsonl took 16777217 bytes");
    }

    #[test]
    fn a_fault_line_of_another_shape_is_an_error() {
        let mut reads = reads(Ok(()), dead_database());
        reads.faults = vec![json!({"kind": "crash", "detail": "web exited"}), json!({"kind": 3})];
        let error = format!("{:#}", assemble(reads).unwrap_err());
        assert!(error.starts_with("faults.jsonl line 2 is not a fault: "), "{error}");
    }

    #[test]
    fn a_fold_error_is_collects_own_failure() {
        let new = Some(r#"{"id": "#.to_string());
        let row = LogRow { tbl: "api_tokens".into(), op: "insert".into(), old: None, new };
        let snapshot = Snapshot { tables: boot(), log: vec![row] };
        let error = assemble(reads(Ok(()), Ok(snapshot))).unwrap_err().to_string();
        let expected = "cannot fold the change log into net changes, a Miroir bug: ";
        assert!(error.starts_with(expected), "{error}");
    }

    #[test]
    fn the_tables_are_the_ones_captured_at_boot_without_their_ignored_columns() {
        let boot = boot();
        let mut reads = reads(Ok(()), dead_database());
        let ignored = BTreeSet::from(["last_used_at".to_string()]);
        reads.ignore.columns.insert(boot[0].evidence_name(), ignored);
        let evidence = assemble(reads).unwrap();
        let mut tables = Vec::new();
        for table in evidence.tables {
            tables.push((table.name, table.columns, table.pk));
        }
        let strings = |all: &[&str]| -> Vec<String> { all.iter().map(|&s| s.into()).collect() };
        assert_eq!(
            tables,
            [
                (boot[0].evidence_name(), strings(&["id", "token"]), strings(&["id"])),
                (boot[2].evidence_name(), strings(&["line"]), strings(&[])),
            ]
        );
    }

    #[test]
    fn an_effects_journal_over_the_evidence_budget_is_a_fault_and_never_read() {
        let dir = new_dir("effects");
        let path = dir.join("effects.jsonl");
        let (effects, fault) = read_effects(&path).unwrap();
        assert!(effects.is_empty() && fault.is_none(), "a missing journal");
        std::fs::write(&path, format!("{}\n", request("/first"))).unwrap();
        let (effects, fault) = read_effects(&path).unwrap();
        assert!(effects == [request("/first")] && fault.is_none());
        // Zero bytes are not JSON, so only a journal left unread passes.
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(evidence::BUDGET + 1).unwrap();
        let (effects, fault) = read_effects(&path).unwrap();
        let fault = fault.unwrap();
        assert_eq!((effects, fault.kind.as_str()), (Vec::new(), fault::EVIDENCE_BUDGET));
        let detail = format!(
            "{} took {} bytes, over the evidence's {}-byte budget, so its effects were left out",
            path.display(),
            evidence::BUDGET + 1,
            evidence::BUDGET
        );
        assert_eq!(fault.detail, detail);
        file.set_len(evidence::BUDGET).unwrap();
        let error = format!("{:#}", read_effects(&path).unwrap_err());
        assert!(error.contains("line 1 is not JSON"), "a journal within budget is read: {error}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn effects_keep_their_journal_order_and_meta_names_this_version() {
        let mut reads = reads(Ok(()), dead_database());
        reads.effects = vec![request("/first"), request("/second")];
        let evidence = assemble(reads).unwrap();
        assert_eq!(evidence.effects, [request("/first"), request("/second")]);
        assert_eq!(evidence.miroir, env!("CARGO_PKG_VERSION"));
    }

    #[tokio::test]
    async fn persist_writes_the_evidence_through_a_temporary_file() {
        let dir = new_dir("persist");
        let target = dir.join("evidence.db");
        let summary = persist(&target, sample()).await.unwrap();
        assert_eq!((summary.changes, summary.effects), (2, 1));
        assert_eq!(pairs(&summary.faults), [(fault::CRASH, "worker exited")]);
        assert_eq!(summary.bytes, std::fs::metadata(&target).unwrap().len());
        assert_eq!(counts(&target).await, (2, 1, 2, 1, 1));
        assert!(!dir.join("evidence.db.tmp").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_stale_temporary_file_and_an_earlier_evidence_file_are_replaced() {
        let dir = new_dir("stale");
        let target = dir.join("evidence.db");
        std::fs::write(dir.join("evidence.db.tmp"), "left by a collect that was killed").unwrap();
        std::fs::write(&target, "an earlier collect's").unwrap();
        persist(&target, sample()).await.unwrap();
        assert_eq!(counts(&target).await, (2, 1, 2, 1, 1));
        assert!(!dir.join("evidence.db.tmp").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn evidence_over_the_budget_keeps_meta_tables_and_faults_only() {
        assert_eq!(evidence::BUDGET, 16 * 1024 * 1024, "§3.6 sets the evidence budget");
        let dir = new_dir("over");
        let target = dir.join("evidence.db");
        let mut evidence = sample();
        let text = "x".repeat(usize::try_from(evidence::BUDGET).unwrap());
        evidence.changes.push(note(3, &text));
        let summary = persist(&target, evidence).await.unwrap();
        assert_eq!((summary.changes, summary.effects), (0, 0));
        let kinds: Vec<&str> = summary.faults.iter().map(|fault| fault.kind.as_str()).collect();
        assert_eq!(kinds, [fault::CRASH, fault::EVIDENCE_BUDGET]);
        let detail = &summary.faults[1].detail;
        let left_out = format!(
            "over its {}-byte budget, so its 3 changes and 1 effects were left out",
            evidence::BUDGET
        );
        assert!(detail.starts_with("the evidence took "), "{detail}");
        assert!(detail.ends_with(&left_out), "{detail}");
        assert!(summary.bytes < evidence::BUDGET, "{}", summary.bytes);
        assert_eq!(summary.bytes, std::fs::metadata(&target).unwrap().len());
        assert_eq!(counts(&target).await, (2, 1, 0, 0, 2));
        assert!(!dir.join("evidence.db.tmp").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn evidence_under_the_budget_is_kept_whole() {
        let dir = new_dir("under");
        let target = dir.join("evidence.db");
        let mut evidence = sample();
        let size = evidence::BUDGET - 1024 * 1024;
        evidence.changes.push(note(3, &"x".repeat(usize::try_from(size).unwrap())));
        let summary = persist(&target, evidence).await.unwrap();
        assert_eq!((summary.changes, summary.effects, summary.faults.len()), (3, 1, 1));
        assert!(summary.bytes > size && summary.bytes <= evidence::BUDGET, "{}", summary.bytes);
        assert_eq!(counts(&target).await, (2, 1, 3, 1, 1));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn an_evidence_file_that_cannot_be_written_is_an_error() {
        let dir = new_dir("unwritable");
        let target = dir.join("missing").join("evidence.db");
        let error = format!("{:#}", persist(&target, sample()).await.unwrap_err());
        assert!(error.starts_with("cannot create the evidence file "), "{error}");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
