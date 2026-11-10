//! The `run_code` tool: the client of the sandbox container, and the proxy its code reaches
//! the apps through.

use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use reqwest::Url;
use serde_json::{Map, Value, json};
use tokio::sync::{OnceCell, OwnedRwLockReadGuard, OwnedSemaphorePermit, RwLock, Semaphore};
use tokio::time::{Instant, timeout};
use uuid::Uuid;

use crate::edb::ActionRow;
use crate::env::Env;
use crate::manifest::api_variable;
use crate::task::Task;
use crate::tools::{
    BODY_LIMIT, FORWARD_LIMIT, Failure, Forward, Forwarded, ToolError, allow_keys,
    constant_time_eq, cut, http_client,
};

/// Port of the proxy the sandbox's code calls the apps through.
pub const PROXY_PORT: u16 = 8001;
const RUN_LIMIT_S: u64 = 60;
const RUN_SLACK: Duration = Duration::from_secs(20);
const COMMAND_LIMIT: usize = 1024 * 1024;
const PERMITS: usize = 16;
const PERMIT_WAIT: Duration = Duration::from_secs(10);
const BODY_WAIT: Duration = Duration::from_secs(30);
const HEALTH_LIMIT: Duration = Duration::from_secs(1);
const SANDBOX_WAIT: Duration = Duration::from_secs(600);
const PROBE_LIMIT_S: u64 = 60;
const PROBE: &str = r#"import json, socket
from concurrent.futures import ThreadPoolExecutor


def reach(target):
    host, port = target
    try:
        socket.create_connection((host.strip("[]"), port), timeout=2).close()
        return True
    except OSError:
        return False


with ThreadPoolExecutor(16) as pool:
    hits = list(pool.map(reach, TARGETS + [["1.1.1.1", 443]]))
reached = [f"{host}:{port}" for (host, port), hit in zip(TARGETS, hits) if hit]
print(json.dumps({"tried": len(TARGETS), "reached": reached, "online": hits[-1]}))
"#;

/// Client of the sandbox container at `MIROIR_SANDBOX`.
pub struct Sandbox {
    url: Url,
    port: u16,
    client: reqwest::Client,
    peer: OnceCell<String>,
    probed: AtomicBool,
    active: Arc<RwLock<Option<Active>>>,
    permits: Arc<Semaphore>,
    slack: Duration,
}

struct Active {
    token: String,
    persona: String,
}

impl Sandbox {
    pub fn new(url: &str) -> Result<Sandbox, String> {
        let url = Url::parse(url).map_err(|e| format!("MIROIR_SANDBOX is not a URL: {e}"))?;
        let plain = url.scheme() == "http" && url.host_str().is_some() && url.path() == "/";
        if !plain || url.query().is_some() {
            return Err(
                "MIROIR_SANDBOX must be http://HOST:PORT, such as http://sandbox:8000".into()
            );
        }
        Ok(Sandbox {
            port: url.port_or_known_default().unwrap_or(80),
            url,
            client: http_client()?,
            peer: OnceCell::new(),
            probed: AtomicBool::new(false),
            active: Arc::new(RwLock::new(None)),
            permits: Arc::new(Semaphore::new(PERMITS)),
            slack: RUN_SLACK,
        })
    }

    /// Probed, and answering its health check.
    pub async fn healthy(&self) -> bool {
        self.probed.load(Ordering::SeqCst) && self.health(HEALTH_LIMIT).await.is_ok()
    }

    async fn health(&self, limit: Duration) -> Result<String, String> {
        let url = self.url.join("health").map_err(|e| e.to_string())?;
        let ask = async {
            let response = self.client.get(url).send().await?;
            let status = response.status();
            Ok::<_, reqwest::Error>((status, response.json::<Value>().await?))
        };
        let (status, body) = timeout(limit, ask)
            .await
            .map_err(|_| "the sandbox's /health did not answer in time".to_string())?
            .map_err(|e| format!("cannot reach the sandbox: {e}"))?;
        match body["peer"].as_str() {
            Some(peer) if status.is_success() => Ok(peer.to_string()),
            _ => Err(format!("the sandbox's /health answered {status}: {body}")),
        }
    }

    async fn peer(&self) -> Result<&str, String> {
        let peer = self.peer.get_or_try_init(|| self.health(Duration::from_secs(5))).await?;
        Ok(peer.as_str())
    }

    /// Run a command in the episode's workspace; each `API_` URL carries a token valid for
    /// this call only.
    pub async fn run(
        &self,
        workspace: &str,
        command: &str,
        persona: &str,
        apps: &[&str],
    ) -> Result<Value, Failure> {
        let peer = self.peer().await.map_err(|e| Failure::env("transport_error", e))?;
        let token = Uuid::new_v4().simple().to_string();
        let env: BTreeMap<String, String> = apps
            .iter()
            .map(|app| (api_variable(app), format!("http://{peer}:{PROXY_PORT}/{token}/{app}")))
            .collect();
        *self.active.write().await = Some(Active { token, persona: persona.to_string() });
        let reply = self.call(workspace, command, &env, RUN_LIMIT_S).await;
        *self.active.write().await = None;
        reply
    }

    async fn call(
        &self,
        workspace: &str,
        command: &str,
        env: &BTreeMap<String, String>,
        timeout_s: u64,
    ) -> Result<Value, Failure> {
        let url =
            self.url.join("run").map_err(|e| Failure::env("execution_error", e.to_string()))?;
        let body = json!({"workspace": workspace, "command": command, "timeout_s": timeout_s,
                          "env": env});
        let send = async {
            let response = self.client.post(url).json(&body).send().await?;
            let status = response.status();
            Ok::<_, reqwest::Error>((status, response.bytes().await?))
        };
        let limit = Duration::from_secs(timeout_s) + self.slack;
        let (status, bytes) = timeout(limit, send)
            .await
            .map_err(|_| {
                let message = format!("the sandbox did not answer within {} s", limit.as_secs());
                Failure::env("execution_error", message)
            })?
            .map_err(|e| {
                Failure::env("transport_error", format!("cannot reach the sandbox: {e}"))
            })?;
        match serde_json::from_slice::<Value>(&bytes) {
            Ok(reply) if status.is_success() && reply["output"].is_string() => Ok(reply),
            _ => {
                let text = String::from_utf8_lossy(&bytes);
                let message = format!("the sandbox answered {status}: {}", cut(&text, 500));
                Err(Failure::env("execution_error", message))
            }
        }
    }

    /// Fail unless the sandbox reaches none of the env's listening ports but `own`, at the
    /// env's address or on loopback.
    pub async fn probe(&self, own: &[u16]) -> Result<(), String> {
        let peer = self.wait().await?;
        let ip: IpAddr = peer
            .trim_matches(['[', ']'])
            .parse()
            .map_err(|_| format!("the sandbox sees this env at '{peer}', which is no address"))?;
        if ip.is_loopback() {
            return Err(
                "the sandbox reaches this env on loopback, so it shares the env's network \
                        namespace; give it its own"
                    .into(),
            );
        }
        let mut ports = BTreeSet::new();
        for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
            match tokio::fs::read_to_string(table).await {
                Ok(text) => ports.extend(listening(&text)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("cannot read {table}: {e}")),
            }
        }
        let targets = targets(&peer, &ports, self.port);
        let allowed: Vec<String> = own.iter().map(|port| format!("{peer}:{port}")).collect();
        let workspace = format!("probe-{}", Uuid::new_v4());
        let reply = self
            .call(&workspace, &probe_command(&targets), &BTreeMap::new(), PROBE_LIMIT_S)
            .await
            .map_err(|failure| format!("the probe could not run: {}", failure.error.message))?;
        if judge(&reply, targets.len(), &allowed)? {
            tracing::warn!(
                "the sandbox can reach the internet, so the agent's code can too; run it on an \
                 internal network"
            );
        }
        self.probed.store(true, Ordering::SeqCst);
        tracing::info!(
            tried = targets.len(),
            "the sandbox reaches nothing of the env's but the proxy"
        );
        Ok(())
    }

    async fn wait(&self) -> Result<String, String> {
        let deadline = Instant::now() + SANDBOX_WAIT;
        loop {
            match self.health(Duration::from_secs(2)).await {
                Ok(peer) => {
                    let _ = self.peer.set(peer.clone());
                    return Ok(peer);
                }
                Err(e) if Instant::now() >= deadline => {
                    return Err(format!("the sandbox never answered at {}: {e}", self.url));
                }
                Err(_) => tokio::time::sleep(Duration::from_secs(1)).await,
            }
        }
    }
}

/// The `command` argument of a `run_code` call: 1 byte to 1 MiB of text.
pub fn command(arguments: &Map<String, Value>) -> Result<&str, ToolError> {
    allow_keys(arguments, &["command"])?;
    match arguments.get("command") {
        Some(Value::String(command)) if !command.is_empty() && command.len() <= COMMAND_LIMIT => {
            Ok(command)
        }
        _ => Err(ToolError::invalid("command must be a non-empty string of at most 1 MiB")),
    }
}

fn listening(table: &str) -> BTreeSet<u16> {
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let (local, state) = (fields.get(1)?, fields.get(3)?);
            if *state != "0A" {
                return None;
            }
            u16::from_str_radix(local.rsplit_once(':')?.1, 16).ok()
        })
        .collect()
}

fn targets(peer: &str, ports: &BTreeSet<u16>, own: u16) -> Vec<(String, u16)> {
    let mut targets: Vec<(String, u16)> = ports.iter().map(|&p| (peer.to_string(), p)).collect();
    targets.extend(ports.iter().filter(|&&p| p != own).map(|&p| ("127.0.0.1".to_string(), p)));
    targets
}

fn probe_command(targets: &[(String, u16)]) -> String {
    let targets = serde_json::to_string(targets).unwrap_or_else(|_| "[]".into());
    format!("python3 - <<'MIROIR_PROBE'\nTARGETS = {targets}\n{PROBE}MIROIR_PROBE\n")
}

fn judge(reply: &Value, targets: usize, allowed: &[String]) -> Result<bool, String> {
    let output = reply["output"].as_str().unwrap_or_default();
    let last = output.lines().rev().find(|line| !line.trim().is_empty()).unwrap_or_default();
    let report = serde_json::from_str::<Value>(last)
        .ok()
        .filter(|_| reply["exit_code"] == 0 && reply.get("timed_out").is_none())
        .ok_or_else(|| format!("the probe did not finish: {}", cut(output, 500)))?;
    let tried = report["tried"].as_u64();
    if tried != u64::try_from(targets).ok() {
        return Err(format!("the probe tried {} of {targets} addresses", tried.unwrap_or(0)));
    }
    let reached = report["reached"].as_array().ok_or("the probe's report has no reached")?;
    let leaks: Vec<&str> = reached
        .iter()
        .filter_map(Value::as_str)
        .filter(|address| !allowed.iter().any(|allowed| allowed == address))
        .collect();
    if !leaks.is_empty() {
        return Err(format!(
            "the sandbox reaches {}: give it its own network namespace, and bind the env's \
             services to 127.0.0.1",
            leaks.join(", ")
        ));
    }
    Ok(report["online"] == true)
}

/// The proxy: forwards `/{token}/{app}/path` to the app as the running call's persona.
pub fn proxy(env: Arc<Env>) -> Router {
    Router::new().fallback(proxied).layer(DefaultBodyLimit::max(FORWARD_LIMIT)).with_state(env)
}

fn refuse(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({"error": message}))).into_response()
}

fn split(path: &str) -> Option<(&str, &str, &str)> {
    let (token, after) = path.strip_prefix('/')?.split_once('/')?;
    let (app, rest) = after.find('/').map_or((after, "/"), |at| after.split_at(at));
    (!token.is_empty() && !app.is_empty()).then_some((token, app, rest))
}

async fn proxied(State(env): State<Arc<Env>>, request: Request) -> Response {
    let Some(sandbox) = &env.sandbox else {
        return refuse(StatusCode::NOT_FOUND, "this env has no sandbox");
    };
    let Some((token, app, rest)) = split(request.uri().path()) else {
        return refuse(StatusCode::NOT_FOUND, "use the URLs in the API_ variables");
    };
    let path = match request.uri().query() {
        Some(query) => format!("{rest}?{query}"),
        None => rest.to_string(),
    };
    let (token, app) = (token.to_string(), app.to_string());
    let Ok(Ok(permit)) = timeout(PERMIT_WAIT, sandbox.permits.clone().acquire_owned()).await else {
        return refuse(StatusCode::SERVICE_UNAVAILABLE, "too many requests at once; send fewer");
    };
    let guard = sandbox.active.clone().read_owned().await;
    let persona = match guard.as_ref() {
        Some(active) if constant_time_eq(active.token.as_bytes(), token.as_bytes()) => {
            active.persona.clone()
        }
        _ => {
            let message = "this URL works only during the run_code call it was given to";
            return refuse(StatusCode::FORBIDDEN, message);
        }
    };
    let base = match request.headers().get(header::HOST).and_then(|v| v.to_str().ok()) {
        Some(host) => format!("http://{host}/{token}/{app}"),
        None => format!("http://127.0.0.1:{PROXY_PORT}/{token}/{app}"),
    };
    let (parts, body) = request.into_parts();
    let call = Call { app, path, method: parts.method, headers: parts.headers, persona, base };
    let held = (guard, permit);
    match tokio::spawn(forward(env.clone(), call, body, held)).await {
        Ok(response) => response,
        Err(e) => refuse(StatusCode::INTERNAL_SERVER_ERROR, &format!("the request failed: {e}")),
    }
}

struct Call {
    app: String,
    path: String,
    method: Method,
    headers: HeaderMap,
    persona: String,
    base: String,
}

async fn forward(
    env: Arc<Env>,
    call: Call,
    body: Body,
    _held: (OwnedRwLockReadGuard<Option<Active>>, OwnedSemaphorePermit),
) -> Response {
    if !env.edb.code_allowed() {
        let message = "this episode's code made too many requests: more than 64 MiB of them";
        return refuse(StatusCode::TOO_MANY_REQUESTS, message);
    }
    let Ok(Ok(body)) = timeout(BODY_WAIT, axum::body::to_bytes(body, FORWARD_LIMIT)).await else {
        return refuse(StatusCode::PAYLOAD_TOO_LARGE, "the body passed 8 MiB, or never came");
    };
    let started = Instant::now();
    let methods = [Method::GET, Method::POST, Method::PUT, Method::PATCH, Method::DELETE];
    let known = methods.contains(&call.method);
    let outcome = if known {
        let persona = env.manifest.personas.get(&call.persona);
        let request = Forward {
            app: &call.app,
            method: call.method.clone(),
            path: &call.path,
            headers: &call.headers,
            body: body.clone(),
            persona,
            base: &call.base,
        };
        env.tools.forward(request).await
    } else {
        Err(ToolError::invalid("method must be GET, POST, PUT, PATCH or DELETE"))
    };
    let latency_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
    record(&env, &call, &body, &outcome, latency_ms).await;
    match outcome {
        Ok(answer) => {
            let mut response = Response::new(Body::from(answer.body));
            *response.status_mut() = answer.status;
            *response.headers_mut() = answer.headers;
            response
        }
        Err(error) => {
            let status = match error.error_type {
                "invalid_args" if !known => StatusCode::METHOD_NOT_ALLOWED,
                "invalid_args" => StatusCode::BAD_REQUEST,
                "tool_not_found" => StatusCode::NOT_FOUND,
                "timeout" => StatusCode::GATEWAY_TIMEOUT,
                _ => StatusCode::BAD_GATEWAY,
            };
            (status, Json(error.to_json())).into_response()
        }
    }
}

async fn record(
    env: &Env,
    call: &Call,
    body: &Bytes,
    outcome: &Result<Forwarded, ToolError>,
    latency_ms: i64,
) {
    let several = env.tools.apis().nth(1).is_some();
    let mut arguments = json!({"method": call.method.as_str(), "path": call.path});
    if several {
        arguments["app"] = json!(call.app);
    }
    if !body.is_empty() {
        arguments["body"] = sent_body(body, &call.headers);
    }
    let (status, result) = match outcome {
        Ok(answer) => {
            (Some(u64::from(answer.status.as_u16())), json!({"status": answer.status.as_u16()}))
        }
        Err(error) => (None, error.to_json()),
    };
    let row = ActionRow {
        tool: "api_request",
        app: several.then_some(call.app.as_str()),
        method: Some(call.method.as_str()),
        path: Some(call.path.as_str()),
        status,
        latency_ms,
        arguments: &arguments,
        result: &result,
        via: Some("run_code"),
    };
    env.edb.add_action(&row).await;
}

fn sent_body(body: &Bytes, headers: &HeaderMap) -> Value {
    let json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|kind| kind.contains("json"));
    if json
        && body.len() <= BODY_LIMIT
        && let Ok(parsed) = serde_json::from_slice(body)
    {
        return parsed;
    }
    Value::String(cut(&String::from_utf8_lossy(body), BODY_LIMIT).to_string())
}

impl Env {
    /// Run the agent's command in the sandbox.
    pub(crate) async fn run_code(
        &self,
        arguments: &Map<String, Value>,
        task: &Task,
        workspace: &str,
    ) -> Result<Value, ToolError> {
        let command = command(arguments)?;
        let Some(persona) = task.persona.as_deref() else {
            let message = "the task has no persona".into();
            return Err(ToolError { error_type: "execution_error", message });
        };
        let Some(sandbox) = &self.sandbox else {
            return Err(ToolError::unknown("run_code"));
        };
        let apps: Vec<&str> = self.tools.apis().collect();
        sandbox.run(workspace, command, persona, &apps).await.map_err(|failure| self.fail(failure))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_table_shows_the_ports_something_listens_on() {
        let table = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid\n\
           0: 0100007F:1538 00000000:0000 0A 00000000:00000000 00:00000000 00000000   999\n\
           1: 00000000:1F40 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0\n\
           2: 0100007F:1538 0100007F:A2C4 01 00000000:00000000 00:00000000 00000000   999\n";
        assert_eq!(listening(table), BTreeSet::from([5432, 8000]), "established is no listener");
        let six = "  sl  local_address                         remote_address                        st\n\
           0: 00000000000000000000000000000000:1F41 00000000000000000000000000000000:0000 0A\n";
        assert_eq!(listening(six), BTreeSet::from([8001]));
        assert!(listening("").is_empty());
    }

    #[test]
    fn the_probe_tries_every_port_on_the_env_and_on_loopback_but_its_own() {
        let targets = targets("10.0.3.5", &BTreeSet::from([5432, 8000, 8001]), 8000);
        let names: Vec<String> =
            targets.iter().map(|(host, port)| format!("{host}:{port}")).collect();
        assert_eq!(
            names,
            ["10.0.3.5:5432", "10.0.3.5:8000", "10.0.3.5:8001", "127.0.0.1:5432", "127.0.0.1:8001"]
        );
        let command = probe_command(&targets[..1]);
        assert!(
            command.starts_with("python3 - <<'MIROIR_PROBE'\nTARGETS = [[\"10.0.3.5\",5432]]\n")
        );
        assert!(command.ends_with("\nMIROIR_PROBE\n"), "{command}");
    }

    fn reply(output: &str) -> Value {
        json!({"exit_code": 0, "output": output})
    }

    #[test]
    fn the_probe_passes_only_when_it_ran_whole_and_reached_only_miroir() {
        let allowed = ["10.0.3.5:8001".to_string()];
        let report = |tried: usize, reached: &[&str]| {
            reply(&format!("{}\n", json!({"tried": tried, "reached": reached, "online": false})))
        };
        assert_eq!(judge(&report(4, &["10.0.3.5:8001"]), 4, &allowed), Ok(false));
        let leak = judge(&report(4, &["10.0.3.5:8001", "127.0.0.1:5432"]), 4, &allowed);
        assert!(leak.unwrap_err().contains("reaches 127.0.0.1:5432"));
        let short = judge(&report(3, &[]), 4, &allowed);
        assert!(short.unwrap_err().contains("tried 3 of 4"), "a probe that skipped some fails");
        let silent = judge(&reply("Traceback (most recent call last): ..."), 4, &allowed);
        assert!(silent.unwrap_err().contains("did not finish"), "no report is no pass");
        let mut cut_off = report(4, &[]);
        cut_off["timed_out"] = json!(true);
        assert!(judge(&cut_off, 4, &allowed).is_err(), "a probe that ran out of time fails");
        let failed = json!({"exit_code": 1, "output": report(4, &[])["output"]});
        assert!(judge(&failed, 4, &allowed).is_err());
        let online = reply(&json!({"tried": 0, "reached": [], "online": true}).to_string());
        assert_eq!(judge(&online, 0, &allowed), Ok(true));
    }

    #[test]
    fn a_proxied_path_names_the_token_and_the_app_and_keeps_the_rest_as_sent() {
        assert_eq!(
            split("/tok/gitea/repos/acme/a%2Fb/issues"),
            Some(("tok", "gitea", "/repos/acme/a%2Fb/issues"))
        );
        assert_eq!(split("/tok/gitea"), Some(("tok", "gitea", "/")));
        assert_eq!(split("/tok/gitea/"), Some(("tok", "gitea", "/")));
        assert_eq!(split("/tok"), None);
        assert_eq!(split("//gitea/x"), None);
        assert_eq!(split("/tok//x"), None);
    }

    #[test]
    fn each_apps_api_has_a_variable_named_after_it() {
        assert_eq!(api_variable("gitea"), "API_GITEA");
        assert_eq!(api_variable("west-ops"), "API_WEST_OPS");
        assert_eq!(api_variable("3d"), "API_3D", "a name may start with a digit");
    }

    #[test]
    fn a_command_is_a_string_that_fits() {
        let args = |value: Value| value.as_object().cloned().unwrap();
        assert_eq!(command(&args(json!({"command": "ls"}))), Ok("ls"));
        assert!(command(&args(json!({"command": ""}))).is_err());
        assert!(command(&args(json!({"command": 7}))).is_err());
        assert!(command(&args(json!({}))).is_err());
        assert!(command(&args(json!({"command": "ls", "cwd": "/"}))).is_err());
        let huge = "x".repeat(COMMAND_LIMIT + 1);
        assert!(command(&args(json!({"command": huge}))).is_err());
    }

    async fn fake(status: StatusCode, body: &'static str, delay: Duration) -> Sandbox {
        let run = move |request: Request| async move {
            tokio::time::sleep(delay).await;
            let sent = axum::body::to_bytes(request.into_body(), 1 << 20).await.unwrap_or_default();
            let echo = String::from_utf8_lossy(&sent).replace('"', "'");
            (status, body.replace("ECHO", &echo))
        };
        let router = Router::new()
            .route(
                "/health",
                axum::routing::get(|| async {
                    Json(json!({"status": "healthy", "peer": "10.9.9.9"}))
                }),
            )
            .route("/run", axum::routing::post(run));
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, router).await });
        Sandbox { slack: Duration::from_millis(200), ..Sandbox::new(&url).unwrap() }
    }

    #[tokio::test]
    async fn a_reply_is_the_result_and_the_code_reaches_each_api_under_the_runs_token() {
        let sandbox =
            fake(StatusCode::OK, r#"{"exit_code": 0, "output": "ECHO"}"#, Duration::ZERO).await;
        let result = sandbox.run("w1", "ls", "maya", &["gitea", "chat"]).await.unwrap();
        let sent = result["output"].as_str().unwrap();
        assert!(sent.contains("'workspace':'w1'") && sent.contains("'timeout_s':60"), "{sent}");
        let token = sent.split("http://10.9.9.9:8001/").nth(1).unwrap().split('/').next().unwrap();
        assert_eq!(token.len(), 32, "{sent}");
        assert!(sent.contains(&format!("'API_GITEA':'http://10.9.9.9:8001/{token}/gitea'")));
        assert!(sent.contains(&format!("'API_CHAT':'http://10.9.9.9:8001/{token}/chat'")));
        assert!(sandbox.active.read().await.is_none(), "the token works only during the run");
    }

    #[tokio::test]
    async fn faults_that_are_the_sandboxs_void_the_grade() {
        let gone = Sandbox::new("http://127.0.0.1:1").unwrap();
        let cases = [
            (gone, "transport_error"),
            (
                fake(StatusCode::INTERNAL_SERVER_ERROR, r#"{"error": "x"}"#, Duration::ZERO).await,
                "execution_error",
            ),
            (
                fake(StatusCode::CONFLICT, r#"{"error": "another run is going"}"#, Duration::ZERO)
                    .await,
                "execution_error",
            ),
            (fake(StatusCode::OK, "not json", Duration::ZERO).await, "execution_error"),
            (
                fake(StatusCode::OK, r#"{"exit_code": 0, "output": ""}"#, Duration::from_secs(5))
                    .await,
                "execution_error",
            ),
        ];
        for (sandbox, error_type) in cases {
            let started = std::time::Instant::now();
            let failure = sandbox.call("w1", "ls", &BTreeMap::new(), 1).await.unwrap_err();
            assert!(failure.void, "{}", failure.error.message);
            assert_eq!(failure.error.error_type, error_type, "{}", failure.error.message);
            assert!(started.elapsed() < Duration::from_secs(3), "a hang hits the deadline");
        }
    }

    #[test]
    fn a_recorded_body_is_json_when_it_says_so_and_text_otherwise() {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_TYPE, "application/json; charset=utf-8".parse().unwrap());
        let body = Bytes::from_static(br#"{"state": "closed"}"#);
        assert_eq!(sent_body(&body, &headers), json!({"state": "closed"}));
        assert_eq!(sent_body(&body, &HeaderMap::new()), json!(r#"{"state": "closed"}"#));
        let big = Bytes::from("é".repeat(BODY_LIMIT));
        let kept = sent_body(&big, &HeaderMap::new());
        assert!(kept.as_str().unwrap().len() <= BODY_LIMIT);
    }
}
