//! The env: its manifest, tasks, processes and databases, and the restore each reset runs.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::sync::Mutex;

use crate::code;
use crate::db::{Db, Shift, Table};
use crate::edb::EpisodeDb;
use crate::episode::Episode;
use crate::manifest::{Manifest, ManifestError, content_hash};
use crate::mcp;
use crate::state;
use crate::supervisor::Supervisor;
use crate::task::Task;
use crate::tools::Tools;

/// How long processes get to exit after SIGTERM.
pub const STOP_LIMIT: Duration = Duration::from_secs(30);
/// How long a database gets to answer once started.
pub const DATABASE_LIMIT: Duration = Duration::from_secs(60);
const HOOK_LIMIT: Duration = Duration::from_secs(300);
/// How long one bake, seed or import command may run.
pub const BAKE_LIMIT: Duration = Duration::from_secs(3600);
const SCAN_EVERY: u64 = 10;

#[derive(Debug, thiserror::Error)]
pub enum EnvError {
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    #[error("reset failed at step '{step}': {message}")]
    Reset { step: &'static str, message: String },
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Other(String),
}

impl EnvError {
    /// The `OpenEnv` error code a frame carries.
    pub fn code(&self) -> &'static str {
        match self {
            EnvError::Invalid(_) => "VALIDATION_ERROR",
            _ => "EXECUTION_ERROR",
        }
    }
}

/// Where the env's files are: under `/miroir` in an image.
pub struct Paths {
    pub root: PathBuf,
}

impl Default for Paths {
    fn default() -> Self {
        Paths { root: PathBuf::from("/miroir") }
    }
}

impl Paths {
    pub fn manifest(&self) -> PathBuf {
        self.root.join("miroir.yaml")
    }
    pub fn tasks(&self) -> PathBuf {
        self.root.join("tasks")
    }
    /// Baked state, a directory per world.
    pub fn pristine(&self) -> PathBuf {
        self.root.join("pristine")
    }
    /// Copies of the state with the dates already moved, a directory per world.
    pub fn shifted(&self) -> PathBuf {
        self.root.join("shifted")
    }
    /// The state before any world was baked, while several are.
    pub fn unbaked(&self) -> PathBuf {
        self.root.join("unbaked")
    }
    pub fn bake_info(&self) -> PathBuf {
        self.root.join("bake.json")
    }
    pub fn episode_db(&self) -> PathBuf {
        self.root.join("episode.db")
    }
}

/// What `bake.json` records.
#[derive(Serialize, Deserialize)]
pub struct BakeInfo {
    pub baked_at: Timestamp,
    pub miroir_version: String,
    pub manifest_hash: String,
    pub state: Vec<PathBuf>,
}

pub fn load(paths: &Paths) -> Result<(Manifest, Vec<Task>), ManifestError> {
    let manifest = Manifest::load(&paths.manifest())?;
    let tasks = manifest.load_tasks(&paths.tasks())?;
    Ok((manifest, tasks))
}

/// Databases by app.
pub type Dbs = BTreeMap<String, Db>;
/// Each app's tables at reset, which grading compares the end state with.
pub type Catalog = BTreeMap<String, Vec<Table>>;

/// Name the app in an error when there are several databases.
pub fn labelled(dbs: &Dbs, app: &str, e: impl std::fmt::Display) -> String {
    if dbs.len() > 1 { format!("app '{app}': {e}") } else { e.to_string() }
}

/// A baked env, running one episode at a time.
pub struct Env {
    pub paths: Paths,
    pub manifest: Manifest,
    pub tasks: Vec<Task>,
    pub supervisor: Supervisor,
    pub(crate) dbs: Dbs,
    pub edb: Arc<EpisodeDb>,
    pub tools: Tools,
    pub mcp: Option<mcp::Server>,
    pub sandbox: Option<code::Sandbox>,
    pub manifest_hash: String,
    pub tasks_hash: String,
    pub state: Vec<PathBuf>,
    anchor: Timestamp,
    pub(crate) episode: Arc<Mutex<Episode>>,
    restored: AtomicBool,
    leaked: AtomicBool,
    stopping: AtomicBool,
    resets: AtomicU64,
    baseline: Mutex<Option<state::Scan>>,
    restoring: Mutex<Restoring>,
}

#[derive(Default)]
struct Restoring {
    stamps: state::Stamps,
    source: Option<(String, Option<Shift>)>,
    saved: BTreeMap<String, Shift>,
}

impl Env {
    /// Open a baked env; nothing runs until `boot`.
    pub async fn open(paths: Paths, sandbox: Option<code::Sandbox>) -> Result<Env, EnvError> {
        let (manifest, tasks) = load(&paths)?;
        let info = bake_info(&paths).await?;
        let anchor = manifest.anchor().unwrap_or(info.baked_at);
        let state = info.state;
        for world in &manifest.worlds {
            let baked = tokio::fs::metadata(paths.pristine().join(world)).await;
            if !baked.is_ok_and(|meta| meta.is_dir()) {
                let message = format!("world '{world}' has no baked state; bake the image again");
                return Err(failed(&paths.pristine().display().to_string(), message));
            }
        }
        let supervisor = Supervisor::new(manifest.env.clone()).map_err(|e| EnvError::Other(e.0))?;
        let dbs = Db::for_apps(&manifest).map_err(|e| EnvError::Other(e.0))?;
        let edb = EpisodeDb::create(&paths.episode_db())
            .await
            .map_err(|e| EnvError::Other(format!("cannot create the episode DB: {e}")))?;
        let spec = |path: &Path| {
            if state.iter().any(|root| path.starts_with(root)) {
                state::pristine_dir(&paths.pristine().join(&manifest.worlds[0]), path)
            } else {
                path.to_path_buf()
            }
        };
        let tools =
            Tools::new(&manifest, spec).map_err(|e| EnvError::Other(format!("api: {e}")))?;
        let mcp = manifest.mcp().map(|(app, server)| mcp::Server::new(app, &server.url));
        let mcp = mcp.transpose().map_err(EnvError::Other)?;
        let episode = Arc::new(Mutex::new(Episode::default()));
        let text = tokio::fs::read(paths.manifest()).await.map_err(|e| failed("manifest", e))?;
        let hashes: Vec<&str> = tasks.iter().map(|t| t.hash.as_str()).collect();
        Ok(Env {
            paths,
            manifest_hash: content_hash(&text),
            tasks_hash: content_hash(hashes.concat().as_bytes()),
            state,
            manifest,
            tasks,
            supervisor,
            dbs,
            edb: Arc::new(edb),
            tools,
            mcp,
            sandbox,
            anchor,
            episode,
            restored: AtomicBool::new(false),
            leaked: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
            resets: AtomicU64::new(0),
            baseline: Mutex::new(None),
            restoring: Mutex::new(Restoring::default()),
        })
    }

    /// Refuse resets once files changed outside the state directories.
    pub(crate) fn refuse_leaked(&self) -> Result<(), EnvError> {
        if self.leaked.load(Ordering::SeqCst) {
            let message = "files changed outside the state directories; replace this container";
            return Err(reset_failed("leak", message));
        }
        Ok(())
    }

    /// Every 10th reset, compare the files outside the state directories with the boot
    /// scan.
    pub(crate) async fn look_for_leaks(&self) -> Result<(), EnvError> {
        let done = self.resets.fetch_add(1, Ordering::SeqCst) + 1;
        if !done.is_multiple_of(SCAN_EVERY) {
            return Ok(());
        }
        let baseline = self.baseline.lock().await;
        let Some(baseline) = baseline.as_ref() else { return Ok(()) };
        let changed = state::diff(baseline, &state::scan(&self.state).await);
        if changed.is_empty() {
            return Ok(());
        }
        self.leaked.store(true, Ordering::SeqCst);
        let shown = changed.iter().take(10).cloned().collect::<Vec<_>>().join(", ");
        Err(reset_failed(
            "leak",
            format!(
                "{} paths changed outside the state directories, so episodes can see each other; \
                 add them to `state`, then replace this container: {shown}",
                changed.len()
            ),
        ))
    }

    /// Restored, with no leak and no crashed process.
    pub fn healthy(&self) -> bool {
        self.restored.load(Ordering::SeqCst)
            && !self.leaked.load(Ordering::SeqCst)
            && self.supervisor.crash().is_none()
    }

    /// How far a reset now would move the dates.
    pub fn shift(&self) -> Shift {
        self.shift_at(Timestamp::now())
    }

    pub(crate) fn shift_at(&self, now: Timestamp) -> Shift {
        shift_between(self.anchor, now)
    }

    /// Restore the first world, then scan the files that leaks are checked against.
    pub async fn boot(&self) -> Result<(), EnvError> {
        let episode = self.episode.lock().await;
        self.restore(&self.manifest.worlds[0], None, self.shift(), false, true).await?;
        drop(episode);
        *self.baseline.lock().await = Some(state::scan(&self.state).await);
        Ok(())
    }

    /// Restore the first world after a crash or a failed restore.
    pub async fn mend(&self) -> Result<(), EnvError> {
        let settled = || self.healthy() || self.leaked.load(Ordering::SeqCst);
        if settled() || self.baseline.lock().await.is_none() {
            return Ok(());
        }
        let mut episode = self.episode.lock().await;
        if settled() {
            return Ok(());
        }
        *episode = Episode::default();
        tracing::warn!("restoring the product, which a crash or a failed restore broke");
        self.restore(&self.manifest.worlds[0], None, self.shift(), false, true).await?;
        tracing::info!("env ready again");
        Ok(())
    }

    pub fn stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }

    /// Stop every process, killing what outlives `STOP_LIMIT`.
    pub async fn shutdown(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        if let Err(e) = self.supervisor.stop(STOP_LIMIT).await {
            tracing::warn!("{e}; killing what is left");
            if let Err(e) = self.supervisor.kill_all().await {
                tracing::error!("{e}");
            }
        }
    }

    /// Restore a world: files, datastores, date shift, hook and setup, then the apps.
    pub async fn restore(
        &self,
        world: &str,
        setup: Option<&str>,
        shift: Shift,
        fingerprint: bool,
        reuse: bool,
    ) -> Result<Restored, EnvError> {
        if let Some(server) = &self.mcp {
            server.forget().await;
        }
        let restored = self.restore_steps(world, setup, shift, fingerprint, reuse).await;
        self.restored.store(restored.is_ok(), Ordering::SeqCst);
        restored
    }

    async fn restore_steps(
        &self,
        world: &str,
        setup: Option<&str>,
        shift: Shift,
        fingerprint: bool,
        reuse: bool,
    ) -> Result<Restored, EnvError> {
        let mut laps = Laps::start();
        self.supervisor.kill_all().await.map_err(|e| reset_failed("kill", e))?;
        laps.lap("kill");
        let mut restoring = self.restoring.lock().await;
        let shifted = reuse && restoring.saved.get(world) == Some(&shift);
        self.restore_files(&mut restoring, world, shifted.then_some(shift)).await?;
        laps.lap("restore");
        self.start_datastores().await?;
        laps.lap("datastores");
        if !shifted {
            self.shift_all(shift).await?;
        }
        laps.lap("shift");
        let moved = shift.timestamps != 0 || shift.dates != 0;
        if reuse && !shifted && moved && self.manifest.time.hook.is_none() {
            self.save_shifted(&mut restoring, world, shift).await?;
            laps.lap("save");
        }
        if let Some(hook) = &self.manifest.time.hook {
            let (days, date_days) = (shift.timestamps.to_string(), shift.dates.to_string());
            let extra = [
                ("MIROIR_SHIFT_DAYS", days.as_str()),
                ("MIROIR_SHIFT_DATE_DAYS", date_days.as_str()),
            ];
            self.supervisor
                .run_within(hook, &extra, HOOK_LIMIT)
                .await
                .map_err(|e| reset_failed("hook", e))?;
            laps.lap("hook");
        }
        if let Some(setup) = setup {
            for db in self.dbs.values() {
                db.execute(setup).await.map_err(|e| reset_failed("setup", e))?;
            }
            laps.lap("setup");
        }
        let print = if fingerprint {
            let print = self.fingerprints().await?;
            laps.lap("fingerprint");
            Some(print)
        } else {
            None
        };
        self.supervisor
            .start(self.manifest.app_processes())
            .await
            .map_err(|e| reset_failed("processes", e))?;
        laps.lap("apps");
        self.supervisor
            .wait_quiet(self.manifest.quiet.as_ref())
            .await
            .map_err(|e| reset_failed("quiet", e))?;
        laps.lap("quiet");
        let catalog = self.clear_logs().await?;
        self.edb.clear().await.map_err(|e| reset_failed("clear", e))?;
        laps.lap("clear");
        if let Some(crash) = self.supervisor.crash() {
            return Err(reset_failed("processes", crash));
        }
        Ok(Restored { print, seconds: laps.finish(), catalog })
    }

    async fn restore_files(
        &self,
        restoring: &mut Restoring,
        world: &str,
        shift: Option<Shift>,
    ) -> Result<(), EnvError> {
        let from = (world.to_string(), shift);
        if restoring.source.as_ref() != Some(&from) {
            restoring.stamps.clear();
            restoring.source = Some(from);
        }
        let dirs = self.state.clone();
        let copies = if shift.is_some() { self.paths.shifted() } else { self.paths.pristine() };
        let source = copies.join(world);
        let mut stamps = std::mem::take(&mut restoring.stamps);
        restoring.stamps = tokio::task::spawn_blocking(move || {
            for dir in state::TEMP_DIRS {
                state::empty(Path::new(dir))?;
            }
            state::restore(&dirs, &source, &mut stamps).map(|()| stamps)
        })
        .await
        .map_err(|e| reset_failed("restore", e))?
        .map_err(|e| reset_failed("restore", e))?;
        Ok(())
    }

    async fn save_shifted(
        &self,
        restoring: &mut Restoring,
        world: &str,
        shift: Shift,
    ) -> Result<(), EnvError> {
        restoring.saved.remove(world);
        for (app, db) in &self.dbs {
            db.compact().await.map_err(|e| reset_failed("save", labelled(&self.dbs, app, e)))?;
        }
        self.supervisor.stop(STOP_LIMIT).await.map_err(|e| reset_failed("save", e))?;
        let (dirs, target) = (self.state.clone(), self.paths.shifted().join(world));
        restoring.stamps = tokio::task::spawn_blocking(move || state::save(&dirs, &target))
            .await
            .map_err(|e| reset_failed("save", e))?
            .map_err(|e| reset_failed("save", e))?;
        restoring.source = Some((world.to_string(), Some(shift)));
        restoring.saved.insert(world.to_string(), shift);
        self.start_datastores().await
    }

    /// Restore the first world's files and start only the datastores, for `import`.
    pub async fn start_datastores_alone(&self) -> Result<(), EnvError> {
        let mut restoring = self.restoring.lock().await;
        self.restore_files(&mut restoring, &self.manifest.worlds[0], None).await?;
        self.start_datastores().await
    }

    async fn start_datastores(&self) -> Result<(), EnvError> {
        let started = self.supervisor.start(self.manifest.datastores()).await;
        started.map_err(|e| reset_failed("datastores", e))?;
        for (app, db) in &self.dbs {
            let answering = db.wait_answering(false, DATABASE_LIMIT).await;
            answering.map_err(|e| reset_failed("database", labelled(&self.dbs, app, e)))?;
        }
        Ok(())
    }

    async fn shift_all(&self, shift: Shift) -> Result<(), EnvError> {
        for (app, db) in &self.dbs {
            db.shift(shift)
                .await
                .map_err(|e| reset_failed("shift", labelled(&self.dbs, app, e)))?;
        }
        Ok(())
    }

    async fn fingerprints(&self) -> Result<BTreeMap<String, String>, EnvError> {
        let several = self.dbs.len() > 1;
        let mut prints = BTreeMap::new();
        for (app, db) in &self.dbs {
            let print = db.fingerprint().await;
            let print =
                print.map_err(|e| reset_failed("fingerprint", labelled(&self.dbs, app, e)))?;
            prints.extend(print.into_iter().map(|(table, hash)| {
                (if several { format!("{app}.{table}") } else { table }, hash)
            }));
        }
        Ok(prints)
    }

    async fn clear_logs(&self) -> Result<Catalog, EnvError> {
        let mut catalog = Catalog::new();
        for (app, db) in &self.dbs {
            let clear_failed = |e| reset_failed("clear", labelled(&self.dbs, app, e));
            db.clear_log().await.map_err(clear_failed)?;
            catalog.insert(app.clone(), db.tables().await.map_err(clear_failed)?);
        }
        Ok(catalog)
    }
}

/// What a restore reports: fingerprints if asked, step timings and the table catalog.
pub struct Restored {
    pub print: Option<BTreeMap<String, String>>,
    pub seconds: Map<String, Value>,
    pub catalog: Catalog,
}

struct Laps {
    started: Instant,
    since: Instant,
    seconds: Map<String, Value>,
}

impl Laps {
    fn start() -> Laps {
        let now = Instant::now();
        Laps { started: now, since: now, seconds: Map::new() }
    }

    fn lap(&mut self, step: &str) {
        self.seconds.insert(step.into(), json!(rounded_seconds(self.since.elapsed())));
        self.since = Instant::now();
    }

    fn finish(mut self) -> Map<String, Value> {
        self.seconds.insert("total".into(), json!(rounded_seconds(self.started.elapsed())));
        self.seconds
    }
}

/// Seconds, to the millisecond.
pub fn rounded_seconds(elapsed: Duration) -> f64 {
    (elapsed.as_secs_f64() * 1000.0).round() / 1000.0
}

async fn bake_info(paths: &Paths) -> Result<BakeInfo, EnvError> {
    let file = paths.bake_info();
    let text = tokio::fs::read(&file).await.map_err(|e| {
        EnvError::Other(format!("{}: {e}; the image must run `miroir bake`", file.display()))
    })?;
    serde_json::from_slice(&text).map_err(|e| EnvError::Other(format!("{}: {e}", file.display())))
}

pub fn failed(what: &str, e: impl std::fmt::Display) -> EnvError {
    EnvError::Other(format!("{what}: {e}"))
}

pub fn reset_failed(step: &'static str, e: impl std::fmt::Display) -> EnvError {
    EnvError::Reset { step, message: e.to_string() }
}

/// The shift from `from` to `to`: whole days for timestamps, calendar days for dates.
pub fn shift_between(from: Timestamp, to: Timestamp) -> Shift {
    Shift { timestamps: days_between(from, to), dates: calendar_days_between(from, to) }
}

/// Whole days from `from` to `to`, rounded down.
pub fn days_between(from: Timestamp, to: Timestamp) -> i64 {
    let nanos = to.duration_since(from).as_nanos();
    i64::try_from(nanos.div_euclid(86_400 * 1_000_000_000)).unwrap_or(0)
}

/// UTC calendar days from `from`'s date to `to`'s.
pub fn calendar_days_between(from: Timestamp, to: Timestamp) -> i64 {
    to.as_second().div_euclid(86_400) - from.as_second().div_euclid(86_400)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_round_down_in_whole_days() {
        let anchor: Timestamp = "2026-10-01T12:00:00Z".parse().unwrap();
        let at = |s: &str| days_between(anchor, s.parse().unwrap());
        assert_eq!(at("2026-10-02T11:59:59Z"), 0);
        assert_eq!(at("2026-10-02T12:00:00Z"), 1);
        assert_eq!(at("2026-10-27T14:03:11Z"), 26);
        assert_eq!(at("2026-10-01T11:59:59Z"), -1);
    }

    #[test]
    fn dates_move_by_calendar_days_however_late_the_reset() {
        let anchor: Timestamp = "2026-10-01T18:00:00Z".parse().unwrap();
        let at = |s: &str| {
            let now: Timestamp = s.parse().unwrap();
            (days_between(anchor, now), calendar_days_between(anchor, now))
        };
        assert_eq!(at("2026-10-01T23:59:59Z"), (0, 0));
        assert_eq!(at("2026-10-02T00:00:00Z"), (0, 1), "the calendar moved, 6 hours after bake");
        assert_eq!(at("2026-10-02T17:59:59Z"), (0, 1));
        assert_eq!(at("2026-10-02T18:00:00Z"), (1, 1));
        assert_eq!(at("2026-10-03T00:00:00Z"), (1, 2));
        assert_eq!(at("2026-09-30T23:00:00Z"), (-1, -1));
    }

    #[test]
    fn laps_count_each_step_once_and_the_total_counts_them_all() {
        let mut laps = Laps::start();
        std::thread::sleep(Duration::from_millis(20));
        laps.lap("kill");
        laps.lap("restore");
        let seconds = laps.finish();
        let of = |step: &str| seconds[step].as_f64().unwrap();
        assert!(of("kill") >= 0.02, "{seconds:?}");
        assert!(of("restore") < of("kill"), "the second lap starts where the first ended");
        assert!(of("total") >= of("kill"), "{seconds:?}");
    }
}
