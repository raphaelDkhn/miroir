use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::common::{Container, Options, Session, docker_ok, get, manifest_with_processes};

fn call(tool: &str, arguments: &Value) -> Value {
    json!({"type": "call_tool", "tool_name": tool, "arguments": arguments})
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn path_escape_rejected() {
    let env = Container::start(&Options::default());
    let mut session = Session::connect(&env).await;
    session.reset(json!({"task_id": "count-overdue-invoices"})).await;
    let escapes = [
        "deals",
        "/../etc/passwd",
        "/deals/%2e%2e/%2E%2E/x",
        "/a%2f..%2fb",
        "/x/..\\y",
        "http://127.0.0.1:5432/",
    ];
    for path in escapes {
        let reply =
            session.step(call("api_request", &json!({"method": "GET", "path": path}))).await;
        let observation = &reply["data"]["observation"];
        assert_eq!(observation["error"]["error_type"], "invalid_args", "{path}: {reply}");
        assert_eq!(observation["result"], Value::Null);
        assert_eq!(reply["data"]["reward"], 0.0);
    }
    let fine =
        session.step(call("api_request", &json!({"method": "GET", "path": "//deals/1"}))).await;
    assert_eq!(fine["data"]["observation"]["error"], Value::Null, "{fine}");
    let state = session.state().await;
    assert_eq!(state["step_count"], escapes.len() + 1, "failed calls still count as steps");
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn persona_header_wins() {
    let env = Container::start(&Options::default());
    let mut session = Session::connect(&env).await;
    session.reset(json!({"task_id": "count-overdue-invoices"})).await;
    let headers = json!({"Authorization": "Bearer seed-token-priya", "Host": "evil.test"});
    let reply = session
        .step(call("api_request", &json!({"method": "GET", "path": "/me", "headers": headers})))
        .await;
    let result = &reply["data"]["observation"]["result"];
    assert_eq!(result["status"], 200, "{reply}");
    assert_eq!(
        result["body"]["email"], "dana@acme.test",
        "the persona's token replaced the agent's"
    );
    assert_eq!(result["headers"]["content-type"], "application/json");
}

const SLOW_RESET: &str = "instruction: Wait.
setup: SELECT pg_sleep(5)
checks:
  - {name: ok, weight: 1, sql: SELECT true}
any_change: true
golden:
  - {tool: submit}
null_max: 1
";

#[tokio::test]
#[ignore = "needs Docker"]
async fn pings_are_answered_during_a_slow_reset() {
    let env = Container::start(&Options {
        files: &[("/miroir/tasks/slow-reset.yaml", SLOW_RESET)],
        ..Options::default()
    });
    env.wait_for_log("env ready", Duration::from_secs(60));
    let mut session = Session::connect(&env).await;
    let reset = json!({"type": "reset", "data": {"task_id": "slow-reset"}});
    let pong = session.ping_during(reset).await;
    assert!(pong < Duration::from_secs(2), "the pong took {pong:?} during a 5 s reset");
    let reply = session.send(json!({"type": "state"})).await;
    assert_eq!(reply["type"], "observation", "the reset's reply comes first: {reply}");
    let state = session.receive().await.expect("the queued frame's reply");
    assert_eq!(state["data"]["task_id"], "slow-reset", "{state}");
}

const SLOW_GRADE: &str = "instruction: Wait.
checks:
  - {name: slow, weight: 1, command: [sleep, '2']}
any_change: true
golden:
  - {tool: submit}
null_max: 1
";

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_disconnect_does_not_cancel_grading() {
    let env = Container::start(&Options {
        files: &[("/miroir/tasks/slow-grade.yaml", SLOW_GRADE)],
        ..Options::default()
    });
    let mut session = Session::connect(&env).await;
    session.reset(json!({"task_id": "slow-grade"})).await;
    let submit = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                        "params": {"name": "submit", "arguments": {"answer": "x"}}});
    let call = reqwest::Client::new().post(env.url("/mcp")).json(&submit).send();
    let gave_up = tokio::time::timeout(Duration::from_millis(1500), call).await;
    assert!(gave_up.is_err(), "the client hung up during the check");
    let state = session.state().await;
    assert_eq!((state["done"].as_bool(), state["reward"].as_f64()), (Some(true), Some(1.0)));
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn mcp_refuses_calls_it_cannot_queue() {
    let env = Container::start(&Options {
        files: &[("/miroir/tasks/slow-grade.yaml", SLOW_GRADE)],
        ..Options::default()
    });
    let mut session = Session::connect(&env).await;
    session.reset(json!({"task_id": "slow-grade"})).await;
    let client = reqwest::Client::new();
    let submit = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                        "params": {"name": "submit", "arguments": {"answer": "x"}}});
    let calls = (0..30).map(|_| {
        let call = client.post(env.url("/mcp")).json(&submit).send();
        async move { call.await.unwrap().json::<Value>().await.unwrap() }
    });
    let replies = futures_util::future::join_all(calls).await;
    let refused = replies
        .iter()
        .filter(|r| r["error"]["message"].as_str().is_some_and(|m| m.contains("too many")))
        .count();
    assert!((1..30).contains(&refused), "{refused} of 30 calls were refused");
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn idle_connections_cannot_use_up_descriptors() {
    let env = Container::start(&Options {
        docker_args: &["--ulimit", "nofile=512:512"],
        ..Options::default()
    });
    env.wait_for_log("env ready", Duration::from_secs(60));
    let mut session = Session::connect(&env).await;
    let address = format!("127.0.0.1:{}", env.port);
    let mut idle = Vec::new();
    for _ in 0..600 {
        idle.push(tokio::net::TcpStream::connect(&address).await.unwrap());
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    session.reset(json!({"task_id": "count-overdue-invoices"})).await;
    let done = session.step(call("submit", &json!({"answer": "3"}))).await;
    assert_eq!(done["data"]["done"], true, "{done}");
    let started = Instant::now();
    while crate::common::try_get(&env.url("/health")).is_none() {
        assert!(started.elapsed() < Duration::from_secs(90), "idle connections never expired");
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn null_arguments_count_as_left_out() {
    let env = Container::start(&Options::default());
    let mut session = Session::connect(&env).await;
    session.reset(json!({"task_id": "count-overdue-invoices"})).await;
    let arguments = json!({"method": "GET", "path": "/me", "headers": null, "body": null});
    let reply = session.step(call("api_request", &arguments)).await;
    assert_eq!(reply["data"]["observation"]["error"], Value::Null, "{reply}");
    assert_eq!(reply["data"]["observation"]["result"]["status"], 200, "{reply}");
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn second_websocket_rejected() {
    let env = Container::start(&Options::default());
    let mut first = Session::connect(&env).await;
    first.reset(json!({"task_id": "count-overdue-invoices"})).await;
    let mut second = Session::connect(&env).await;
    let refusal = second.receive().await.expect("an error frame");
    assert_eq!(refusal["type"], "error");
    assert_eq!(refusal["data"]["code"], "CAPACITY_REACHED");
    assert!(second.receive().await.is_none(), "the second connection is closed");

    let mut other =
        Session::connect_url(&format!("ws://127.0.0.1:{}/ws?session_id=other", env.port)).await;
    assert_eq!(other.receive().await.unwrap()["data"]["code"], "SESSION_ERROR");

    first.close().await;
    let session_id = session_id(&env).await;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let mut next = Session::connect_url(&format!(
            "ws://127.0.0.1:{}/ws?session_id={session_id}",
            env.port
        ))
        .await;
        let state = next.send(json!({"type": "state"})).await;
        if state["type"] == "state" {
            assert_eq!(state["data"]["task_id"], "count-overdue-invoices");
            break;
        }
        assert!(Instant::now() < deadline, "the slot was never released: {state}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

const KILL_WEB: &str = r#"for p in /proc/[0-9]*; do
    command=$(tr '\0' ' ' < "$p/cmdline" 2>/dev/null)
    [ "$command" = "node dist/server.js " ] && kill -9 "${p#/proc/}"
done; true"#;

#[tokio::test]
#[ignore = "needs Docker"]
async fn crash_ends_episode() {
    let env = Container::start(&Options::default());
    let mut session = Session::connect(&env).await;
    session.reset(json!({"task_id": "count-overdue-invoices", "episode_id": "crash"})).await;
    env.exec_ok(&["sh", "-c", KILL_WEB]);
    let deadline = Instant::now() + Duration::from_secs(60);
    while get(&env.url("/health")).0 != 503 {
        assert!(Instant::now() < deadline, "/health never reported the crash");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(get(&env.url("/health")).1, r#"{"leased":true,"status":"unhealthy"}"#);
    let reply = session.step(call("api_request", &json!({"method": "GET", "path": "/me"}))).await;
    let data = &reply["data"];
    assert_eq!(data["done"], true, "{reply}");
    assert_eq!(data["reward"], 0.0, "a crash earns the harm reward, never no score: {reply}");
    assert_eq!(
        (&data["metadata"]["success"], &data["metadata"]["harm"]),
        (&json!(false), &json!(true))
    );
    assert_eq!(data["metadata"]["crashed"], json!({"process": "web", "exit_code": 137}));
    assert_eq!(data["metadata"]["fault"], "process 'web' exited with code 137");
    assert_eq!(data["metadata"]["invalid"], Value::Null);
    assert_eq!(data["metadata"]["termination"], "crash");
    let after = session.step(json!({"type": "list_tools"})).await;
    assert_eq!(after["type"], "error", "steps after the end fail until the next reset");
    let state = session.state().await;
    assert_eq!((state["done"].as_bool(), state["success"].as_bool()), (Some(true), Some(false)));

    session.reset(json!({"task_id": "count-overdue-invoices"})).await;
    assert_eq!(get(&env.url("/health")).0, 200);
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn token_guards_every_route_but_health() {
    let env = Container::start(&Options {
        docker_args: &["-e", "MIROIR_TOKEN=s3cret"],
        ..Options::default()
    });
    env.wait_healthy();
    assert_eq!(get(&env.url("/health")).0, 200);
    for path in ["/metadata", "/schema", "/list_environments", "/ws"] {
        assert_eq!(get(&env.url(path)).0, 401, "{path}");
    }
    let client = reqwest::Client::new();
    let authorized = client.get(env.url("/metadata")).bearer_auth("s3cret").send().await.unwrap();
    assert_eq!(authorized.status(), 200);
    let wrong = client.get(env.url("/metadata")).bearer_auth("s3cre").send().await.unwrap();
    assert_eq!(wrong.status(), 401);
    let list = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
    let status = |path: &str| {
        let sent = client.post(env.url(path)).json(&list).send();
        async move { sent.await.unwrap().status().as_u16() }
    };
    assert_eq!(status("/mcp?token=s3cret").await, 200, "for MCP clients that send no headers");
    assert_eq!(status("/mcp?token=s3cre").await, 401);
    assert_eq!(get(&env.url("/metadata?token=s3cret")).0, 401, "only /mcp takes it in the URL");
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn web_pages_are_refused() {
    let env = Container::start(&Options::default());
    env.wait_healthy();
    let client = reqwest::Client::new();
    let from_page = |request: reqwest::RequestBuilder| {
        let sent = request.header("Origin", "https://evil.test").send();
        async move { sent.await.unwrap().status().as_u16() }
    };
    assert_eq!(from_page(client.get(env.url("/ws"))).await, 403);
    let call = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
    assert_eq!(from_page(client.post(env.url("/mcp")).body(call.to_string())).await, 403);
    let plain = client.post(env.url("/mcp")).body(call.to_string()).send().await.unwrap();
    assert_eq!(plain.status(), 200, "the same call without an Origin works");
    assert_eq!(from_page(client.get(env.url("/health"))).await, 200);
}

#[test]
#[ignore = "needs Docker"]
fn an_empty_token_is_refused() {
    crate::common::build_images();
    let out =
        crate::common::docker(&["run", "--rm", "-e", "MIROIR_TOKEN=", crate::common::ENV_IMAGE]);
    assert!(!out.status.success());
    let logs = String::from_utf8_lossy(&out.stderr);
    assert!(logs.contains("MIROIR_TOKEN must not be empty"), "{logs}");
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn task_api_lists_tasks_without_checks() {
    let env = Container::start(&Options::default());
    let client = reqwest::Client::new();
    let post = |path: &str, body: Value| {
        let request = client.post(env.url(path)).json(&body);
        async move {
            let response = request.send().await.unwrap();
            (response.status().as_u16(), response.json::<Value>().await.unwrap())
        }
    };
    assert_eq!(get(&env.url("/list_environments")).1, r#"["acme-crm"]"#);
    let splits: Value = serde_json::from_str(&get(&env.url("/acme-crm/splits")).1).unwrap();
    assert_eq!(
        splits,
        json!([{"name": "train", "type": "train"}, {"name": "validation", "type": "validation"}])
    );
    let (_, count) = post("/acme-crm/num_tasks", json!({"split": "train"})).await;
    assert_eq!(count, json!({"num_tasks": 6}));
    let (_, listed) = post("/acme-crm/tasks", json!({"split": "train"})).await;
    let first = &listed["tasks"][0];
    assert_eq!(first["id"], "count-overdue-invoices", "tasks come in ID order");
    let keys: Vec<&String> = first.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["id", "instruction", "max_steps", "persona", "split", "task_hash", "world"]);
    assert_eq!(first["task_hash"].as_str().unwrap().len(), 16, "{first}");
    assert_eq!(first["world"], "default", "a manifest without worlds has one");
    let (_, one) = post("/acme-crm/task", json!({"split": "validation", "index": 1})).await;
    assert_eq!(one["task"]["id"], "export-open-deals");
    let (status, _) = post("/acme-crm/task", json!({"split": "validation", "index": 9})).await;
    assert_eq!(status, 400);
    let (_, range) = post("/acme-crm/task_range", json!({"split": "train", "start": 4})).await;
    assert_eq!(range["tasks"].as_array().unwrap().len(), 2);
    assert_eq!(get(&env.url("/other-env/splits")).0, 404);
    let metadata: Value = serde_json::from_str(&get(&env.url("/metadata")).1).unwrap();
    assert_eq!(metadata["name"], "acme-crm");
    assert_eq!(metadata["description"], "Acme CRM, a sales CRM for small teams");
    assert_eq!(metadata["version"], "acme-crm:dev");
    assert_eq!(metadata["miroir_version"], env!("CARGO_PKG_VERSION"));
    for hash in ["manifest_hash", "tasks_hash"] {
        assert_eq!(metadata[hash].as_str().unwrap().len(), 16, "{metadata}");
    }
    let openapi: Value = serde_json::from_str(&get(&env.url("/openapi.json")).1).unwrap();
    assert_eq!(openapi["info"]["version"], env!("CARGO_PKG_VERSION"));
    assert!(openapi["paths"]["/mcp"].is_object() && openapi["paths"]["/reset"].is_null());
}

async fn session_id(env: &Container) -> String {
    let create = json!({"jsonrpc": "2.0", "id": 1, "method": "openenv/session/create"});
    let client = reqwest::Client::new();
    let reply: Value =
        client.post(env.url("/mcp")).json(&create).send().await.unwrap().json().await.unwrap();
    reply["result"]["session_id"].as_str().unwrap().to_string()
}

async fn mcp(env: &Container, method: &str, params: &Value) -> Value {
    let request = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
    let client = reqwest::Client::new();
    client.post(env.url("/mcp")).json(&request).send().await.unwrap().json().await.unwrap()
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn reset_picks_by_index_and_refuses_what_it_does_not_know() {
    let env = Container::start(&Options::default());
    let mut session = Session::connect(&env).await;
    let task = |reply: &Value| reply["observation"]["task_id"].as_str().unwrap().to_string();
    let reset = |data: Value| json!({"type": "reset", "data": data});

    let first = session.reset(json!({"split": "validation", "index": 0})).await;
    assert_eq!(task(&first), "close-won-deal");
    let second = session.reset(json!({"split": "validation", "index": 1})).await;
    assert_eq!(task(&second), "export-open-deals", "the same index as the task API");
    let by_id = session.reset(json!({"task_id": "pay-invoice", "index": 0})).await;
    assert_eq!(task(&by_id), "pay-invoice", "task_id wins");

    let too_far = session.send(reset(json!({"split": "validation", "index": 9}))).await;
    assert_eq!(too_far["data"]["code"], "VALIDATION_ERROR", "{too_far}");
    let unknown = session.send(reset(json!({"seed": 1, "options": {"task_id": "x"}}))).await;
    assert_eq!(unknown["data"]["code"], "VALIDATION_ERROR", "{unknown}");
    assert!(unknown["data"]["message"].as_str().unwrap().contains("options"), "{unknown}");
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_call_for_another_session_or_episode_is_refused() {
    let env = Container::start(&Options::default());
    let mut session = Session::connect(&env).await;
    session.reset(json!({"task_id": "count-overdue-invoices", "episode_id": "now"})).await;
    let submit = json!({"name": "submit", "arguments": {"answer": "3"}});
    let with = |key: &str, value: &str| {
        let mut params = submit.clone();
        params[key] = json!(value);
        params
    };

    let other_container = mcp(&env, "tools/call", &with("session_id", "miroir-other")).await;
    assert_eq!(other_container["error"]["code"], -32602, "{other_container}");
    let over = mcp(&env, "tools/call", &with("episode_id", "earlier")).await;
    assert_eq!(over["result"]["isError"], true, "a tool error, so MCP clients do not retry");
    let shown = over["result"]["content"][0]["text"].as_str().unwrap();
    assert!(shown.contains("'earlier' is over"), "{over}");
    let state = session.state().await;
    assert_eq!((state["step_count"].as_u64(), state["done"].as_bool()), (Some(0), Some(false)));

    let stale = json!({"type": "call_tool", "tool_name": "submit", "arguments": {},
                       "metadata": {"episode_id": "earlier"}});
    let refused = session.step(stale).await;
    assert_eq!(refused["data"]["code"], "VALIDATION_ERROR", "{refused}");
    let in_url = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": submit});
    let late = reqwest::Client::new().post(env.url("/mcp?episode_id=earlier")).json(&in_url);
    let late: Value = late.send().await.unwrap().json().await.unwrap();
    assert_eq!(late["result"]["isError"], true, "the URL names the episode for a harness: {late}");
    assert_eq!(session.state().await["step_count"], 0, "a refused call is no step");

    let id = session_id(&env).await;
    let mut right = with("session_id", &id);
    right["episode_id"] = json!("now");
    let done = mcp(&env, "tools/call", &right).await;
    assert_eq!(done["result"]["done"], true, "{done}");
    let content = done["result"]["content"].as_array().unwrap();
    assert_eq!(content[0]["text"], json!({"submitted": true}).to_string(), "{done}");
    let last = content.last().unwrap()["text"].as_str().unwrap();
    assert!(last.contains("the episode is over; stop calling tools"), "the model is told: {done}");
    let after = mcp(&env, "tools/call", &right).await;
    assert_eq!(after["result"]["isError"], true, "{after}");
    assert_eq!(after["result"]["error"]["error_type"], "execution_error", "{after}");
    let shown = after["result"]["content"][0]["text"].as_str().unwrap();
    assert!(shown.contains("the episode is over; stop calling tools"), "{after}");
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn listing_tools_counts_as_a_step_but_cannot_end_the_episode() {
    let task = r"
instruction: Say so.
max_steps: 1
checks:
  - {name: ok, weight: 1, sql: SELECT true}
any_change: true
golden:
  - {tool: submit}
null_max: 1
";
    let env = Container::start(&Options {
        files: &[("/miroir/tasks/one-step.yaml", task)],
        ..Options::default()
    });
    let mut session = Session::connect(&env).await;
    session.reset(json!({"task_id": "one-step"})).await;
    let listed = session.step(json!({"type": "list_tools"})).await;
    assert_eq!(listed["data"]["done"], false, "{listed}");
    assert_eq!(session.state().await["step_count"], 1);
    let done = session.step(call("api_request", &json!({"method": "GET", "path": "/me"}))).await;
    assert_eq!(done["data"]["done"], true, "{done}");
    assert_eq!(done["data"]["metadata"]["termination"], "max_steps", "{done}");
    assert_eq!(done["data"]["metadata"]["task_hash"].as_str().unwrap().len(), 16, "{done}");
}

const BAD_SETUP: &str = r"
instruction: Never starts.
setup: SELECT 1/0
checks:
  - {name: ok, weight: 1, sql: SELECT true}
any_change: true
golden:
  - {tool: submit}
null_max: 1
";

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_broken_env_mends_itself_once_no_session_holds_it() {
    let env = Container::start(&Options {
        files: &[("/miroir/tasks/bad-setup.yaml", BAD_SETUP)],
        ..Options::default()
    });
    env.wait_healthy();
    let mut session = Session::connect(&env).await;
    let failed = session.send(json!({"type": "reset", "data": {"task_id": "bad-setup"}})).await;
    assert_eq!(failed["type"], "error", "{failed}");
    tokio::time::sleep(Duration::from_secs(15)).await;
    assert_eq!(
        get(&env.url("/health")).0,
        503,
        "an env its session holds is the session's to reset"
    );
    session.close().await;
    env.wait_healthy();

    env.exec_ok(&["sh", "-c", KILL_WEB]);
    crate::common::retry("the env to mend a crash while idle", || {
        (env.logs().matches("restoring the product").count() >= 2).then_some(())
    });
    env.wait_healthy();
    let mut session = Session::connect(&env).await;
    session.reset(json!({"task_id": "count-overdue-invoices"})).await;
    let me = session.step(call("api_request", &json!({"method": "GET", "path": "/me"}))).await;
    assert_eq!(me["data"]["observation"]["result"]["status"], 200, "{me}");
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn health_says_when_the_env_is_unusable() {
    let manifest = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/acme-crm/miroir.yaml"
    ))
    .unwrap()
    .replace("anchor: bake", "anchor: bake\n  hook: [sleep, '3']");
    let env = Container::start(&Options {
        files: &[("/miroir/miroir.yaml", &manifest), ("/miroir/tasks/bad-setup.yaml", BAD_SETUP)],
        ..Options::default()
    });
    let (code, body) = get(&env.url("/health"));
    assert_eq!(
        (code, body.as_str()),
        (503, r#"{"leased":false,"status":"unhealthy"}"#),
        "boot takes 3 s or more"
    );
    env.wait_healthy();

    let mut session = Session::connect(&env).await;
    let failed = session.send(json!({"type": "reset", "data": {"task_id": "bad-setup"}})).await;
    assert_eq!(failed["type"], "error", "{failed}");
    assert_eq!(get(&env.url("/health")).0, 503, "a failed restore leaves a broken product");
    session.reset(json!({"task_id": "count-overdue-invoices"})).await;
    assert_eq!(get(&env.url("/health")).0, 200, "and a good one mends it");
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn mcp_answers_a_standard_client() {
    let env = Container::start(&Options::default());
    let client = reqwest::Client::new();
    let hello = json!({"protocolVersion": "2025-03-26", "capabilities": {},
                       "clientInfo": {"name": "test", "version": "1"}});
    let init = mcp(&env, "initialize", &hello).await;
    assert_eq!(init["result"]["protocolVersion"], "2025-03-26", "{init}");
    assert!(init["result"]["capabilities"]["tools"].is_object(), "{init}");
    assert_eq!(init["result"]["serverInfo"]["name"], "acme-crm", "{init}");
    let future = json!({"protocolVersion": "2099-01-01", "capabilities": {}});
    let offered = mcp(&env, "initialize", &future).await;
    assert_eq!(offered["result"]["protocolVersion"], "2025-06-18", "not a version it lacks");
    let note = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
    let accepted = client.post(env.url("/mcp")).json(&note).send().await.unwrap();
    assert_eq!(accepted.status(), 202);
    assert_eq!(accepted.text().await.unwrap(), "", "a notification gets no reply");
    assert_eq!(mcp(&env, "ping", &json!({})).await["result"], json!({}));

    let mut session = Session::connect(&env).await;
    session.reset(json!({"task_id": "count-overdue-invoices"})).await;
    let read = json!({"name": "api_request", "arguments": {"method": "GET", "path": "/me"}});
    let worked = mcp(&env, "tools/call", &read).await;
    let result = &worked["result"];
    assert_eq!(result["isError"], false, "{worked}");
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("dana@acme.test"), "{worked}");
    assert_eq!(result["result"]["status"], 200, "the OpenEnv fields stay: {worked}");
    let unknown = mcp(&env, "tools/call", &json!({"name": "nope"})).await;
    assert_eq!(unknown["result"]["isError"], true, "{unknown}");
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_finished_episode_keeps_its_record_until_the_next_reset() {
    let env = Container::start(&Options::default());
    let mut session = Session::connect(&env).await;
    let golden = [
        ("api_request", json!({"method": "GET", "path": "/users?role=rep"})),
        ("api_request", json!({"method": "PATCH", "path": "/deals/2001", "body": {"owner_id": 7}})),
        ("submit", json!({"answer": "Reassigned deal 2001 to Marcus, who was emailed."})),
    ];
    let record = |episode_id: &str| json!({"type": "record", "data": {"episode_id": episode_id}});
    let mut signatures = Vec::new();
    for (run, over_mcp) in [("websocket", false), ("mcp", true)] {
        session.reset(json!({"task_id": "notify-new-owner", "episode_id": run})).await;
        let early = session.send(record(run)).await;
        assert_eq!(early["data"]["code"], "VALIDATION_ERROR", "{early}");
        assert!(early["data"]["message"].as_str().unwrap().contains("once the episode is done"));
        for (tool, arguments) in &golden {
            if over_mcp {
                let params = json!({"name": tool, "arguments": arguments, "episode_id": run});
                assert_eq!(mcp(&env, "tools/call", &params).await["result"]["isError"], false);
            } else {
                session.step(call(tool, arguments)).await;
            }
        }
        let reply = session.send(record(run)).await;
        assert_eq!(reply["type"], "record", "{reply}");
        let record = &reply["data"];
        assert_eq!(
            (record["episode_id"].as_str(), record["reward"].as_f64()),
            (Some(run), Some(1.0))
        );
        let replayed: Vec<&Value> = record["actions"].as_array().unwrap().iter().collect();
        assert_eq!(replayed.len(), golden.len(), "{record}");
        for (action, (tool, arguments)) in replayed.iter().zip(&golden) {
            assert_eq!((&action["tool"], &action["arguments"]), (&json!(tool), arguments));
        }
        assert_eq!(record["actions"][1]["method"], "PATCH");
        assert_eq!(record["changes"][0]["tbl"], "deals", "{record}");
        assert_eq!(record["effects"][0]["data"]["to"], json!(["marcus@acme.test"]), "{record}");
        assert_eq!(record["diagnostics"]["calls"], 3);
        assert_eq!(record["metadata"]["termination"], "submit", "the grade, even over MCP");
        assert!(record["timings"]["reset_s"]["total"].is_number(), "{record}");
        assert!(record.get("truncated").is_none(), "{record}");
        signatures.push(record["signature"].as_str().unwrap().to_string());
    }
    assert_eq!(signatures[0], signatures[1], "two golden runs reach one end state");

    session.reset(json!({"task_id": "notify-new-owner", "episode_id": "empty"})).await;
    let gone = session.send(record("mcp")).await;
    assert!(gone["data"]["message"].as_str().unwrap().contains("'mcp' is over"), "{gone}");
    session.step(call("submit", &json!({}))).await;
    let empty = session.send(json!({"type": "record"})).await;
    assert_eq!(empty["data"]["changes"], json!([]), "{empty}");
    assert_ne!(empty["data"]["signature"], json!(signatures[0]));
    let unknown = mcp(&env, "record", &json!({})).await;
    assert_eq!(unknown["error"]["code"], -32601, "MCP clients reach the agent: {unknown}");
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_call_past_max_seconds_ends_the_episode_as_it_stands() {
    let env = Container::start(&Options::default());
    let mut session = Session::connect(&env).await;
    let task = "count-overdue-invoices";
    for refused in [json!(0), json!(86_401), json!(1.5)] {
        let data = json!({"task_id": task, "max_seconds": refused});
        let reply = session.send(json!({"type": "reset", "data": data})).await;
        assert_eq!(reply["data"]["code"], "VALIDATION_ERROR", "{refused}: {reply}");
    }

    session.reset(json!({"task_id": task, "max_seconds": 1})).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let late = session.step(call("submit", &json!({"answer": "3"}))).await;
    let data = &late["data"];
    assert_eq!(data["done"], true, "{late}");
    assert_eq!(data["observation"]["error"]["error_type"], "timeout", "{late}");
    assert_eq!(data["metadata"]["termination"], "max_seconds", "{late}");
    assert_eq!(data["reward"], 0.0, "the right answer came too late to count: {late}");
    assert_eq!(session.state().await["step_count"], 0, "a late call is no step");
    let record = session.send(json!({"type": "record"})).await;
    assert_eq!(record["data"]["actions"], json!([]), "a late call never runs: {record}");

    session.reset(json!({"task_id": task, "max_seconds": 1})).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let params = json!({"name": "submit", "arguments": {"answer": "3"}});
    let over_mcp = mcp(&env, "tools/call", &params).await;
    assert_eq!(over_mcp["result"]["isError"], true, "{over_mcp}");
    assert_eq!(over_mcp["result"]["done"], true, "{over_mcp}");
    let shown = over_mcp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(shown.contains("1 s ran out"), "{over_mcp}");

    session.reset(json!({"task_id": task, "max_seconds": 60})).await;
    let golden = [
        ("api_request", json!({"method": "GET", "path": "/contacts?q=Northwind"})),
        ("api_request", json!({"method": "GET", "path": "/invoices?contact_id=101&overdue=true"})),
        ("submit", json!({"answer": "3"})),
    ];
    let mut last = Value::Null;
    for (tool, arguments) in &golden {
        last = session.step(call(tool, arguments)).await;
    }
    assert_eq!(last["data"]["reward"], 1.0, "{last}");
    assert_eq!(last["data"]["metadata"]["termination"], "submit", "{last}");
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn health_says_whether_a_session_holds_the_env() {
    let env = Container::start(&Options::default());
    env.wait_healthy();
    assert_eq!(get(&env.url("/health")).1, r#"{"leased":false,"status":"healthy"}"#);
    let mut session = Session::connect(&env).await;
    session.state().await;
    assert_eq!(get(&env.url("/health")).1, r#"{"leased":true,"status":"healthy"}"#);
    session.close().await;
    assert_eq!(get(&env.url("/health")).1, r#"{"leased":false,"status":"healthy"}"#);
}

#[tokio::test]
#[ignore = "needs Docker"]
async fn a_stopping_env_is_unhealthy_and_takes_no_new_session() {
    let stubborn = r#"  - {name: stubborn, command: [sh, -c, "trap '' TERM; exec sleep 1000"]}"#;
    let manifest = manifest_with_processes(stubborn);
    let env = Container::start(&Options {
        files: &[("/miroir/miroir.yaml", &manifest)],
        ..Options::default()
    });
    env.wait_healthy();
    docker_ok(&["kill", "--signal", "TERM", &env.id]);
    let deadline = Instant::now() + Duration::from_secs(10);
    while get(&env.url("/health")).0 != 503 {
        assert!(Instant::now() < deadline, "/health never said the env is stopping");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let mut late = Session::connect(&env).await;
    let refusal = late.receive().await.expect("an error frame");
    assert_eq!(refusal["data"]["code"], "CAPACITY_REACHED", "{refusal}");
    assert!(refusal["data"]["message"].as_str().unwrap().contains("shutting down"), "{refusal}");
}
