//! The HTTP server: `OpenEnv` over a WebSocket at `/ws`, MCP over JSON-RPC at `/mcp`, and
//! the task listings, behind an optional bearer token.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::extract::Path;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

use crate::clock::WAIT;
use crate::env::{Env, EnvError};
use crate::episode::{Action, ResetArgs, coworker_names};
use crate::mcp::{MCP_VERSION, MCP_VERSIONS};
use crate::task::{SPLITS, TOOLS, Task};
use crate::tools::constant_time_eq;

const IDLE: std::time::Duration = std::time::Duration::from_secs(30);
const QUEUE_LIMIT: usize = 16;
const FRAME_LIMIT: usize = 8 * 1024 * 1024;
const CLOSE_LIMIT: std::time::Duration = std::time::Duration::from_secs(2);
const CONNECTION_LIMIT: usize = 256;
const MCP_CALL_LIMIT: usize = 8;
const MEND_EVERY: std::time::Duration = std::time::Duration::from_secs(10);
const MEND_BACKOFF_LIMIT: std::time::Duration = std::time::Duration::from_secs(300);
/// What an MCP client's model reads after the call that ended its episode.
const LAST_CALL: &str = "That was the last tool call: the episode is over; stop calling tools.";

/// Server state: the env, its bearer token, and the one session it serves at a time.
pub struct App {
    env: Arc<Env>,
    token: Option<String>,
    session_id: String,
    session_taken: AtomicBool,
    mcp_calls: Semaphore,
    schemas: Value,
}

/// Serve HTTP/1 connections, at most 256 at once.
pub async fn serve(listener: TcpListener, router: Router) {
    let slots = Arc::new(Semaphore::new(CONNECTION_LIMIT));
    while let Ok(slot) = slots.clone().acquire_owned().await {
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(e) => {
                tracing::error!("cannot accept a connection: {e}");
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                continue;
            }
        };
        let service = TowerToHyperService::new(router.clone());
        tokio::spawn(async move {
            let _slot = slot;
            let connection = http1::Builder::new()
                .timer(TokioTimer::new())
                .serve_connection(TokioIo::new(stream), service)
                .with_upgrades();
            if let Err(e) = connection.await {
                tracing::debug!("a connection ended: {e}");
            }
        });
    }
}

async fn mend(app: Arc<App>) {
    let mut wait = MEND_EVERY;
    loop {
        tokio::time::sleep(wait).await;
        if app.session_taken.load(Ordering::SeqCst) {
            continue;
        }
        wait = match app.env.mend().await {
            Ok(()) => MEND_EVERY,
            Err(e) => {
                tracing::error!("cannot restore the broken env: {e}");
                (wait * 2).min(MEND_BACKOFF_LIMIT)
            }
        };
    }
}

/// The env's routes; also starts the task that restores a broken env.
pub fn router(env: Arc<Env>, token: Option<String>) -> Router {
    let schema = |text: &str| serde_json::from_str::<Value>(text).unwrap_or(Value::Null);
    let schemas = json!({
        "action": schema(include_str!("schemas/action.json")),
        "observation": schema(include_str!("schemas/observation.json")),
        "reset_observation": schema(include_str!("schemas/reset_observation.json")),
        "state": schema(include_str!("schemas/state.json")),
        "reset": schema(include_str!("schemas/reset.json")),
    });
    let app = Arc::new(App {
        env,
        token,
        session_id: format!("miroir-{}", uuid::Uuid::new_v4()),
        session_taken: AtomicBool::new(false),
        mcp_calls: Semaphore::new(MCP_CALL_LIMIT),
        schemas,
    });
    tokio::spawn(mend(app.clone()));
    Router::new()
        .route("/health", get(health))
        .route("/metadata", get(metadata))
        .route("/openapi.json", get(openapi))
        .route(
            "/schema",
            get(|State(app): State<Arc<App>>| async move { Json(app.schemas.clone()) }),
        )
        .route("/ws", get(websocket))
        .route("/mcp", post(mcp))
        .route(
            "/list_environments",
            get(|State(app): State<Arc<App>>| async move { Json(json!([app.env.manifest.name])) }),
        )
        .route("/{name}/splits", get(splits))
        .route("/{name}/tasks", post(tasks))
        .route("/{name}/num_tasks", post(num_tasks))
        .route("/{name}/task", post(task))
        .route("/{name}/task_range", post(task_range))
        .layer(middleware::from_fn_with_state(app.clone(), authorize))
        .with_state(app)
}

async fn authorize(State(app): State<Arc<App>>, request: Request, next: Next) -> Response {
    if request.uri().path() == "/health" {
        return next.run(request).await;
    }
    if request.headers().contains_key(header::ORIGIN) {
        let refusal = Json(json!({"detail": "requests from web pages are refused"}));
        return (StatusCode::FORBIDDEN, refusal).into_response();
    }
    if let Some(token) = &app.token {
        let sent = request.headers().get(header::AUTHORIZATION).and_then(|v| v.to_str().ok());
        let expected = format!("Bearer {token}");
        let in_header =
            sent.is_some_and(|sent| constant_time_eq(sent.as_bytes(), expected.as_bytes()));
        let in_url = request.uri().path() == "/mcp"
            && Query::<McpQuery>::try_from_uri(request.uri())
                .ok()
                .and_then(|Query(query)| query.token)
                .is_some_and(|sent| constant_time_eq(sent.as_bytes(), token.as_bytes()));
        if !in_header && !in_url {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"detail": "missing or wrong bearer token"})),
            )
                .into_response();
        }
    }
    next.run(request).await
}

async fn health(State(app): State<Arc<App>>) -> (StatusCode, Json<Value>) {
    let leased = app.session_taken.load(Ordering::SeqCst);
    let sandbox = match &app.env.sandbox {
        Some(sandbox) => sandbox.healthy().await,
        None => true,
    };
    if app.env.healthy() && !app.env.stopping() && sandbox {
        (StatusCode::OK, Json(json!({"status": "healthy", "leased": leased})))
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"status": "unhealthy", "leased": leased})))
    }
}

async fn metadata(State(app): State<Arc<App>>) -> Json<Value> {
    let manifest = &app.env.manifest;
    let version = std::env::var("MIROIR_ENV_VERSION").ok();
    Json(json!({
        "name": manifest.name,
        "description": manifest.description,
        "version": version,
        "miroir_version": env!("CARGO_PKG_VERSION"),
        "manifest_hash": app.env.manifest_hash,
        "tasks_hash": app.env.tasks_hash,
    }))
}

async fn openapi(State(app): State<Arc<App>>) -> Json<Value> {
    let route = |method: &str, summary: &str| json!({method: {"summary": summary, "responses": {"200": {"description": "OK"}}}});
    Json(json!({
        "openapi": "3.1.0",
        "info": {"title": app.env.manifest.name, "version": env!("CARGO_PKG_VERSION")},
        "paths": {
            "/health": route(
                "get",
                "503 until the first restore, when the env is unusable, while its sandbox doesn't \
                 answer, and once it is shutting down; `leased` says whether a session holds it",
            ),
            "/metadata": route("get", "Name, description, versions and hashes"),
            "/schema": route("get", "JSON schemas of the frames"),
            "/ws": route(
                "get",
                "The OpenEnv session: reset, step, state, record and close frames, and the \
                 coworkers and coworker_reply frames of a client that answers for coworkers",
            ),
            "/mcp": route("post", "MCP over JSON-RPC"),
            "/list_environments": route("get", "This env's name"),
            "/{name}/splits": route("get", "The splits that have tasks"),
            "/{name}/tasks": route("post", "A split's tasks"),
            "/{name}/num_tasks": route("post", "How many tasks a split has"),
            "/{name}/task": route("post", "A split's task at an index"),
            "/{name}/task_range": route("post", "A range of a split's tasks"),
        },
    }))
}

#[derive(Deserialize)]
struct SessionQuery {
    session_id: Option<String>,
}

async fn websocket(
    upgrade: WebSocketUpgrade,
    Query(query): Query<SessionQuery>,
    State(app): State<Arc<App>>,
) -> Response {
    upgrade
        .max_message_size(FRAME_LIMIT)
        .on_upgrade(move |socket| session(socket, app, query.session_id))
}

struct SessionSlot<'a>(&'a AtomicBool);

impl Drop for SessionSlot<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

async fn session(mut socket: WebSocket, app: Arc<App>, session_id: Option<String>) {
    if let Some(id) = session_id.filter(|id| *id != app.session_id) {
        let message = format!("Unknown session_id: {id}; this env's is '{}'", app.session_id);
        refuse(socket, error_frame("SESSION_ERROR", message)).await;
        return;
    }
    if app.env.stopping() {
        refuse(socket, error_frame("CAPACITY_REACHED", "this env is shutting down")).await;
        return;
    }
    if app.session_taken.swap(true, Ordering::SeqCst) {
        let message = "this env serves one session at a time; another connection holds it";
        refuse(socket, error_frame("CAPACITY_REACHED", message)).await;
        return;
    }
    let slot = SessionSlot(&app.session_taken);
    let mut queued = VecDeque::new();
    let mut pinged = false;
    loop {
        let message = if let Some(message) = queued.pop_front() {
            message
        } else {
            match tokio::time::timeout(IDLE, socket.recv()).await {
                Ok(Some(Ok(message))) => message,
                Err(_) if !pinged => {
                    pinged = true;
                    if !send(&mut socket, Message::Ping(axum::body::Bytes::new())).await {
                        break;
                    }
                    continue;
                }
                _ => break,
            }
        };
        pinged = false;
        let reply = match message {
            Message::Text(text) => {
                reading_during(&mut socket, &mut queued, frame_reply(&app, text.as_str())).await
            }
            Message::Binary(_) => Some(error_frame("INVALID_JSON", "frames must be JSON text")),
            Message::Close(_) => None,
            Message::Ping(_) | Message::Pong(_) => continue,
        };
        let Some(reply) = reply else { break };
        if !send(&mut socket, Message::Text(reply.to_string().into())).await {
            break;
        }
    }
    drop(slot);
    send(&mut socket, Message::Close(None)).await;
    let _ = tokio::time::timeout(CLOSE_LIMIT, socket.recv()).await;
}

async fn send(socket: &mut WebSocket, message: Message) -> bool {
    tokio::time::timeout(IDLE, socket.send(message)).await.is_ok_and(|sent| sent.is_ok())
}

async fn reading_during<T>(
    socket: &mut WebSocket,
    queued: &mut VecDeque<Message>,
    work: impl Future<Output = T>,
) -> T {
    tokio::pin!(work);
    let mut open = true;
    loop {
        tokio::select! {
            output = &mut work => return output,
            incoming = socket.recv(), if open && queued.len() < QUEUE_LIMIT => match incoming {
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                Some(Ok(message)) => queued.push_back(message),
                _ => open = false,
            },
        }
    }
}

async fn refuse(mut socket: WebSocket, frame: Value) {
    send(&mut socket, Message::Text(frame.to_string().into())).await;
    send(&mut socket, Message::Close(None)).await;
}

async fn frame_reply(app: &App, text: &str) -> Option<Value> {
    let frame: Value = match serde_json::from_str(text) {
        Ok(frame) => frame,
        Err(e) => return Some(error_frame("INVALID_JSON", format!("Invalid JSON: {e}"))),
    };
    let Some(fields) = frame.as_object() else {
        return Some(error_frame("VALIDATION_ERROR", "a frame must be a JSON object"));
    };
    if let Some(extra) = fields.keys().find(|k| *k != "type" && *k != "data") {
        return Some(error_frame("VALIDATION_ERROR", format!("unexpected field '{extra}'")));
    }
    let data = fields.get("data").cloned().unwrap_or_else(|| json!({}));
    let kind = fields.get("type").and_then(Value::as_str).unwrap_or_default();
    let env = &app.env;
    Some(match kind {
        "reset" => match parsed::<ResetArgs>(kind, data) {
            Ok(args) => reply("observation", env.reset(args).await),
            Err(refusal) => refusal,
        },
        "step" => match Action::parse(&data) {
            Ok(action) => reply("observation", env.step(action).await),
            Err(e) => error_frame("VALIDATION_ERROR", e),
        },
        "state" => json!({"type": "state", "data": env.state().await}),
        "record" => match parsed::<RecordArgs>(kind, data) {
            Ok(args) => reply(kind, env.record(args.episode_id.as_deref()).await),
            Err(refusal) => refusal,
        },
        "coworkers" => match parsed::<PullArgs>(kind, data) {
            Ok(PullArgs {}) => reply(kind, env.coworker_turns().await),
            Err(refusal) => refusal,
        },
        "coworker_reply" => match parsed::<ReplyArgs>(kind, data) {
            Ok(args) => reply(kind, env.coworker_reply(args.coworker, args.text).await),
            Err(refusal) => refusal,
        },
        "close" => return None,
        _ => error_frame("UNKNOWN_TYPE", format!("Unknown message type: {kind}")),
    })
}

fn parsed<T: serde::de::DeserializeOwned>(kind: &str, data: Value) -> Result<T, Value> {
    serde_json::from_value(data)
        .map_err(|e| error_frame("VALIDATION_ERROR", format!("bad {kind} data: {e}")))
}

fn reply(kind: &str, result: Result<Value, EnvError>) -> Value {
    match result {
        Ok(data) => json!({"type": kind, "data": data}),
        Err(e) => error_frame(e.code(), e.to_string()),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordArgs {
    episode_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PullArgs {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplyArgs {
    coworker: String,
    text: String,
}

fn error_frame(code: &str, message: impl Into<String>) -> Value {
    json!({"type": "error", "data": {"message": message.into(), "code": code}})
}

#[derive(Deserialize)]
struct McpQuery {
    episode_id: Option<String>,
    token: Option<String>,
    surface: Option<String>,
}

async fn mcp(
    State(app): State<Arc<App>>,
    Query(query): Query<McpQuery>,
    body: axum::body::Bytes,
) -> Response {
    let request: Value = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(_) => return Json(rpc_error(&Value::Null, -32700, "Parse error")).into_response(),
    };
    let notification = request.get("id").is_none()
        && request["method"].as_str().is_some_and(|m| m.starts_with("notifications/"));
    if notification {
        return StatusCode::ACCEPTED.into_response();
    }
    Json(mcp_reply(&app, &request, &query).await).into_response()
}

async fn mcp_reply(app: &App, request: &Value, query: &McpQuery) -> Value {
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let well_formed = request.get("jsonrpc") == Some(&json!("2.0"))
        && request.get("method").is_some_and(Value::is_string)
        && (id.is_null() || id.is_string() || id.is_i64() || id.is_u64());
    if !well_formed {
        return rpc_error(&id, -32600, "Invalid Request");
    }
    let params = match request.get("params") {
        None => json!({}),
        Some(params @ Value::Object(_)) => params.clone(),
        Some(_) => return rpc_error(&id, -32602, "Params must be an object"),
    };
    let surface = match surface(app, query.surface.as_deref()) {
        Ok(surface) => surface,
        Err(message) => return rpc_error(&id, -32602, &message),
    };
    let result = match request["method"].as_str().unwrap_or_default() {
        "initialize" => {
            let asked = params.get("protocolVersion").and_then(Value::as_str);
            json!({
                "protocolVersion": asked.filter(|v| MCP_VERSIONS.contains(v)).unwrap_or(MCP_VERSION),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": app.env.manifest.name, "version": env!("CARGO_PKG_VERSION")},
            })
        }
        "ping" => json!({}),
        "openenv/session/create" => json!({"session_id": app.session_id}),
        "openenv/session/close" => json!({"session_id": app.session_id, "closed": false}),
        "tools/list" => match mcp_tools(app, surface, query.episode_id.as_deref()).await {
            Ok(tools) => json!({"tools": tools}),
            Err(message) => return rpc_error(&id, -32603, &message),
        },
        "tools/call" => match mcp_call_tool(app, &params, query.episode_id.as_deref()).await {
            Ok(result) => result,
            Err((code, message)) => return rpc_error(&id, code, &message),
        },
        method => return rpc_error(&id, -32601, &format!("Method not found: {method}")),
    };
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

#[derive(Clone, Copy)]
enum Surface {
    Api,
    Mcp,
    Code,
}

fn surface(app: &App, surface: Option<&str>) -> Result<Surface, String> {
    match surface {
        None | Some("api") => Ok(Surface::Api),
        Some("mcp") if app.env.mcp.is_some() => Ok(Surface::Mcp),
        Some("mcp") => Err("surface 'mcp' needs an app's MCP server, and this env has none".into()),
        Some("code") if app.env.sandbox.is_some() => Ok(Surface::Code),
        Some("code") => {
            Err("surface 'code' needs a sandbox (MIROIR_SANDBOX), and this env has none".into())
        }
        Some(other) => Err(format!("unknown surface '{other}': use api, mcp or code")),
    }
}

async fn mcp_tools(
    app: &App,
    surface: Surface,
    episode_id: Option<&str>,
) -> Result<Vec<Value>, String> {
    let shaped = |t: Value| {
        json!({"name": t["name"], "description": t["description"],
               "inputSchema": t["input_schema"]})
    };
    let clock = app.env.offers_wait(episode_id).await;
    let own = app.env.tools.list(matches!(surface, Surface::Code), clock).into_iter();
    if !matches!(surface, Surface::Mcp) {
        return Ok(own.map(shaped).collect());
    }
    let listed = match app.env.mcp_tools().await {
        Some(Ok(listed)) => listed.to_vec(),
        Some(Err(failure)) => return Err(failure.error.message),
        None => Vec::new(),
    };
    let kept = own.filter(|t| t["name"] == "submit" || t["name"] == WAIT);
    Ok(listed.into_iter().chain(kept.map(shaped)).collect())
}

async fn mcp_call_tool(
    app: &App,
    params: &Value,
    url_episode: Option<&str>,
) -> Result<Value, (i64, String)> {
    let session = params.get("session_id").and_then(Value::as_str);
    if let Some(session) = session.filter(|session| *session != app.session_id) {
        return Err((-32602, format!("Unknown session_id: {session}")));
    }
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or((-32602, "Missing 'name' in params".to_string()))?;
    let arguments = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
    let action = Action::parse(&json!({
        "type": "call_tool",
        "tool_name": name,
        "arguments": arguments,
        "metadata": {
            "episode_id": params.get("episode_id").and_then(Value::as_str).or(url_episode),
        },
    }))
    .map_err(|e| (-32602, e))?;
    let Ok(_call) = app.mcp_calls.try_acquire() else {
        return Err((-32000, "too many calls are waiting; try again".to_string()));
    };
    let envelope = match app.env.step(action).await {
        Ok(envelope) => envelope,
        Err(e) => {
            let error = json!({"error_type": "execution_error", "message": e.to_string()});
            return Ok(json!({
                "tool_name": name,
                "result": null,
                "error": error,
                "content": [{"type": "text", "text": error.to_string()}],
                "isError": true,
            }));
        }
    };
    let mut observation = envelope["observation"].clone();
    let is_error = !observation["error"].is_null();
    let passed = observation["result"]["content"].clone();
    observation["content"] = if !is_error && passed.is_array() && !TOOLS.contains(&name) {
        passed
    } else {
        let shown = observation[if is_error { "error" } else { "result" }].to_string();
        json!([{"type": "text", "text": shown}])
    };
    if envelope["done"] == true
        && let Some(content) = observation["content"].as_array_mut()
    {
        content.push(json!({"type": "text", "text": LAST_CALL}));
    }
    observation["isError"] = Value::Bool(is_error);
    observation["done"] = envelope["done"].clone();
    observation["reward"] = envelope["reward"].clone();
    Ok(observation)
}

fn rpc_error(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message, "data": null}})
}

#[derive(Deserialize)]
struct SplitRequest {
    split: String,
    index: Option<usize>,
    start: Option<usize>,
    stop: Option<usize>,
}

type TaskApiResult = Result<Json<Value>, (StatusCode, Json<Value>)>;

fn task_api_error(status: StatusCode, detail: &str) -> (StatusCode, Json<Value>) {
    (status, Json(json!({"detail": detail})))
}

fn split_tasks<'a>(
    app: &'a App,
    name: &str,
    split: &str,
) -> Result<Vec<&'a Task>, (StatusCode, Json<Value>)> {
    if !name.eq_ignore_ascii_case(&app.env.manifest.name) {
        return Err(task_api_error(
            StatusCode::NOT_FOUND,
            &format!("unknown environment '{name}'"),
        ));
    }
    if !SPLITS.contains(&split) {
        return Err(task_api_error(StatusCode::BAD_REQUEST, &format!("unknown split '{split}'")));
    }
    Ok(app.env.tasks.iter().filter(|t| t.split == split).collect())
}

fn task_json(env: &Env, task: &Task) -> Value {
    let mut listed = json!({
        "id": task.id,
        "split": task.split,
        "world": task.world,
        "instruction": task.instruction.trim(),
        "persona": env.persona_json(task),
        "max_steps": task.max_steps,
        "task_hash": task.hash,
    });
    if let Some(prompt) = &env.manifest.system_prompt {
        listed["system_prompt"] = json!(prompt.trim());
    }
    coworker_names(listed, task)
}

async fn splits(State(app): State<Arc<App>>, Path(name): Path<String>) -> TaskApiResult {
    let mut found = Vec::new();
    for split in SPLITS {
        if !split_tasks(&app, &name, split)?.is_empty() {
            found.push(json!({"name": split, "type": split}));
        }
    }
    Ok(Json(Value::Array(found)))
}

async fn tasks(
    State(app): State<Arc<App>>,
    Path(name): Path<String>,
    Json(request): Json<SplitRequest>,
) -> TaskApiResult {
    let tasks: Vec<Value> = split_tasks(&app, &name, &request.split)?
        .into_iter()
        .map(|t| task_json(&app.env, t))
        .collect();
    Ok(Json(json!({"tasks": tasks, "env_name": app.env.manifest.name})))
}

async fn num_tasks(
    State(app): State<Arc<App>>,
    Path(name): Path<String>,
    Json(request): Json<SplitRequest>,
) -> TaskApiResult {
    Ok(Json(json!({"num_tasks": split_tasks(&app, &name, &request.split)?.len()})))
}

async fn task(
    State(app): State<Arc<App>>,
    Path(name): Path<String>,
    Json(request): Json<SplitRequest>,
) -> TaskApiResult {
    let tasks = split_tasks(&app, &name, &request.split)?;
    let found = request.index.and_then(|i| tasks.get(i));
    let task =
        found.ok_or_else(|| task_api_error(StatusCode::BAD_REQUEST, "Invalid task index"))?;
    Ok(Json(json!({"task": task_json(&app.env, task)})))
}

async fn task_range(
    State(app): State<Arc<App>>,
    Path(name): Path<String>,
    Json(request): Json<SplitRequest>,
) -> TaskApiResult {
    let tasks = split_tasks(&app, &name, &request.split)?;
    let stop = request.stop.unwrap_or(tasks.len()).min(tasks.len());
    let start = request.start.unwrap_or(0).min(stop);
    let range: Vec<Value> = tasks[start..stop].iter().map(|t| task_json(&app.env, t)).collect();
    Ok(Json(json!({"tasks": range})))
}
