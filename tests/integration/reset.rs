use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::common::{
    Container, ENV_IMAGE, MYSQL_ENV_IMAGE, Options, Session, bake, baked_at, docker, docker_ok,
    manifest_with_processes, retry,
};
use serde_json::json;

#[tokio::test]
#[ignore = "needs Docker"]
async fn reset_restores_data() {
    let env = Container::start(&Options::default());
    let mut session = Session::connect(&env).await;
    let observation =
        session.reset(json!({"task_id": "count-overdue-invoices", "episode_id": "ep-1"})).await;
    assert_eq!(observation["observation"]["task_id"], "count-overdue-invoices");
    assert_eq!(observation["reward"], json!(null));
    let state = session.state().await;
    assert_eq!(state["episode_id"], "ep-1");
    assert_eq!(state["step_count"], 0);

    let original =
        env.psql("SELECT name || ':' || (SELECT count(*) FROM notes) FROM deals WHERE id = 1");
    env.psql(
        "UPDATE deals SET name = 'changed' WHERE id = 1; \
         INSERT INTO notes (deal_id, author_id, text) VALUES (1, 1, 'x')",
    );
    env.exec_ok(&[
        "sh",
        "-c",
        "echo x > /app/uploads/junk; echo x > /tmp/junk; redis-cli set junk 1",
    ]);
    assert_ne!(
        env.psql("SELECT name || ':' || (SELECT count(*) FROM notes) FROM deals WHERE id = 1"),
        original
    );

    session.reset(json!({"task_id": "count-overdue-invoices"})).await;
    assert_eq!(
        env.psql("SELECT name || ':' || (SELECT count(*) FROM notes) FROM deals WHERE id = 1"),
        original
    );
    let leftovers = env.exec_ok(&["sh", "-c", "ls -A /app/uploads /tmp; redis-cli exists junk"]);
    assert_eq!(leftovers.lines().collect::<Vec<_>>(), ["/app/uploads:", "", "/tmp:", "0"]);
    let state = session.state().await;
    assert_ne!(state["episode_id"], "ep-1", "a reset without episode_id generates one");
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn reset_kills_postgres_backends() {
    let env = Container::start(&Options::default());
    let mut session = Session::connect(&env).await;
    session.reset(json!({"task_id": "count-overdue-invoices"})).await;
    docker_ok(&[
        "exec",
        "-d",
        &env.id,
        "psql",
        "-h",
        "127.0.0.1",
        "-U",
        "postgres",
        "crm",
        "-c",
        "SELECT pg_sleep(600)",
    ]);
    let backend = retry("the sleeping backend", || {
        let pid = env.psql("SELECT pid FROM pg_stat_activity WHERE query LIKE 'SELECT pg_sleep%'");
        (!pid.is_empty()).then_some(pid)
    });
    let sid = env.exec_ok(&["sh", "-c", &format!("cut -d' ' -f6 /proc/{backend}/stat")]);
    assert_eq!(sid, backend, "the backend leads its own session");

    let started = Instant::now();
    session.reset(json!({"task_id": "count-overdue-invoices"})).await;
    assert!(started.elapsed() < Duration::from_secs(30));
    let gone = env.exec(&["test", "-e", &format!("/proc/{backend}")]);
    assert!(!gone.status.success(), "backend {backend} survived the reset");
    let sleeping =
        env.psql("SELECT count(*) FROM pg_stat_activity WHERE query LIKE 'SELECT pg_sleep%'");
    assert_eq!(sleeping, "0");
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn reset_kills_a_process_whose_name_is_not_utf8() {
    let env = Container::start(&Options::default());
    let mut session = Session::connect(&env).await;
    session.reset(json!({"task_id": "count-overdue-invoices"})).await;
    let start = r#"name="$(printf '/var/tmp/\377\376sleep')"; cp /bin/sleep "$name"
        setsid "$name" 600 > /dev/null 2>&1 &
        echo $! > /var/tmp/pid"#;
    env.exec_ok(&["sh", "-c", start]);
    let pid = env.exec_ok(&["cat", "/var/tmp/pid"]);
    assert!(env.exec(&["test", "-e", &format!("/proc/{pid}")]).status.success(), "it started");
    session.reset(json!({"task_id": "count-overdue-invoices"})).await;
    let survived = env.exec(&["test", "-e", &format!("/proc/{pid}")]).status.success();
    assert!(!survived, "process {pid} survived the reset");
}

struct Shifted {
    baseline: Vec<i64>,
    moved: Vec<i64>,
    touched_now: bool,
}

const DATES: &str = "SELECT extract(epoch FROM d.created_at)::bigint, \
    extract(epoch FROM d.updated_at)::bigint, d.expected_close - date '2000-01-01', \
    c.birthday - date '2000-01-01', extract(epoch FROM a.occurred_at)::bigint, \
    (SELECT max(day) - date '2000-01-01' FROM exchange_rates), \
    (SELECT min(valid_until) - date '2000-01-01' FROM exchange_rates), \
    (SELECT extract(epoch FROM min(starts))::bigint FROM meetings), \
    d.synced_unix, (SELECT min(synced_unix) FROM deals) \
    FROM deals d, contacts c, activities a WHERE d.id = 1042 AND c.id = 101 AND a.deal_id = 1042 \
    ORDER BY a.occurred_at LIMIT 1";

fn shifted() -> &'static Shifted {
    static SHIFTED: OnceLock<Shifted> = OnceLock::new();
    SHIFTED.get_or_init(|| {
        let manifest = aged_manifest();
        let measure = |files: &[(&str, &str)]| {
            let env = Container::start(&Options { files, ..Options::default() });
            env.wait_for_log("env ready", Duration::from_secs(90));
            let row = env.psql(DATES);
            let values = row.split('|').map(|v| v.parse().unwrap()).collect();
            (env, values)
        };
        let ((_, baseline), (shifted_env, shifted)) = std::thread::scope(|scope| {
            let base = scope.spawn(|| measure(&[]));
            let moved = scope.spawn(|| measure(&[("/miroir/miroir.yaml", &manifest)]));
            (base.join().unwrap(), moved.join().unwrap())
        });
        shifted_env.psql("UPDATE deals SET name = name WHERE id = 1042");
        let age = shifted_env
            .psql("SELECT extract(epoch FROM now() - updated_at)::int FROM deals WHERE id = 1042");
        Shifted { baseline, moved: shifted, touched_now: age.parse::<i64>().unwrap() < 60 }
    })
}

#[test]
#[ignore = "needs Docker"]
fn shift_moves_dates_except_excluded() {
    let Shifted { baseline, moved: shifted, .. } = shifted();
    let ten_days = 10 * 86_400;
    assert_eq!(shifted[0] - baseline[0], ten_days, "deals.created_at");
    assert_eq!(shifted[2] - baseline[2], 10, "deals.expected_close, a date");
    assert_eq!(shifted[3], baseline[3], "contacts.birthday is in time.exclude");
    assert_eq!(shifted[4] - baseline[4], ten_days, "activities.occurred_at");
    assert_eq!(shifted[8] - baseline[8], ten_days, "deals.synced_unix, Unix seconds in time.epoch");
    assert_eq!((baseline[9], shifted[9]), (0, 0), "0 means never synced, and stays");
}

#[test]
#[ignore = "needs Docker"]
fn shift_moves_dates_past_constraints_and_keeps_never_dates() {
    let Shifted { baseline, moved: shifted, .. } = shifted();
    assert_eq!(shifted[5] - baseline[5], 10, "exchange_rates.day, in the primary key");
    assert_eq!(shifted[6], baseline[6], "exchange_rates.valid_until is 9999-12-31");
    assert_eq!(shifted[7] - baseline[7], 10 * 86_400, "meetings, in an exclusion constraint");
}

#[test]
#[ignore = "needs Docker"]
fn an_open_transaction_fails_a_reset_rather_than_hanging_it() {
    let sql = "BEGIN; UPDATE deals SET name = name WHERE id = 1; SELECT pg_sleep(3600)";
    let apps = format!(
        "  - {{name: holder, command: [psql, -h, 127.0.0.1, -U, postgres, -d, crm, -c, \"{sql}\"]}}"
    );
    let manifest = format!("{}quiet: {{command: [sleep, '2']}}\n", manifest_with_processes(&apps));
    let env = Container::start(&Options {
        files: &[("/miroir/miroir.yaml", &manifest)],
        ..Options::default()
    });
    env.wait_for_log("reset failed at step 'clear'", Duration::from_secs(60));
    assert!(env.logs().contains("make quiet wait for it"), "{}", env.logs());
}

#[test]
#[ignore = "needs Docker"]
fn hook_gets_the_shift_and_its_failure_stops_the_reset() {
    let manifest = |hook: &str| {
        std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/fixtures/acme-crm/miroir.yaml"
        ))
        .unwrap()
        .replace(
            "anchor: bake",
            &format!("anchor: \"2020-01-01T00:00:00Z\"\n  hook: [sh, -c, '{hook}']"),
        )
    };
    let fine = manifest(
        "echo $MIROIR_SHIFT_DAYS $MIROIR_SHIFT_DATE_DAYS > /var/tmp/shifted; sleep 1000 &",
    );
    let env = Container::start(&Options {
        files: &[("/miroir/miroir.yaml", &fine)],
        ..Options::default()
    });
    env.wait_for_log("env ready", Duration::from_secs(90));
    let shifted = env.exec_ok(&["cat", "/var/tmp/shifted"]);
    let days: Vec<i64> = shifted.split(' ').map(|d| d.parse().unwrap()).collect();
    assert!(days[0] > 2000, "{shifted} days since 2020");
    assert!(days[1] - days[0] <= 1, "dates move by calendar days: {shifted}");
    let failing = manifest("exit 3");
    let env = Container::start(&Options {
        files: &[("/miroir/miroir.yaml", &failing)],
        ..Options::default()
    });
    assert_eq!(env.wait_exit(), 1, "{}", env.logs());
    assert!(env.logs().contains("at step 'hook'"), "{}", env.logs());
}

#[test]
#[ignore = "needs Docker"]
fn dates_stay_the_same_days_from_today_late_in_the_day() {
    crate::common::build_images();
    let today = jiff::Timestamp::now().to_zoned(jiff::tz::TimeZone::UTC).date();
    let anchor = today.yesterday().unwrap().at(23, 59, 59, 0).in_tz("UTC").unwrap();
    let manifest = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/acme-crm/miroir.yaml"
    ))
    .unwrap()
    .replace("anchor: bake", &format!("anchor: \"{}\"", anchor.timestamp()));
    let env = Container::start(&Options {
        files: &[("/miroir/miroir.yaml", &manifest)],
        ..Options::default()
    });
    env.wait_for_log("env ready", Duration::from_secs(90));
    let baked = baked_at(ENV_IMAGE).to_zoned(jiff::tz::TimeZone::UTC).date();
    let newest = env.psql("SELECT max(day) FROM exchange_rates");
    assert_eq!(newest, baked.tomorrow().unwrap().to_string(), "a calendar day after the seed");
}

#[test]
#[ignore = "needs Docker"]
fn shift_skips_product_triggers() {
    let shifted = shifted();
    assert_eq!(shifted.moved[1] - shifted.baseline[1], 10 * 86_400, "deals.updated_at");
    assert!(shifted.touched_now, "the product trigger fires outside the shift");
}

const MYSQL_DATES: &str = "SELECT UNIX_TIMESTAMP(p.updated_at), UNIX_TIMESTAMP(o.updated_at), \
    UNIX_TIMESTAMP(o.placed_at), DATEDIFF(o.ship_by, '2000-01-01'), UNIX_TIMESTAMP(c.created_at), \
    (SELECT DATEDIFF(MAX(day), '2000-01-01') FROM price_days), \
    (SELECT DATEDIFF(MIN(valid_until), '2000-01-01') FROM price_days), \
    (SELECT UNIX_TIMESTAMP(MIN(stamp_until)) FROM price_days), \
    (SELECT MAX(stamped_unix) FROM price_days), (SELECT MIN(stamped_unix) FROM price_days) \
    FROM products p, orders o, customers c WHERE p.id = 4 AND o.id = 1007 AND c.id = 3";

fn mysql(env: &Container, sql: &str) -> String {
    env.exec_ok(&["mysql", "-h", "127.0.0.1", "-u", "root", "-N", "-B", "shop", "-e", sql])
}

#[test]
#[ignore = "needs Docker"]
fn mysql_shift_keeps_on_update_columns() {
    let anchor = baked_at(MYSQL_ENV_IMAGE) - jiff::SignedDuration::from_hours(240);
    let manifest = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/mini-shop/miroir.yaml"
    ))
    .unwrap()
    .replace("anchor: bake", &format!("anchor: \"{anchor}\""));
    let measure = |files: &[(&str, &str)]| {
        let env =
            Container::start(&Options { image: MYSQL_ENV_IMAGE, files, ..Options::default() });
        env.wait_for_log("env ready", Duration::from_secs(90));
        let values: Vec<i64> =
            mysql(&env, MYSQL_DATES).split('\t').map(|v| v.trim().parse().unwrap()).collect();
        (env, values)
    };
    let ((_, baseline), (moved_env, moved)) = std::thread::scope(|scope| {
        let base = scope.spawn(|| measure(&[]));
        let shifted = scope.spawn(|| measure(&[("/miroir/miroir.yaml", &manifest)]));
        (base.join().unwrap(), shifted.join().unwrap())
    });
    let ten_days = 10 * 86_400;
    assert_eq!(
        moved[0] - baseline[0],
        ten_days,
        "products.updated_at, TIMESTAMP ON UPDATE CURRENT_TIMESTAMP"
    );
    assert_eq!(
        moved[1] - baseline[1],
        ten_days,
        "orders.updated_at, TIMESTAMP ON UPDATE CURRENT_TIMESTAMP"
    );
    assert_eq!(moved[2] - baseline[2], ten_days, "orders.placed_at, DATETIME");
    assert_eq!(moved[3] - baseline[3], 10, "orders.ship_by, DATE");
    assert_eq!(moved[4] - baseline[4], ten_days, "customers.created_at, DATETIME");
    assert_eq!(moved[5] - baseline[5], 10, "price_days.day, the primary key");
    assert_eq!(moved[6], baseline[6], "price_days.valid_until is 9999-12-31");
    assert_eq!(moved[7], baseline[7], "price_days.stamp_until is the end of TIMESTAMP's range");
    assert_eq!(moved[8] - baseline[8], ten_days, "price_days.stamped_unix, INT UNSIGNED seconds");
    assert_eq!((baseline[9], moved[9]), (0, 0), "0 means never stamped, and stays");
    let age = mysql(
        &moved_env,
        "UPDATE products SET stock = stock + 1 WHERE id = 4; \
        SELECT TIMESTAMPDIFF(SECOND, updated_at, NOW()) FROM products WHERE id = 4",
    );
    assert!(age.parse::<i64>().unwrap().abs() < 60, "updated_at moved to now: {age}");
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn apps_start_after_setup() {
    let env = Container::start(&Options::default());
    let mut session = Session::connect(&env).await;
    session.reset(json!({"task_id": "reassign-stale-deals"})).await;
    assert_eq!(env.product_get("/users/12")["status"], "leaving");
    session.reset(json!({"task_id": "count-overdue-invoices"})).await;
    assert_eq!(env.product_get("/users/12")["status"], "active");
}

const FIXED_TIMES: &str = "SELECT UNIX_TIMESTAMP(o.placed_at), UNIX_TIMESTAMP(o.updated_at), \
    DATEDIFF(o.ship_by, '2000-01-01'), (SELECT DATEDIFF(MAX(day), '2000-01-01') FROM price_days), \
    (SELECT DATEDIFF(MIN(valid_until), '2000-01-01') FROM price_days), \
    (SELECT UNIX_TIMESTAMP(MIN(stamp_until)) FROM price_days), \
    (SELECT MAX(stamped_unix) FROM price_days), \
    (SELECT COUNT(*) FROM price_days WHERE CAST(retired_at AS CHAR) LIKE '0000%'), \
    (SELECT TIMESTAMPDIFF(SECOND, created_at, NOW()) FROM customers \
     WHERE email = 'seeded@shop.test') FROM orders o WHERE o.id = 1007";

#[tokio::test]
#[ignore = "needs Docker"]
async fn bake_moves_a_captured_world_from_its_capture() {
    const IMAGE: &str = "mini-shop-captured";
    let captured = jiff::Timestamp::now() - jiff::SignedDuration::from_hours(400 * 24);
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/mini-shop");
    let context = std::env::temp_dir().join(format!("miroir-captured-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&context);
    std::fs::create_dir_all(context.join("tasks")).unwrap();
    std::fs::copy(fixture.join("Dockerfile.miroir"), context.join("Dockerfile.miroir")).unwrap();
    for entry in std::fs::read_dir(fixture.join("tasks")).unwrap() {
        let path = entry.unwrap().path();
        std::fs::copy(&path, context.join("tasks").join(path.file_name().unwrap())).unwrap();
    }
    let manifest = std::fs::read_to_string(fixture.join("miroir.yaml")).unwrap();
    let (anchor, worlds) = ("time:\n  anchor: bake\n", "worlds: [north, south]\n");
    assert!(manifest.contains(anchor) && manifest.contains(worlds), "the manifest changed");
    let seed = "seed:\n  - [mysql, -h, 127.0.0.1, -u, root, shop, -e, \"INSERT INTO customers \
                (name, email) VALUES ('Seeded Late', 'seeded@shop.test')\"]\n";
    let manifest = manifest
        .replacen(anchor, &format!("{anchor}  captured: {{south: \"{captured}\"}}\n"), 1)
        .replacen(worlds, &format!("{worlds}{seed}"), 1);
    std::fs::write(context.join("miroir.yaml"), manifest).unwrap();
    let overrides = context.join("overrides.json");
    let target = json!({"target": {"mini-shop-env": {
        "context": context, "tags": [IMAGE], "output": ["type=docker"]}}});
    std::fs::write(&overrides, target.to_string()).unwrap();
    bake(&["-f", "docker-bake.hcl", "-f", overrides.to_str().unwrap(), "mini-shop-env"]);
    std::fs::remove_dir_all(&context).unwrap();

    let env = Container::start(&Options { image: IMAGE, ..Options::default() });
    let mut session = Session::connect(&env).await;
    let times = |env: &Container| -> Vec<i64> {
        mysql(env, FIXED_TIMES).split('\t').map(|v| v.trim().parse().unwrap()).collect()
    };
    session.reset(json!({"task_id": "count-pending-orders"})).await;
    let north = times(&env);
    session.reset(json!({"task_id": "south-pending-orders"})).await;
    let south = times(&env);
    let baked = baked_at(IMAGE).as_second();
    drop(session);
    drop(env);
    docker(&["rmi", "-f", IMAGE]);

    let day = 86_400;
    let near = |seconds: i64, days: i64| (seconds - days * day).abs() < 600;
    assert!(near(south[0] - north[0], 400), "placed_at, DATETIME: {south:?} vs {north:?}");
    assert!(near(south[1] - north[1], 400), "updated_at, TIMESTAMP: {south:?} vs {north:?}");
    assert!([400, 401].contains(&(south[2] - north[2])), "ship_by, DATE, by calendar days");
    assert!([400, 401].contains(&(south[3] - north[3])), "price_days.day, a DATE key");
    assert_eq!(south[4], north[4], "9999-12-31 stays");
    assert_eq!(south[5], north[5], "the end of TIMESTAMP's range stays");
    assert!([400, 401].contains(&((south[6] - north[6]) / day)), "stamped_unix, seconds");
    assert_eq!((north[7], south[7]), (1, 1), "the zero date stays zero");
    assert!((0..3600).contains(&south[8]), "the seed wrote after the move: {}s old", south[8]);
    assert!((north[0] - (baked - 12 * day)).abs() < 3600, "north, uncaptured, stays as seeded");
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn each_world_resets_to_its_own_data() {
    let env = Container::start(&Options { image: MYSQL_ENV_IMAGE, ..Options::default() });
    let mut session = Session::connect(&env).await;
    let stock = || mysql(&env, "SELECT stock FROM products WHERE id = 1");
    session.reset(json!({"task_id": "south-pending-orders"})).await;
    assert_eq!(stock(), "39", "south's own draw");
    session.reset(json!({"task_id": "count-pending-orders"})).await;
    assert_eq!(stock(), "59", "north's, after a switch");
    session.reset(json!({"task_id": "south-pending-orders"})).await;
    assert_eq!(stock(), "39", "south's again, with nothing of north's left");
    let submit = json!({"type": "call_tool", "tool_name": "submit", "arguments": {"answer": "10"}});
    let done = session.step(submit).await;
    assert_eq!(done["data"]["reward"], 1.0, "{done}");
    assert_eq!(done["data"]["metadata"]["world"], "south", "{done}");
}

fn aged_manifest() -> String {
    let anchor = baked_at(ENV_IMAGE) - jiff::SignedDuration::from_hours(240);
    std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/acme-crm/miroir.yaml"))
        .unwrap()
        .replace("anchor: bake", &format!("anchor: \"{anchor}\""))
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn an_aged_env_shifts_once_and_every_reset_still_restores() {
    let dated = "SELECT id, created_at::date, text FROM notes ORDER BY id LIMIT 3";
    let fresh = Container::start(&Options::default());
    let mut session = Session::connect(&fresh).await;
    session.reset(json!({"task_id": "count-overdue-invoices"})).await;
    let baseline =
        fresh.psql("SELECT id, created_at::date + 10, text FROM notes ORDER BY id LIMIT 3");
    drop(session);
    drop(fresh);

    let manifest = aged_manifest();
    let env = Container::start(&Options {
        files: &[("/miroir/miroir.yaml", &manifest)],
        ..Options::default()
    });
    env.wait_healthy();
    let mut session = Session::connect(&env).await;
    assert_eq!(env.psql(dated), baseline, "boot shifted the dates");
    for round in 0..3 {
        let reset = session.reset(json!({"task_id": "count-overdue-invoices"})).await;
        let seconds = &reset["observation"]["metadata"]["reset_s"];
        assert!(seconds.get("save").is_none(), "only the first restore saves: {seconds}");
        assert_eq!(env.psql(dated), baseline, "round {round}");
        env.psql("UPDATE notes SET text = 'episode', created_at = now() WHERE id <= 3");
        env.psql("DELETE FROM activities");
    }
    let last = session.reset(json!({"task_id": "count-overdue-invoices"})).await;
    assert_eq!(env.psql(dated), baseline, "the episode's writes are gone");
    assert_ne!(env.psql("SELECT count(*) FROM activities"), "0", "and so is the delete");
    let golden = session
        .step(json!({"type": "call_tool", "tool_name": "submit", "arguments": {"answer": "3"}}))
        .await;
    assert_eq!(golden["data"]["done"], true, "{golden}");
    let spent = last["observation"]["metadata"]["reset_s"]["shift"].as_f64().unwrap();
    assert!(spent < 0.05, "the rows were not rewritten: {spent}");
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_write_outside_state_ends_the_env() {
    let env = Container::start(&Options::default());
    env.wait_for_log("env ready", Duration::from_secs(90));
    let mut session = Session::connect(&env).await;
    env.exec_ok(&["touch", "/app/left-behind"]);
    let reset = json!({"type": "reset", "data": {"task_id": "count-overdue-invoices"}});
    let mut replies = Vec::new();
    for _ in 0..10 {
        replies.push(session.send(reset.clone()).await);
    }
    let last = replies.pop().unwrap();
    assert!(replies.iter().all(|reply| reply["type"] == "observation"), "{replies:?}");
    assert_eq!(last["type"], "error", "the tenth reset scans: {last}");
    let message = last["data"]["message"].as_str().unwrap();
    assert!(message.contains("/app/left-behind"), "{message}");
    assert_eq!(crate::common::get(&env.url("/health")).0, 503);
    let refused = session.send(reset.clone()).await;
    assert!(refused["data"]["message"].as_str().unwrap().contains("replace this container"));
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn writes_to_the_temp_directories_are_not_leaks() {
    let env = Container::start(&Options::default());
    env.wait_for_log("env ready", Duration::from_secs(90));
    let mut session = Session::connect(&env).await;
    env.exec_ok(&["touch", "/tmp/scratch", "/var/tmp/scratch", "/dev/shm/scratch"]);
    for _ in 0..10 {
        session.reset(json!({"task_id": "count-overdue-invoices"})).await;
    }
    let left = env.exec_ok(&["ls", "-A", "/tmp", "/var/tmp", "/dev/shm"]);
    assert!(!left.contains("scratch"), "every reset empties them: {left}");
    assert_eq!(crate::common::get(&env.url("/health")).0, 200, "and the tenth one's scan passed");
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn reset_reports_what_each_step_took() {
    let env = Container::start(&Options::default());
    let mut session = Session::connect(&env).await;
    let reset = session.reset(json!({"task_id": "reassign-stale-deals"})).await;
    let seconds = reset["observation"]["metadata"]["reset_s"].as_object().unwrap();
    for step in ["kill", "restore", "datastores", "shift", "setup", "apps", "quiet", "clear"] {
        let spent = seconds[step].as_f64().unwrap();
        assert!(spent >= 0.0 && spent <= seconds["total"].as_f64().unwrap(), "{step}: {seconds:?}");
    }
    assert!(!seconds.contains_key("hook"), "this manifest has no hook: {seconds:?}");
    assert!(env.logs().contains("reset done"));
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn reset_p50_under_5s() {
    let env = Container::start(&Options::default());
    let mut session = Session::connect(&env).await;
    let mut times = Vec::new();
    for _ in 0..11 {
        let started = Instant::now();
        session.reset(json!({"task_id": "reassign-stale-deals"})).await;
        times.push(started.elapsed());
    }
    times.sort();
    let (p50, p95) = (times[times.len() / 2], times[times.len() * 95 / 100]);
    eprintln!("reset p50 {p50:?}, p95 {p95:?}");
    assert!(p50 < Duration::from_secs(5), "reset p50 is {p50:?}");
}
