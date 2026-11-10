//! `miroir sandbox`: runs the agent's bash commands as user `code` in `/work`, one at a
//! time, for the env next door.

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::os::fd::OwnedFd;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, PoisonError};
use std::time::Duration;

use anyhow::Context;
use axum::extract::{ConnectInfo, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rustix::process::{Pid, Signal};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::net::unix::pipe;
use tokio::sync::{Mutex, OwnedMutexGuard};
use tokio::time::{Instant, timeout};

use crate::state;
use crate::supervisor::{self, exit_code, lookup_user};
use crate::tools::{BODY_LIMIT, cut};

const WORK: &str = "/work";
const USER: &str = "code";
const PATH: &str = "/usr/local/bin:/usr/bin:/bin";
const MAX_TIMEOUT_S: u64 = 600;
const WEDGED_AFTER: Duration = Duration::from_secs(30);
const DRAIN_LIMIT: Duration = Duration::from_secs(5);

/// A `/run` request: workspace, command, timeout and environment.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunArgs {
    pub workspace: String,
    pub command: String,
    pub timeout_s: u64,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

impl RunArgs {
    fn check(&self) -> Result<(), String> {
        if self.workspace.is_empty() || self.workspace.len() > 255 {
            return Err("workspace must be 1 to 255 characters".into());
        }
        if self.command.is_empty() {
            return Err("command must not be empty".into());
        }
        if !(1..=MAX_TIMEOUT_S).contains(&self.timeout_s) {
            return Err(format!("timeout_s must be 1 to {MAX_TIMEOUT_S}"));
        }
        Ok(())
    }
}

struct Sandbox {
    user: (u32, u32),
    workspace: Arc<Mutex<Option<String>>>,
    due: std::sync::Mutex<Option<Instant>>,
}

/// Serve `/health` and `/run`.
pub async fn serve(port: u16) -> anyhow::Result<()> {
    let user = lookup_user(USER).await.map_err(anyhow::Error::msg)?;
    let sandbox = Arc::new(Sandbox {
        user,
        workspace: Arc::new(Mutex::new(None)),
        due: std::sync::Mutex::new(None),
    });
    let router =
        Router::new().route("/health", get(health)).route("/run", post(run)).with_state(sandbox);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .with_context(|| format!("cannot listen on port {port}"))?;
    tracing::info!(port, "serving the sandbox");
    axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>())
        .await
        .context("the sandbox server stopped")
}

async fn health(
    State(sandbox): State<Arc<Sandbox>>,
    ConnectInfo(caller): ConnectInfo<SocketAddr>,
) -> Response {
    let due = *sandbox.due.lock().unwrap_or_else(PoisonError::into_inner);
    if due.is_some_and(|due| Instant::now() > due) {
        let wedged = json!({"status": "wedged"});
        return (StatusCode::SERVICE_UNAVAILABLE, Json(wedged)).into_response();
    }
    Json(json!({"status": "healthy", "peer": host(caller.ip())})).into_response()
}

fn host(ip: IpAddr) -> String {
    match ip.to_canonical() {
        IpAddr::V4(ip) => ip.to_string(),
        IpAddr::V6(ip) => format!("[{ip}]"),
    }
}

async fn run(State(sandbox): State<Arc<Sandbox>>, Json(args): Json<RunArgs>) -> Response {
    if let Err(message) = args.check() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": message}))).into_response();
    }
    let Ok(workspace) = sandbox.workspace.clone().try_lock_owned() else {
        let busy = json!({"error": "another run is going"});
        return (StatusCode::CONFLICT, Json(busy)).into_response();
    };
    let due = Instant::now() + Duration::from_secs(args.timeout_s) + WEDGED_AFTER;
    *sandbox.due.lock().unwrap_or_else(PoisonError::into_inner) = Some(due);
    match tokio::spawn(execute(sandbox, workspace, args)).await {
        Ok(Ok(result)) => Json(result).into_response(),
        Ok(Err(message)) => {
            tracing::error!("a run failed: {message}");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": message}))).into_response()
        }
        Err(e) => {
            let message = format!("the run failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": message}))).into_response()
        }
    }
}

async fn execute(
    sandbox: Arc<Sandbox>,
    mut workspace: OwnedMutexGuard<Option<String>>,
    args: RunArgs,
) -> Result<Value, String> {
    let started = Instant::now();
    let outcome = run_command(&sandbox, &mut workspace, &args).await;
    let swept = supervisor::kill_others().await.map_err(|e| e.to_string());
    let outcome = match (outcome, swept) {
        (Ok(running), Ok(())) => running.finish().await,
        (Err(e), _) | (_, Err(e)) => Err(e),
    };
    *sandbox.due.lock().unwrap_or_else(PoisonError::into_inner) = None;
    let seconds = started.elapsed().as_secs_f64();
    tracing::info!(workspace = %args.workspace, seconds, ok = outcome.is_ok(), "ran");
    outcome
}

struct Ended {
    status: std::process::ExitStatus,
    timed_out: bool,
    output: tokio::task::JoinHandle<std::io::Result<(Vec<u8>, bool)>>,
}

impl Ended {
    async fn finish(self) -> Result<Value, String> {
        let drained = timeout(DRAIN_LIMIT, self.output)
            .await
            .map_err(|_| "the output never closed".to_string())?
            .map_err(|e| format!("the output reader failed: {e}"))?
            .map_err(|e| format!("cannot read the output: {e}"))?;
        Ok(result(exit_code(self.status), self.timed_out, drained))
    }
}

async fn run_command(
    sandbox: &Sandbox,
    workspace: &mut Option<String>,
    args: &RunArgs,
) -> Result<Ended, String> {
    if workspace.as_deref() != Some(args.workspace.as_str()) {
        let user = sandbox.user;
        tokio::task::spawn_blocking(move || fresh_work(user))
            .await
            .map_err(|e| format!("emptying the workspace failed: {e}"))?
            .map_err(|e| format!("cannot empty the workspace: {e}"))?;
        *workspace = Some(args.workspace.clone());
    }
    let (reader, writer) = std::io::pipe().map_err(|e| format!("cannot make a pipe: {e}"))?;
    let second = writer.try_clone().map_err(|e| format!("cannot copy the pipe: {e}"))?;
    let mut command = tokio::process::Command::new("/bin/bash");
    command
        .arg("-c")
        .arg(&args.command)
        .current_dir(WORK)
        .env_clear()
        .envs([("PATH", PATH), ("HOME", WORK), ("LANG", "C.UTF-8")])
        .envs(&args.env)
        .stdin(Stdio::null())
        .stdout(writer)
        .stderr(second)
        .uid(sandbox.user.0)
        .gid(sandbox.user.1)
        .process_group(0);
    let child = command.spawn().map_err(|e| format!("cannot start bash: {e}"));
    drop(command);
    let mut child = child?;
    let reader = pipe::Receiver::from_owned_fd(OwnedFd::from(reader))
        .map_err(|e| format!("cannot read the pipe: {e}"))?;
    let output = tokio::spawn(read_capped(reader, BODY_LIMIT + 4));
    let limit = Duration::from_secs(args.timeout_s);
    let (status, timed_out) = if let Ok(status) = timeout(limit, child.wait()).await {
        (status, false)
    } else {
        let group = child.id().and_then(|id| i32::try_from(id).ok()).and_then(Pid::from_raw);
        if let Some(group) = group {
            let _ = rustix::process::kill_process_group(group, Signal::KILL);
        }
        (child.wait().await, true)
    };
    let status = status.map_err(|e| format!("cannot wait for bash: {e}"))?;
    Ok(Ended { status, timed_out, output })
}

fn fresh_work(user: (u32, u32)) -> std::io::Result<()> {
    for dir in state::TEMP_DIRS.iter().chain([&WORK]) {
        state::empty(Path::new(dir))?;
    }
    std::fs::create_dir_all(WORK)?;
    std::os::unix::fs::chown(WORK, Some(user.0), Some(user.1))?;
    std::fs::set_permissions(WORK, std::fs::Permissions::from_mode(0o700))
}

async fn read_capped(
    mut reader: impl AsyncRead + Unpin,
    limit: usize,
) -> std::io::Result<(Vec<u8>, bool)> {
    let mut kept = Vec::new();
    let mut more = false;
    let mut buffer = [0_u8; 8192];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            return Ok((kept, more));
        }
        let room = limit.saturating_sub(kept.len());
        kept.extend_from_slice(&buffer[..read.min(room)]);
        more |= read > room;
    }
}

fn result(exit_code: i32, timed_out: bool, (bytes, more): (Vec<u8>, bool)) -> Value {
    let text = String::from_utf8_lossy(&bytes);
    let output = cut(&text, BODY_LIMIT);
    let mut result = json!({"exit_code": exit_code, "output": output});
    if timed_out {
        result["timed_out"] = Value::Bool(true);
    }
    if more || output.len() < text.len() {
        result["truncated"] = Value::Bool(true);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hosts_address_fits_in_a_url() {
        assert_eq!(host("10.0.3.5".parse().unwrap()), "10.0.3.5");
        assert_eq!(host("::ffff:10.0.3.5".parse().unwrap()), "10.0.3.5", "mapped is IPv4");
        assert_eq!(host("fd00::5".parse().unwrap()), "[fd00::5]");
    }

    #[tokio::test]
    async fn output_is_kept_up_to_its_limit_and_drained_past_it() {
        let text = "x".repeat(100);
        assert_eq!(read_capped(text.as_bytes(), 1000).await.unwrap(), (text.clone().into(), false));
        let (kept, more) = read_capped("y".repeat(50_000).as_bytes(), 20_000).await.unwrap();
        assert_eq!((kept.len(), more), (20_000, true));
    }

    #[test]
    fn the_result_cuts_output_on_a_char_boundary_and_says_so() {
        let short = result(0, false, (b"done\n".to_vec(), false));
        assert_eq!(short, json!({"exit_code": 0, "output": "done\n"}));
        let long = "é".repeat(BODY_LIMIT).into_bytes();
        let cut = result(137, true, (long[..BODY_LIMIT + 4].to_vec(), true));
        let output = cut["output"].as_str().unwrap();
        assert!(output.len() <= BODY_LIMIT && output.len() > BODY_LIMIT - 2);
        assert!(output.chars().all(|c| c == 'é'), "no half character at the end");
        assert_eq!(
            (cut["timed_out"].clone(), cut["truncated"].clone()),
            (json!(true), json!(true))
        );
        let exact = result(1, false, ("z".repeat(BODY_LIMIT).into_bytes(), false));
        assert!(exact.get("truncated").is_none(), "exactly 64 KiB fits");
    }

    #[test]
    fn a_run_names_its_workspace_a_command_and_a_limit_it_can_keep() {
        let args = |workspace: &str, command: &str, timeout_s| RunArgs {
            workspace: workspace.into(),
            command: command.into(),
            timeout_s,
            env: BTreeMap::new(),
        };
        assert_eq!(args("w1", "ls", 60).check(), Ok(()));
        assert!(args("", "ls", 60).check().unwrap_err().contains("workspace"));
        assert!(args("w1", "", 60).check().unwrap_err().contains("command"));
        assert!(args("w1", "ls", 0).check().unwrap_err().contains("timeout_s"));
        assert!(args("w1", "ls", 601).check().unwrap_err().contains("timeout_s"));
        let unknown = serde_json::from_value::<RunArgs>(json!({
            "workspace": "w1", "command": "ls", "timeout_s": 1, "user": "root"
        }));
        assert!(unknown.is_err(), "no field the server doesn't know");
    }
}
