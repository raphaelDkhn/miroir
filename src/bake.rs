//! `miroir bake`, the last `RUN` of a world Dockerfile: seed the world, install change capture,
//! and record the bake.

use std::collections::{BTreeSet, HashSet};
use std::fmt::Display;
use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use jiff::Timestamp;
use jiff::tz::TimeZone;
use serde::{Deserialize, Serialize};

use crate::manifest::Manifest;
use crate::paths::Paths;
use crate::pg::{self, Ignore, Pg, Shift, Table};
use crate::run::{STOP_LIMIT, remove_dir, shift_dates, start_datastores};
use crate::sinks;
use crate::supervisor::{QUIET_LIMIT, READY_LIMIT, Supervisor};

/// How long one `bake` or `seed` command may run.
const COMMAND_LIMIT: Duration = Duration::from_secs(3600);

/// The columns the grader adds to every row image, which no captured column may shadow.
const RESERVED: [&str; 2] = ["_op", "_change"];

/// `/miroir/bake.json`: when the bake started, and which Miroir baked.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BakeInfo {
    /// The bake's start, from which every boot shifts the world's dates.
    pub baked_at: jiff::Timestamp,
    /// The version of the `miroir` binary that baked.
    pub miroir: String,
}

/// The shift that moves data captured at `captured` to `started`; an error when `captured` is
/// later.
pub fn captured_shift(
    captured: jiff::Timestamp,
    started: jiff::Timestamp,
) -> Result<Shift, String> {
    if captured > started {
        return Err(format!(
            "the data was captured at {captured}, after this bake began at {started}: set \
             database.time.captured to when the dump was taken"
        ));
    }
    Ok(pg::shift_between(captured, started))
}

/// Bake the world into the image.
pub async fn bake(paths: &Paths, manifest: &Manifest) -> anyhow::Result<()> {
    let started = Timestamp::now();
    let file = paths.bake_info();
    if file.try_exists().map_err(|e| failed(1, file.display(), e))? {
        let refusal = "this image is already baked: run `miroir bake` once, as the last RUN of a \
                       Dockerfile whose base is not a world image";
        return Err(failed(1, file.display(), refusal));
    }
    let captured = manifest.database.time.captured;
    let captured = captured.map(|at| captured_shift(at, started)).transpose();
    let captured = captured.map_err(|e| failed(5, "database.time.captured", e))?;
    if let Some(sinks) = &manifest.sinks {
        sinks::start(sinks, None).await.map_err(|e| failed(2, "sinks", e))?;
    }
    let supervisor = Supervisor::new(manifest.env.clone(), None);
    seed(manifest, &supervisor, captured).await?;
    supervisor.stop(STOP_LIMIT).await.map_err(|e| failed(9, "stopping the processes", e))?;
    one_day(started, Timestamp::now()).map_err(|e| failed(10, "the clock", e))?;
    finish(paths, started)?;
    tracing::info!(baked_at = %started, "baked");
    Ok(())
}

/// Steps 3 to 8: the datastores, the bake commands, the move of captured data, the other
/// processes, the seed commands, then change capture. The database handle ends with them,
/// because Postgres stops on SIGTERM only once every session has ended.
async fn seed(
    manifest: &Manifest,
    supervisor: &Supervisor,
    captured: Option<Shift>,
) -> anyhow::Result<()> {
    let pg = Pg::new(&manifest.database);
    start_datastores(supervisor, manifest, &pg).await.map_err(|e| failed(3, "datastores", e))?;
    run_commands(supervisor, 4, "bake", &manifest.bake).await?;
    if let Some(shift) = captured {
        shift_dates(&pg, shift).await.map_err(|e| failed(5, "moving the captured data", e))?;
    }
    supervisor.start(&manifest.apps(), READY_LIMIT).await.map_err(|e| failed(6, "processes", e))?;
    let quiet = manifest.quiet.as_deref();
    supervisor.wait_quiet(quiet, QUIET_LIMIT).await.map_err(|e| failed(6, "quiet", e))?;
    running(supervisor, 6)?;
    run_commands(supervisor, 7, "seed", &manifest.seed).await?;
    supervisor.wait_quiet(quiet, QUIET_LIMIT).await.map_err(|e| failed(7, "quiet", e))?;
    running(supervisor, 7)?;
    capture(&pg).await
}

/// Run each command of a step in turn, within the command limit.
async fn run_commands(
    supervisor: &Supervisor,
    step: u8,
    kind: &str,
    commands: &[Vec<String>],
) -> anyhow::Result<()> {
    for command in commands {
        let shown = command.join(" ");
        tracing::info!(command = %shown, "{kind} command");
        supervisor
            .run_within(command, COMMAND_LIMIT)
            .await
            .map_err(|e| failed(step, format_args!("{kind} command `{shown}`"), e))?;
    }
    Ok(())
}

/// Fail when a process exited since it started.
fn running(supervisor: &Supervisor, step: u8) -> anyhow::Result<()> {
    let Some(crash) = supervisor.crash() else { return Ok(()) };
    let error = format!("it exited during the bake ({}): check its output above", crash.status);
    Err(failed(step, format_args!("process '{}'", crash.process), error))
}

/// Step 8: capture the changes of every table not ignored, from an empty change log.
async fn capture(pg: &Pg) -> anyhow::Result<()> {
    let tables = pg.catalog().await.map_err(|e| failed(8, "the catalog", e))?;
    let ignore = pg.ignore(&tables).map_err(|e| failed(8, "database.ignore", e))?;
    sqlite_names(&tables, &ignore).map_err(|e| failed(8, "captured names", e))?;
    pg.install_capture(&tables, &ignore).await.map_err(|e| {
        failed(8, "change capture", format_args!("{e}: database.url must connect as a superuser"))
    })?;
    pg.clear_log().await.map_err(|e| failed(8, "miroir.log", e))
}

/// Refuse captured names the grader's SQLite copy of the tables cannot hold.
fn sqlite_names(tables: &[Table], ignore: &Ignore) -> Result<(), String> {
    let (mut clashes, mut names) = (Vec::new(), HashSet::new());
    for table in tables {
        let name = table.evidence_name();
        if ignore.tables.contains(&name) {
            continue;
        }
        let folded = name.to_ascii_lowercase();
        if folded.starts_with("sqlite_") || !names.insert(folded) {
            clashes.push(name.clone());
        }
        clashes.extend(column_clashes(table, &name, ignore.columns.get(&name)));
    }
    if clashes.is_empty() {
        return Ok(());
    }
    Err(format!(
        "{}: the grader copies captured tables into SQLite, which ignores ASCII case in names and \
         reserves table names starting sqlite_, and adds the columns _op and _change to every \
         row image: rename each, or add it to database.ignore",
        clashes.join(", ")
    ))
}

/// The captured columns of `table` equal, ignoring ASCII case, to an earlier one or to a
/// column the grader adds.
fn column_clashes(table: &Table, name: &str, ignored: Option<&BTreeSet<String>>) -> Vec<String> {
    let mut seen: HashSet<String> = RESERVED.map(String::from).into();
    let mut clashes = Vec::new();
    for column in &table.columns {
        let captured = !ignored.is_some_and(|ignored| ignored.contains(&column.name));
        if captured && !seen.insert(column.name.to_ascii_lowercase()) {
            clashes.push(format!("{name}.{}", column.name));
        }
    }
    clashes
}

/// Step 10: refuse a bake that ran past midnight UTC, because each boot moves dates by calendar
/// days from the bake's start, so rows written after midnight would read a day ahead.
fn one_day(started: Timestamp, ended: Timestamp) -> Result<(), String> {
    let day = |at: Timestamp| at.to_zoned(TimeZone::UTC).date();
    if day(started) == day(ended) {
        return Ok(());
    }
    Err(format!(
        "the bake ran past midnight UTC, from {started} to {ended}, so rows written after \
         midnight would read a day ahead of the rest: build the image again"
    ))
}

/// Step 11: record the bake, leave an empty evidence directory and no runtime state, and keep
/// the Miroir root from the product's users.
fn finish(paths: &Paths, started: Timestamp) -> anyhow::Result<()> {
    let info = BakeInfo { baked_at: started, miroir: env!("CARGO_PKG_VERSION").into() };
    let file = paths.bake_info();
    std::fs::write(&file, serde_json::to_vec_pretty(&info)?)
        .map_err(|e| failed(11, file.display(), e))?;
    let evidence = paths.evidence_dir();
    std::fs::create_dir_all(&evidence).map_err(|e| failed(11, evidence.display(), e))?;
    let run_dir = paths.run_dir();
    remove_dir(&run_dir).map_err(|e| failed(11, run_dir.display(), e))?;
    let Some(root) = file.parent() else {
        return Err(failed(11, file.display(), "the Miroir root has no directory to make private"));
    };
    std::fs::set_permissions(root, Permissions::from_mode(0o700))
        .map_err(|e| failed(11, root.display(), e))
}

/// A failed bake step: its number, the command, process or setting it was at, and the error,
/// which says what to change.
fn failed(step: u8, at: impl Display, error: impl Display) -> anyhow::Error {
    anyhow::anyhow!("bake step {step}, {at}: {error}")
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::manifest::{Database, DbTime, Sinks};
    use crate::pg::{Column, ColumnKind};

    /// A directory under the temp directory, removed with what it holds when dropped.
    pub(crate) struct TempRoot(PathBuf);

    impl std::ops::Deref for TempRoot {
        type Target = Path;

        fn deref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Numbers the temp roots of this process: tests run in parallel, and the clock alone can
    /// give two of them one name.
    static ROOTS: AtomicUsize = AtomicUsize::new(0);

    /// A new, empty directory under the temp directory.
    pub(crate) fn temp_root(name: &str) -> TempRoot {
        let nanos =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let n = ROOTS.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir()
            .join(format!("miroir-world-{name}-{}-{n}-{nanos}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        TempRoot(dir)
    }

    /// A manifest with no process, sink or command.
    pub(crate) fn manifest() -> Manifest {
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

    fn at(text: &str) -> Timestamp {
        text.parse().unwrap()
    }

    fn table(name: &str, columns: &[&str]) -> Table {
        let column = |name: &&str| Column {
            name: (*name).into(),
            kind: ColumnKind::Other,
            generated: false,
            pk: false,
            sql_type: "text".into(),
        };
        Table {
            name: name.into(),
            schema: "public".into(),
            qualified: format!("public.{name}"),
            columns: columns.iter().map(column).collect(),
            partitioned: false,
            id: 1,
            captured: false,
        }
    }

    #[test]
    fn captured_data_moves_from_its_capture_to_the_bake_start() {
        let captured = at("2026-10-26T06:00:00Z");
        for started in ["2026-11-09T14:00:00Z", "2026-11-09T05:00:00Z", "2026-10-26T06:00:00Z"] {
            let started = at(started);
            assert_eq!(
                captured_shift(captured, started),
                Ok(pg::shift_between(captured, started)),
                "the bake's own shift arithmetic, from {captured} to {started}"
            );
        }
    }

    #[test]
    fn data_captured_after_the_bake_began_is_refused() {
        let error = captured_shift(at("2026-11-09T14:00:01Z"), at("2026-11-09T14:00:00Z"));
        let error = error.unwrap_err();
        assert!(error.contains("captured at 2026-11-09T14:00:01Z"), "{error}");
        assert!(error.contains("after this bake began at 2026-11-09T14:00:00Z"), "{error}");
        assert!(error.ends_with("set database.time.captured to when the dump was taken"));
    }

    #[test]
    fn a_bake_that_runs_past_midnight_utc_is_refused() {
        assert_eq!(one_day(at("2026-11-05T00:00:00Z"), at("2026-11-05T23:59:59.999Z")), Ok(()));
        let error = one_day(at("2026-11-05T23:59:59Z"), at("2026-11-06T00:00:00Z")).unwrap_err();
        assert!(error.contains("ran past midnight UTC"), "{error}");
        assert!(error.ends_with("build the image again"), "{error}");
        let local = one_day(at("2026-11-05T23:30:00+02:00"), at("2026-11-06T00:30:00+02:00"));
        assert_eq!(local, Ok(()), "past local midnight, still 5 November in UTC");
    }

    #[test]
    fn captured_columns_sqlite_cannot_tell_apart_are_refused() {
        let notes = table("notes", &["id", "_op", "text", "_change"]);
        let error = sqlite_names(std::slice::from_ref(&notes), &Ignore::default());
        let error = error.unwrap_err();
        let name = notes.evidence_name();
        assert!(error.starts_with(&format!("{name}._op, {name}._change: ")), "{error}");
        assert!(error.ends_with("rename each, or add it to database.ignore"), "{error}");
        let cased = table("notes", &["Id", "id", "_OP", "_Change"]);
        let error = sqlite_names(&[cased], &Ignore::default()).unwrap_err();
        assert!(error.starts_with("notes.id, notes._OP, notes._Change: "), "{error}");
        let plain = table("deals", &["id", "op", "change"]);
        assert_eq!(sqlite_names(&[plain], &Ignore::default()), Ok(()));
    }

    #[test]
    fn tables_sqlite_cannot_tell_apart_or_reserves_are_refused() {
        let mut audit = table("sqlite_stat1", &["id"]);
        audit.schema = "audit".into();
        let tables =
            [table("User", &["id"]), table("user", &["id"]), table("SQLite_stat1", &["id"]), audit];
        let error = sqlite_names(&tables, &Ignore::default()).unwrap_err();
        assert!(error.starts_with("user, SQLite_stat1: "), "{error}");
        let mut ignore = Ignore::default();
        ignore.tables.extend(["user".to_string(), "SQLite_stat1".to_string()]);
        assert_eq!(sqlite_names(&tables, &ignore), Ok(()), "audit.sqlite_stat1 is allowed");
    }

    #[test]
    fn an_ignored_table_or_column_may_be_named_op_or_change() {
        let notes = table("notes", &["id", "_op", "_change"]);
        let name = notes.evidence_name();
        let mut ignore = Ignore::default();
        ignore.tables.insert(name.clone());
        assert_eq!(sqlite_names(std::slice::from_ref(&notes), &ignore), Ok(()));
        let mut ignore = Ignore::default();
        ignore.columns.insert(name.clone(), BTreeSet::from(["_op".to_string()]));
        let error = sqlite_names(&[notes], &ignore).unwrap_err();
        assert!(error.starts_with(&format!("{name}._change: ")), "only _change is captured");
    }

    #[tokio::test]
    async fn a_failed_command_names_its_step_and_its_argv() {
        let supervisor = Supervisor::new(BTreeMap::new(), None);
        let commands = [vec!["false".to_string()], vec!["true".to_string()]];
        let error = run_commands(&supervisor, 4, "bake", &commands).await.unwrap_err();
        let error = error.to_string();
        assert!(error.starts_with("bake step 4, bake command `false`: "), "{error}");
        assert!(run_commands(&supervisor, 7, "seed", &[]).await.is_ok(), "no seed command");
    }

    #[tokio::test]
    async fn an_image_baked_once_is_not_baked_again() {
        let root = temp_root("baked");
        let paths = Paths::new(&*root);
        std::fs::write(paths.bake_info(), "{}").unwrap();
        let error = bake(&paths, &manifest()).await.unwrap_err().to_string();
        let file = paths.bake_info().display().to_string();
        assert!(error.starts_with(&format!("bake step 1, {file}: ")), "{error}");
        assert!(error.contains("this image is already baked"), "{error}");
        assert_eq!(std::fs::read_to_string(paths.bake_info()).unwrap(), "{}");
        assert!(!paths.evidence_dir().exists(), "nothing ran");
    }

    #[tokio::test]
    async fn a_sink_port_in_use_fails_with_the_sinks_own_fix() {
        let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = taken.local_addr().unwrap().port();
        let root = temp_root("taken");
        let mut manifest = manifest();
        manifest.sinks = Some(Sinks { smtp: Some(port), http: None, responses: BTreeMap::new() });
        let error = bake(&Paths::new(&*root), &manifest).await.unwrap_err().to_string();
        let sink = format!("bake step 2, sinks: the SMTP sink cannot listen on 127.0.0.1:{port}: ");
        assert!(error.starts_with(&sink), "{error}");
        let fix = "; free the port, or give the sink another one in the manifest's sinks";
        assert!(error.ends_with(fix), "{error}");
    }

    #[tokio::test]
    async fn a_capture_after_the_bake_began_fails_before_anything_starts() {
        let root = temp_root("future");
        let paths = Paths::new(&*root);
        let mut manifest = manifest();
        manifest.database.time.captured = Some(at("2999-01-01T00:00:00Z"));
        let error = bake(&paths, &manifest).await.unwrap_err().to_string();
        assert!(error.starts_with("bake step 5, database.time.captured: "), "{error}");
        assert!(!paths.bake_info().exists() && !paths.evidence_dir().exists());
    }

    #[test]
    fn finishing_records_the_bake_and_makes_the_root_private() {
        let root = temp_root("finish");
        let paths = Paths::new(&*root);
        std::fs::create_dir_all(paths.run_dir().join("nested")).unwrap();
        std::fs::write(paths.ready(), "").unwrap();
        let started = at("2026-11-17T10:00:00Z");
        finish(&paths, started).unwrap();
        let info: BakeInfo =
            serde_json::from_slice(&std::fs::read(paths.bake_info()).unwrap()).unwrap();
        assert_eq!((info.baked_at, info.miroir.as_str()), (started, env!("CARGO_PKG_VERSION")));
        assert_eq!(std::fs::read_dir(paths.evidence_dir()).unwrap().count(), 0);
        assert!(!paths.run_dir().exists(), "no runtime state in the image");
        let mode = std::fs::metadata(&*root).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        finish(&paths, started).unwrap();
    }

    #[test]
    fn a_failed_finish_names_the_step_and_the_file() {
        let root = temp_root("gone");
        let paths = Paths::new(root.join("missing"));
        let error = finish(&paths, at("2026-11-17T10:00:00Z")).unwrap_err().to_string();
        let file = paths.bake_info().display().to_string();
        assert!(error.starts_with(&format!("bake step 11, {file}: ")), "{error}");
    }
}
