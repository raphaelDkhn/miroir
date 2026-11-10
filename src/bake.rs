//! `miroir bake`: seeds each world at image build and keeps its state as the pristine copy.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use jiff::Timestamp;

use crate::db::{Db, Shift};
use crate::env::{
    BAKE_LIMIT, BakeInfo, DATABASE_LIMIT, Dbs, EnvError, Paths, STOP_LIMIT, calendar_days_between,
    failed, labelled, shift_between,
};
use crate::manifest::{Manifest, content_hash};
use crate::state;
use crate::supervisor::{Supervisor, lookup_user};

/// Bake every world in turn, each from the same unbaked state, then write `bake.json`.
pub async fn run(paths: &Paths, manifest: &Manifest) -> Result<(), EnvError> {
    let started = Timestamp::now();
    let captured = captured_shifts(manifest, started).map_err(|e| failed("time", e))?;
    let supervisor = Supervisor::new(manifest.env.clone()).map_err(|e| EnvError::Other(e.0))?;
    let dbs = Db::for_apps(manifest).map_err(|e| EnvError::Other(e.0))?;
    let several = manifest.worlds.len() > 1;
    let unbaked = if several { Some(save_unbaked(paths, manifest).await?) } else { None };
    let mut roots = Vec::new();
    for (i, world) in manifest.worlds.iter().enumerate() {
        let baked = async {
            if let Some(dirs) = unbaked.as_ref().filter(|_| i > 0) {
                restore_unbaked(paths, dirs).await?;
            }
            bake_world(&supervisor, &dbs, manifest, world, captured.get(world).copied()).await?;
            keep_world(paths, manifest, world, unbaked.as_deref()).await
        };
        roots = baked.await.map_err(|e| {
            if several { EnvError::Other(format!("world '{world}': {e}")) } else { e }
        })?;
    }
    if unbaked.is_some() {
        let dir = paths.unbaked();
        tokio::fs::remove_dir_all(&dir)
            .await
            .map_err(|e| failed("state", format!("{}: {e}", dir.display())))?;
    }
    finish_bake(paths, manifest, started, roots).await
}

async fn bake_world(
    supervisor: &Supervisor,
    dbs: &Dbs,
    manifest: &Manifest,
    world: &str,
    captured: Option<Shift>,
) -> Result<(), EnvError> {
    let extra = [("MIROIR_WORLD", world)];
    tracing::info!(world, "baking world");
    supervisor.start(manifest.datastores()).await.map_err(|e| failed("datastores", e))?;
    for (app, db) in dbs {
        let answering = db.wait_answering(true, DATABASE_LIMIT).await;
        answering.map_err(|e| failed("database", labelled(dbs, app, e)))?;
    }
    for command in &manifest.bake {
        tracing::info!(world, command = %command.join(" "), "bake command");
        supervisor
            .run_within(command, &extra, BAKE_LIMIT)
            .await
            .map_err(|e| failed("bake command", e))?;
    }
    for (app, db) in dbs {
        let answering = db.wait_answering(false, DATABASE_LIMIT).await;
        answering.map_err(|e| failed("database", labelled(dbs, app, e)))?;
    }
    if let Some(shift) = captured {
        tracing::info!(world, days = shift.timestamps, "moving the captured data to the anchor");
        for (app, db) in dbs {
            let moved = db.shift(shift).await;
            moved.map_err(|e| failed("captured world", labelled(dbs, app, e)))?;
        }
    }
    supervisor.start(manifest.app_processes()).await.map_err(|e| failed("processes", e))?;
    supervisor.wait_quiet(manifest.quiet.as_ref()).await.map_err(|e| failed("quiet", e))?;
    if !manifest.seed.is_empty() {
        for command in &manifest.seed {
            tracing::info!(world, command = %command.join(" "), "seed command");
            supervisor
                .run_within(command, &extra, BAKE_LIMIT)
                .await
                .map_err(|e| failed("seed command", e))?;
        }
        supervisor.wait_quiet(manifest.quiet.as_ref()).await.map_err(|e| failed("quiet", e))?;
    }
    if let Some(crash) = supervisor.crash() {
        return Err(failed("processes", crash));
    }
    for (app, db) in dbs {
        let capture_failed = |e| failed("change capture", labelled(dbs, app, e));
        for warning in db.install_capture().await.map_err(capture_failed)? {
            tracing::warn!(world, app = app.as_str(), "{warning}");
        }
        db.clear_log().await.map_err(capture_failed)?;
    }
    supervisor.stop(STOP_LIMIT).await.map_err(|e| failed("stop", e))
}

async fn save_unbaked(paths: &Paths, manifest: &Manifest) -> Result<Vec<PathBuf>, EnvError> {
    let dirs = state_roots(manifest).await?;
    let (saved, target) = (dirs.clone(), paths.unbaked());
    tokio::task::spawn_blocking(move || state::save(&saved, &target))
        .await
        .map_err(|e| failed("state", e))?
        .map_err(|e| failed("state", format!("cannot save it before the first world: {e}")))?;
    Ok(dirs)
}

async fn restore_unbaked(paths: &Paths, dirs: &[PathBuf]) -> Result<(), EnvError> {
    let (dirs, source) = (dirs.to_vec(), paths.unbaked());
    tokio::task::spawn_blocking(move || {
        for dir in state::TEMP_DIRS {
            state::empty(Path::new(dir))?;
        }
        state::restore(&dirs, &source, &mut state::Stamps::new())
    })
    .await
    .map_err(|e| failed("state", e))?
    .map_err(|e| failed("state", e))
}

async fn keep_world(
    paths: &Paths,
    manifest: &Manifest,
    world: &str,
    before: Option<&[PathBuf]>,
) -> Result<Vec<PathBuf>, EnvError> {
    let roots = state_roots(manifest).await?;
    if let Some(before) = before {
        same_state(world, before, &roots).map_err(|e| failed("state", e))?;
    }
    let (moved, pristine) = (roots.clone(), paths.pristine().join(world));
    tokio::task::spawn_blocking(move || state::bake_move(&moved, &pristine))
        .await
        .map_err(|e| failed("state", e))?
        .map_err(|e| failed("state", e))?;
    Ok(roots)
}

fn same_state(world: &str, before: &[PathBuf], after: &[PathBuf]) -> Result<(), String> {
    let missing = |from: &[PathBuf], to: &[PathBuf]| -> Vec<String> {
        from.iter().filter(|path| !to.contains(path)).map(|p| p.display().to_string()).collect()
    };
    let (added, gone) = (missing(after, before), missing(before, after));
    if added.is_empty() && gone.is_empty() {
        return Ok(());
    }
    let shown = |paths: &[String]| if paths.is_empty() { "none".into() } else { paths.join(", ") };
    Err(format!(
        "baking world '{world}' changed what the processes can write (new: {}; gone: {}), so \
         the next world would bake on its files: create those paths in the image before bake, or \
         list state",
        shown(&added),
        shown(&gone)
    ))
}

fn one_day(started: Timestamp, ended: Timestamp) -> Result<(), String> {
    if calendar_days_between(started, ended) == 0 {
        return Ok(());
    }
    Err(format!(
        "the bake ran past midnight UTC, from {started} to {ended}, so data seeded before midnight \
         would read a day older than it was written: bake the image again"
    ))
}

async fn finish_bake(
    paths: &Paths,
    manifest: &Manifest,
    started: Timestamp,
    roots: Vec<PathBuf>,
) -> Result<(), EnvError> {
    let baked_at = Timestamp::now();
    if manifest.anchor().is_none() {
        one_day(started, baked_at).map_err(|e| failed("time", e))?;
    }
    let text = tokio::fs::read(paths.manifest()).await.map_err(|e| failed("manifest", e))?;
    let info = BakeInfo {
        baked_at,
        miroir_version: env!("CARGO_PKG_VERSION").into(),
        manifest_hash: content_hash(&text),
        state: roots,
    };
    let json = serde_json::to_vec_pretty(&info).map_err(|e| failed("bake.json", e))?;
    tokio::fs::write(paths.bake_info(), json).await.map_err(|e| failed("bake.json", e))?;
    for (path, mode) in
        [(paths.pristine(), 0o700), (paths.tasks(), 0o700), (paths.manifest(), 0o600)]
    {
        tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
            .await
            .map_err(|e| failed("permissions", e))?;
    }
    Ok(())
}

async fn state_roots(manifest: &Manifest) -> Result<Vec<PathBuf>, EnvError> {
    match &manifest.state {
        Some(dirs) => Ok(dirs.clone()),
        None => derive_state(manifest).await,
    }
}

async fn derive_state(manifest: &Manifest) -> Result<Vec<PathBuf>, EnvError> {
    let mut users = Vec::new();
    for process in &manifest.processes {
        let name = process.user.as_deref().unwrap_or("root");
        let (uid, gid) = lookup_user(name).await.map_err(|e| failed("state", e))?;
        if uid == 0 {
            let message =
                format!("process '{}' runs as uid 0: give it a user, or list state", process.name);
            return Err(failed("state", message));
        }
        users.push((uid, gid));
    }
    users.sort_unstable();
    users.dedup();
    let skip: Vec<PathBuf> =
        state::UNWALKED.iter().chain(&state::TEMP_DIRS).map(PathBuf::from).collect();
    let walked = skip.clone();
    let roots =
        tokio::task::spawn_blocking(move || state::writable_roots(Path::new("/"), &users, &walked))
            .await
            .map_err(|e| failed("state", e))?
            .map_err(|e| failed("state", e))?;
    if roots.is_empty() {
        return Err(failed("state", "no process can write anything to restore: list state"));
    }
    for root in &roots {
        if let Some(inside) = skip.iter().find(|s| s.starts_with(root)) {
            let message = format!(
                "a process can write {}, which holds {}: make it read-only, or list state",
                root.display(),
                inside.display()
            );
            return Err(failed("state", message));
        }
        tracing::info!(path = %root.display(), "state, as a process can write it");
    }
    Ok(roots)
}

fn captured_shifts(
    manifest: &Manifest,
    started: Timestamp,
) -> Result<BTreeMap<String, Shift>, String> {
    let to = manifest.anchor().unwrap_or(started);
    let mut shifts = BTreeMap::new();
    for (world, &at) in &manifest.time.captured {
        if at > started {
            return Err(format!(
                "time.captured has world '{world}' captured at {at}, after this bake began at \
                 {started}: a snapshot can't come from the future"
            ));
        }
        shifts.insert(world.clone(), shift_between(at, to));
    }
    Ok(shifts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_captured_world_moves_to_the_anchor_keeping_each_time_of_day() {
        let at = |s: &str| -> Timestamp { s.parse().unwrap() };
        let manifest = |time: &str| -> Manifest {
            let yaml = format!(
                "miroir: 1\nname: t\ndescription: t\nprocesses: []\npersonas: {{}}\napps: {{}}\n\
                 worlds: [week1, week2]\ntime: {time}\n"
            );
            serde_saphyr::from_str(&yaml).unwrap()
        };
        let bake = manifest("{captured: {week1: 2026-10-26T06:00:00Z}}");
        let afternoon = captured_shifts(&bake, at("2026-11-09T14:00:00Z")).unwrap();
        assert_eq!(afternoon["week1"], Shift { timestamps: 14, dates: 14 });
        let early = captured_shifts(&bake, at("2026-11-09T05:00:00Z")).unwrap();
        assert_eq!(
            early["week1"],
            Shift { timestamps: 13, dates: 14 },
            "a day apart, as a reset at an earlier time of day than the anchor's moves them"
        );
        assert!(!early.contains_key("week2"), "a world without a capture stays");
        let fixed = "{anchor: \"2026-10-01T00:00:00Z\", captured: {week1: 2026-10-26T06:00:00Z}}";
        let back = captured_shifts(&manifest(fixed), at("2026-11-09T14:00:00Z")).unwrap();
        assert_eq!(
            back["week1"],
            Shift { timestamps: -26, dates: -25 },
            "back to an earlier fixed anchor, and no newer than it"
        );
        let future = captured_shifts(&bake, at("2026-10-26T05:59:59Z")).unwrap_err();
        assert!(future.contains("can't come from the future"), "{future}");
    }

    #[test]
    fn a_world_must_leave_the_processes_the_same_state_to_write() {
        let paths = |list: &[&str]| list.iter().map(PathBuf::from).collect::<Vec<_>>();
        let before = paths(&["/etc/app.ini", "/var/lib/db"]);
        assert_eq!(same_state("north", &before, &before), Ok(()));
        let err = same_state("south", &before, &paths(&["/srv/extra", "/var/lib/db"])).unwrap_err();
        assert!(err.contains("baking world 'south' changed"), "{err}");
        assert!(err.contains("(new: /srv/extra; gone: /etc/app.ini)"), "{err}");
        let grown =
            same_state("south", &before, &paths(&["/etc/app.ini", "/srv/x", "/var/lib/db"]));
        assert!(grown.unwrap_err().contains("(new: /srv/x; gone: none)"));
    }

    #[test]
    fn a_bake_that_runs_past_midnight_utc_is_refused() {
        let at = |s: &str| s.parse::<Timestamp>().unwrap();
        assert_eq!(one_day(at("2026-11-05T00:00:00Z"), at("2026-11-05T23:59:59Z")), Ok(()));
        let err = one_day(at("2026-11-05T23:55:00Z"), at("2026-11-06T00:05:00Z")).unwrap_err();
        assert!(err.contains("ran past midnight UTC") && err.contains("bake the image again"));
    }
}
