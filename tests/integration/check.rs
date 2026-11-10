use serde_json::{Value, json};

use crate::common::{ENV_IMAGE, MYSQL_ENV_IMAGE, run_check, run_check_with};

fn passed(report: &Value, stage: &str) -> bool {
    match stage {
        "static" | "idle" => report["stages"][stage]["passed"] == true,
        _ => report["tasks"][0][stage]["passed"] == true,
    }
}

fn verdicts(report: &Value) -> Vec<(&'static str, bool)> {
    ["idle", "golden", "null", "repeat", "reset", "writes"]
        .into_iter()
        .map(|s| (s, passed(report, s)))
        .collect()
}

fn expect_only(report: &Value, logs: &str, failing: &[&str]) {
    for (stage, ok) in verdicts(report) {
        assert_eq!(ok, !failing.contains(&stage), "stage {stage} in {report:#}\n{logs}");
    }
    assert_eq!(report["passed"], false);
}

fn acme_manifest() -> String {
    std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/acme-crm/miroir.yaml"))
        .unwrap()
}

const READ_TASK: &str = r"
instruction: How many of Northwind Traders' open invoices are past due?
checks:
  - {name: answer, weight: 1.0, db: episode, sql: SELECT lower(trim(value)) = '3' FROM answer}
allowed_changes: []
golden:
  - {tool: api_request, arguments: {method: GET, path: /invoices?contact_id=101&overdue=true}}
  - {tool: submit, arguments: {answer: '3'}}
";

#[test]
#[ignore = "needs Docker"]
fn check_passes_fixture() {
    let (code, report, logs) = run_check(ENV_IMAGE, &[], &[]);
    assert_eq!(code, 0, "{report:#}\n{logs}");
    assert_eq!(report["passed"], true);
    let tasks = report["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 8);
    for task in tasks {
        for stage in ["golden", "null", "repeat", "reset", "writes", "drift"] {
            assert_eq!(task[stage]["passed"], true, "{}: {stage}", task["id"]);
        }
    }
    let reassign = tasks.iter().find(|t| t["id"] == "reassign-stale-deals").unwrap();
    let statuses: Vec<String> = reassign["golden"]["statuses"]
        .as_array()
        .unwrap()
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(statuses, ["200", "201", "200", "201", "200", "201", "\"ok\""]);
    assert_eq!(report["stages"]["mcp"], json!({"passed": true, "tools": 3}));
    let close = tasks.iter().find(|t| t["id"] == "close-won-deal").unwrap();
    let rewards: Vec<(&str, f64)> = close["probes"]["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|run| (run["name"].as_str().unwrap(), run["reward"].as_f64().unwrap()))
        .filter(|(name, _)| name.contains("mcp"))
        .collect();
    assert_eq!(
        rewards,
        [("through-the-crm-mcp-server", 1.0), ("closes-another-deal-through-mcp", 0.0)]
    );
    eprintln!("timings: {}", report["timings"]);
    assert!(report["timings"]["reset_p50_s"].as_f64().unwrap() < 10.0);
}

#[test]
#[ignore = "needs Docker"]
fn mysql_check_passes_fixture() {
    let (code, report, logs) = run_check(MYSQL_ENV_IMAGE, &[], &[]);
    assert_eq!(code, 0, "{report:#}\n{logs}");
    let tasks = report["tasks"].as_array().unwrap();
    let ids: Vec<&str> = tasks.iter().map(|t| t["id"].as_str().unwrap()).collect();
    let north = ["cancel-stale-order", "count-pending-orders", "restock-sold-out"];
    assert_eq!(ids, [&north[..], &["south-pending-orders"]].concat(), "by world, then id");
    for task in tasks {
        for stage in ["golden", "null", "repeat", "reset", "writes"] {
            assert_eq!(task[stage]["passed"], true, "{}: {stage}", task["id"]);
        }
    }
    assert_eq!(report["stages"]["idle"]["passed"], true, "both worlds idle quietly: {report:#}");
}

#[test]
#[ignore = "needs Docker"]
fn check_takes_one_world_at_a_time() {
    let (code, report, logs) = run_check_with(MYSQL_ENV_IMAGE, &[], &[], "--world south");
    assert_eq!(code, 0, "{report:#}\n{logs}");
    let ids: Vec<&str> =
        report["tasks"].as_array().unwrap().iter().map(|t| t["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["south-pending-orders"]);
    let probe = &report["tasks"][0]["probes"]["runs"][0];
    assert_eq!((&probe["name"], &probe["passed"]), (&json!("north-count"), &json!(true)));
    let (code, report, logs) = run_check_with(MYSQL_ENV_IMAGE, &[], &[], "--world east");
    assert_ne!(code, 0, "{report:#}");
    assert!(logs.contains("unknown world 'east'; the manifest has north, south"), "{logs}");
}

#[test]
#[ignore = "needs Docker"]
fn check_fails_wrong_golden() {
    let task = r"
instruction: Log a call on deals 1200 and 1201.
checks:
  - name: logged
    weight: 1.0
    db: episode
    sql: SELECT count(*) / 2.0 FROM changes WHERE tbl = 'activities' AND op = 'insert'
any_change: true
golden:
  - tool: api_request
    arguments: {method: POST, path: /deals/1200/activities, body: {kind: call, summary: Call}}
  - {tool: submit, arguments: {}}
";
    let (code, report, logs) = run_check(ENV_IMAGE, &[], &[("wrong-golden", task)]);
    assert_eq!(code, 1);
    expect_only(&report, &logs, &["golden"]);
    assert_eq!(report["tasks"][0]["golden"]["reward"], 0.5);
}

#[test]
#[ignore = "needs Docker"]
fn check_says_why_a_golden_never_ended() {
    let task = READ_TASK.replace("{answer: '3'}", "{answer: Northwind owes $1,200}");
    let (code, report, logs) = run_check(ENV_IMAGE, &[], &[("unquoted", &task)]);
    assert_eq!(code, 1);
    let error = report["tasks"][0]["error"].as_str().unwrap_or_default();
    assert!(error.contains("never ended the episode"), "{report:#}\n{logs}");
    assert!(error.contains("unexpected argument '200'"), "{error}");
}

#[test]
#[ignore = "needs Docker"]
fn check_fails_an_image_that_only_works_on_bake_day() {
    let manifest = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/acme-crm/miroir.yaml"
    ))
    .unwrap()
    .replace("anchor: bake", "anchor: bake\n  hook: [sh, -c, 'test \"$MIROIR_SHIFT_DAYS\" -le 0']");
    let files = [("/miroir/miroir.yaml", manifest.as_str())];
    let (code, report, logs) = run_check(ENV_IMAGE, &files, &[("read", READ_TASK)]);
    assert_eq!(code, 1, "{report:#}\n{logs}");
    let error = report["stages"]["idle"]["error"].as_str().unwrap_or_default();
    assert!(error.contains("a reset a day later would fail"), "{report:#}");
    assert_eq!(report["tasks"][0]["passed"], true, "only the later reset fails: {report:#}");
}

#[test]
#[ignore = "needs Docker"]
fn check_fails_check_passing_null_run() {
    let task = READ_TASK.replace(
        "sql: SELECT lower(trim(value)) = '3' FROM answer",
        "sql: SELECT count(*) = 0 FROM changes",
    );
    let (code, report, logs) = run_check(ENV_IMAGE, &[], &[("passes-on-null", &task)]);
    assert_eq!(code, 1);
    expect_only(&report, &logs, &["null"]);
    assert_eq!(report["tasks"][0]["null"]["reward"], 1.0);
}

#[test]
#[ignore = "needs Docker"]
fn check_fails_nondeterministic_check() {
    let task = r"
instruction: How many of Northwind Traders' open invoices are past due?
checks:
  - name: answer
    weight: 1.0
    command: [sh, -c, 'test ! -e /run/miroir-seen && touch /run/miroir-seen']
allowed_changes: []
golden:
  - {tool: api_request, arguments: {method: GET, path: /invoices?contact_id=101&overdue=true}}
  - {tool: submit, arguments: {answer: '3'}}
";
    let (code, report, logs) = run_check(ENV_IMAGE, &[], &[("nondeterministic", task)]);
    assert_eq!(code, 1);
    expect_only(&report, &logs, &["repeat"]);
}

#[test]
#[ignore = "needs Docker"]
fn check_fails_writes_outside_state() {
    let stray =
        "  - {name: stray, command: [sh, -c, 'date +%s%N > /app/stray.txt; exec sleep 100000']}";
    let manifest =
        acme_manifest().replace("  - {name: worker,", &format!("{stray}\n  - {{name: worker,"));
    let (code, report, logs) =
        run_check(ENV_IMAGE, &[("/miroir/miroir.yaml", &manifest)], &[("read", READ_TASK)]);
    assert_eq!(code, 1);
    expect_only(&report, &logs, &["writes"]);
    let changed = report["tasks"][0]["writes"]["changed"].as_array().unwrap();
    assert!(changed.iter().any(|p| p == "/app/stray.txt"), "{changed:?}");
    let root = &report["stages"]["users"]["root"];
    assert_eq!(*root, serde_json::json!(["stray"]), "it has no user, so it runs as root");
}

#[test]
#[ignore = "needs Docker"]
fn check_fails_an_answer_check_that_a_hedge_passes() {
    let task = READ_TASK.replace(
        "sql: SELECT lower(trim(value)) = '3' FROM answer",
        "sql: SELECT value LIKE '%3%' FROM answer",
    );
    let (code, report, logs) = run_check(ENV_IMAGE, &[], &[("contains", &task)]);
    assert_eq!(code, 1, "{report:#}\n{logs}");
    expect_only(&report, &logs, &[]);
    let hedge = &report["tasks"][0]["hedge"];
    assert_eq!((hedge["passed"].as_bool(), hedge["reward"].as_f64()), (Some(false), Some(1.0)));
}

#[test]
#[ignore = "needs Docker"]
fn check_passes_an_integer_answer_and_skips_its_hedge() {
    let task = READ_TASK
        .replace("instruction: How many", "answer: integer\ninstruction: How many")
        .replace("lower(trim(value)) = '3'", "value = '3'")
        .replace("{answer: '3'}", "{answer: ' 03 '}");
    let (code, report, logs) = run_check(ENV_IMAGE, &[], &[("integer", &task)]);
    assert_eq!(code, 0, "{report:#}\n{logs}");
    let integer = &report["tasks"][0];
    assert_eq!(integer["golden"]["metadata"]["answer"], "3", "{integer:#}");
    assert!(integer.get("hedge").is_none(), "a whole number cannot hedge: {integer:#}");
}

#[test]
#[ignore = "needs Docker"]
fn check_moves_a_golden_date_answer_with_the_data() {
    let manifest = acme_manifest().replace("anchor: bake", "anchor: \"2026-01-01T00:00:00Z\"");
    let task = r"
instruction: When does Acme's first quarter end? Submit the date.
answer: date
checks:
  - {name: answer, weight: 1.0, db: episode, sql: SELECT value = '2026-03-31' FROM answer}
allowed_changes: []
golden:
  - {tool: submit, arguments: {answer: '2026-03-31'}}
";
    let files = [("/miroir/miroir.yaml", manifest.as_str())];
    let (code, report, logs) = run_check(ENV_IMAGE, &files, &[("quarter", task)]);
    assert_eq!(code, 0, "{report:#}\n{logs}");
    assert!(report["shift_days"].as_i64().unwrap() > 0, "the data moved: {report:#}");
    let golden = &report["tasks"][0]["golden"];
    assert_eq!(golden["metadata"]["answer"], "2026-03-31", "as the task file has it: {golden:#}");
}

#[test]
#[ignore = "needs Docker"]
fn check_fails_scheduler_left_on() {
    let manifest = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/acme-crm/miroir.yaml"
    ))
    .unwrap()
    .replace(r#"SCHEDULER_ENABLED: "false""#, r#"SCHEDULER_ENABLED: "true""#);
    let (code, report, logs) =
        run_check(ENV_IMAGE, &[("/miroir/miroir.yaml", &manifest)], &[("read", READ_TASK)]);
    assert_eq!(code, 1);
    assert!(!passed(&report, "idle"), "{report:#}\n{logs}");
    let idle = &report["stages"]["idle"];
    assert!(idle["effects"].as_i64().unwrap() > 0, "{idle}");
    assert!(!idle["changes"].as_array().unwrap().is_empty(), "{idle}");
}

#[test]
#[ignore = "needs Docker"]
fn check_fails_a_golden_whose_call_errors() {
    let task = READ_TASK.replace("/invoices?contact_id=101&overdue=true", "/no-such-route");
    let (code, report, logs) = run_check(ENV_IMAGE, &[], &[("bad-call", &task)]);
    assert_eq!(code, 1, "{report:#}\n{logs}");
    expect_only(&report, &logs, &["golden"]);
    let golden = &report["tasks"][0]["golden"];
    assert_eq!(golden["reward"], 1.0, "the checks pass, and the call still failed: {golden}");
    assert_eq!(golden["statuses"][0], 404, "{golden}");
}

#[test]
#[ignore = "needs Docker"]
fn check_fails_a_gate_that_fails_on_an_empty_run() {
    let task = READ_TASK.replace(
        "allowed_changes: []",
        "  - {name: said_something, gate: true, db: episode, sql: SELECT value <> '' FROM answer}\n\
         allowed_changes: []",
    );
    let (code, report, logs) = run_check(ENV_IMAGE, &[], &[("positive-gate", &task)]);
    assert_eq!(code, 1, "{report:#}\n{logs}");
    expect_only(&report, &logs, &["null"]);
    assert_eq!(report["tasks"][0]["null"]["reward"], 0.0, "the reward alone would pass");
}

#[test]
#[ignore = "needs Docker"]
fn check_holds_probes_to_their_bounds() {
    let probes = r"
probes:
  - {name: wrong-answer, steps: [{tool: submit, arguments: {answer: '4'}}], max_reward: 0}
  - {name: no-lookup, steps: [{tool: submit, arguments: {answer: '3'}}], min_reward: 1}
";
    let (code, report, logs) =
        run_check(ENV_IMAGE, &[], &[("probed", &format!("{READ_TASK}{probes}"))]);
    assert_eq!(code, 0, "{report:#}\n{logs}");
    let runs = report["tasks"][0]["probes"]["runs"].as_array().unwrap();
    assert_eq!((runs[0]["reward"].as_f64(), runs[1]["reward"].as_f64()), (Some(0.0), Some(1.0)));

    let wrong = r"
probes:
  - {name: earns-too-much, steps: [{tool: submit, arguments: {answer: '3'}}], max_reward: 0.5}
  - {name: earns-too-little, steps: [{tool: submit, arguments: {answer: '4'}}], min_reward: 1}
";
    let (code, report, logs) =
        run_check(ENV_IMAGE, &[], &[("probed", &format!("{READ_TASK}{wrong}"))]);
    assert_eq!(code, 1, "{report:#}\n{logs}");
    expect_only(&report, &logs, &[]);
    assert_eq!(report["tasks"][0]["probes"]["passed"], false, "{report:#}");
    let runs = report["tasks"][0]["probes"]["runs"].as_array().unwrap();
    assert!(runs.iter().all(|run| run["passed"] == false), "{runs:#?}");
}

#[test]
#[ignore = "needs Docker"]
fn check_mutate_reports_what_each_call_is_worth() {
    let task = r"
instruction: Log a call on deals 1200 and 1201.
checks:
  - name: logged
    weight: 1.0
    db: episode
    sql: SELECT count(*) / 2.0 FROM changes WHERE tbl = 'activities' AND op = 'insert'
any_change: true
golden:
  - {tool: api_request, arguments: {method: GET, path: /deals/1200}}
  - tool: api_request
    arguments: {method: POST, path: /deals/1200/activities, body: {kind: call, summary: Call}}
  - tool: api_request
    arguments: {method: POST, path: /deals/1201/activities, body: {kind: call, summary: Call}}
  - {tool: submit, arguments: {}}
";
    let (code, report, logs) = run_check_with(ENV_IMAGE, &[], &[("two-calls", task)], "--mutate");
    assert_eq!(code, 0, "mutations report and do not judge: {report:#}\n{logs}");
    let runs = report["tasks"][0]["mutations"].as_array().unwrap();
    let summary: Vec<(i64, &str, f64, bool)> = runs
        .iter()
        .map(|run| {
            (
                run["dropped"].as_i64().unwrap(),
                run["call"].as_str().unwrap(),
                run["reward"].as_f64().unwrap(),
                run["success"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            (1, "GET /deals/1200", 1.0, true),
            (2, "POST /deals/1200/activities", 0.5, false),
            (3, "POST /deals/1201/activities", 0.5, false),
        ]
    );
}

fn with_hook(script: &str) -> String {
    acme_manifest().replace("anchor: bake", &format!("anchor: bake\n  hook: [sh, -c, '{script}']"))
}

#[test]
#[ignore = "needs Docker"]
fn check_fails_an_image_due_to_expire() {
    let manifest = with_hook(r#"test "$MIROIR_SHIFT_DAYS" -lt 100"#);
    let files = [("/miroir/miroir.yaml", manifest.as_str())];
    let (code, report, logs) = run_check(ENV_IMAGE, &files, &[("read", READ_TASK)]);
    assert_eq!(code, 1, "{report:#}\n{logs}");
    let error = report["stages"]["idle"]["error"].as_str().unwrap_or_default();
    assert!(error.contains("a reset 400 days later would fail"), "{report:#}");
}

#[test]
#[ignore = "needs Docker"]
fn check_fails_a_product_that_needs_dates_and_timestamps_in_step() {
    let midnight = jiff::Timestamp::now().as_second().div_euclid(86_400) * 86_400;
    let anchor = jiff::Timestamp::from_second(midnight).unwrap();
    let manifest = with_hook(r#"test "$MIROIR_SHIFT_DAYS" = "$MIROIR_SHIFT_DATE_DAYS""#)
        .replace("anchor: bake", &format!("anchor: \"{anchor}\""));
    let files = [("/miroir/miroir.yaml", manifest.as_str())];
    let (code, report, logs) = run_check(ENV_IMAGE, &files, &[("read", READ_TASK)]);
    assert_eq!(code, 1, "{report:#}\n{logs}");
    expect_only(&report, &logs, &[]);
    let drift = &report["tasks"][0]["drift"];
    assert_eq!(drift["passed"], false, "{report:#}");
    assert!(drift["error"].as_str().unwrap_or_default().contains("step 'hook'"), "{drift}");
}

#[test]
#[ignore = "needs Docker"]
fn check_fails_a_check_that_errors_on_an_empty_run() {
    let task = READ_TASK.replace(
        "allowed_changes: []",
        "  - {name: fragile, gate: true, db: episode,\n     sql: SELECT 1 FROM answer WHERE value = '3' UNION SELECT abs(-9223372036854775808) FROM actions WHERE (SELECT count(*) FROM answer WHERE value = '3') = 0}\nallowed_changes: []",
    );
    let (code, report, logs) = run_check(ENV_IMAGE, &[], &[("fragile", &task)]);
    assert_eq!(code, 1, "{report:#}\n{logs}");
    assert_eq!(report["tasks"][0]["null"]["passed"], false, "{report:#}");
}
