//! `miroir run`, the world image's `CMD`: boot the world, audit its listeners, write the ready
//! marker, and supervise until SIGTERM. Bake reuses its datastore start, date shift and stop
//! limit.

use std::fmt::Display;
use std::fs::DirBuilder;
use std::io::ErrorKind;
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Context;
use jiff::Timestamp;
use tokio::signal::unix::{SignalKind, signal};

use crate::bake::BakeInfo;
use crate::manifest::Manifest;
use crate::paths::Paths;
use crate::pg::{self, Pg, Shift};
use crate::sinks::{self, Recording};
use crate::supervisor::{QUIET_LIMIT, READY_LIMIT, Supervisor};
use crate::{journal, listen};

/// How long the processes get to exit after SIGTERM before SIGKILL, at bake and at shutdown.
pub(crate) const STOP_LIMIT: Duration = Duration::from_secs(30);

/// Boot the world and supervise it until SIGTERM.
pub async fn run(paths: &Paths, manifest: &Manifest) -> anyhow::Result<()> {
    let info = baked(&paths.bake_info())?;
    let run_dir = paths.run_dir();
    // The image never holds the runtime directory, so only an earlier boot made it. Refuse
    // before touching it: it holds that run's journals.
    if run_dir.exists() {
        let error = "this container booted before, so its dates may have moved already and its \
                     database holds that run's changes: start a new container from the image";
        return Err(failed(2, run_dir.display(), error));
    }
    fresh_run_dir(&run_dir).map_err(|e| failed(2, run_dir.display(), e))?;
    let mut terminate = signal(SignalKind::terminate()).context("cannot watch for SIGTERM")?;
    let supervisor = Supervisor::new(manifest.env.clone(), Some(paths.faults()));
    tokio::select! {
        booted = boot(paths, manifest, &supervisor, info.baked_at) => {
            booted?;
            terminate.recv().await;
        }
        _ = terminate.recv() => {}
    }
    tracing::info!("SIGTERM: stopping the world");
    supervisor
        .stop(STOP_LIMIT)
        .await
        .map_err(|e| anyhow::anyhow!("stopping the world after SIGTERM: {e}"))
}

/// Step 1: `bake.json`, which only a baked image has.
fn baked(path: &Path) -> anyhow::Result<BakeInfo> {
    let unreadable =
        |e: &dyn Display| failed(1, path.display(), format_args!("{e}: bake the image again"));
    let text = match std::fs::read(path) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            anyhow::bail!("this image was not baked: add `RUN miroir bake` to its Dockerfile")
        }
        Err(e) => return Err(unreadable(&e)),
    };
    serde_json::from_slice(&text).map_err(|e| unreadable(&e))
}

/// Step 2: remove the runtime directory, so no stale ready marker survives, and make it again,
/// empty and private.
fn fresh_run_dir(dir: &Path) -> std::io::Result<()> {
    remove_dir(dir)?;
    DirBuilder::new().mode(0o700).create(dir)
}

/// Remove `dir` and everything in it; a missing directory is already removed.
pub(crate) fn remove_dir(dir: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(dir) {
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        removed => removed,
    }
}

/// Steps 3 to 9: everything up to the ready marker, then the timing line.
async fn boot(
    paths: &Paths,
    manifest: &Manifest,
    supervisor: &Supervisor,
    baked_at: Timestamp,
) -> anyhow::Result<()> {
    let mut timing = Timing::start();
    if let Some(sinks) = &manifest.sinks {
        let recording = Recording { effects: paths.effects(), faults: paths.faults() };
        sinks::start(sinks, Some(recording)).await.map_err(|e| failed(3, "sinks", e))?;
    }
    let pg = Pg::new(&manifest.database);
    start_datastores(supervisor, manifest, &pg).await.map_err(|e| failed(4, "datastores", e))?;
    timing.lap("datastores");
    let shift = pg::shift_between(baked_at, Timestamp::now());
    shift_dates(&pg, shift).await.map_err(|e| failed(5, "date shift", e))?;
    timing.lap("shift");
    supervisor.start(&manifest.apps(), READY_LIMIT).await.map_err(|e| failed(6, "processes", e))?;
    timing.lap("apps");
    supervisor
        .wait_quiet(manifest.quiet.as_deref(), QUIET_LIMIT)
        .await
        .map_err(|e| failed(6, "quiet", e))?;
    timing.lap("quiet");
    start_episode(paths, &pg).await?;
    if let Some(crash) = supervisor.crash() {
        let error = format!("it exited during boot ({}): check its output above", crash.status);
        return Err(failed(7, format_args!("process '{}'", crash.process), error));
    }
    let listeners = listen::current().map_err(|e| failed(8, "listener audit", e))?;
    listen::audit(&listeners, manifest).map_err(|e| failed(8, "listener audit", e))?;
    let ready = paths.ready();
    std::fs::write(&ready, "").map_err(|e| failed(9, ready.display(), e))?;
    tracing::info!("{}", timing.line());
    Ok(())
}

/// Start the datastores until ready, then wait until the database answers (bake step 3, boot
/// step 4). The error names the process or setting, and the fix.
pub(crate) async fn start_datastores(
    supervisor: &Supervisor,
    manifest: &Manifest,
    pg: &Pg,
) -> Result<(), String> {
    supervisor.start(&manifest.datastores(), READY_LIMIT).await.map_err(|e| e.to_string())?;
    pg.wait_answering(READY_LIMIT).await.map_err(|e| {
        format!("database.url: {e}: check that a datastore process serves it to a superuser")
    })
}

/// Move the database's time columns by `shift`, unless both of its numbers are 0 (boot step 5;
/// bake step 5 moves captured data with it).
pub(crate) async fn shift_dates(pg: &Pg, shift: Shift) -> Result<(), String> {
    if shift == (Shift { timestamps: 0, dates: 0 }) {
        return Ok(());
    }
    tracing::info!(days = shift.timestamps, calendar_days = shift.dates, "shifting dates");
    let tables = pg.catalog().await.map_err(|e| e.to_string())?;
    pg.shift(&tables, shift).await.map_err(|e| {
        format!(
            "{e}: if a constraint refuses a moved value, list its column in \
             database.time.exclude"
        )
    })
}

/// Step 7: start the episode clean, with no change log and no journal lines, and record the
/// catalog it starts from.
async fn start_episode(paths: &Paths, pg: &Pg) -> anyhow::Result<()> {
    pg.clear_log().await.map_err(|e| failed(7, "miroir.log", e))?;
    for journal in [paths.effects(), paths.faults()] {
        journal::clear(&journal).map_err(|e| failed(7, journal.display(), e))?;
    }
    let tables = pg.catalog().await.map_err(|e| failed(7, "the catalog", e))?;
    let catalog = paths.catalog();
    std::fs::write(&catalog, serde_json::to_vec(&tables)?)
        .map_err(|e| failed(7, catalog.display(), e))?;
    Ok(())
}

/// A failed boot step: its number, the process, command or file it was at, and the error, which
/// says what to change.
fn failed(step: u8, at: impl Display, error: impl Display) -> anyhow::Error {
    anyhow::anyhow!("boot step {step}, {at}: {error}")
}

/// The boot's step timings, for its one-line summary.
struct Timing {
    started: Instant,
    lap_started: Instant,
    laps: Vec<(&'static str, Duration)>,
}

impl Timing {
    fn start() -> Timing {
        let now = Instant::now();
        Timing { started: now, lap_started: now, laps: Vec::new() }
    }

    /// End `step`, which began where the previous one ended.
    fn lap(&mut self, step: &'static str) {
        let now = Instant::now();
        self.laps.push((step, now - self.lap_started));
        self.lap_started = now;
    }

    fn line(&self) -> String {
        timing_line(self.started.elapsed(), &self.laps)
    }
}

/// `booted in 7.2 s: datastores 1.1, shift 0.4, apps 3.0, quiet 1.5`
fn timing_line(total: Duration, laps: &[(&str, Duration)]) -> String {
    let mut steps = Vec::new();
    for (step, took) in laps {
        steps.push(format!("{step} {:.1}", took.as_secs_f64()));
    }
    format!("booted in {:.1} s: {}", total.as_secs_f64(), steps.join(", "))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::bake::tests::{manifest, temp_root};

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[tokio::test]
    async fn an_image_never_baked_does_not_boot() {
        let root = temp_root("unbaked");
        let paths = Paths::new(&*root);
        let error = run(&paths, &manifest()).await.unwrap_err();
        assert_eq!(
            error.to_string(),
            "this image was not baked: add `RUN miroir bake` to its Dockerfile"
        );
        assert!(!paths.run_dir().exists(), "nothing ran");
    }

    #[tokio::test]
    async fn an_unreadable_bake_json_names_the_file_and_the_fix() {
        let roots = [temp_root("garbled"), temp_root("directory")];
        let garbled = Paths::new(&*roots[0]);
        std::fs::write(garbled.bake_info(), "{\"baked_at\": 1}").unwrap();
        let directory = Paths::new(&*roots[1]);
        std::fs::create_dir(directory.bake_info()).unwrap();
        for paths in [garbled, directory] {
            let error = run(&paths, &manifest()).await.unwrap_err().to_string();
            let file = paths.bake_info().display().to_string();
            assert!(error.starts_with(&format!("boot step 1, {file}: ")), "{error}");
            assert!(error.ends_with(": bake the image again"), "{error}");
            assert!(!paths.run_dir().exists(), "nothing ran");
        }
    }

    #[test]
    fn a_bake_json_reads_back_as_written() {
        let root = temp_root("bake-json");
        let paths = Paths::new(&*root);
        let baked_at: Timestamp = "2026-11-17T10:00:00Z".parse().unwrap();
        let info = BakeInfo { baked_at, miroir: "0.1.0".into() };
        std::fs::write(paths.bake_info(), serde_json::to_vec(&info).unwrap()).unwrap();
        let read = baked(&paths.bake_info()).unwrap();
        assert_eq!((read.baked_at, read.miroir.as_str()), (baked_at, "0.1.0"));
    }

    #[tokio::test]
    async fn a_container_that_booted_before_refuses_to_boot_again() {
        let root = temp_root("rebooted");
        let paths = Paths::new(&*root);
        let info = BakeInfo { baked_at: Timestamp::now(), miroir: "0.1.0".into() };
        std::fs::write(paths.bake_info(), serde_json::to_vec(&info).unwrap()).unwrap();
        std::fs::create_dir(paths.run_dir()).unwrap();
        std::fs::write(paths.ready(), "").unwrap();
        let error = run(&paths, &manifest()).await.unwrap_err().to_string();
        let dir = paths.run_dir().display().to_string();
        let refusal = format!("boot step 2, {dir}: this container booted before, ");
        assert!(error.starts_with(&refusal), "{error}");
        assert!(error.ends_with(": start a new container from the image"), "{error}");
        assert!(paths.ready().exists(), "the earlier run's files are kept for inspection");
        let again = run(&paths, &manifest()).await.unwrap_err().to_string();
        assert!(again.starts_with(&refusal), "every later boot refuses too: {again}");
    }

    #[test]
    fn a_stale_run_dir_is_replaced_by_an_empty_private_one() {
        let root = temp_root("stale");
        let dir = root.join("run");
        std::fs::create_dir_all(dir.join("nested")).unwrap();
        std::fs::write(dir.join("ready"), "").unwrap();
        std::fs::write(dir.join("nested").join("effects.jsonl"), "{}\n").unwrap();
        fresh_run_dir(&dir).unwrap();
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0, "no stale ready marker");
        assert_eq!(mode(&dir), 0o700);
    }

    #[test]
    fn a_missing_run_dir_is_made_private() {
        let root = temp_root("missing");
        let dir = root.join("run");
        fresh_run_dir(&dir).unwrap();
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        assert_eq!(mode(&dir), 0o700);
    }

    #[test]
    fn a_run_path_that_is_a_file_is_an_error_not_a_missing_directory() {
        let root = temp_root("file");
        let file = root.join("run");
        std::fs::write(&file, "").unwrap();
        assert!(remove_dir(&file).is_err());
        assert!(fresh_run_dir(&file).is_err());
        assert!(file.is_file(), "left as it was");
    }

    #[tokio::test]
    async fn the_dates_move_unless_both_numbers_are_zero() {
        // Nothing listens on port 1, so any database access fails.
        let mut database = manifest().database;
        database.url = "postgres://postgres@127.0.0.1:1/none".into();
        let pg = Pg::new(&database);
        assert_eq!(shift_dates(&pg, Shift { timestamps: 0, dates: 0 }).await, Ok(()));
        for moved in [(0, 1), (1, 1), (-1, -1), (1, 0)] {
            let shift = Shift { timestamps: moved.0, dates: moved.1 };
            assert!(shift_dates(&pg, shift).await.is_err(), "{shift:?} reads the database");
        }
    }

    #[test]
    fn the_timing_line_gives_each_step_to_a_tenth_of_a_second() {
        let ms = Duration::from_millis;
        let laps =
            [("datastores", ms(1100)), ("shift", ms(440)), ("apps", ms(2960)), ("quiet", ms(1500))];
        assert_eq!(
            timing_line(ms(7200), &laps),
            "booted in 7.2 s: datastores 1.1, shift 0.4, apps 3.0, quiet 1.5"
        );
    }

    #[test]
    fn each_lap_counts_its_own_step_and_the_total_counts_them_all() {
        let mut timing = Timing::start();
        std::thread::sleep(Duration::from_millis(20));
        timing.lap("datastores");
        timing.lap("shift");
        let [(first, slow), (second, quick)] = timing.laps[..] else { panic!("{:?}", timing.laps) };
        assert_eq!((first, second), ("datastores", "shift"));
        assert!(slow >= Duration::from_millis(20) && quick < slow, "{:?}", timing.laps);
        assert!(timing.started.elapsed() >= slow + quick);
        assert!(timing.line().starts_with("booted in "), "{}", timing.line());
    }
}
