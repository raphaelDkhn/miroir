use serde_json::{Value, json};

use crate::common::{Container, ENV_IMAGE, MYSQL_ENV_IMAGE, Options, Session, run_check};

const ASK: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/fixtures/acme-crm/coworker-tasks/ask-priya.yaml"
));

fn note(text: &str) -> Value {
    json!({"type": "call_tool", "tool_name": "api_request",
           "arguments": {"method": "POST", "path": "/deals/1100/notes", "body": {"text": text}}})
}

fn submit() -> Value {
    json!({"type": "call_tool", "tool_name": "submit", "arguments": {}})
}

fn start(task: &str) -> Container {
    Container::start(&Options {
        files: &[("/miroir/tasks/ask-priya.yaml", task)],
        ..Options::default()
    })
}

async fn pull(session: &mut Session) -> Value {
    let reply = session.send(json!({"type": "coworkers"})).await;
    assert_eq!(reply["type"], "coworkers", "{reply}");
    reply["data"]["turns"].clone()
}

async fn answer(session: &mut Session, text: &str) -> Value {
    session
        .send(json!({"type": "coworker_reply", "data": {"coworker": "priya", "text": text}}))
        .await
}

fn refused(reply: &Value, says: &str) {
    assert_eq!(reply["type"], "error", "{reply}");
    assert_eq!(reply["data"]["code"], "VALIDATION_ERROR", "{reply}");
    assert!(reply["data"]["message"].as_str().unwrap().contains(says), "{reply}");
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_coworker_task_needs_a_client_that_answers_for_them() {
    let env = start(ASK);
    let mut session = Session::connect(&env).await;
    let named = session.send(json!({"type": "reset", "data": {"task_id": "ask-priya"}})).await;
    refused(&named, "reset it with coworkers: true");
    let drawn = session.send(json!({"type": "reset", "data": {"split": "test"}})).await;
    refused(&drawn, "split 'test' has no tasks without coworkers; reset with coworkers: true");
    let reset = session.reset(json!({"split": "test", "coworkers": true})).await;
    assert_eq!(reset["observation"]["task_id"], "ask-priya", "{reset}");
    assert_eq!(reset["observation"]["metadata"]["coworkers"], json!(["priya"]), "{reset}");
    let train = session.reset(json!({"split": "train", "index": 0})).await;
    assert!(train["observation"]["metadata"].get("coworkers").is_none(), "{train}");
    let listed = reqwest::Client::new()
        .post(env.url("/acme-crm/tasks"))
        .json(&json!({"split": "test"}))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let task = &listed["tasks"][0];
    assert_eq!(task["coworkers"], json!(["priya"]), "{task}");
    assert!(!task.to_string().contains("fell back"), "a script never leaves the env: {task}");
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_coworker_answers_what_reached_them_as_themselves() {
    let env = start(ASK);
    let mut session = Session::connect(&env).await;
    session.reset(json!({"task_id": "ask-priya", "coworkers": true})).await;
    assert_eq!(pull(&mut session).await, json!([]), "nothing has reached her yet");
    refused(&answer(&mut session, "Hi.").await, "priya has no message to answer");
    session.step(note("Waiting on legal.")).await;
    assert_eq!(pull(&mut session).await, json!([]), "a note that doesn't name her");
    session.step(note("@Priya which stage is deal 1100 at?")).await;
    let turns = pull(&mut session).await;
    assert_eq!(turns.as_array().map(Vec::len), Some(1), "{turns}");
    assert_eq!(turns[0]["coworker"], "priya");
    let messages = turns[0]["messages"].as_array().unwrap();
    let system = messages[0]["content"].as_str().unwrap();
    assert!(system.starts_with("You are Priya Shah, account executive at Acme"), "{system}");
    assert!(system.contains("Dana") && system.contains("fell back to qualified"), "{system}");
    assert_eq!(
        messages[1],
        json!({"role": "user", "content": "@Priya which stage is deal 1100 at?"})
    );
    assert_eq!(pull(&mut session).await, turns, "a turn not answered comes back");

    let sent = answer(&mut session, "  Qualified: they asked to re-scope.  ").await;
    assert_eq!(sent["data"]["status"], 201, "{sent}");
    let notes = env.product_get("/deals/1100/notes");
    let last = notes.as_array().unwrap().last().unwrap().clone();
    assert_eq!(last["author_id"], 12, "the note is Priya's: {last}");
    assert_eq!(last["text"], "Qualified: they asked to re-scope.");
    assert_eq!(pull(&mut session).await, json!([]), "her own note is no message to her");

    session.step(note("@priya thanks. Who owns it now?")).await;
    let turns = pull(&mut session).await;
    let said: Vec<&str> = turns[0]["messages"].as_array().unwrap()[1..]
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(said, ["user", "assistant", "user"], "{turns}");

    session.step(submit()).await;
    assert_eq!(pull(&mut session).await, json!([]), "a done episode owes no answers");
    refused(&answer(&mut session, "Late.").await, "the episode is done");
    let notes = env.product_get("/deals/1100/notes");
    assert!(notes.as_array().unwrap().iter().all(|n| n["text"] != "Late."), "{notes}");

    let record = session.send(json!({"type": "record"})).await;
    let actions = record["data"]["actions"].as_array().unwrap();
    let replies: Vec<&Value> = actions.iter().filter(|a| a["tool"] == "coworker_reply").collect();
    assert_eq!(replies.len(), 1, "{actions:#?}");
    assert_eq!(replies[0]["via"], "coworker");
    assert_eq!(replies[0]["status"], 201);
    assert_eq!(replies[0]["path"], "/deals/1100/notes");
    let said = json!({"coworker": "priya", "text": "Qualified: they asked to re-scope."});
    assert_eq!(replies[0]["arguments"], said);
    let diagnostics = &record["data"]["diagnostics"];
    assert_eq!(diagnostics["coworker_replies"], 1, "{diagnostics}");
    assert_eq!(diagnostics["calls"], 4, "three notes and a submit: {diagnostics}");
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn no_mcp_call_reaches_a_coworker() {
    let env = start(ASK);
    let mut session = Session::connect(&env).await;
    session.reset(json!({"task_id": "ask-priya", "coworkers": true})).await;
    session.step(note("@priya which stage?")).await;
    let client = reqwest::Client::new();
    let rpc = |method: &str, params: Value| {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let request = client.post(env.url("/mcp")).json(&body);
        async move { request.send().await.unwrap().json::<Value>().await.unwrap() }
    };
    let call = rpc(
        "tools/call",
        json!({"name": "coworker_reply", "arguments": {"coworker": "priya", "text": "proposal"}}),
    )
    .await;
    let error = &call["result"]["error"];
    assert_eq!(error["error_type"], "tool_not_found", "{call}");
    for method in ["coworkers", "coworker_reply"] {
        let reply = rpc(method, json!({})).await;
        assert_eq!(reply["error"]["code"], -32601, "{method}: {reply}");
    }
    let notes = env.product_get("/deals/1100/notes");
    assert!(notes.as_array().unwrap().iter().all(|n| n["author_id"] != 12), "{notes}");
    let turns = pull(&mut session).await;
    assert_eq!(turns.as_array().map(Vec::len), Some(1), "she still owes her answer: {turns}");
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_broken_inbox_fails_the_reset() {
    let broken = ASK.replacen("FROM notes WHERE", "FROM nowhere WHERE", 1);
    let env = start(&broken);
    let mut session = Session::connect(&env).await;
    let reset = json!({"type": "reset", "data": {"task_id": "ask-priya", "coworkers": true}});
    let reply = session.send(reset).await;
    assert_eq!(reply["type"], "error", "{reply}");
    let message = reply["data"]["message"].as_str().unwrap();
    assert!(message.contains("'coworkers'") && message.contains("priya"), "{message}");
}

#[test]
#[ignore = "needs Docker"]
fn check_answers_for_coworkers_from_the_golden_run() {
    let unreached = ASK.replacen("@priya which stage", "which stage", 1);
    let (code, report, logs) =
        run_check(ENV_IMAGE, &[], &[("ask-priya", ASK), ("ask-nobody", &unreached)]);
    assert_eq!(code, 1, "{report:#}\n{logs}");
    let task = |id: &str| {
        report["tasks"].as_array().unwrap().iter().find(|t| t["id"] == id).unwrap().clone()
    };
    let asked = task("ask-priya");
    assert_eq!(asked["passed"], true, "{asked:#}");
    assert_eq!(asked["golden"]["statuses"], json!([201, 201, 200, "ok"]), "{asked:#}");
    assert_eq!(asked["golden"]["reward"], 1.0);
    assert_eq!(asked["null"]["passed"], true);
    assert_eq!(asked["probes"]["passed"], true, "a guess without asking earns nothing");
    let nobody = task("ask-nobody");
    assert_eq!(nobody["golden"]["passed"], false, "{nobody:#}");
    assert_eq!(nobody["golden"]["statuses"][1], "nothing reached priya", "{nobody:#}");
}

const MYSQL_TASK: &str = r#"
world: north
instruction: Nothing to do.
coworkers:
  - name: sam
    description: Sam, who runs the warehouse
    script: You know nothing about it.
    headers: {shop: {X-Api-Key: seed-key-ops}}
    inbox: {sql: "SELECT JSON_ARRAYAGG(JSON_OBJECT('id', 1, 'text', 'hello'))"}
    reply: {method: POST, path: /orders, body: {note: "{text}"}}
checks:
  - {name: none, weight: 1.0, db: episode, sql: SELECT 1}
allowed_changes: []
golden:
  - {tool: coworker_reply, arguments: {coworker: sam, text: Hi.}}
  - {tool: submit}
"#;

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_mysql_inbox_reads_too() {
    let env = Container::start(&Options {
        image: MYSQL_ENV_IMAGE,
        files: &[("/miroir/tasks/ask-sam.yaml", MYSQL_TASK)],
        ..Options::default()
    });
    let mut session = Session::connect(&env).await;
    session.reset(json!({"task_id": "ask-sam", "coworkers": true})).await;
    assert_eq!(pull(&mut session).await, json!([]), "the message there at reset counts as seen");
}
