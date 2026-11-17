//! `miroir run` in the world image (§3.5): the boot, its refusals, the date shift, crashes and
//! the SIGTERM stop.

use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::common::{WORLD, World, docker_ok, grade, manifest_with, spec};

const DAYS: i64 = 10;
const DAY_SECONDS: i64 = 86_400;

/// A world created with `manifest` in place of the fixture's: its boot must fail with `error`.
fn refuses_to_boot(manifest: &str, error: &str) {
    let world = World::create(&[("/miroir/miroir.yaml", manifest)], &[]);
    world.start();
    assert_eq!(world.wait_exit(), 1, "{}", world.logs());
    assert!(world.logs().contains(error), "no {error:?} in:\n{}", world.logs());
    assert!(!world.logs().contains("booted in"), "{}", world.logs());
}

#[test]
#[ignore = "needs Docker and `docker buildx bake`"]
fn a_world_boots_stops_on_sigterm_and_boots_again_from_a_fresh_run_dir() {
    let world = World::boot();
    let logs = world.logs();
    let line = logs.lines().find(|line| line.contains("booted in ")).unwrap_or_else(|| {
        panic!("no timing line in:\n{logs}");
    });
    let (_, steps) = line.split_once("booted in ").unwrap().1.split_once(": ").unwrap();
    let names: Vec<&str> = steps.split(", ").map(|step| step.split(' ').next().unwrap()).collect();
    assert_eq!(names, ["datastores", "shift", "apps", "quiet"], "{line}");
    let modes = world.exec_ok(&["stat", "-c", "%a %U", "/miroir", "/miroir/run"]);
    assert_eq!(modes, "700 root\n700 root");
    assert_eq!(world.exec_ok(&["ls", "-A", "/miroir/run"]), "catalog.json\nready");
    world.exec_ok(&["touch", "/miroir/run/stale"]);
    let stopping = Instant::now();
    docker_ok(&["stop", "--time", "30", world.id()]);
    assert!(stopping.elapsed() < Duration::from_secs(20), "the processes outlived SIGTERM");
    assert_eq!(world.wait_exit(), 0, "{}", world.logs());
    assert!(world.logs().contains("SIGTERM: stopping the world"), "{}", world.logs());
    world.start();
    world.wait_ready();
    assert_eq!(world.exec_ok(&["ls", "-A", "/miroir/run"]), "catalog.json\nready", "no stale file");
}

#[test]
#[ignore = "needs Docker and `docker buildx bake`"]
fn sigterm_while_the_world_boots_stops_every_process_it_started() {
    let world = World::create(&[], &[]);
    world.start();
    let stopping = Instant::now();
    docker_ok(&["stop", "--time", "30", world.id()]);
    assert!(stopping.elapsed() < Duration::from_secs(20), "the processes outlived SIGTERM");
    assert_eq!(world.wait_exit(), 0, "{}", world.logs());
    let logs = world.logs();
    assert!(logs.contains("SIGTERM: stopping the world") && !logs.contains("booted in"), "{logs}");
}

#[test]
#[ignore = "needs Docker and `docker buildx bake`"]
fn a_world_whose_postgres_listens_on_every_address_refuses_to_boot() {
    let open = manifest_with("listen_addresses=127.0.0.1", "listen_addresses=0.0.0.0");
    refuses_to_boot(
        &open,
        "boot step 8, listener audit: port 5432 (db) listens on 0.0.0.0: bind it to 127.0.0.1, \
         or list it in expose",
    );
}

#[test]
#[ignore = "needs Docker and `docker buildx bake`"]
fn a_world_whose_exposed_port_listens_on_loopback_only_refuses_to_boot() {
    let closed = manifest_with("HOST: 0.0.0.0", "HOST: 127.0.0.1");
    refuses_to_boot(
        &closed,
        "boot step 8, listener audit: expose lists 3000 (web), but nothing listens on it off \
         loopback: check the product's bind address",
    );
}

#[test]
#[ignore = "needs Docker and `docker buildx bake`"]
fn a_process_that_exits_during_boot_fails_it_naming_the_process() {
    let flaky = "\n  - {name: flaky, command: [sh, -c, 'exit 3'], ready: {tcp: 7001}}\nenv:\n";
    refuses_to_boot(
        &manifest_with("\nenv:\n", flaky),
        "boot step 6, processes: process 'flaky' exited with status 3 before it was ready",
    );
}

/// Time columns of rows the seed planted, as numbers: seconds for timestamps and Unix times,
/// days since 2000-01-01 for dates.
const TIMES: &str = "\
SELECT extract(epoch FROM d.created_at)::bigint, extract(epoch FROM d.updated_at)::bigint,
       d.expected_close - date '2000-01-01', c.birthday - date '2000-01-01',
       extract(epoch FROM a.occurred_at)::bigint,
       (SELECT max(day) - date '2000-01-01' FROM exchange_rates),
       (SELECT min(valid_until) - date '2000-01-01' FROM exchange_rates),
       (SELECT extract(epoch FROM min(starts))::bigint FROM meetings),
       d.synced_unix, (SELECT min(synced_unix) FROM deals)
FROM deals d, contacts c, activities a
WHERE d.id = 1042 AND c.id = 101 AND a.deal_id = 1042
ORDER BY a.occurred_at LIMIT 1";

fn times(world: &World) -> Vec<i64> {
    world.psql(TIMES).split('|').map(|value| value.parse().unwrap()).collect()
}

/// The image's `bake.json`, its bake moved `days` earlier.
fn baked_earlier(days: i64) -> String {
    let baked = docker_ok(&["run", "--rm", WORLD, "cat", "/miroir/bake.json"]);
    let mut info: Value = serde_json::from_str(&baked).unwrap();
    let at: jiff::Timestamp = info["baked_at"].as_str().unwrap().parse().unwrap();
    let earlier = at.checked_sub(jiff::SignedDuration::from_hours(24 * days)).unwrap();
    info["baked_at"] = json!(earlier.to_string());
    info.to_string()
}

#[test]
#[ignore = "needs Docker and `docker buildx bake`"]
fn a_world_baked_days_earlier_boots_with_every_time_column_moved_as_many_days() {
    let base = times(&World::boot());
    let world = World::boot_with(&[("/miroir/bake.json", &baked_earlier(DAYS))], &[]);
    assert!(world.logs().contains(&format!("days={DAYS} calendar_days={DAYS}")), "no shift");
    let moved = times(&world);
    let delta: Vec<i64> = moved.iter().zip(&base).map(|(moved, base)| moved - base).collect();
    let (seconds, dates) = (DAYS * DAY_SECONDS, DAYS);
    assert_eq!(delta[0], seconds, "deals.created_at, a timestamp");
    assert_eq!(delta[1], seconds, "deals.updated_at, whose product trigger the shift skips");
    assert_eq!(delta[2], dates, "deals.expected_close, a date");
    assert_eq!(delta[3], 0, "contacts.birthday, in database.time.exclude");
    assert_eq!(delta[4], seconds, "activities.occurred_at");
    assert_eq!(delta[5], dates, "exchange_rates.day, in the primary key and a CHECK");
    assert_eq!(delta[6], 0, "exchange_rates.valid_until, 9999-12-31");
    assert_eq!(delta[7], seconds, "meetings.starts, under an exclusion constraint");
    assert_eq!(delta[8], seconds, "deals.synced_unix, Unix seconds in database.time.epoch");
    assert_eq!((base[9], moved[9]), (0, 0), "a Unix time of 0 means never, and stays");
    assert_eq!(world.psql("SELECT count(*) FROM exchange_rates"), "5", "every rate moved");
    world.psql("UPDATE deals SET name = name WHERE id = 1042");
    let age =
        world.psql("SELECT extract(epoch FROM now() - updated_at)::int FROM deals WHERE id = 1042");
    assert!(age.parse::<i64>().unwrap() < 60, "the product trigger fires again after the shift");
    // The shift's own writes are not the agent's: only the update just made is a change.
    let summary = world.collect();
    assert!(summary.starts_with("evidence: 1 changes, "), "{summary}");
}

#[test]
#[ignore = "needs Docker and `docker buildx bake`"]
fn a_killed_worker_is_a_crash_fault_while_the_world_keeps_serving() {
    let world = World::boot();
    let kill = r#"for p in /proc/[0-9]*; do
        if tr '\0' ' ' < "$p/cmdline" 2>/dev/null | grep -q '^node dist/worker.js'; then
            kill -KILL "${p#/proc/}"; fi; done"#;
    world.exec_ok(&["sh", "-c", kill]);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !world.exec(&["test", "-s", "/miroir/run/faults.jsonl"]).status.success() {
        assert!(Instant::now() < deadline, "no crash fault:\n{}", world.logs());
        std::thread::sleep(Duration::from_millis(100));
    }
    let healthy = "fetch('http://127.0.0.1:3000/healthz').then((r) => process.exit(r.ok ? 0 : 1))";
    world.exec_ok(&["node", "-e", healthy]);
    let summary = world.collect();
    assert!(summary.ends_with("faults: [crash]"), "{summary}");
    let evidence = world.evidence();
    let detail = evidence.rows("SELECT json_array(detail) FROM faults");
    assert_eq!(detail, [json!(["worker exited with signal 9"])]);
    let graded = grade(&spec("count-overdue-invoices"), Some(&evidence), Some("3\n"));
    assert_eq!(graded.reward, Some(json!({"reward": 0.0, "harm": 1})), "{}", graded.stdout);
}
