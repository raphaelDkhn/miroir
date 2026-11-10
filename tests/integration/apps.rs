use jiff::SignedDuration;
use serde_json::{Value, json};

use crate::common::{APPS_IMAGE, Container, Options, Session, baked_at, run_check};

fn call(app: &str, method: &str, path: &str, body: Option<Value>) -> Value {
    let mut arguments = json!({"app": app, "method": method, "path": path});
    if let Some(body) = body {
        arguments["body"] = body;
    }
    json!({"type": "call_tool", "tool_name": "api_request", "arguments": arguments})
}

fn docs(app: &str) -> Value {
    json!({"type": "call_tool", "tool_name": "api_docs", "arguments": {"app": app}})
}

fn submit() -> Value {
    json!({"type": "call_tool", "tool_name": "submit", "arguments": {}})
}

fn won() -> Value {
    json!({"status": "won", "stage": "won"})
}

fn scores(done: &Value) -> Value {
    let checks = done["data"]["metadata"]["checks"].as_array().unwrap();
    checks.iter().map(|c| (c["name"].as_str().unwrap().to_string(), c["score"].clone())).collect()
}

fn start(files: &[(&str, &str)]) -> Container {
    Container::start(&Options { image: APPS_IMAGE, files, ..Options::default() })
}

#[test]
#[ignore = "needs Docker"]
fn apps_check_passes() {
    let (code, report, logs) = run_check(APPS_IMAGE, &[], &[]);
    assert_eq!(code, 0, "{report:#}\n{logs}");
    let tasks = report["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 2);
    for task in tasks {
        for stage in ["golden", "null", "repeat", "reset", "writes", "drift"] {
            assert_eq!(task[stage]["passed"], true, "{}: {stage}", task["id"]);
        }
    }
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn each_app_has_its_own_api_headers_and_docs() {
    let env = start(&[]);
    let mut session = Session::connect(&env).await;
    session.reset(json!({"task_id": "close-east-note-west"})).await;
    let listed = session.step(json!({"type": "list_tools"})).await;
    let tools = listed["data"]["observation"]["tools"].as_array().unwrap().clone();
    let schema =
        |name: &str| tools.iter().find(|t| t["name"] == name).unwrap()["input_schema"].clone();
    let request = schema("api_request");
    assert_eq!(request["properties"]["app"]["enum"], json!(["east", "west", "west-ops"]));
    assert_eq!(request["required"], json!(["app", "method", "path"]));
    assert_eq!(
        schema("api_docs")["properties"]["app"]["enum"],
        json!(["east"]),
        "only east has docs"
    );
    for (app, name) in [("east", "Dana Lee"), ("west", "Alex Kim"), ("west-ops", "Sam Ortiz")] {
        let me = session.step(call(app, "GET", "/me", None)).await;
        assert_eq!(me["data"]["observation"]["result"]["body"]["name"], name, "{me}");
    }
    let bare = json!({"type": "call_tool", "tool_name": "api_request",
                      "arguments": {"method": "GET", "path": "/me"}});
    let error = |reply: &Value| reply["data"]["observation"]["error"]["message"].clone();
    let missing = session.step(bare).await;
    assert_eq!(error(&missing), "app is required: one of east, west, west-ops", "{missing}");
    let ledger = session.step(call("ledger", "GET", "/me", None)).await;
    assert!(error(&ledger).as_str().unwrap().starts_with("unknown app 'ledger'"), "{ledger}");
    let east = session.step(docs("east")).await;
    assert!(
        east["data"]["observation"]["result"]["operations"]
            .as_array()
            .is_some_and(|o| !o.is_empty())
    );
    let west = session.step(docs("west")).await;
    assert_eq!(west["data"]["observation"]["error"]["error_type"], "tool_not_found", "{west}");
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn changes_calls_and_scope_name_their_app() {
    let env = start(&[]);
    let mut session = Session::connect(&env).await;
    session.reset(json!({"task_id": "close-east-note-west"})).await;
    session.step(call("west", "PATCH", "/deals/1100", Some(won()))).await;
    let done = session.step(submit()).await;
    let outside = &done["data"]["metadata"]["out_of_scope"];
    assert_eq!(
        (&outside[0]["app"], &outside[0]["tbl"]),
        (&json!("west"), &json!("deals")),
        "{done}"
    );
    assert_eq!(scores(&done)["west_still_open"], 0.0, "{done}");
    let record = session.send(json!({"type": "record"})).await["data"].clone();
    assert_eq!(record["actions"][0]["app"], "west", "{record}");
    let changes = record["changes"].as_array().unwrap();
    assert!(!changes.is_empty() && changes.iter().all(|c| c["app"] == "west"), "{record}");
    assert_eq!(env.psql_in("east", "SELECT status FROM deals WHERE id = 1100"), "open");
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_second_api_writes_under_its_databases_app() {
    let env = start(&[]);
    let mut session = Session::connect(&env).await;
    session.reset(json!({"task_id": "close-east-note-west"})).await;
    session.step(call("east", "PATCH", "/deals/1100", Some(won()))).await;
    let note = Some(json!({"text": "The East office closed this deal."}));
    session.step(call("west-ops", "POST", "/deals/1100/notes", note)).await;
    let done = session.step(submit()).await;
    let scores = scores(&done);
    assert_eq!(scores["scope"], 1.0, "west's allowed changes cover west-ops' writes: {done}");
    assert_eq!(scores["one_note"], 1.0, "{done}");
    assert_eq!(scores["noted"], 0.0, "Sam, not Alex, wrote it: {done}");
    let record = session.send(json!({"type": "record"})).await["data"].clone();
    assert_eq!(record["actions"][1]["app"], "west-ops", "{record}");
    let notes: Vec<&Value> =
        record["changes"].as_array().unwrap().iter().filter(|c| c["tbl"] == "notes").collect();
    assert_eq!(notes.len(), 1, "{record}");
    assert_eq!(notes[0]["app"], "west", "{record}");
}

const LEDGER_TASK: &str = r"
instruction: Leave the books alone.
checks:
  - {name: one_entry, weight: 1.0, app: ledger, sql: SELECT count(*) = 1 FROM entries}
allowed_changes: []
golden:
  - {tool: submit, arguments: {}}
";

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_database_without_an_api_is_graded() {
    let env = start(&[("/miroir/tasks/ledger.yaml", LEDGER_TASK)]);
    let mut session = Session::connect(&env).await;
    session.reset(json!({"task_id": "ledger"})).await;
    env.psql_in("ledger", "INSERT INTO entries (memo, booked) VALUES ('stray', current_date)");
    let done = session.step(submit()).await;
    let outside = &done["data"]["metadata"]["out_of_scope"];
    assert_eq!(
        (&outside[0]["app"], &outside[0]["tbl"]),
        (&json!("ledger"), &json!("entries")),
        "{done}"
    );
    assert_eq!(scores(&done)["one_entry"], 0.0, "the check read the ledger: {done}");
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn each_database_shifts_by_its_own_time() {
    let baked = baked_at(APPS_IMAGE);
    let anchor = baked - SignedDuration::from_hours(240);
    let manifest = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/acme-apps/miroir.yaml"
    ))
    .unwrap()
    .replace("anchor: bake", &format!("anchor: \"{anchor}\""));
    let env = start(&[("/miroir/miroir.yaml", &manifest)]);
    let mut session = Session::connect(&env).await;
    session.reset(json!({"task_id": "count-overdue-both"})).await;
    let utc = |t: jiff::Timestamp| t.to_zoned(jiff::tz::TimeZone::UTC).date();
    let shift = (utc(jiff::Timestamp::now()) - utc(anchor)).get_days();
    let date = |db: &str, sql: &str| env.psql_in(db, sql).parse::<jiff::civil::Date>().unwrap();
    let birthday = "SELECT birthday FROM contacts WHERE birthday IS NOT NULL ORDER BY id LIMIT 1";
    let moved = (date("west", birthday) - date("east", birthday)).get_days();
    assert_eq!(moved, shift, "east excludes birthdays, west moves them");
    let close = "SELECT expected_close FROM deals WHERE id = 1100";
    assert_eq!(date("east", close), date("west", close), "both move their other dates alike");
    let booked = env.psql_in("ledger", "SELECT booked - current_date FROM entries WHERE id = 1");
    let at_bake = (utc(baked) - utc(jiff::Timestamp::now())).get_days() - 30;
    assert_eq!(booked.parse::<i32>().unwrap(), at_bake + shift, "the ledger's date moved too");
}

const URL_TASK: &str = r#"
instruction: Do nothing.
checks:
  - name: west_url
    weight: 0.5
    app: west
    command:
      - sh
      - -c
      - test "$(psql "$MIROIR_DATABASE_URL" -tAc 'SELECT current_database()')" = west
  - name: no_url
    weight: 0.5
    command: [sh, -c, 'test -z "$MIROIR_DATABASE_URL"']
allowed_changes: []
golden:
  - {tool: submit, arguments: {}}
"#;

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_command_check_gets_its_apps_url() {
    let env = start(&[("/miroir/tasks/url.yaml", URL_TASK)]);
    let mut session = Session::connect(&env).await;
    session.reset(json!({"task_id": "url"})).await;
    let done = session.step(submit()).await;
    assert_eq!(scores(&done), json!({"west_url": 1.0, "no_url": 1.0, "scope": 1.0}), "{done}");
}
