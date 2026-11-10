use jiff::{SignedDuration, Timestamp};
use serde_json::{Value, json};

use crate::common::{Container, ENV_IMAGE, MYSQL_ENV_IMAGE, Options, Session, run_check};

const WATCH: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/fixtures/acme-crm/clock-tasks/watch-deal.yaml"
));
const CANCEL: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/fixtures/mini-shop/clock-tasks/cancel-later.yaml"
));

fn start(task: &str) -> Container {
    Container::start(&Options {
        files: &[("/miroir/tasks/watch-deal.yaml", task)],
        ..Options::default()
    })
}

fn call(tool: &str, arguments: &Value) -> Value {
    json!({"type": "call_tool", "tool_name": tool, "arguments": arguments})
}

fn wait(minutes: u32) -> Value {
    call("wait", &json!({"minutes": minutes}))
}

fn stage(stage: &str) -> Value {
    call(
        "api_request",
        &json!({"method": "PATCH", "path": "/deals/1100", "body": {"stage": stage}}),
    )
}

fn submit() -> Value {
    call("submit", &json!({}))
}

fn result(reply: &Value) -> &Value {
    &reply["data"]["observation"]["result"]
}

fn graded(reply: &Value) -> &Value {
    assert_eq!(reply["data"]["done"], true, "{reply}");
    &reply["data"]["observation"]["metadata"]
}

fn at(text: &Value) -> Timestamp {
    text.as_str().unwrap().parse().unwrap()
}

async fn names(session: &mut Session) -> Vec<String> {
    let listed = session.step(json!({"type": "list_tools"})).await;
    let tools = listed["data"]["observation"]["tools"].as_array().unwrap().clone();
    tools.iter().map(|t| t["name"].as_str().unwrap().to_string()).collect()
}

async fn mcp_names(env: &Container, query: &str) -> Vec<String> {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
    let reply = reqwest::Client::new().post(env.url(&format!("/mcp{query}"))).json(&body);
    let reply: Value = reply.send().await.unwrap().json().await.unwrap();
    let tools = reply["result"]["tools"].as_array().unwrap().clone();
    tools.iter().map(|t| t["name"].as_str().unwrap().to_string()).collect()
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn wait_is_listed_only_for_a_task_with_a_clock() {
    let env = start(WATCH);
    let mut session = Session::connect(&env).await;
    session.reset(json!({"task_id": "watch-deal", "episode_id": "e-clock"})).await;
    assert!(names(&mut session).await.contains(&"wait".to_string()));
    assert!(mcp_names(&env, "?episode_id=e-clock").await.contains(&"wait".to_string()));
    assert!(mcp_names(&env, "").await.contains(&"wait".to_string()), "one list for every task");
    session.reset(json!({"task_id": "close-won-deal", "episode_id": "e-plain"})).await;
    assert_eq!(names(&mut session).await, ["api_request", "api_docs", "submit"]);
    let plain = mcp_names(&env, "?episode_id=e-plain").await;
    assert_eq!(plain, ["api_request", "api_docs", "submit"]);
    let created = env.product_get("/deals/1100")["created_at"].clone();
    let refused = session.step(wait(30)).await;
    let error = &refused["data"]["observation"]["error"];
    assert_eq!(error["error_type"], "tool_not_found", "{refused}");
    assert!(error["message"].as_str().unwrap().contains("no clock"), "{refused}");
    assert_eq!(env.product_get("/deals/1100")["created_at"], created, "nothing moved");
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_wait_moves_the_data_and_wakes_at_the_first_event() {
    let env = start(WATCH);
    let mut session = Session::connect(&env).await;
    session.reset(json!({"task_id": "watch-deal"})).await;
    let deal = env.product_get("/deals/1100");
    let due = env.product_get("/invoices/4020")["due_date"].clone();
    assert_eq!(deal["synced_unix"], 0, "{deal}");

    let waited = session.step(wait(30)).await;
    assert_eq!(result(&waited), &json!({"waited_minutes": 30, "elapsed_minutes": 30}));
    let moved = env.product_get("/deals/1100");
    let back = at(&deal["created_at"]).duration_since(at(&moved["created_at"]));
    assert_eq!(back, SignedDuration::from_mins(30), "{deal} -> {moved}");
    assert_eq!(moved["synced_unix"], 0, "0 stands for never, and stays put");
    assert_eq!(env.product_get("/invoices/4020")["due_date"], due, "half an hour moves no date");

    let woke = session.step(wait(600)).await;
    assert_eq!(result(&woke), &json!({"waited_minutes": 60, "elapsed_minutes": 90}), "{woke}");
    let notes = env.product_get("/deals/1100/notes");
    let last = notes.as_array().unwrap().last().unwrap().clone();
    assert_eq!((&last["author_id"], &last["text"]), (&json!(12), &json!("The buyer signed.")));

    let day = session.step(wait(1_410)).await;
    assert_eq!(result(&day), &json!({"waited_minutes": 1_410, "elapsed_minutes": 1_500}));
    let moved = env.product_get("/deals/1100");
    let back = at(&deal["created_at"]).duration_since(at(&moved["created_at"]));
    assert_eq!(back, SignedDuration::from_mins(1_500));
    let due: jiff::civil::Date = due.as_str().unwrap()[..10].parse().unwrap();
    let moved_due: jiff::civil::Date =
        env.product_get("/invoices/4020")["due_date"].as_str().unwrap()[..10].parse().unwrap();
    assert_eq!(due.since(moved_due).unwrap().get_days(), 1, "a whole day of the clock passed");

    let done = session.step(submit()).await;
    let metadata = graded(&done);
    assert_eq!(
        metadata["clock"],
        json!({"minutes": 1_500, "events": [{"name": "signed", "minute": 90, "status": 201}]})
    );
    let record = session.send(json!({"type": "record"})).await;
    let actions = record["data"]["actions"].as_array().unwrap();
    let events: Vec<&Value> = actions.iter().filter(|a| a["tool"] == "event").collect();
    assert_eq!(events.len(), 1, "{actions:#?}");
    assert_eq!((&events[0]["via"], &events[0]["status"]), (&json!("event"), &json!(201)));
    assert_eq!(events[0]["arguments"], json!({"event": "signed"}));
    assert_eq!(events[0]["path"], "/deals/1100/notes");
    let diagnostics = &record["data"]["diagnostics"];
    assert_eq!((&diagnostics["events"], &diagnostics["calls"]), (&json!(1), &json!(4)));
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_row_changed_before_and_after_a_wait_changed_only_what_the_agent_changed() {
    let env = start(WATCH);
    let mut session = Session::connect(&env).await;
    session.reset(json!({"task_id": "watch-deal"})).await;
    session.step(stage("qualified")).await;
    session.step(wait(30)).await;
    session.step(stage("won")).await;
    let done = session.step(submit()).await;
    let metadata = graded(&done);
    assert_eq!(metadata["out_of_scope_count"], 0, "{metadata:#}");
    let record = session.send(json!({"type": "record"})).await;
    let changes = record["data"]["changes"].as_array().unwrap();
    let deal = changes.iter().find(|c| c["tbl"] == "deals").unwrap();
    assert_eq!(deal["columns"], json!(["stage", "updated_at"]), "{deal}");
    let counted = changes.iter().filter(|c| c["tbl"] == "notes").count();
    assert_eq!(counted, 1, "each change once, though a milestone wrote them first: {changes:#?}");
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_milestone_counts_at_its_minute_and_the_clock_runs_out_at_the_end() {
    let env = start(WATCH);
    let mut session = Session::connect(&env).await;
    let mut play = async |steps: &[Value]| {
        session.reset(json!({"task_id": "watch-deal"})).await;
        let mut last = Value::Null;
        for step in steps {
            last = session.step(step.clone()).await;
        }
        let record = session.send(json!({"type": "record"})).await;
        (graded(&last).clone(), last["data"]["reward"].clone(), record["data"].clone())
    };
    let (golden, reward, first) = play(&[wait(600), stage("won"), submit()]).await;
    assert_eq!(reward, 1.0, "{golden:#}");
    assert_eq!(golden["checks"][0]["by"], 210, "{golden:#}");
    assert_eq!(golden["clock"]["minutes"], 210, "the clock ran out to the milestone");
    let (_, reward, again) = play(&[wait(600), stage("won"), submit()]).await;
    assert_eq!(reward, 1.0);
    assert_eq!(first["signature"], again["signature"], "two runs that end alike");
    let (late, reward, _) = play(&[wait(600), wait(121), stage("won"), submit()]).await;
    assert_eq!(reward, 0.0, "won after the milestone's minute: {late:#}");
    let (early, reward, record) = play(&[stage("won"), submit()]).await;
    assert_eq!(reward, 0.0, "won before the note came: {early:#}");
    assert_ne!(first["signature"], record["signature"], "won before the wait, not after it");
    let (null, reward, record) = play(&[submit()]).await;
    assert_eq!(reward, 0.0, "{null:#}");
    assert_eq!(null["clock"]["events"][0]["minute"], 90, "the note came after the submit");
    let events = record["actions"].as_array().unwrap().iter().filter(|a| a["tool"] == "event");
    assert_eq!(events.count(), 1);
}

#[test]
#[ignore = "needs Docker"]
fn check_passes_a_clock_task_and_fails_one_whose_event_is_refused() {
    let anonymous =
        WATCH.replacen("    headers: {crm: {Authorization: Bearer seed-token-priya}}\n", "", 1);
    assert_ne!(anonymous, WATCH, "test patch does not apply");
    let (code, report, logs) =
        run_check(ENV_IMAGE, &[], &[("watch-deal", WATCH), ("watch-anonymous", &anonymous)]);
    assert_eq!(code, 1, "{report:#}\n{logs}");
    let task = |id: &str| {
        report["tasks"].as_array().unwrap().iter().find(|t| t["id"] == id).unwrap().clone()
    };
    let watched = task("watch-deal");
    assert_eq!(watched["passed"], true, "{watched:#}");
    assert_eq!(watched["golden"]["statuses"], json!(["ok", 200, "ok"]), "{watched:#}");
    assert_eq!(watched["null"]["reward"], 0.0, "doing nothing waits the whole task out");
    let anonymous = task("watch-anonymous");
    assert_eq!(anonymous["golden"]["passed"], false, "{anonymous:#}");
    let events = &anonymous["golden"]["metadata"]["clock"]["events"];
    assert_eq!(events[0]["status"], 401, "{anonymous:#}");
}

fn mysql(env: &Container, sql: &str) -> Vec<String> {
    let out = env.exec_ok(&["mysql", "-h127.0.0.1", "-uroot", "shop", "-N", "-e", sql]);
    out.split('\t').map(str::to_string).collect()
}

fn datetime(text: &str) -> jiff::civil::DateTime {
    text.parse().unwrap()
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_wait_moves_every_kind_of_mysql_time_and_the_change_log_with_it() {
    let env = Container::start(&Options {
        image: MYSQL_ENV_IMAGE,
        files: &[("/miroir/tasks/cancel-later.yaml", CANCEL)],
        ..Options::default()
    });
    let mut session = Session::connect(&env).await;
    session.reset(json!({"task_id": "cancel-later"})).await;
    let order = "SELECT placed_at, ship_by, updated_at FROM orders WHERE id = 1007";
    let days = "SELECT GROUP_CONCAT(day ORDER BY day), GROUP_CONCAT(valid_until ORDER BY day), \
                GROUP_CONCAT(stamp_until ORDER BY day), \
                GROUP_CONCAT(COALESCE(CAST(retired_at AS CHAR), '-') ORDER BY day), \
                GROUP_CONCAT(stamped_unix ORDER BY day) FROM price_days";
    mysql(&env, "UPDATE orders SET status = 'held' WHERE id = 1007");
    let (before, days_before) = (mysql(&env, order), mysql(&env, days));
    let waited = session.step(wait(1_500)).await;
    assert_eq!(result(&waited)["elapsed_minutes"], 1_500, "{waited}");
    let (after, days_after) = (mysql(&env, order), mysql(&env, days));
    let back = |i: usize| datetime(&before[i]).duration_since(datetime(&after[i]));
    assert_eq!(back(0), SignedDuration::from_mins(1_500), "DATETIME: {before:?} -> {after:?}");
    assert_eq!(back(2), SignedDuration::from_mins(1_500), "TIMESTAMP: {before:?} -> {after:?}");
    let date = |text: &str| text.parse::<jiff::civil::Date>().unwrap();
    assert_eq!(date(&before[1]).since(date(&after[1])).unwrap().get_days(), 1, "DATE");
    let day_lists = |row: &[String]| row[0].split(',').map(date).collect::<Vec<_>>();
    for (was, is) in day_lists(&days_before).iter().zip(day_lists(&days_after)) {
        assert_eq!(was.since(is).unwrap().get_days(), 1, "a DATE key moves, in order");
    }
    assert_eq!(days_after[1..4], days_before[1..4], "9999, 2038 and zero dates stay put");
    let unix =
        |row: &[String]| row[4].split(',').map(|n| n.parse::<i64>().unwrap()).collect::<Vec<_>>();
    for (was, is) in unix(&days_before).iter().zip(unix(&days_after)) {
        assert!(*was == 0 && is == 0 || was - is == 90_000, "{was} -> {is}");
    }
    mysql(&env, "UPDATE orders SET status = 'shipped' WHERE id = 1007");
    session
        .step(call("api_request", &json!({"method": "POST", "path": "/orders/1012/cancel"})))
        .await;
    let done = session.step(submit()).await;
    let metadata = graded(&done);
    assert_eq!(metadata["out_of_scope_count"], 0, "{metadata:#}");
    assert_eq!(done["data"]["reward"], 1.0, "scored at minute 2000: {metadata:#}");
}
