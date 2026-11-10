use serde_json::{Value, json};

use crate::common::{Container, ENV_IMAGE, Options, Session, run_check};

async fn rpc(env: &Container, query: &str, method: &str, params: Value) -> Value {
    let request = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
    let call = reqwest::Client::new().post(env.url(&format!("/mcp{query}"))).json(&request);
    call.send().await.unwrap().json().await.unwrap()
}

fn names(listed: &Value) -> Vec<&str> {
    let tools = listed["result"]["tools"].as_array().unwrap();
    tools.iter().map(|tool| tool["name"].as_str().unwrap()).collect()
}

fn call(name: &str, arguments: &Value) -> Value {
    json!({"type": "call_tool", "tool_name": name, "arguments": arguments})
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn the_mcp_surface_lists_the_apps_tools_and_submit() {
    let env = Container::start(&Options::default());
    let mut session = Session::connect(&env).await;
    session.reset(json!({"task_id": "close-won-deal"})).await;
    let own = rpc(&env, "", "tools/list", json!({})).await;
    assert_eq!(names(&own), ["api_request", "api_docs", "submit"], "/mcp lists what it did");
    let api = rpc(&env, "?surface=api", "tools/list", json!({})).await;
    assert_eq!(names(&api), names(&own));
    let listed = rpc(&env, "?surface=mcp", "tools/list", json!({})).await;
    assert_eq!(names(&listed), ["whoami", "get_deal", "update_deal", "submit"]);
    assert_eq!(listed["result"]["tools"][1]["inputSchema"]["required"], json!(["deal_id"]));
    for surface in ["cli", ""] {
        let refused = rpc(&env, &format!("?surface={surface}"), "tools/list", json!({})).await;
        assert_eq!(refused["error"]["code"], -32602, "{refused}");
        let message = refused["error"]["message"].as_str().unwrap();
        assert!(message.contains(&format!("unknown surface '{surface}'")), "{refused}");
    }
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn an_mcp_call_acts_as_the_persona_and_is_recorded_and_graded() {
    let env = Container::start(&Options::default());
    let mut session = Session::connect(&env).await;
    session.reset(json!({"task_id": "close-won-deal"})).await;
    let me = session.step(call("whoami", &json!({}))).await;
    let text = me["data"]["observation"]["result"]["content"][0]["text"].as_str().unwrap();
    let user: Value = serde_json::from_str(text).unwrap();
    assert_eq!(user["email"], "dana@acme.test", "the persona's token reached the API: {me}");

    let won = json!({"name": "update_deal",
                     "arguments": {"deal_id": 1100, "status": "won", "stage": "won"}});
    let over_mcp = rpc(&env, "?surface=mcp", "tools/call", won).await;
    assert_eq!(over_mcp["result"]["isError"], false, "{over_mcp}");
    let deal: Value =
        serde_json::from_str(over_mcp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(deal["status"], "won", "the server's content comes through as it was");

    let missing = session.step(call("update_deal", &json!({"deal_id": 999_999}))).await;
    let error = &missing["data"]["observation"]["error"];
    assert_eq!(error["error_type"], "execution_error", "the server's error is the agent's");
    let unknown = session.step(call("delete_deal", &json!({"deal_id": 1100}))).await;
    let error = &unknown["data"]["observation"]["error"];
    assert_eq!(error["error_type"], "tool_not_found", "{unknown}");

    let done = session.step(call("submit", &json!({"answer": "Deal 1100 is won."}))).await;
    assert_eq!(done["data"]["reward"], 1.0, "graded on the same diff: {done}");
    let record = session.send(json!({"type": "record"})).await["data"].clone();
    let rows: Vec<Value> = record["actions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| json!([a["tool"], a["method"], a["path"], a["status"], a.get("app")]))
        .collect();
    assert_eq!(
        rows,
        [
            json!(["whoami", null, null, null, null]),
            json!(["update_deal", null, null, null, null]),
            json!(["update_deal", null, null, null, null]),
            json!(["delete_deal", null, null, null, null]),
            json!(["submit", null, null, null, null]),
        ]
    );
    assert_eq!(record["diagnostics"]["calls"], 5);
}

#[test]
#[ignore = "needs Docker"]
fn check_fails_a_probe_whose_tool_the_server_lacks() {
    let task = r"
instruction: Mark deal 1100 as won.
checks:
  - {name: won, weight: 1.0, sql: SELECT status = 'won' FROM deals WHERE id = 1100}
allowed_changes:
  - {table: deals, op: update, columns: [status, stage, updated_at]}
golden:
  - {tool: update_deal, arguments: {deal_id: 1100, status: won, stage: won}}
  - {tool: submit, arguments: {}}
probes:
  - name: renamed-away
    steps: [{tool: close_deal, arguments: {deal_id: 1100}}, {tool: submit, arguments: {}}]
    max_reward: 0
";
    let (code, report, logs) = run_check(ENV_IMAGE, &[], &[("through-mcp", task)]);
    assert_eq!(code, 1, "{report:#}\n{logs}");
    let found = &report["tasks"][0];
    assert_eq!(found["golden"]["passed"], true, "a golden run may call the server's tools");
    assert_eq!(found["golden"]["statuses"], json!(["ok", "ok"]));
    let run = &found["probes"]["runs"][0];
    let error = run["error"].as_str().unwrap_or_default();
    assert!(error.contains("calls 'close_deal', which is no tool"), "{run:#}");
    assert_eq!(report["stages"]["mcp"], json!({"passed": true, "tools": 3}));
}
