use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::common::{
    Container, ENV_IMAGE, Options, SANDBOX_IMAGE, Session, docker, docker_ok, run_check,
    run_check_with_sandbox,
};

async fn rpc(env: &Container, query: &str, method: &str) -> Value {
    let request = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": {}});
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

async fn run(session: &mut Session, command: &str) -> String {
    let reply = session.step(call("run_code", &json!({"command": command}))).await;
    let observation = &reply["data"]["observation"];
    assert!(observation["error"].is_null(), "{reply}");
    assert_eq!(observation["result"]["exit_code"], 0, "{reply}");
    observation["result"]["output"].as_str().unwrap().to_string()
}

fn paired() -> Container {
    let env = Container::start(&Options { sandbox: true, ..Options::default() });
    env.wait_healthy();
    env
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn the_code_surface_lists_the_code_tool_and_needs_a_sandbox() {
    let env = paired();
    let code = rpc(&env, "?surface=code", "tools/list").await;
    assert_eq!(names(&code), ["api_docs", "run_code", "submit"]);
    let own = rpc(&env, "", "tools/list").await;
    assert_eq!(names(&own), ["api_request", "api_docs", "submit"], "the default is as it was");

    let alone = Container::start(&Options::default());
    let refused = rpc(&alone, "?surface=code", "tools/list").await;
    assert_eq!(refused["error"]["code"], -32602, "{refused}");
    assert!(refused["error"]["message"].as_str().unwrap().contains("needs a sandbox"));
    let mut session = Session::connect(&alone).await;
    session.reset(json!({"task_id": "close-won-deal"})).await;
    let reply = session.step(call("run_code", &json!({"command": "ls"}))).await;
    let error = &reply["data"]["observation"]["error"];
    assert_eq!(error["error_type"], "tool_not_found", "{reply}");
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn code_acts_as_the_persona_and_each_request_it_makes_is_recorded_and_graded() {
    let env = paired();
    let mut session = Session::connect(&env).await;
    session.reset(json!({"task_id": "close-won-deal"})).await;
    let me = run(&mut session, r#"curl -s "$API_CRM/me""#).await;
    let user: Value = serde_json::from_str(&me).unwrap();
    assert_eq!(user["email"], "dana@acme.test", "the persona's token reached the API: {me}");
    let patch = r#"curl -s -o /dev/null -w '%{http_code}' -X PATCH -H 'content-type: application/json' \
        -d '{"status": "won", "stage": "won"}' "$API_CRM/deals/1100""#;
    assert_eq!(run(&mut session, patch).await, "200");
    let done = session.step(call("submit", &json!({"answer": "Deal 1100 is won."}))).await;
    assert_eq!(done["data"]["reward"], 1.0, "graded on the diff the code made: {done}");

    let record = session.send(json!({"type": "record"})).await["data"].clone();
    let rows: Vec<Value> = record["actions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| json!([a["tool"], a.get("via"), a["method"], a["path"], a["status"]]))
        .collect();
    assert_eq!(
        rows,
        [
            json!(["api_request", "run_code", "GET", "/me", 200]),
            json!(["run_code", null, null, null, null]),
            json!(["api_request", "run_code", "PATCH", "/deals/1100", 200]),
            json!(["run_code", null, null, null, null]),
            json!(["submit", null, null, null, null]),
        ],
        "a run's requests come just before it"
    );
    assert_eq!(record["actions"][2]["arguments"]["body"], json!({"status": "won", "stage": "won"}));
    assert_eq!(record["diagnostics"]["calls"], 3);
    assert_eq!(record["diagnostics"]["code_requests"], 2);
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn nothing_outlives_a_call_and_files_last_only_for_the_episode() {
    let env = paired();
    let mut session = Session::connect(&env).await;
    let again = json!({"task_id": "close-won-deal", "episode_id": "same-id"});
    session.reset(again.clone()).await;
    let started = Instant::now();
    assert_eq!(run(&mut session, "sleep 300 & echo kept > f; echo started").await, "started\n");
    assert!(started.elapsed() < Duration::from_secs(10), "a background job holds no call open");
    let sleeping =
        "for p in /proc/[0-9]*; do cat $p/comm 2>/dev/null; done | grep -cx sleep || true";
    assert_eq!(run(&mut session, sleeping).await, "0\n", "the sweep killed it");
    assert_eq!(run(&mut session, "cat f").await, "kept\n", "files last for the episode");
    session.reset(again).await;
    assert_eq!(run(&mut session, "ls -A; ls -A /tmp").await, "", "even under the same episode_id");
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_url_from_an_earlier_call_is_refused_and_not_recorded() {
    let env = paired();
    let mut session = Session::connect(&env).await;
    session.reset(json!({"task_id": "close-won-deal"})).await;
    let url = run(&mut session, r#"echo -n "$API_CRM""#).await;
    assert!(url.ends_with("/crm") && url.contains(":8001/"), "{url}");
    let stale = format!("curl -s -o /dev/null -w '%{{http_code}}' '{url}/me'");
    assert_eq!(run(&mut session, &stale).await, "403");
    session.step(call("submit", &json!({}))).await;
    let record = session.send(json!({"type": "record"})).await["data"].clone();
    assert_eq!(record["diagnostics"]["code_requests"], 0, "{record}");
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn the_sandbox_cuts_a_run_at_its_limit_and_its_output_at_64_kib() {
    let env = paired();
    let sandbox = env.sandbox.as_ref().unwrap();
    let client = reqwest::Client::new();
    let post = |command: &str, timeout_s: u64| {
        let body = json!({"workspace": "w", "command": command, "timeout_s": timeout_s});
        client.post(sandbox.url("/run")).json(&body).send()
    };
    let started = Instant::now();
    let slow: Value = post("sleep 5", 1).await.unwrap().json().await.unwrap();
    assert_eq!(slow["timed_out"], true, "{slow}");
    assert!(started.elapsed() < Duration::from_secs(4));
    let long: Value =
        post("head -c 200000 /dev/zero | tr '\\0' a", 10).await.unwrap().json().await.unwrap();
    assert_eq!(long["truncated"], true);
    assert_eq!(long["output"].as_str().unwrap().len(), 64 * 1024);
}

#[test]
#[ignore = "needs Docker"]
fn an_env_refuses_a_sandbox_that_shares_its_network_namespace() {
    let env = Container::start(&Options {
        docker_args: &["-e", "MIROIR_SANDBOX=http://127.0.0.1:9100"],
        ..Options::default()
    });
    let network = format!("container:{}", env.id);
    let sandbox =
        docker_ok(&["run", "-d", "--network", &network, "-e", "MIROIR_PORT=9100", SANDBOX_IMAGE]);
    let code = env.wait_exit();
    let _ = docker(&["rm", "-f", &sandbox]);
    assert_ne!(code, 0);
    assert!(env.logs().contains("shares the env's network namespace"), "{}", env.logs());
}

#[test]
#[ignore = "needs Docker"]
fn an_env_refuses_a_sandbox_that_reaches_a_service_past_loopback() {
    let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/acme-crm/miroir.yaml");
    let manifest = std::fs::read_to_string(fixture).unwrap();
    let open = manifest.replace(", --bind, 127.0.0.1]", "]");
    assert_ne!(open, manifest, "the fixture binds Redis to loopback");
    let env = Container::start(&Options {
        files: &[("/miroir/miroir.yaml", &open)],
        sandbox: true,
        ..Options::default()
    });
    assert_ne!(env.wait_exit(), 0);
    let logs = env.logs();
    assert!(logs.contains(":6379") && logs.contains("the sandbox reaches"), "{logs}");
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_dead_sandbox_voids_the_episode_and_fails_health() {
    let env = paired();
    docker_ok(&["kill", &env.sandbox.as_ref().unwrap().id]);
    let health = reqwest::get(env.url("/health")).await.unwrap();
    assert_eq!(health.status(), 503);
    let mut session = Session::connect(&env).await;
    session.reset(json!({"task_id": "close-won-deal"})).await;
    let reply = session.step(call("run_code", &json!({"command": "ls"}))).await;
    assert_eq!(reply["data"]["observation"]["error"]["error_type"], "transport_error", "{reply}");
    let done = session.step(call("submit", &json!({}))).await;
    assert_eq!(done["data"]["reward"], Value::Null, "no score for the sandbox's fault: {done}");
}

const CODE_TASK: &str = r#"
instruction: Mark deal 1100 as won.
checks:
  - {name: won, weight: 1.0, sql: SELECT status = 'won' FROM deals WHERE id = 1100}
  - name: only_deal_1100
    gate: true
    db: episode
    sql: SELECT count(*) = 0 FROM changes WHERE tbl = 'deals' AND json_extract(pk, '$.id') <> 1100
allowed_changes:
  - {table: deals, op: update, columns: [status, stage, updated_at]}
golden:
  - tool: run_code
    arguments:
      command: >-
        curl -sf -X PATCH -H 'content-type: application/json'
        -d '{"status": "won", "stage": "won"}' "$API_CRM/deals/1100" > /dev/null
  - {tool: submit, arguments: {}}
probes:
  - name: wins-another-deal-in-code-too
    steps:
      - tool: run_code
        arguments:
          command: >-
            for id in 1100 1043; do curl -sf -X PATCH -H 'content-type: application/json'
            -d '{"status": "won", "stage": "won"}' "$API_CRM/deals/$id" > /dev/null; done
      - {tool: submit, arguments: {}}
    max_reward: 0
"#;

#[test]
#[ignore = "needs Docker"]
fn check_replays_runs_written_in_code_with_a_sandbox_and_refuses_them_without() {
    let (code, report, logs) = run_check_with_sandbox(ENV_IMAGE, &[("in-code", CODE_TASK)]);
    assert_eq!(code, 0, "{report:#}\n{logs}");
    assert_eq!(report["stages"]["sandbox"], json!({"passed": true}));
    let task = &report["tasks"][0];
    assert_eq!(task["golden"]["statuses"], json!(["ok", "ok"]));
    assert_eq!(task["probes"]["passed"], true, "{task:#}");

    let (code, report, _) = run_check(ENV_IMAGE, &[], &[("in-code", CODE_TASK)]);
    assert_eq!(code, 1);
    let error = report["tasks"][0]["error"].as_str().unwrap_or_default();
    assert!(error.contains("calls 'run_code', which needs a sandbox"), "{report:#}");
    assert!(report["stages"].get("sandbox").is_none());
}
