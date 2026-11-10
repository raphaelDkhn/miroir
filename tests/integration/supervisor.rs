use std::time::{Duration, Instant};

use crate::common::{
    Container, ENV_IMAGE, MYSQL_ENV_IMAGE, Options, Session, bake, bake_outcome, build_images,
    docker_ok, manifest_with_processes,
};
use serde_json::json;

#[test]
#[ignore = "needs Docker"]
fn processes_start_in_order() {
    let apps = r#"  - name: first
    command:
      - sh
      - -c
      - sleep 1; echo first >> /tmp/order; exec node -e "require('net').createServer().listen(7001)"
    ready: {tcp: 7001}
  - {name: second, command: [sh, -c, 'echo second >> /tmp/order; exec sleep 1000']}"#;
    let manifest = manifest_with_processes(apps);
    let env = Container::start(&Options {
        files: &[("/miroir/miroir.yaml", &manifest)],
        ..Options::default()
    });
    env.wait_for_log("env ready", Duration::from_secs(60));
    assert_eq!(env.exec_ok(&["cat", "/tmp/order"]), "first\nsecond");
    let logs = env.logs();
    let started = |name: &str| logs.find(&format!("process={name} pid=")).unwrap();
    let ready = |name: &str| logs.find(&format!("ready process={name}")).unwrap();
    assert!(ready("db") < started("first"), "{logs}");
    assert!(ready("first") < started("second"), "{logs}");
}

#[test]
#[ignore = "needs Docker"]
fn ready_timeout_names_process() {
    let apps = "  - {name: web, command: [sleep, '1000'], ready: {tcp: 7999, timeout_s: 2}}";
    let manifest = manifest_with_processes(apps);
    let env = Container::start(&Options {
        files: &[("/miroir/miroir.yaml", &manifest)],
        ..Options::default()
    });
    assert_eq!(env.wait_exit(), 1, "{}", env.logs());
    let logs = env.logs();
    assert!(logs.contains("process 'web' was not ready after 2 s"), "{logs}");
}

#[test]
#[ignore = "needs Docker"]
fn refuses_to_run_under_a_host_init() {
    build_images();
    let script = "cp /bin/sh /tmp/init; exec /tmp/init -c 'miroir check; echo exit=$?'";
    let out =
        crate::common::docker(&["run", "--rm", "--entrypoint", "sh", ENV_IMAGE, "-c", script]);
    let (stdout, stderr) =
        (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert_eq!(stdout.trim(), "exit=1", "{stderr}");
    assert!(stderr.contains("must run as the container's entrypoint"), "{stderr}");
}

#[test]
#[ignore = "needs Docker"]
fn bake_moves_state() {
    build_images();
    let script = r#"
        for d in /var/lib/postgresql/data /var/lib/redis /app/uploads; do
          echo "$d $(ls -A "$d" | wc -l) $(stat -c '%U %a' "$d")"
        done
        test -f /miroir/pristine/default/var/lib/postgresql/data/PG_VERSION && echo pg
        test -f /miroir/pristine/default/var/lib/redis/dump.rdb && echo rdb
        cat /miroir/bake.json"#;
    let outsider = "test ! -r /miroir/miroir.yaml && test ! -r /miroir/tasks \
        && test ! -w /miroir/pristine/default/var/lib/postgresql/data";
    let closed = crate::common::docker(&[
        "run",
        "--rm",
        "--entrypoint",
        "su",
        ENV_IMAGE,
        "postgres",
        "-s",
        "/bin/sh",
        "-c",
        outsider,
    ]);
    assert!(closed.status.success(), "the product's users can reach the grader's files");
    let out = docker_ok(&["run", "--rm", "--entrypoint", "sh", ENV_IMAGE, "-c", script]);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "/var/lib/postgresql/data 0 postgres 700", "{out}");
    assert_eq!(lines[1], "/var/lib/redis 0 redis 750", "{out}");
    assert_eq!(lines[2], "/app/uploads 0 node 755", "{out}");
    assert_eq!(&lines[3..5], ["pg", "rdb"], "{out}");
    let json = lines[5..].join("\n");
    let info: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert!(info["baked_at"].as_str().unwrap().ends_with('Z'), "{info}");
    assert_eq!(info["miroir_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(info["manifest_hash"].as_str().unwrap().len(), 16);
    let listed = json!(["/var/lib/postgresql/data", "/var/lib/redis", "/app/uploads"]);
    assert_eq!(info["state"], listed, "bake.json keeps the state the manifest lists");
}

#[test]
#[ignore = "needs Docker"]
fn seed_commands_run_with_the_apps_up() {
    let env = Container::start(&Options::default());
    env.wait_healthy();
    let note = env.psql(
        "SELECT u.email || ': ' || n.text FROM notes n JOIN users u ON u.id = n.author_id \
         WHERE n.deal_id = 1100 AND n.text = 'Seeded through the API'",
    );
    assert_eq!(note, "dana@acme.test: Seeded through the API", "posted as Dana, and baked");
}

const DERIVED: [&str; 6] = [
    "/run/mysqld",
    "/var/lib/mysql",
    "/var/lib/mysql-files",
    "/var/lib/mysql-keyring",
    "/var/lib/shopdb",
    "/var/log/mysqld.log",
];

#[tokio::test]
#[ignore = "needs Docker"]
async fn derived_state_is_what_the_processes_can_write() {
    build_images();
    let info =
        docker_ok(&["run", "--rm", "--entrypoint", "cat", MYSQL_ENV_IMAGE, "/miroir/bake.json"]);
    let info: serde_json::Value = serde_json::from_str(&info).unwrap();
    assert_eq!(info["state"], json!(DERIVED), "what mysql and nobody can write");
    let env = Container::start(&Options { image: MYSQL_ENV_IMAGE, ..Options::default() });
    let mut session = Session::connect(&env).await;
    let leftovers = ["/var/lib/mysql/left-behind", "/run/mysqld/left-behind"];
    for _ in 0..10 {
        session.reset(json!({"task_id": "count-pending-orders"})).await;
        docker_ok(&[&["exec", "-u", "nobody", &env.id, "touch"][..], &leftovers[..]].concat());
    }
    session.reset(json!({"task_id": "count-pending-orders"})).await;
    for path in leftovers {
        assert!(!env.exec(&["test", "-e", path]).status.success(), "{path} survived a reset");
    }
    env.wait_healthy();
}

#[test]
#[ignore = "needs Docker"]
fn mysql_cascade_warning() {
    build_images();
    let overrides =
        std::env::temp_dir().join(format!("miroir-cascade-{}.json", std::process::id()));
    let target = r#"{"target": {"mini-shop-env": {
        "no-cache-filter": ["env"], "output": ["type=cacheonly"]}}}"#;
    std::fs::write(&overrides, target).unwrap();
    let output =
        bake(&["-f", "docker-bake.hcl", "-f", overrides.to_str().unwrap(), "mini-shop-env"]);
    std::fs::remove_file(&overrides).unwrap();
    let warning = "foreign key items_order on order_items references orders with ON DELETE CASCADE";
    assert!(output.contains(warning), "no cascade warning in the bake output:\n{output}");
    assert!(output.contains("MySQL fires no triggers for cascaded changes"), "{output}");
    assert_eq!(
        output.matches("WARN foreign key").count(),
        2,
        "only items_order cascades, warned once for each of the two worlds:\n{output}"
    );
    assert!(output.contains("trigger orders_keep_total on orders also fires"), "{output}");
}

#[test]
#[ignore = "needs Docker"]
fn a_world_that_makes_new_state_fails_the_bake() {
    build_images();
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/mini-shop");
    let context = std::env::temp_dir().join(format!("miroir-new-state-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&context);
    std::fs::create_dir_all(context.join("tasks")).unwrap();
    std::fs::copy(fixture.join("Dockerfile.miroir"), context.join("Dockerfile.miroir")).unwrap();
    for entry in std::fs::read_dir(fixture.join("tasks")).unwrap() {
        let path = entry.unwrap().path();
        std::fs::copy(&path, context.join("tasks").join(path.file_name().unwrap())).unwrap();
    }
    let seed = "  - [node, dist/seed.js]\n";
    let south_only = "  - [sh, -c, 'if [ \"$MIROIR_WORLD\" = south ]; then \
                      install -d -o nobody /srv/extra; fi']\n";
    let manifest = std::fs::read_to_string(fixture.join("miroir.yaml")).unwrap();
    assert!(manifest.contains(seed), "the fixture's manifest changed");
    std::fs::write(
        context.join("miroir.yaml"),
        manifest.replacen(seed, &format!("{seed}{south_only}"), 1),
    )
    .unwrap();
    let overrides = context.join("overrides.json");
    let target =
        json!({"target": {"mini-shop-env": {"context": context, "output": ["type=cacheonly"]}}});
    std::fs::write(&overrides, target.to_string()).unwrap();
    let file = overrides.to_str().unwrap();
    let (built, output) = bake_outcome(&["-f", "docker-bake.hcl", "-f", file, "mini-shop-env"]);
    std::fs::remove_dir_all(&context).unwrap();
    assert!(!built, "a world that makes new state baked:\n{output}");
    let refused = "world 'south': state: baking world 'south' changed what the processes can \
                   write (new: /srv/extra; gone: none)";
    assert!(output.contains(refused), "{output}");
}

#[test]
#[ignore = "needs Docker"]
fn sigterm_stops_everything() {
    let env = Container::start(&Options::default());
    env.wait_for_log("env ready", Duration::from_secs(60));
    assert_eq!(crate::common::get(&env.url("/health")).0, 200);
    let running = env.exec_ok(&["sh", "-c", "ls -d /proc/[0-9]* | wc -l"]);
    assert!(running.parse::<u32>().unwrap() > 4, "only {running} processes");

    let started = Instant::now();
    docker_ok(&["stop", "-t", "25", &env.id]);
    assert!(started.elapsed() < Duration::from_secs(20), "SIGTERM was not handled in time");
    assert_eq!(env.wait_exit(), 0, "{}", env.logs());
    let logs = env.logs();
    assert!(logs.contains("database system is shut down"), "{logs}");
    assert!(logs.contains("stopped"), "{logs}");
}

#[test]
#[ignore = "needs Docker"]
fn boot_fails_when_a_process_exits() {
    let manifest = manifest_with_processes("  - {name: flaky, command: [sh, -c, 'exit 3']}");
    let env = Container::start(&Options {
        files: &[("/miroir/miroir.yaml", &manifest)],
        ..Options::default()
    });
    assert_eq!(env.wait_exit(), 1, "{}", env.logs());
    let logs = env.logs();
    let expected = "reset failed at step 'processes': process 'flaky' exited with code 3";
    assert!(logs.contains(expected), "{logs}");
    assert!(!logs.contains("env ready"), "{logs}");
}
