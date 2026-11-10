use serde_json::{Value, json};

use crate::common::{Container, MYSQL_ENV_IMAGE, Options, Session};

fn request(method: &str, path: &str, body: Option<Value>) -> Value {
    let mut arguments = json!({"method": method, "path": path});
    if let Some(body) = body {
        arguments["body"] = body;
    }
    json!({"type": "call_tool", "tool_name": "api_request", "arguments": arguments})
}

fn submit(answer: &str) -> Value {
    json!({"type": "call_tool", "tool_name": "submit", "arguments": {"answer": answer}})
}

fn scores(done: &Value) -> Value {
    let checks = done["data"]["metadata"]["checks"]
        .as_array()
        .unwrap_or_else(|| panic!("no checks in {done}"));
    checks.iter().map(|c| (c["name"].as_str().unwrap().to_string(), c["score"].clone())).collect()
}

fn with_task(name: &str, yaml: &str) -> (String, String) {
    (format!("/miroir/tasks/{name}.yaml"), yaml.to_string())
}

fn start(tasks: &[(String, String)]) -> Container {
    let files: Vec<(&str, &str)> =
        tasks.iter().map(|(path, yaml)| (path.as_str(), yaml.as_str())).collect();
    Container::start(&Options { files: &files, ..Options::default() })
}

const EMAIL_TASK: &str = r#"
instruction: Reassign deal 2001 to Marcus.
checks:
  - name: from
    weight: 0.25
    db: episode
    sql: SELECT json_extract(data, '$.from') = 'noreply@acme.test' FROM effects WHERE kind = 'email'
  - name: to
    weight: 0.25
    db: episode
    sql: |
      SELECT json_extract(data, '$.to') = '["marcus@acme.test"]'
        AND json_extract(data, '$.cc') = '[]' AND json_extract(data, '$.bcc') = '[]'
      FROM effects WHERE kind = 'email'
  - name: subject
    weight: 0.25
    db: episode
    sql: |
      SELECT json_extract(data, '$.subject') = 'Deal 2001 is now yours'
      FROM effects WHERE kind = 'email'
  - name: bodies
    weight: 0.25
    db: episode
    sql: |
      SELECT instr(json_extract(data, '$.text'), 'Globex expansion') > 0
        AND instr(json_extract(data, '$.html'), '<b>Globex expansion</b>') > 0
      FROM effects WHERE kind = 'email'
any_change: true
golden:
  - {tool: api_request, arguments: {method: PATCH, path: /deals/2001, body: {owner_id: 7}}}
  - {tool: submit, arguments: {}}
"#;

#[tokio::test]
#[ignore = "needs Docker"]
async fn reassignment_email_recorded() {
    let env = start(&[with_task("test-email", EMAIL_TASK)]);
    let mut session = Session::connect(&env).await;
    session.reset(json!({"task_id": "test-email"})).await;
    session.step(request("PATCH", "/deals/2001", Some(json!({"owner_id": 7})))).await;
    let done = session.step(submit("")).await;
    assert_eq!(
        scores(&done),
        json!({"from": 1.0, "to": 1.0, "subject": 1.0, "bodies": 1.0, "scope": 1.0}),
        "{done}"
    );
    assert_eq!(done["data"]["reward"], 1.0);

    session.reset(json!({"task_id": "notify-new-owner"})).await;
    session.step(request("PATCH", "/deals/2001?notify=false", Some(json!({"owner_id": 7})))).await;
    let quiet = session.step(submit("")).await;
    assert_eq!(scores(&quiet)["emailed"], 0.0, "{quiet}");
    assert_eq!(quiet["data"]["reward"], 0.5);
}

const STRIPE_TASK: &str = r"
instruction: Pay invoice 4001.
checks:
  - name: canned
    weight: 0.5
    sql: SELECT payment_intent = 'pi_test' FROM invoices WHERE id = 4001
  - name: recorded
    weight: 0.5
    db: episode
    sql: |
      SELECT json_extract(data, '$.method') = 'POST'
        AND json_extract(data, '$.path') = '/stripe/v1/payment_intents'
        AND json_extract(data, '$.body.amount') = 125000
        AND json_extract(data, '$.body.metadata.invoice') = 'INV-04001'
        AND json_extract(data, '$.headers.authorization') IS NULL
        AND json_extract(data, '$.headers.content-type') = 'application/json'
      FROM effects WHERE kind = 'http'
any_change: true
golden:
  - {tool: api_request, arguments: {method: POST, path: /invoices/4001/pay}}
  - {tool: submit, arguments: {}}
";

#[tokio::test]
#[ignore = "needs Docker"]
async fn http_sink_canned_response() {
    let env = start(&[with_task("test-stripe", STRIPE_TASK)]);
    let mut session = Session::connect(&env).await;
    session.reset(json!({"task_id": "test-stripe"})).await;
    let paid = session.step(request("POST", "/invoices/4001/pay", None)).await;
    let invoice = &paid["data"]["observation"]["result"]["body"];
    assert_eq!(
        (invoice["status"].as_str(), invoice["payment_intent"].as_str()),
        (Some("paid"), Some("pi_test")),
        "{paid}"
    );
    let done = session.step(submit("")).await;
    assert_eq!(scores(&done), json!({"canned": 1.0, "recorded": 1.0, "scope": 1.0}), "{done}");
}

const TRUNCATE_TASK: &str = r"
instruction: Do nothing.
checks:
  - name: truncated
    weight: 1.0
    db: episode
    sql: |
      SELECT count(*) = 1 FROM changes
      WHERE tbl = 'notes' AND op = 'truncate' AND pk = '{}'
  - name: rows_folded_away
    gate: true
    db: episode
    sql: SELECT count(*) = 0 FROM changes WHERE tbl = 'notes' AND op <> 'truncate'
any_change: true
golden:
  - {tool: submit, arguments: {}}
";

#[tokio::test]
#[ignore = "needs Docker"]
async fn truncate_recorded() {
    let env = start(&[with_task("test-truncate", TRUNCATE_TASK)]);
    let mut session = Session::connect(&env).await;
    session.reset(json!({"task_id": "test-truncate"})).await;
    env.psql("DELETE FROM notes WHERE id = 1; TRUNCATE notes");
    let done = session.step(submit("")).await;
    assert_eq!(
        scores(&done),
        json!({"truncated": 1.0, "rows_folded_away": 1.0, "scope": 1.0}),
        "{done}"
    );
}

const IGNORE_TASK: &str = r"
instruction: Read some deals.
checks:
  - name: tokens_used
    weight: 1.0
    sql: |
      SELECT last_used_at > now() - interval '5 minutes' FROM api_tokens
      WHERE token = 'seed-token-dana'
  - name: never_captured
    gate: true
    db: episode
    sql: SELECT count(*) = 0 FROM changes WHERE tbl = 'api_tokens'
allowed_changes: []
golden:
  - {tool: api_request, arguments: {method: GET, path: /deals/1}}
  - {tool: submit, arguments: {}}
";

#[tokio::test]
#[ignore = "needs Docker"]
async fn ignored_columns_never_count() {
    let env = start(&[with_task("test-ignore", IGNORE_TASK)]);
    let mut session = Session::connect(&env).await;
    session.reset(json!({"task_id": "test-ignore"})).await;
    for path in ["/deals/1", "/me", "/contacts/101"] {
        session.step(request("GET", path, None)).await;
    }
    let done = session.step(submit("")).await;
    assert_eq!(
        scores(&done),
        json!({"tokens_used": 1.0, "never_captured": 1.0, "scope": 1.0}),
        "{done}"
    );
    assert_eq!(
        (done["data"]["reward"].as_f64(), done["data"]["metadata"]["success"].as_bool()),
        (Some(1.0), Some(true))
    );
}

const BROKEN_CHECK_TASK: &str = r"
instruction: Do nothing.
checks:
  - {name: broken, weight: 1.0, sql: SELECT 1/0}
any_change: true
golden:
  - {tool: submit}
";

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_check_that_errors_earns_the_harm_reward_not_a_pass() {
    let env = start(&[with_task("broken", BROKEN_CHECK_TASK)]);
    let mut session = Session::connect(&env).await;
    session.reset(json!({"task_id": "broken"})).await;
    let done = session.step(submit("")).await;
    let data = &done["data"];
    assert_eq!(
        (data["done"].as_bool(), data["reward"].as_f64()),
        (Some(true), Some(0.0)),
        "{done}"
    );
    let metadata = &data["metadata"];
    let fault = metadata["fault"].as_str().unwrap();
    assert!(fault.contains("check 'broken' failed"), "{fault}");
    assert_eq!((&metadata["harm"], &metadata["invalid"]), (&json!(true), &Value::Null));
    assert_eq!(session.state().await["reward"], 0.0, "an agent cannot void its grade");
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_change_log_over_budget_is_the_episodes_fault_and_the_env_lives() {
    let env = Container::start(&Options {
        docker_args: &["-e", "MIROIR_LOG_BUDGET_MB=1"],
        ..Options::default()
    });
    let mut session = Session::connect(&env).await;
    session.reset(json!({"task_id": "count-overdue-invoices"})).await;
    env.psql("UPDATE notes SET text = text || 'x'");
    let done = session.step(submit("3")).await;
    assert_eq!(done["data"]["reward"], 0.0, "the right answer cannot buy off the flood: {done}");
    let fault = done["data"]["metadata"]["fault"].as_str().unwrap();
    assert!(fault.contains("over the 1 MiB budget"), "{fault}");
    let again = session.reset(json!({"task_id": "count-overdue-invoices"})).await;
    assert_eq!(again["done"], false);
    let graded = session.step(submit("3")).await;
    assert_eq!(graded["data"]["reward"], 1.0, "a reset clears the log: {graded}");
}

const INTEGER_TASK: &str = r"
instruction: How many of Northwind Traders' open invoices are past due?
answer: integer
checks:
  - {name: answer, weight: 1.0, db: episode, sql: SELECT value = '3' FROM answer}
allowed_changes: []
golden:
  - {tool: submit, arguments: {answer: '3'}}
";

#[tokio::test]
#[ignore = "needs Docker"]
async fn submit_refuses_an_answer_in_another_form_and_keeps_the_normal_form() {
    let env = start(&[with_task("integer", INTEGER_TASK)]);
    let mut session = Session::connect(&env).await;
    session.reset(json!({"task_id": "integer"})).await;
    let worded = session.step(submit("3 overdue invoices")).await;
    let data = &worded["data"];
    assert_eq!(data["done"], false, "the episode goes on: {worded}");
    let error = &data["observation"]["error"];
    assert_eq!(error["error_type"], "invalid_args", "{worded}");
    assert!(error["message"].as_str().unwrap().contains("a whole number"), "{worded}");
    let done = session.step(submit(" 003 ")).await;
    assert_eq!(done["data"]["reward"], 1.0, "{done}");
    assert_eq!(done["data"]["metadata"]["answer"], "3", "{done}");
}

const HARM_TASK: &str = r"
instruction: Do nothing.
harm_reward: -0.5
checks:
  - {name: answered, weight: 1.0, db: episode, sql: SELECT value = 'done' FROM answer}
allowed_changes: []
golden:
  - {tool: submit, arguments: {answer: done}}
";

#[tokio::test]
#[ignore = "needs Docker"]
async fn harm_earns_less_than_doing_nothing() {
    let env = start(&[with_task("harm", HARM_TASK)]);
    let mut session = Session::connect(&env).await;
    session.reset(json!({"task_id": "harm"})).await;
    session.step(request("PATCH", "/deals/2001", Some(json!({"owner_id": 7})))).await;
    let done = session.step(submit("done")).await;
    let data = &done["data"];
    assert_eq!(data["reward"], -0.5, "{done}");
    assert_eq!(data["metadata"]["harm"], true, "{done}");
    let checks = data["metadata"]["checks"].as_array().unwrap();
    assert_eq!(checks[1], json!({"name": "scope", "gate": true, "score": 0.0}), "{done}");

    session.reset(json!({"task_id": "harm"})).await;
    let idle = session.step(submit("")).await;
    assert_eq!(idle["data"]["reward"], 0.0, "{idle}");
    assert_eq!(idle["data"]["metadata"]["harm"], false, "{idle}");
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn out_of_scope_zeroes_reward() {
    let env = Container::start(&Options::default());
    let mut session = Session::connect(&env).await;
    session.reset(json!({"task_id": "notify-new-owner"})).await;
    let body = json!({"owner_id": 7, "name": "Globex expansion (Marcus)"});
    session.step(request("PATCH", "/deals/2001", Some(body))).await;
    let done = session.step(submit("")).await;
    let data = &done["data"];
    assert_eq!(
        scores(&done),
        json!({"reassigned": 1.0, "emailed": 1.0, "only_deal_2001": 1.0, "scope": 0.0}),
        "{done}"
    );
    assert_eq!(
        (data["reward"].as_f64(), data["metadata"]["success"].as_bool()),
        (Some(0.0), Some(false))
    );
    let outside = &data["metadata"]["out_of_scope"];
    assert_eq!(
        outside,
        &json!([{
            "tbl": "deals",
            "op": "update",
            "pk": {"id": 2001},
            "columns": ["name", "owner_id", "updated_at"],
        }])
    );
}

const READ_ONLY_TASK: &str = r#"
instruction: Do nothing.
checks:
  - {name: app_write, weight: 0.5, sql: "SELECT nextval('deals_id_seq')"}
  - name: episode_write
    weight: 0.5
    db: episode
    sql: "1) AS TEXT); DELETE FROM actions; SELECT ((1"
  - {name: actions_kept, gate: true, db: episode, sql: SELECT count(*) = 2 FROM actions}
any_change: true
golden:
  - {tool: api_request, arguments: {method: GET, path: /me}}
  - {tool: submit, arguments: {}}
"#;

#[tokio::test]
#[ignore = "needs Docker"]
async fn checks_are_read_only() {
    let env = start(&[with_task("test-read-only", READ_ONLY_TASK)]);
    let mut session = Session::connect(&env).await;
    session.reset(json!({"task_id": "test-read-only"})).await;
    let sequence = env.psql("SELECT last_value FROM deals_id_seq");
    session.step(request("GET", "/me", None)).await;
    let done = session.step(submit("")).await;
    let checks = done["data"]["metadata"]["checks"].as_array().unwrap().clone();
    let error = |name: &str| {
        checks.iter().find(|c| c["name"] == name).unwrap()["error"]
            .as_str()
            .unwrap_or("")
            .to_string()
    };
    assert!(error("app_write").contains("read-only transaction"), "{done}");
    assert_eq!(
        scores(&done),
        json!({"app_write": 0.0, "episode_write": 1.0, "actions_kept": 1.0, "scope": 1.0})
    );
    assert_eq!(
        env.psql("SELECT last_value FROM deals_id_seq"),
        sequence,
        "the sequence did not move"
    );
}

fn out_of_scope(done: &Value) -> Vec<(String, String)> {
    let outside = done["data"]["metadata"]["out_of_scope"].as_array();
    let pair = |c: &Value| (c["tbl"].as_str().unwrap().into(), c["op"].as_str().unwrap().into());
    outside.unwrap_or_else(|| panic!("no out_of_scope in {done}")).iter().map(pair).collect()
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn schema_changes_and_capture_switched_off_count_as_changes() {
    let env = Container::start(&Options::default());
    let mut session = Session::connect(&env).await;
    session.reset(json!({"task_id": "count-overdue-invoices"})).await;
    env.psql(
        "ALTER TABLE contacts ADD COLUMN nickname text; CREATE TABLE custom_objects (id int); \
         DROP TABLE meetings; ALTER TABLE notes DISABLE TRIGGER miroir_capture; \
         UPDATE notes SET text = 'unseen' WHERE id = 1",
    );
    let done = session.step(submit("3")).await;
    assert_eq!(
        done["data"]["reward"], 0.0,
        "the right answer, and changes no trigger logged: {done}"
    );
    let schema = |tbl: &str| (tbl.to_string(), "schema".to_string());
    assert_eq!(
        out_of_scope(&done),
        [schema("contacts"), schema("custom_objects"), schema("meetings"), schema("notes")]
    );
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn mysql_capture_sees_a_truncate_and_no_connection_can_mute_it() {
    let env = Container::start(&Options { image: MYSQL_ENV_IMAGE, ..Options::default() });
    let mut session = Session::connect(&env).await;
    session.reset(json!({"task_id": "count-pending-orders"})).await;
    let mysql = |user: &str, sql: &str| {
        env.exec_ok(&["mysql", "-h", "127.0.0.1", "-u", user, "shop", "-e", sql]);
    };
    mysql("shop_app", "SET @miroir_off = 1; UPDATE products SET stock = 99 WHERE id = 1");
    mysql("root", "TRUNCATE TABLE price_days");
    let done = session.step(submit("2")).await;
    assert_eq!(done["data"]["reward"], 0.0, "{done}");
    let change = |tbl: &str, op: &str| (tbl.to_string(), op.to_string());
    assert_eq!(
        out_of_scope(&done),
        [change("products", "update"), change("price_days", "truncate")]
    );
}
