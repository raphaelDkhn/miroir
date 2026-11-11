//! `miroir check`: replays each task's golden, null, repeated, probe, hedge and drift runs,
//! and checks that the env idles, resets cleanly and writes only inside its state.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use crate::clock::WAIT;
use crate::coworker::COWORKER_REPLY;
use crate::db::Shift;
use crate::env::{Env, EnvError};
use crate::episode::{Action, ResetArgs};
use crate::state::{diff, scan};
use crate::task::{Answer, GoldenStep, Probe, Task, move_date};

const IDLE: Duration = Duration::from_secs(30);

/// A fingerprint of the data after a reset: each table's hash, time columns aside.
type Print = BTreeMap<String, String>;

/// The first fingerprint of each world and `setup`, with the task it was taken for.
type Firsts = BTreeMap<(String, Option<String>), (String, Print)>;

/// What `check` gathers across tasks: reset and grading times, and the first fingerprints.
#[derive(Default)]
struct Gathered {
    resets: Vec<Duration>,
    grading: Vec<Duration>,
    firsts: Firsts,
}

/// Check the env and its tasks, or only `world`'s, and return the JSON report.
pub async fn run(env: &Arc<Env>, mutate: bool, world: Option<&str>) -> Value {
    let shift = env.shift();
    let mut gathered = Gathered::default();
    let users = users(env);
    let checked: Vec<&Task> =
        env.tasks.iter().filter(|task| world.is_none_or(|world| task.world == world)).collect();
    let idle = idle_worlds(env, &checked, shift, &mut gathered.resets).await;
    let mcp = mcp_tools(env).await;
    let sandbox = sandbox(env).await;
    let mut tasks = Vec::new();
    for task in checked {
        let mut report = check_task(env, task, shift, &mut gathered).await;
        if mutate {
            report["mutations"] = mutations(env, task, shift).await;
        }
        tasks.push(report);
    }
    let passed = users["passed"] == true
        && idle["passed"] == true
        && mcp.as_ref().is_none_or(|mcp| mcp["passed"] == true)
        && sandbox.as_ref().is_none_or(|sandbox| sandbox["passed"] == true)
        && tasks.iter().all(|t| t["passed"] == true);
    let mut stages = json!({"static": {"passed": true}, "users": users, "idle": idle});
    if let Some(mcp) = mcp {
        stages["mcp"] = mcp;
    }
    if let Some(sandbox) = sandbox {
        stages["sandbox"] = sandbox;
    }
    json!({
        "passed": passed,
        "env": env.manifest.name,
        "shift_days": shift.timestamps,
        "stages": stages,
        "tasks": tasks,
        "timings": {
            "reset_p50_s": percentile(&gathered.resets, 50),
            "reset_p95_s": percentile(&gathered.resets, 95),
            "grading_p50_s": percentile(&gathered.grading, 50),
            "grading_max_s": percentile(&gathered.grading, 100),
        },
    })
}

async fn mcp_tools(env: &Env) -> Option<Value> {
    Some(match env.mcp_tools().await? {
        Ok([]) => json!({"passed": false, "error": "the app's MCP server lists no tools"}),
        Ok(tools) => json!({"passed": true, "tools": tools.len()}),
        Err(failure) => json!({"passed": false, "error": failure.error.message}),
    })
}

async fn sandbox(env: &Env) -> Option<Value> {
    Some(match env.sandbox.as_ref()?.probe(&[crate::code::PROXY_PORT]).await {
        Ok(()) => json!({"passed": true}),
        Err(e) => json!({"passed": false, "error": e}),
    })
}

fn users(env: &Env) -> Value {
    let root: Vec<&str> =
        env.manifest.processes.iter().filter(|p| p.user.is_none()).map(|p| &*p.name).collect();
    json!({"passed": root.is_empty(), "root": root})
}

async fn idle_worlds(
    env: &Env,
    tasks: &[&Task],
    shift: Shift,
    resets: &mut Vec<Duration>,
) -> Value {
    let mut report = json!({"passed": false, "error": "no tasks"});
    let mut idled: Vec<&str> = Vec::new();
    for task in tasks {
        if idled.contains(&task.world.as_str()) {
            continue;
        }
        idled.push(&task.world);
        report = idle(env, task, shift, resets).await;
        if report["passed"] != true {
            if env.manifest.worlds.len() > 1 {
                report["world"] = json!(task.world);
            }
            break;
        }
    }
    report
}

async fn idle(env: &Env, task: &Task, shift: Shift, resets: &mut Vec<Duration>) -> Value {
    for (days, when) in [(1, "a day"), (400, "400 days")] {
        let later = Shift { timestamps: shift.timestamps + days, dates: shift.dates + days };
        if let Err(e) = timed_reset(env, task, later, resets).await {
            let error = format!("a reset {when} later would fail: {e}");
            return json!({"passed": false, "error": error});
        }
    }
    if let Err(e) = timed_reset(env, task, shift, resets).await {
        return json!({"passed": false, "error": e});
    }
    tokio::time::sleep(IDLE).await;
    let crashed = env.supervisor.crash();
    let crashed = crashed.map(|c| json!({"process": c.process, "exit_code": c.exit_code}));
    let changes = env.changes_so_far().await;
    let effects = env.edb.effect_count().await.map_err(|e| e.to_string());
    match (changes, effects) {
        (Ok(changes), Ok(effects)) => json!({
            "passed": changes.is_empty() && effects == 0 && crashed.is_none(),
            "changes": changes.iter().map(crate::grade::NetChange::summary).collect::<Vec<_>>(),
            "effects": effects,
            "crashed": crashed,
        }),
        (Err(e), _) | (_, Err(e)) => json!({"passed": false, "error": e}),
    }
}

async fn check_task(env: &Arc<Env>, task: &Task, shift: Shift, gathered: &mut Gathered) -> Value {
    let before = scan(&env.state).await;
    let mut prints = Vec::new();
    let golden_run = Replay { label: "golden", steps: &task.golden, fingerprint: true };
    let empty_steps = [GoldenStep { tool: "submit".into(), arguments: serde_json::Map::new() }];
    let empty_run = Replay { label: "null", steps: &empty_steps, fingerprint: true };
    let golden = replay(env, task, shift, &golden_run).await;
    let empty = replay(env, task, shift, &empty_run).await;
    let repeats =
        [replay(env, task, shift, &golden_run).await, replay(env, task, shift, &golden_run).await];
    let mut report = json!({"id": task.id});
    let mut failures = Vec::new();
    for run in std::iter::once(&golden).chain([&empty]).chain(&repeats) {
        match run {
            Ok(run) => {
                gathered.resets.push(run.reset);
                gathered.grading.push(run.grading);
                prints.push(run.print.clone());
            }
            Err(e) => failures.push(e.clone()),
        }
    }
    if let Some(error) = failures.first() {
        report["passed"] = json!(false);
        report["error"] = json!(error);
        return report;
    }
    let (golden, empty) = (golden.unwrap_or_default(), empty.unwrap_or_default());
    let events = list(&golden.metadata["clock"]["events"]);
    let golden_ok = golden.success
        && golden.statuses.iter().all(call_worked)
        && events.iter().all(|event| call_worked(&event["status"]));
    report["golden"] = json!({
        "passed": golden_ok,
        "reward": golden.reward,
        "success": golden.success,
        "statuses": golden.statuses,
        "checks": golden.checks,
        "metadata": golden.metadata,
    });
    let null_ok =
        empty.reward <= task.null_max && gates_hold(&empty.checks) && !spoiled(&empty.metadata);
    report["null"] = json!({
        "passed": null_ok,
        "reward": empty.reward,
        "null_max": task.null_max,
        "checks": empty.checks,
        "metadata": empty.metadata,
    });
    let repeated: Vec<Value> = repeats
        .iter()
        .flatten()
        .map(|r| json!({"reward": r.reward, "checks": r.checks, "metadata": r.metadata}))
        .collect();
    let repeat_ok = repeats.iter().flatten().all(|r| {
        r.reward.to_bits() == golden.reward.to_bits()
            && r.checks == golden.checks
            && !spoiled(&r.metadata)
    });
    report["repeat"] = json!({"passed": repeat_ok, "runs": repeated});
    report["reset"] = reset_stage(task, &prints, &mut gathered.firsts);
    let reset_ok = report["reset"]["passed"] == true;
    let mut probes_ok = true;
    if !task.probes.is_empty() {
        let runs = probes(env, task, shift).await;
        probes_ok = runs.iter().all(|run| run["passed"] == true);
        report["probes"] = json!({"passed": probes_ok, "runs": runs});
    }
    if let Some(hedge) = hedge(env, task, shift).await {
        report["hedge"] = hedge;
    }
    report["drift"] = drift(env, task, shift, &golden_run, &golden).await;
    let changed = diff(&before, &scan(&env.state).await);
    report["writes"] = json!({"passed": changed.is_empty(), "changed": changed});
    let hedge_ok = report.get("hedge").is_none_or(|hedge| hedge["passed"] == true);
    let others_ok = reset_ok
        && probes_ok
        && hedge_ok
        && changed.is_empty()
        && report["drift"]["passed"] == true;
    report["passed"] = json!(golden_ok && null_ok && repeat_ok && others_ok);
    report
}

/// The reset stage: every fingerprint of a task's runs hashes the same as its first, and as the
/// first of its world with the same `setup`, whichever task that was for: an episode that left
/// something a reset did not restore shows up in the tasks checked after it.
fn reset_stage(task: &Task, prints: &[Print], firsts: &mut Firsts) -> Value {
    let Some(own) = prints.first() else {
        return json!({"passed": false, "differing_tables": [], "differing_from_first": []});
    };
    let differing = tables_unlike(own, prints);
    let key = (task.world.clone(), task.setup.clone());
    let (first, print) = firsts.entry(key).or_insert_with(|| (task.id.clone(), own.clone()));
    let unlike = tables_unlike(print, prints);
    json!({
        "passed": differing.is_empty() && unlike.is_empty(),
        "differing_tables": differing,
        "first_reset": first,
        "differing_from_first": unlike,
    })
}

/// The tables whose hash differs between `reference` and any of `prints`, or that one lacks.
fn tables_unlike(reference: &Print, prints: &[Print]) -> Vec<String> {
    let mut tables = BTreeSet::new();
    for print in prints {
        for table in reference.keys().chain(print.keys()) {
            if reference.get(table) != print.get(table) {
                tables.insert(table.clone());
            }
        }
    }
    tables.into_iter().collect()
}

async fn drift(
    env: &Arc<Env>,
    task: &Task,
    shift: Shift,
    golden_run: &Replay<'_>,
    golden: &Run,
) -> Value {
    let earlier = Shift { timestamps: shift.dates - 1, dates: shift.dates };
    let run = Replay { label: golden_run.label, steps: golden_run.steps, fingerprint: false };
    match replay(env, task, earlier, &run).await {
        Ok(run) => json!({
            "passed": run.reward.to_bits() == golden.reward.to_bits() && run.checks == golden.checks,
            "reward": run.reward,
            "checks": run.checks,
        }),
        Err(error) => json!({"passed": false, "error": error}),
    }
}

async fn hedge(env: &Arc<Env>, task: &Task, shift: Shift) -> Option<Value> {
    match task.answer {
        Some(Answer::Integer | Answer::Date) => return None,
        Some(Answer::Ids) | None => {}
    }
    let answer = task.golden.last()?.arguments.get("answer")?.as_str()?.trim();
    if answer.is_empty() {
        return None;
    }
    let arguments = json!({"answer": hedged(answer)}).as_object()?.clone();
    let steps = [GoldenStep { tool: "submit".into(), arguments }];
    let run = Replay { label: "hedge", steps: &steps, fingerprint: false };
    Some(match replay(env, task, shift, &run).await {
        Ok(run) => json!({
            "passed": run.reward <= task.null_max && run.metadata["invalid"].is_null(),
            "reward": run.reward,
            "null_max": task.null_max,
            "checks": run.checks,
            "metadata": run.metadata,
        }),
        Err(error) => json!({"passed": false, "error": error}),
    })
}

fn hedged(answer: &str) -> String {
    let guesses = ["0", "1", "2", "yes", "no", "true", "false", "none"];
    let mut lines: Vec<&str> =
        guesses.into_iter().filter(|guess| !guess.eq_ignore_ascii_case(answer)).collect();
    lines.insert(2, answer);
    lines.join("\n")
}

fn call_worked(status: &Value) -> bool {
    *status == "ok" || status.as_u64().is_some_and(|code| code < 400)
}

fn spoiled(metadata: &Value) -> bool {
    !metadata["fault"].is_null() || !metadata["invalid"].is_null()
}

fn gates_hold(checks: &Value) -> bool {
    list(checks).iter().all(|c| c["gate"] != true || c["score"].as_f64().is_some_and(|s| s >= 1.0))
}

async fn probes(env: &Arc<Env>, task: &Task, shift: Shift) -> Vec<Value> {
    let mut runs = Vec::new();
    for probe in &task.probes {
        let label = format!("probe '{}'", probe.name);
        let probed = Replay { label: &label, steps: &probe.steps, fingerprint: false };
        runs.push(probe_report(probe, replay(env, task, shift, &probed).await));
    }
    runs
}

fn probe_report(probe: &Probe, run: Result<Run, String>) -> Value {
    let (name, most, least) = (&probe.name, probe.max_reward, probe.min_reward);
    let run = match run {
        Ok(run) => run,
        Err(error) => return json!({"name": name, "passed": false, "error": error}),
    };
    let bounded =
        most.is_none_or(|most| run.reward <= most) && least.is_none_or(|least| run.reward >= least);
    json!({
        "name": name,
        "passed": !spoiled(&run.metadata) && bounded,
        "reward": run.reward,
        "max_reward": most,
        "min_reward": least,
        "statuses": run.statuses,
        "checks": run.checks,
        "metadata": run.metadata,
    })
}

async fn mutations(env: &Arc<Env>, task: &Task, shift: Shift) -> Value {
    let mut runs = Vec::new();
    for dropped in 0..task.golden.len().saturating_sub(1) {
        let mut steps = task.golden.clone();
        let call = describe_call(&steps.remove(dropped));
        let mutated = Replay { label: "mutated", steps: &steps, fingerprint: false };
        let number = dropped + 1;
        runs.push(match replay(env, task, shift, &mutated).await {
            Ok(run) => json!({
                "dropped": number,
                "call": call,
                "reward": run.reward,
                "success": run.success,
                "statuses": run.statuses,
                "checks": run.checks,
            }),
            Err(error) => json!({"dropped": number, "call": call, "error": error}),
        });
    }
    Value::Array(runs)
}

fn describe_call(step: &GoldenStep) -> String {
    let text = |key: &str| step.arguments.get(key).and_then(Value::as_str).unwrap_or("?");
    let call = match step.tool.as_str() {
        "api_request" => format!("{} {}", text("method"), text("path")),
        COWORKER_REPLY => return format!("{} replies", text("coworker")),
        WAIT => {
            let minutes = step.arguments.get("minutes").unwrap_or(&Value::Null);
            return format!("wait {minutes} min");
        }
        other => other.to_string(),
    };
    match step.arguments.get("app").and_then(Value::as_str) {
        Some(app) => format!("{app}: {call}"),
        None => call,
    }
}

#[derive(Default)]
struct Run {
    reward: f64,
    success: bool,
    checks: Value,
    metadata: Value,
    statuses: Vec<Value>,
    print: Print,
    reset: Duration,
    grading: Duration,
}

struct Replay<'a> {
    label: &'a str,
    steps: &'a [GoldenStep],
    fingerprint: bool,
}

async fn replay(
    env: &Arc<Env>,
    task: &Task,
    shift: Shift,
    replay: &Replay<'_>,
) -> Result<Run, String> {
    let started = Instant::now();
    let args = ResetArgs { task_id: Some(task.id.clone()), ..ResetArgs::default() };
    let (_, print) =
        env.reset_with(args, Some(shift), replay.fingerprint).await.map_err(|e| e.to_string())?;
    let mut run =
        Run { reset: started.elapsed(), print: print.unwrap_or_default(), ..Run::default() };
    let mut last = Value::Null;
    for step in replay.steps {
        if step.tool == COWORKER_REPLY {
            run.statuses.push(answer(env, task, replay, step).await?);
            continue;
        }
        let started = Instant::now();
        let action = Action::CallTool {
            name: step.tool.clone(),
            arguments: as_sent(step, task, shift),
            episode_id: None,
        };
        last = env.step(action).await.map_err(|e| e.to_string())?;
        run.grading = started.elapsed();
        let observation = &last["observation"];
        if observation["error"]["error_type"] == "tool_not_found" {
            let why = if step.tool == "run_code" {
                "needs a sandbox: run check with MIROIR_SANDBOX set"
            } else {
                "is no tool; an app's MCP server may have renamed it"
            };
            return Err(format!(
                "task {}: the {} run calls '{}', which {why}",
                task.id, replay.label, step.tool
            ));
        }
        run.statuses.push(status(&step.tool, observation));
        if last["done"] == true {
            break;
        }
    }
    if last["done"] != true {
        let error = last["observation"]["error"]["message"].as_str().unwrap_or("no error");
        return Err(format!(
            "task {}: the {} run never ended the episode; its last step failed: {error}",
            task.id, replay.label
        ));
    }
    run.reward = last["reward"].as_f64().unwrap_or(0.0);
    run.metadata = last["metadata"].clone();
    run.success = run.metadata["success"] == true;
    run.checks = run.metadata["checks"].clone();
    Ok(run)
}

async fn answer(
    env: &Arc<Env>,
    task: &Task,
    replay: &Replay<'_>,
    step: &GoldenStep,
) -> Result<Value, String> {
    let text = |key: &str| step.arguments.get(key).and_then(Value::as_str).unwrap_or_default();
    let (name, answer) = (text("coworker"), text("text"));
    let failed =
        |e: EnvError| format!("task {}: the {} run's reply for {name}: {e}", task.id, replay.label);
    let turns = env.coworker_turns().await.map_err(failed)?;
    if !list(&turns["turns"]).iter().any(|turn| turn["coworker"] == name) {
        return Ok(json!(format!("nothing reached {name}")));
    }
    let sent = env.coworker_reply(name.into(), answer.into()).await.map_err(failed)?;
    Ok(sent["status"].clone())
}

fn status(tool: &str, observation: &Value) -> Value {
    let error = &observation["error"];
    match tool {
        "api_request" => observation["result"]["status"].clone(),
        "run_code" if error.is_null() => match observation["result"]["exit_code"].as_i64() {
            Some(0) => json!("ok"),
            code => json!(format!("exit {}", code.unwrap_or(-1))),
        },
        _ if error.is_null() => json!("ok"),
        _ => error["error_type"].clone(),
    }
}

fn as_sent(step: &GoldenStep, task: &Task, shift: Shift) -> Map<String, Value> {
    let mut arguments = step.arguments.clone();
    let date = match task.answer {
        Some(Answer::Date) if step.tool == "submit" => {
            arguments.get("answer").and_then(Value::as_str)
        }
        Some(Answer::Date | Answer::Integer | Answer::Ids) | None => None,
    };
    if let Some(moved) = date.and_then(|date| move_date(date.trim(), shift.dates)) {
        arguments.insert("answer".into(), Value::String(moved));
    }
    arguments
}

async fn timed_reset(
    env: &Env,
    task: &Task,
    shift: Shift,
    resets: &mut Vec<Duration>,
) -> Result<(), String> {
    let started = Instant::now();
    let args = ResetArgs { task_id: Some(task.id.clone()), ..ResetArgs::default() };
    env.reset_with(args, Some(shift), false).await.map_err(|e| e.to_string())?;
    resets.push(started.elapsed());
    Ok(())
}

fn percentile(samples: &[Duration], pct: usize) -> Option<f64> {
    let mut sorted = samples.to_vec();
    sorted.sort();
    let index = (sorted.len() * pct / 100).min(sorted.len().checked_sub(1)?);
    Some((sorted[index].as_secs_f64() * 100.0).round() / 100.0)
}

/// The report as the table `check` prints, with the reasons each stage failed.
pub fn summary(report: &Value) -> String {
    let mut lines = vec![match report["env"].as_str() {
        Some(env) => format!("miroir check: {env}"),
        None => "miroir check".into(),
    }];
    let stages = &report["stages"];
    lines.push(format!("{:<28} {}", "static", verdict(&stages["static"])));
    if let Some(error) = stages["static"]["error"].as_str() {
        lines.extend(error.lines().map(|line| format!("  {line}")));
        lines.push("FAILED".into());
        return lines.join("\n");
    }
    lines.push(format!("{:<28} {}", "users", verdict(&stages["users"])));
    for process in list(&stages["users"]["root"]).iter().filter_map(Value::as_str) {
        lines.push(format!(
            "  process '{process}' has no user, so it runs as root and could read the tasks' \
             checks and golden runs; set its user, or user: root to keep it"
        ));
    }
    lines.push(format!("{:<28} {}", "idle", verdict(&stages["idle"])));
    if stages["idle"]["passed"] == false {
        lines.extend(idle_reasons(&stages["idle"]).iter().map(|r| format!("  {r}")));
    }
    if stages["mcp"].is_object() {
        let mcp = &stages["mcp"];
        let tools = mcp["tools"].as_u64().map(|n| format!(" ({n} tools)")).unwrap_or_default();
        lines.push(format!("{:<28} {}{tools}", "mcp", verdict(mcp)));
        lines.extend(mcp["error"].as_str().map(|error| format!("  {error}")));
    }
    if stages["sandbox"].is_object() {
        let sandbox = &stages["sandbox"];
        lines.push(format!("{:<28} {}", "sandbox", verdict(sandbox)));
        lines.extend(sandbox["error"].as_str().map(|error| format!("  {error}")));
    }
    lines.push(format!(
        "{:<28} {:<7}{:<6}{:<8}{:<7}{:<7}golden statuses",
        "task", "golden", "null", "repeat", "reset", "writes"
    ));
    for task in list(&report["tasks"]) {
        let statuses: Vec<String> = list(&task["golden"]["statuses"])
            .iter()
            .map(|s| s.to_string().trim_matches('"').to_string())
            .collect();
        lines.push(format!(
            "{:<28} {:<7}{:<6}{:<8}{:<7}{:<7}{}",
            task["id"].as_str().unwrap_or("?"),
            verdict(&task["golden"]),
            verdict(&task["null"]),
            verdict(&task["repeat"]),
            verdict(&task["reset"]),
            verdict(&task["writes"]),
            statuses.join(" ")
        ));
        lines.extend(task_reasons(task).iter().map(|r| format!("  {r}")));
    }
    let seconds =
        |key: &str| report["timings"][key].as_f64().map_or("-".into(), |s| format!("{s} s"));
    lines.push(format!(
        "reset p50 {}, p95 {}; grading p50 {}, max {}",
        seconds("reset_p50_s"),
        seconds("reset_p95_s"),
        seconds("grading_p50_s"),
        seconds("grading_max_s")
    ));
    lines.push(if report["passed"] == true { "PASSED".into() } else { "FAILED".into() });
    lines.join("\n")
}

fn verdict(stage: &Value) -> &'static str {
    match stage["passed"].as_bool() {
        Some(true) => "pass",
        Some(false) => "FAIL",
        None => "-",
    }
}

fn list(value: &Value) -> &[Value] {
    value.as_array().map_or(&[], Vec::as_slice)
}

const LISTED: usize = 10;

fn capped(mut lines: Vec<String>) -> Vec<String> {
    if lines.len() > LISTED {
        let more = lines.len() - LISTED;
        lines.truncate(LISTED);
        lines.push(format!("and {more} more"));
    }
    lines
}

fn change(summary: &Value) -> String {
    let (op, tbl) = (summary["op"].as_str().unwrap_or("?"), summary["tbl"].as_str().unwrap_or("?"));
    let tbl = match summary["app"].as_str() {
        Some(app) => format!("{app}.{tbl}"),
        None => tbl.to_string(),
    };
    let columns: Vec<&str> = list(&summary["columns"]).iter().filter_map(Value::as_str).collect();
    let columns =
        if columns.is_empty() { String::new() } else { format!(" ({})", columns.join(", ")) };
    format!("{op} {tbl} {}{columns}", summary["pk"])
}

fn idle_reasons(idle: &Value) -> Vec<String> {
    let reasons = idle_problems(idle);
    match idle["world"].as_str() {
        Some(world) => {
            reasons.into_iter().map(|reason| format!("world '{world}': {reason}")).collect()
        }
        None => reasons,
    }
}

fn idle_problems(idle: &Value) -> Vec<String> {
    if let Some(error) = idle["error"].as_str() {
        return vec![error.to_string()];
    }
    let mut lines = Vec::new();
    let crash = &idle["crashed"];
    if let Some(process) = crash["process"].as_str() {
        lines.push(format!(
            "process '{process}' exited with code {} while idle",
            crash["exit_code"]
        ));
    }
    let changes = list(&idle["changes"]);
    if !changes.is_empty() || idle["effects"] != 0 {
        lines.push(format!(
            "{} s with no agent action produced {} change(s) and {} effect(s)",
            IDLE.as_secs(),
            changes.len(),
            idle["effects"]
        ));
        lines.extend(capped(changes.iter().map(change).collect()));
    }
    lines
}

type Reasons = fn(&Value) -> Vec<String>;

fn task_reasons(task: &Value) -> Vec<String> {
    let mut lines: Vec<String> =
        task["error"].as_str().map(|e| format!("error: {e}")).into_iter().collect();
    let stages: [(&str, Reasons); 8] = [
        ("golden", golden_reasons),
        ("null", null_reasons),
        ("repeat", repeat_reasons),
        ("reset", reset_reasons),
        ("writes", writes_reasons),
        ("probes", probes_reasons),
        ("hedge", hedge_reasons),
        ("drift", drift_reasons),
    ];
    for (stage, reasons) in stages {
        if task[stage]["passed"] == false {
            lines.extend(reasons(task).into_iter().map(|r| format!("{stage}: {r}")));
        }
    }
    lines.extend(mutation_lines(task));
    lines
}

fn drift_reasons(task: &Value) -> Vec<String> {
    let (golden, drift) = (&task["golden"], &task["drift"]);
    if let Some(error) = drift["error"].as_str() {
        return vec![format!("with timestamps a day behind dates: {error}")];
    }
    let mut lines = vec![format!(
        "with timestamps a day behind dates, as before bake's time of day, the golden run earned \
         {}, not {}",
        drift["reward"], golden["reward"]
    )];
    lines.extend(scores(&drift["checks"], |score| score < 1.0));
    lines
}

fn hedge_reasons(task: &Value) -> Vec<String> {
    let hedge = &task["hedge"];
    if let Some(error) = hedge["error"].as_str() {
        return vec![error.to_string()];
    }
    let mut lines = problems(&hedge["metadata"]);
    let earned = hedge["reward"].as_f64().unwrap_or_default();
    if let Some(most) = hedge["null_max"].as_f64().filter(|most| earned > *most) {
        lines.push(format!(
            "the golden answer among guesses, with nothing done, earned {earned:.2}, above \
             null_max {most}; compare the whole answer rather than look for the right one in it"
        ));
        lines.extend(scores(&hedge["checks"], |score| score > 0.0));
    }
    lines
}

fn probes_reasons(task: &Value) -> Vec<String> {
    let mut lines = Vec::new();
    for run in list(&task["probes"]["runs"]).iter().filter(|run| run["passed"] != true) {
        let name = run["name"].as_str().unwrap_or("?");
        let mut reasons = problems(&run["metadata"]);
        reasons.extend(run["error"].as_str().map(str::to_string));
        let earned = run["reward"].as_f64().unwrap_or_default();
        if let Some(most) = run["max_reward"].as_f64().filter(|most| earned > *most) {
            reasons.push(format!("earned {earned:.2}, above max_reward {most}"));
            reasons.extend(scores(&run["checks"], |score| score > 0.0));
        }
        if let Some(least) = run["min_reward"].as_f64().filter(|least| earned < *least) {
            reasons.push(format!("earned {earned:.2}, below min_reward {least}"));
            reasons.extend(scores(&run["checks"], |score| score < 1.0));
        }
        lines.extend(reasons.into_iter().map(|reason| format!("'{name}': {reason}")));
    }
    lines
}

fn mutation_lines(task: &Value) -> Vec<String> {
    let mut lines = Vec::new();
    let mut unneeded = Vec::new();
    for run in list(&task["mutations"]) {
        let (number, call) = (&run["dropped"], run["call"].as_str().unwrap_or("?"));
        if let Some(error) = run["error"].as_str() {
            lines.push(format!("mutate: without call {number} ({call}): {error}"));
        } else if run["success"] == true {
            unneeded.push(number.to_string());
        } else if let Some(earned) = run["reward"].as_f64().filter(|earned| *earned > 0.0) {
            lines
                .push(format!("mutate: without call {number} ({call}) it still earns {earned:.2}"));
        }
    }
    if !unneeded.is_empty() {
        let calls = unneeded.join(", ");
        lines
            .push(format!("mutate: it still succeeds without call {calls}: reads, or weak checks"));
    }
    lines
}

fn scores(checks: &Value, keep: fn(f64) -> bool) -> Vec<String> {
    let kept = list(checks)
        .iter()
        .filter(|c| c["name"] != "scope" && c["score"].as_f64().is_some_and(keep));
    kept.map(|c| {
        let error = c["error"].as_str().map(|e| format!(": {e}")).unwrap_or_default();
        format!("check '{}' scored {}{error}", c["name"].as_str().unwrap_or("?"), c["score"])
    })
    .collect()
}

fn problems(metadata: &Value) -> Vec<String> {
    ["fault", "error"]
        .into_iter()
        .filter_map(|key| metadata[key].as_str())
        .map(Into::into)
        .collect()
}

fn golden_reasons(task: &Value) -> Vec<String> {
    let golden = &task["golden"];
    let mut lines = scores(&golden["checks"], |score| score < 1.0);
    for (i, status) in list(&golden["statuses"]).iter().enumerate() {
        if !call_worked(status) {
            lines.push(format!("call {} returned {status}", i + 1));
        }
    }
    for event in list(&golden["metadata"]["clock"]["events"]) {
        if !call_worked(&event["status"]) {
            let name = event["name"].as_str().unwrap_or("?");
            lines.push(format!(
                "event '{name}' got {}, at minute {}",
                event["status"], event["minute"]
            ));
        }
    }
    let outside = list(&golden["metadata"]["out_of_scope"]);
    lines.extend(capped(
        outside.iter().map(|c| format!("outside allowed_changes: {}", change(c))).collect(),
    ));
    lines.extend(problems(&golden["metadata"]));
    lines
}

fn null_reasons(task: &Value) -> Vec<String> {
    let null = &task["null"];
    let mut lines = problems(&null["metadata"]);
    if null["reward"].as_f64() > null["null_max"].as_f64() {
        let (reward, most) = (&null["reward"], &null["null_max"]);
        lines.push(format!("an immediate empty submit earned {reward}, above null_max {most}"));
        lines.extend(scores(&null["checks"], |score| score > 0.0));
    }
    let failed_gates = list(&null["checks"]).iter().filter(|c| !gates_hold(&json!([c])));
    for gate in failed_gates {
        lines.push(format!(
            "gate '{}' fails on an empty submit; a gate says what must not happen, so it must hold \
             when nothing was done",
            gate["name"].as_str().unwrap_or("?")
        ));
    }
    lines
}

fn repeat_reasons(task: &Value) -> Vec<String> {
    let golden = &task["golden"];
    let runs = list(&task["repeat"]["runs"]);
    let mut lines: Vec<String> = runs.iter().flat_map(|r| problems(&r["metadata"])).collect();
    let rewards: Vec<String> =
        std::iter::once(golden).chain(runs).map(|r| r["reward"].to_string()).collect();
    if rewards.iter().any(|r| *r != rewards[0]) {
        lines.push(format!("rewards differ across runs: {}", rewards.join(", ")));
    }
    let score = |c: &Value| match c["error"].as_str() {
        Some(error) => format!("{} ({error})", c["score"]),
        None => c["score"].to_string(),
    };
    for (i, check) in list(&golden["checks"]).iter().enumerate() {
        let all: Vec<&Value> =
            std::iter::once(check).chain(runs.iter().map(|r| &r["checks"][i])).collect();
        if all.iter().any(|c| *c != check) {
            let shown: Vec<String> = all.iter().map(|c| score(c)).collect();
            let name = check["name"].as_str().unwrap_or("?");
            lines.push(format!("check '{name}' scored {} across runs", shown.join(", ")));
        }
    }
    lines
}

fn reset_reasons(task: &Value) -> Vec<String> {
    let reset = &task["reset"];
    let tables = |key: &str| -> String {
        let names: Vec<&str> = list(&reset[key]).iter().filter_map(Value::as_str).collect();
        names.join(", ")
    };
    let mut lines = Vec::new();
    let (within, unlike) = (tables("differing_tables"), tables("differing_from_first"));
    if !within.is_empty() {
        lines.push(format!("the fingerprint after setup changed between resets in {within}"));
    }
    let first = reset["first_reset"].as_str().unwrap_or_default();
    if !unlike.is_empty() && first != task["id"].as_str().unwrap_or_default() {
        lines.push(format!(
            "the fingerprint after setup differs in {unlike} from the first reset of this world \
             and setup, for task '{first}': a run checked in between left data a reset did not \
             restore"
        ));
    }
    lines
}

fn writes_reasons(task: &Value) -> Vec<String> {
    let changed = list(&task["writes"]["changed"]).iter().filter_map(Value::as_str);
    capped(changed.map(|path| format!("changed outside the state directories: {path}")).collect())
}

/// The report of an env that failed to load.
pub fn static_failure(error: &str) -> Value {
    json!({"passed": false, "stages": {"static": {"passed": false, "error": error}}, "tasks": []})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_and_diffs() {
        let samples: Vec<Duration> = (1..=10).map(Duration::from_secs).collect();
        assert_eq!(percentile(&samples, 50), Some(6.0));
        assert_eq!(percentile(&samples, 95), Some(10.0));
        assert_eq!(percentile(&[], 50), None);
        let path = std::path::PathBuf::from;
        let before: crate::state::Scan =
            [(path("/a"), (1, 1, 1, 1)), (path("/b"), (1, 1, 1, 2))].into();
        let after: crate::state::Scan =
            [(path("/a"), (2, 2, 1, 1)), (path("/c"), (1, 1, 1, 3))].into();
        assert_eq!(diff(&before, &after), ["/a", "/b", "/c"]);
    }

    #[test]
    fn summary_shows_the_sandbox_probe() {
        let stages = |sandbox: Value| {
            json!({"static": {"passed": true}, "users": {"passed": true, "root": []},
                   "idle": {"passed": true}, "sandbox": sandbox})
        };
        let passed =
            json!({"env": "gitea", "tasks": [], "stages": stages(json!({"passed": true}))});
        assert!(summary(&passed).contains("\nsandbox                      pass\n"));
        let leak = json!({"passed": false, "error": "the sandbox reaches 10.0.3.5:6379: ..."});
        let failed = json!({"env": "gitea", "tasks": [], "stages": stages(leak)});
        assert!(
            summary(&failed).contains("\nsandbox                      FAIL\n  the sandbox reaches")
        );
    }

    #[test]
    fn a_run_code_step_works_only_when_its_command_exits_zero() {
        let ran = |code: i64| json!({"result": {"exit_code": code, "output": ""}, "error": null});
        assert_eq!(status("run_code", &ran(0)), json!("ok"));
        assert_eq!(status("run_code", &ran(1)), json!("exit 1"));
        assert!(!call_worked(&status("run_code", &ran(1))), "exit 1 is no HTTP status below 400");
        let gone = json!({"result": null, "error": {"error_type": "transport_error"}});
        assert_eq!(status("run_code", &gone), json!("transport_error"));
        let http = json!({"result": {"status": 201}, "error": null});
        assert_eq!(status("api_request", &http), json!(201));
    }

    #[test]
    fn summary_shows_the_app_mcp_servers_listing() {
        let stages = |mcp: Value| {
            json!({"static": {"passed": true}, "users": {"passed": true, "root": []},
                   "idle": {"passed": true}, "mcp": mcp})
        };
        let listing = json!({"passed": true, "tools": 61});
        let listed = json!({"env": "gitea", "tasks": [], "stages": stages(listing)});
        assert!(summary(&listed).contains("\nmcp                          pass (61 tools)\n"));
        let refused =
            json!({"passed": false, "error": "app 'gitea' MCP server lists a tool named 'submit'"});
        let failed = json!({"env": "gitea", "tasks": [], "stages": stages(refused)});
        let text = summary(&failed);
        assert!(
            text.contains("\nmcp                          FAIL\n  app 'gitea' MCP server lists"),
            "{text}"
        );
    }

    fn passing_task() -> Value {
        let pass = json!({"passed": true});
        json!({"id": "pay-invoice", "passed": true, "null": pass, "repeat": pass, "reset": pass,
               "writes": pass, "golden": {"passed": true, "statuses": [200, 200, "ok"]}})
    }

    #[test]
    fn summary_of_a_passing_report_has_no_reasons() {
        let report = json!({
            "passed": true, "env": "acme-crm", "tasks": [passing_task()],
            "stages": {"static": {"passed": true}, "users": {"passed": true, "root": []},
                       "idle": {"passed": true}},
            "timings": {"reset_p50_s": 0.98, "reset_p95_s": 1.26, "grading_p50_s": 0.24,
                        "grading_max_s": 0.47},
        });
        assert_eq!(
            summary(&report),
            "miroir check: acme-crm
static                       pass
users                        pass
idle                         pass
task                         golden null  repeat  reset  writes golden statuses
pay-invoice                  pass   pass  pass    pass   pass   200 200 ok
reset p50 0.98 s, p95 1.26 s; grading p50 0.24 s, max 0.47 s
PASSED"
        );
    }

    #[test]
    fn calls_and_gates_are_judged_as_documented() {
        for (status, worked) in [
            (json!(200), true),
            (json!(302), true),
            (json!(399), true),
            (json!("ok"), true),
            (json!(400), false),
            (json!(500), false),
            (json!("timeout"), false),
            (Value::Null, false),
        ] {
            assert_eq!(call_worked(&status), worked, "{status}");
        }
        let check = |gate: bool, score: f64| json!({"name": "c", "gate": gate, "score": score});
        assert!(gates_hold(&json!([check(true, 1.0), check(false, 0.0)])));
        assert!(!gates_hold(&json!([check(true, 0.5)])));
        assert!(gates_hold(&Value::Null), "a crash has no checks to hold");
    }

    #[test]
    fn summary_explains_probes_and_mutations() {
        let mut task = passing_task();
        task["passed"] = json!(false);
        task["probes"] = json!({"passed": false, "runs": [
            {"name": "guess", "passed": true, "reward": 0.0, "max_reward": 0.0},
            {"name": "whole-invoice", "passed": false, "reward": 0.6, "max_reward": 0.0,
             "checks": [{"name": "posted", "score": 1.0}, {"name": "amount", "score": 0.0}]},
            {"name": "other-route", "passed": false, "reward": 0.25, "min_reward": 1.0,
             "checks": [{"name": "posted", "score": 1.0}, {"name": "amount", "score": 0.0}]},
            {"name": "hung", "passed": false, "error": "reset failed"},
            {"name": "crashed", "passed": false, "reward": 0.0, "max_reward": 0.0,
             "metadata": {"crashed": {"process": "web", "exit_code": 1},
                          "fault": "process 'web' exited with code 1"}},
        ]});
        task["mutations"] = json!([
            {"dropped": 1, "call": "GET /deals", "reward": 1.0, "success": true},
            {"dropped": 2, "call": "POST /refund", "reward": 0.600_000_000_000_000_1, "success": false},
            {"dropped": 3, "call": "POST /post", "reward": 0.0, "success": false},
            {"dropped": 4, "call": "POST /mail", "error": "reset failed"},
        ]);
        let lines = task_reasons(&task);
        for expected in [
            "probes: 'whole-invoice': earned 0.60, above max_reward 0",
            "probes: 'whole-invoice': check 'posted' scored 1.0",
            "probes: 'other-route': earned 0.25, below min_reward 1",
            "probes: 'other-route': check 'amount' scored 0.0",
            "probes: 'hung': reset failed",
            "probes: 'crashed': process 'web' exited with code 1",
            "mutate: without call 2 (POST /refund) it still earns 0.60",
            "mutate: without call 4 (POST /mail): reset failed",
            "mutate: it still succeeds without call 1: reads, or weak checks",
        ] {
            assert!(lines.iter().any(|line| line == expected), "no {expected:?} in {lines:#?}");
        }
        assert!(!lines.iter().any(|line| line.contains("guess")), "{lines:#?}");
        assert!(!lines.iter().any(|line| line.contains("/post")), "no credit, no line: {lines:#?}");
    }

    #[test]
    fn summary_says_why_a_golden_call_or_a_gate_failed() {
        let mut task = passing_task();
        task["golden"] = json!({"passed": false, "statuses": [200, 404, "timeout", "ok"]});
        task["null"] = json!({"passed": false, "reward": 0.0, "null_max": 0.0, "checks": [
            {"name": "kept", "gate": true, "score": 0.0},
            {"name": "fine", "gate": true, "score": 1.0},
        ]});
        let lines = task_reasons(&task);
        assert!(lines.contains(&"golden: call 2 returned 404".to_string()), "{lines:#?}");
        assert!(lines.contains(&r#"golden: call 3 returned "timeout""#.to_string()), "{lines:#?}");
        assert!(lines.iter().any(|l| l.starts_with("null: gate 'kept' fails")), "{lines:#?}");
        assert!(!lines.iter().any(|l| l.contains("'fine'")), "{lines:#?}");
    }

    #[test]
    fn a_coworker_step_says_who_replies_and_fails_golden_when_nothing_reached_them() {
        let step: GoldenStep = serde_json::from_value(json!({
            "tool": "coworker_reply", "arguments": {"coworker": "dan", "text": "Erin."}
        }))
        .unwrap();
        assert_eq!(describe_call(&step), "dan replies");
        let mut task = passing_task();
        task["golden"] = json!({"passed": false, "statuses": [201, "nothing reached dan", "ok"]});
        let lines = task_reasons(&task);
        let expected = r#"golden: call 2 returned "nothing reached dan""#.to_string();
        assert!(lines.contains(&expected), "{lines:#?}");
        assert!(call_worked(&json!(201)) && !call_worked(&json!("nothing reached dan")));
    }

    #[test]
    fn an_idle_failure_names_its_world_when_set() {
        let named = json!({"passed": false, "world": "south", "error": "a reset 400 days later"});
        assert_eq!(idle_reasons(&named), ["world 'south': a reset 400 days later"]);
        let unnamed = json!({"passed": false, "error": "a reset 400 days later"});
        assert_eq!(idle_reasons(&unnamed), ["a reset 400 days later"]);
    }

    #[test]
    fn summary_shows_the_static_error() {
        let report = static_failure("/miroir/tasks/a.yaml: line 6: unknown field\n --> 6:1");
        assert_eq!(
            summary(&report),
            "miroir check\nstatic                       FAIL\n  \
             /miroir/tasks/a.yaml: line 6: unknown field\n   --> 6:1\nFAILED"
        );
    }

    #[test]
    fn summary_says_why_each_stage_failed() {
        let error = "no such function: json_extrct";
        let checks = |noted: f64| {
            json!([{"name": "reassigned", "score": 1.0},
                   {"name": "noted", "score": noted, "error": error},
                   {"name": "scope", "score": 0.0}])
        };
        let outside = json!([{"tbl": "deals", "op": "update", "pk": {"id": 1099},
                              "columns": ["stage"]}]);
        let crashed = json!({"crashed": {"process": "web", "exit_code": 1},
                             "fault": "process 'web' exited with code 1"});
        let quiet = "the quiet command never passed before grading";
        let failing = json!({
            "id": "reassign-stale-deals", "passed": false,
            "golden": {"passed": false, "reward": 0.0, "statuses": [200, 404, "ok"],
                       "checks": checks(0.0),
                       "metadata": {"out_of_scope": outside, "quiet_timeout": true,
                                    "fault": quiet}},
            "null": {"passed": false, "reward": 0.7, "null_max": 0.0, "metadata": {},
                     "checks": [{"name": "reassigned", "score": 1.0},
                                {"name": "noted", "score": 0.0}]},
            "repeat": {"passed": false, "runs": [
                {"reward": 0.0, "checks": checks(0.0), "metadata": {}},
                {"reward": 0.7, "checks": checks(1.0), "metadata": crashed}]},
            "reset": {"passed": false, "differing_tables": ["deals"]},
            "writes": {"passed": false,
                       "changed": (0..12).map(|i| format!("/app/cache/{i}")).collect::<Vec<_>>()},
            "hedge": {"passed": false, "reward": 1.0, "null_max": 0.0, "metadata": {},
                      "checks": [{"name": "answer", "score": 1.0}]},
        });
        let errored = json!({"id": "pay-invoice", "passed": false, "error": "reset: quiet"});
        let report = json!({
            "passed": false, "env": "acme-crm", "tasks": [failing, errored, passing_task()],
            "stages": {"static": {"passed": true}, "users": {"passed": false, "root": ["web"]},
                "idle": {"passed": false, "effects": 2,
                         "crashed": {"process": "worker", "exit_code": 1},
                         "changes": [{"tbl": "notes", "op": "insert", "pk": {"id": 1},
                                      "columns": null}]}},
            "timings": {"reset_p50_s": 0.98, "reset_p95_s": null},
        });
        let text = summary(&report);
        let lines: Vec<&str> = text.lines().collect();
        for expected in [
            "users                        FAIL",
            "  process 'web' has no user, so it runs as root and could read the tasks' checks and \
             golden runs; set its user, or user: root to keep it",
            "idle                         FAIL",
            "  process 'worker' exited with code 1 while idle",
            "  30 s with no agent action produced 1 change(s) and 2 effect(s)",
            r#"  insert notes {"id":1}"#,
            "reassign-stale-deals         FAIL   FAIL  FAIL    FAIL   FAIL   200 404 ok",
            "  golden: check 'noted' scored 0.0: no such function: json_extrct",
            r#"  golden: outside allowed_changes: update deals {"id":1099} (stage)"#,
            "  golden: the quiet command never passed before grading",
            "  null: an immediate empty submit earned 0.7, above null_max 0.0",
            "  null: check 'reassigned' scored 1.0",
            "  repeat: process 'web' exited with code 1",
            "  repeat: rewards differ across runs: 0.0, 0.0, 0.7",
            "  repeat: check 'noted' scored 0.0 (no such function: json_extrct), \
             0.0 (no such function: json_extrct), 1.0 (no such function: json_extrct) \
             across runs",
            "  reset: the fingerprint after setup changed between resets in deals",
            "  writes: changed outside the state directories: /app/cache/9",
            "  writes: and 2 more",
            "  hedge: the golden answer among guesses, with nothing done, earned 1.00, above \
             null_max 0; compare the whole answer rather than look for the right one in it",
            "  hedge: check 'answer' scored 1.0",
            "pay-invoice                  -      -     -       -      -      ",
            "  error: reset: quiet",
            "reset p50 0.98 s, p95 -; grading p50 -, max -",
            "FAILED",
        ] {
            assert!(lines.contains(&expected), "no line {expected:?} in:\n{text}");
        }
        assert!(!text.contains("/app/cache/10"), "the list of writes is capped:\n{text}");
        assert!(!text.contains("'scope'"), "out-of-scope changes explain scope:\n{text}");
        assert!(!text.contains("golden: check 'reassigned'"), "passing checks stay out:\n{text}");
    }

    #[test]
    fn a_hedge_hides_the_answer_among_guesses_that_differ_from_it() {
        assert_eq!(hedged("3"), "0\n1\n3\n2\nyes\nno\ntrue\nfalse\nnone");
        assert_eq!(hedged("0"), "1\n2\n0\nyes\nno\ntrue\nfalse\nnone", "never first");
        assert_eq!(hedged("Yes"), "0\n1\nYes\n2\nno\ntrue\nfalse\nnone");
    }

    #[test]
    fn a_hedge_of_ids_is_more_ids_and_one_of_a_single_value_is_refused() {
        let normal = Answer::Ids.normalize(&hedged("15, 12"), 0).unwrap();
        let ids: Vec<&str> = normal.split(',').collect();
        assert!(ids.contains(&"12") && ids.contains(&"15") && ids.len() > 2, "{normal}");
        assert!(Answer::Integer.normalize(&hedged("7"), 0).is_err(), "so the stage skips it");
        let hedged_date = hedged("2026-11-04");
        assert!(Answer::Date.normalize(&hedged_date, 0).is_err(), "so the stage skips it");
    }

    #[test]
    fn a_golden_date_answer_moves_forward_with_the_data_and_nothing_else_does() {
        let task = |form: &str| -> Task {
            let yaml = format!(
                "instruction: When?\nanswer: {form}\nchecks: []\nany_change: true\ngolden:\n  - \
                 {{tool: api_request, arguments: {{method: GET, path: /x}}}}\n  - \
                 {{tool: submit, arguments: {{answer: ' 2026-03-31 '}}}}\n"
            );
            serde_saphyr::from_str(&yaml).unwrap()
        };
        let shift = Shift { timestamps: 2, dates: 3 };
        let dated = task("date");
        assert_eq!(as_sent(&dated.golden[1], &dated, shift)["answer"], "2026-04-03");
        assert_eq!(as_sent(&dated.golden[0], &dated, shift), dated.golden[0].arguments);
        let ids = task("ids");
        assert_eq!(as_sent(&ids.golden[1], &ids, shift)["answer"], " 2026-03-31 ");
    }

    #[test]
    fn every_reset_of_a_world_and_setup_hashes_the_same_whichever_task_it_was_for() {
        let task = |id: &str, world: &str, setup: Option<&str>| -> Task {
            let setup = setup.map(|sql| format!("setup: {sql}\n")).unwrap_or_default();
            let yaml = format!(
                "id: {id}\nworld: {world}\n{setup}instruction: Do it.\nchecks: []\n\
                 any_change: true\ngolden: [{{tool: submit}}]\n"
            );
            serde_saphyr::from_str(&yaml).unwrap()
        };
        let print = |deals: &str| -> Print {
            [("deals", deals), ("notes", "2:ab")].map(|(t, h)| (t.into(), h.into())).into()
        };
        let mut firsts = BTreeMap::new();
        let clean = [print("3:aa"), print("3:aa")];
        let first = reset_stage(&task("a", "north", None), &clean, &mut firsts);
        assert_eq!(first["passed"], true, "{first}");
        let left =
            reset_stage(&task("b", "north", None), &[print("4:bb"), print("4:bb")], &mut firsts);
        assert_eq!(left["passed"], false, "the same within the task, unlike the world's first");
        assert_eq!(
            (&left["differing_tables"], &left["differing_from_first"]),
            (&json!([]), &json!(["deals"]))
        );
        let reasons = reset_reasons(&json!({"id": "b", "reset": left}));
        assert!(
            reasons[0].contains("differs in deals from the first reset")
                && reasons[0].contains("'a'"),
            "{reasons:?}"
        );
        let other = [print("4:bb"), print("4:bb")];
        let south = reset_stage(&task("c", "south", None), &other, &mut firsts);
        let setup =
            reset_stage(&task("d", "north", Some("DELETE FROM deals")), &other, &mut firsts);
        assert_eq!(
            (&south["passed"], &setup["passed"]),
            (&json!(true), &json!(true)),
            "first of their own"
        );
        let mut gone = print("3:aa");
        gone.remove("notes");
        let missing = reset_stage(&task("e", "north", None), &[print("3:aa"), gone], &mut firsts);
        assert_eq!(missing["differing_tables"], json!(["notes"]), "a table one run lacks is named");
        let reasons = reset_reasons(&json!({"id": "e", "reset": missing}));
        assert_eq!(
            reasons,
            [
                "the fingerprint after setup changed between resets in notes",
                "the fingerprint after setup differs in notes from the first reset of this world and setup, for task 'a': a run checked in between left data a reset did not restore"
            ]
        );
    }

    #[test]
    fn a_change_names_its_app_when_it_has_one() {
        let one = json!({"tbl": "deals", "op": "update", "pk": {"id": 1}, "columns": ["stage"]});
        assert_eq!(change(&one), r#"update deals {"id":1} (stage)"#);
        let two = json!({"app": "west", "tbl": "deals", "op": "delete", "pk": {"id": 2}});
        assert_eq!(change(&two), r#"delete west.deals {"id":2}"#);
    }
}
