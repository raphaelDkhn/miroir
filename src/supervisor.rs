//! Process supervision: start in order and wait until ready, watch for crashes, wait for
//! quiet, and kill every other process in the container.

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::os::unix::process::ExitStatusExt;
use std::process::{ExitStatus, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use rustix::process::{Pid, Signal};
use tokio::process::Command;
use tokio::sync::watch;
use tokio::time::{Instant, sleep, timeout};

use crate::manifest::{Process, Quiet, Ready};

const PROBE_INTERVAL: Duration = Duration::from_millis(100);
const QUIET_INTERVAL: Duration = Duration::from_millis(200);
const CONTAINER_INITS: [&str; 10] = [
    "miroir",
    "tini",
    "tini-static",
    "docker-init",
    "dumb-init",
    "catatonit",
    "sh",
    "dash",
    "bash",
    "ash",
];

/// A supervision step that failed.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct SupervisorError(pub String);

/// A process that exited while it should have been running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Crash {
    pub process: String,
    pub exit_code: i32,
}

impl std::fmt::Display for Crash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "process '{}' exited with code {}", self.process, self.exit_code)
    }
}

struct Running {
    name: String,
    pid: Pid,
    exited: watch::Receiver<Option<i32>>,
}

/// Runs the manifest's processes and commands, with its `env` and without
/// `MIROIR_TOKEN`.
pub struct Supervisor {
    env: BTreeMap<String, String>,
    probe: reqwest::Client,
    generation: Arc<AtomicU64>,
    crash: Arc<Mutex<Option<Crash>>>,
    running: Mutex<Vec<Running>>,
}

impl Supervisor {
    pub fn new(env: BTreeMap<String, String>) -> Result<Supervisor, SupervisorError> {
        let probe = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(2))
            .build()
            .map_err(|e| SupervisorError(format!("cannot build the probe client: {e}")))?;
        Ok(Supervisor {
            env,
            probe,
            generation: Arc::new(AtomicU64::new(0)),
            crash: Arc::new(Mutex::new(None)),
            running: Mutex::new(Vec::new()),
        })
    }

    /// The first unexpected exit since the last kill.
    pub fn crash(&self) -> Option<Crash> {
        self.crash.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Start processes in order, each ready before the next.
    pub async fn start<'a>(
        &self,
        processes: impl Iterator<Item = &'a Process>,
    ) -> Result<(), SupervisorError> {
        for process in processes {
            let exited = self.spawn(process).await?;
            if let Some(ready) = &process.ready {
                self.wait_ready(&process.name, ready, exited).await?;
            }
            tracing::info!(process = %process.name, "ready");
        }
        Ok(())
    }

    async fn spawn(
        &self,
        process: &Process,
    ) -> Result<watch::Receiver<Option<i32>>, SupervisorError> {
        let fail =
            |message: String| SupervisorError(format!("process '{}': {message}", process.name));
        let mut command = self.command(&process.command);
        command.envs(&process.env);
        if let Some(cwd) = &process.cwd {
            command.current_dir(cwd);
        }
        if let Some(user) = &process.user {
            let (uid, gid) = lookup_user(user).await.map_err(fail)?;
            command.uid(uid).gid(gid);
        }
        let mut child = command.spawn().map_err(|e| fail(format!("cannot start: {e}")))?;
        let pid = child
            .id()
            .and_then(|id| i32::try_from(id).ok())
            .and_then(Pid::from_raw)
            .ok_or_else(|| fail("exited immediately".into()))?;
        let (tx, rx) = watch::channel(None);
        let generation = self.generation.load(Ordering::SeqCst);
        let (current, crash, name) =
            (self.generation.clone(), self.crash.clone(), process.name.clone());
        tokio::spawn(async move {
            let code = child.wait().await.map_or(-1, exit_code);
            tx.send_replace(Some(code));
            let unexpected = {
                let mut crash = crash.lock().unwrap_or_else(PoisonError::into_inner);
                let unexpected = current.load(Ordering::SeqCst) == generation;
                if unexpected {
                    crash.get_or_insert(Crash { process: name.clone(), exit_code: code });
                }
                unexpected
            };
            if unexpected {
                tracing::error!(process = %name, exit_code = code, "process exited unexpectedly");
            }
        });
        tracing::info!(process = %process.name, pid = pid.as_raw_pid(), "started");
        let running = Running { name: process.name.clone(), pid, exited: rx.clone() };
        self.running.lock().unwrap_or_else(PoisonError::into_inner).push(running);
        Ok(rx)
    }

    async fn wait_ready(
        &self,
        name: &str,
        ready: &Ready,
        exited: watch::Receiver<Option<i32>>,
    ) -> Result<(), SupervisorError> {
        let deadline = Instant::now() + Duration::from_secs(ready.timeout_s);
        loop {
            if let Some(code) = *exited.borrow() {
                let message =
                    format!("process '{name}' exited with code {code} before it was ready");
                return Err(SupervisorError(message));
            }
            if self.probe(ready).await {
                return Ok(());
            }
            if Instant::now() >= deadline {
                let message = format!("process '{name}' was not ready after {} s", ready.timeout_s);
                return Err(SupervisorError(message));
            }
            sleep(PROBE_INTERVAL).await;
        }
    }

    async fn probe(&self, ready: &Ready) -> bool {
        if let Some(port) = ready.tcp {
            let connect = tokio::net::TcpStream::connect(("127.0.0.1", port));
            return matches!(timeout(Duration::from_secs(1), connect).await, Ok(Ok(_)));
        }
        let Some(url) = &ready.http else { return true };
        match self.probe.get(url).send().await {
            Ok(response) => response.status().is_success() || response.status().is_redirection(),
            Err(_) => false,
        }
    }

    /// SIGKILL every other process in the container; their exits are no crash.
    pub async fn kill_all(&self) -> Result<(), SupervisorError> {
        self.generation.fetch_add(1, Ordering::SeqCst);
        kill_others().await?;
        self.running.lock().unwrap_or_else(PoisonError::into_inner).clear();
        *self.crash.lock().unwrap_or_else(PoisonError::into_inner) = None;
        Ok(())
    }

    /// SIGTERM the processes in reverse order, then wait up to `limit` for every other
    /// process to exit.
    pub async fn stop(&self, limit: Duration) -> Result<(), SupervisorError> {
        self.generation.fetch_add(1, Ordering::SeqCst);
        let deadline = Instant::now() + limit;
        let running =
            std::mem::take(&mut *self.running.lock().unwrap_or_else(PoisonError::into_inner));
        for process in running.iter().rev() {
            let _ = rustix::process::kill_process_group(process.pid, Signal::TERM);
            let mut exited = process.exited.clone();
            let wait = exited.wait_for(Option::is_some);
            if timeout(deadline.saturating_duration_since(Instant::now()), wait).await.is_err() {
                tracing::warn!(process = %process.name, "still running after SIGTERM");
            }
        }
        loop {
            let others = other_processes().await?;
            if others.is_empty() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                let message =
                    format!("still running after {} s: {}", limit.as_secs(), describe(&others));
                return Err(SupervisorError(message));
            }
            sleep(Duration::from_millis(50)).await;
        }
    }

    /// Wait until the quiet command passes twice in a row; without one, a second.
    pub async fn wait_quiet(&self, quiet: Option<&Quiet>) -> Result<(), SupervisorError> {
        let Some(quiet) = quiet else {
            sleep(Duration::from_secs(1)).await;
            return Ok(());
        };
        let deadline = Instant::now() + Duration::from_secs(quiet.timeout_s);
        let mut passes = 0;
        loop {
            let mut command = self.command(&quiet.command);
            command.stdout(Stdio::null()).stderr(Stdio::null());
            let limit = deadline.saturating_duration_since(Instant::now()).max(QUIET_INTERVAL);
            let output = bounded(command, limit).await;
            passes = if output.is_ok_and(|o| o.is_some_and(|o| o.status.success())) {
                passes + 1
            } else {
                0
            };
            if passes == 2 {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(SupervisorError(format!("not quiet after {} s", quiet.timeout_s)));
            }
            sleep(QUIET_INTERVAL).await;
        }
    }

    /// Run a command, failing on a non-zero exit or after `limit`.
    pub async fn run_within(
        &self,
        argv: &[String],
        extra: &[(&str, &str)],
        limit: Duration,
    ) -> Result<(), SupervisorError> {
        let mut command = self.command(argv);
        command.envs(extra.iter().copied());
        let fail = |e: String| SupervisorError(format!("{}: {e}", argv.join(" ")));
        let output = bounded(command, limit)
            .await
            .map_err(|e| fail(e.to_string()))?
            .ok_or_else(|| fail(format!("timed out after {} s", limit.as_secs())))?;
        if output.status.success() {
            Ok(())
        } else {
            let message =
                format!("`{}` exited with code {}", argv.join(" "), exit_code(output.status));
            Err(SupervisorError(message))
        }
    }

    /// Run a command; returns its exit code and stdout.
    pub async fn output(
        &self,
        argv: &[String],
        extra: &[(&str, &str)],
        limit: Duration,
    ) -> Result<(i32, String), SupervisorError> {
        let mut command = self.command(argv);
        command.envs(extra.iter().copied()).stdout(Stdio::piped());
        let fail = |e: String| SupervisorError(format!("{}: {e}", argv.join(" ")));
        let output = bounded(command, limit)
            .await
            .map_err(|e| fail(e.to_string()))?
            .ok_or_else(|| fail(format!("timed out after {} s", limit.as_secs())))?;
        Ok((exit_code(output.status), String::from_utf8_lossy(&output.stdout).into_owned()))
    }

    fn command(&self, argv: &[String]) -> Command {
        let (program, args) = argv.split_first().map_or(("", &[][..]), |(p, a)| (p.as_str(), a));
        let mut command = Command::new(program);
        command
            .args(args)
            .envs(&self.env)
            .env_remove("MIROIR_TOKEN")
            .stdin(Stdio::null())
            .stdout(std::io::stderr())
            .stderr(std::io::stderr())
            .process_group(0);
        command
    }
}

/// Refuse to run unless miroir is the container's entrypoint, or a child of its init.
pub fn ensure_entrypoint() -> Result<(), SupervisorError> {
    let parent = rustix::process::getppid().map_or(0, Pid::as_raw_pid);
    let init = std::fs::read_to_string("/proc/1/comm").unwrap_or_default();
    let init = init.trim();
    if CONTAINER_INITS.contains(&init) && (rustix::process::getpid().is_init() || parent == 1) {
        return Ok(());
    }
    Err(SupervisorError(format!(
        "miroir must run as the container's entrypoint under tini, because reset kills every \
         other process in the container (PID 1 here is '{init}'); in a VM, give it a PID \
         namespace of its own: unshare --pid --fork --mount-proc tini -- miroir run"
    )))
}

/// SIGKILL every process but PID 1 and miroir until none is left.
pub async fn kill_others() -> Result<(), SupervisorError> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let others = other_processes().await?;
        if others.is_empty() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            let message = format!("processes survived SIGKILL: {}", describe(&others));
            return Err(SupervisorError(message));
        }
        for (pid, _) in &others {
            let _ = rustix::process::kill_process(*pid, Signal::KILL);
        }
        sleep(Duration::from_millis(10)).await;
    }
}

async fn bounded(mut command: Command, limit: Duration) -> std::io::Result<Option<Output>> {
    let child = command.kill_on_drop(true).spawn()?;
    let group = child.id().and_then(|id| i32::try_from(id).ok()).and_then(Pid::from_raw);
    if let Ok(output) = timeout(limit, child.wait_with_output()).await {
        return output.map(Some);
    }
    if let Some(group) = group
        && let Err(e) = rustix::process::kill_process_group(group, Signal::KILL)
        && e != rustix::io::Errno::SRCH
    {
        tracing::warn!(group = group.as_raw_pid(), "cannot kill a command's process group: {e}");
    }
    Ok(None)
}

/// The exit code, or 128 plus the signal that killed the process.
pub(crate) fn exit_code(status: ExitStatus) -> i32 {
    status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

/// A user's uid and gid, from `/etc/passwd`.
pub(crate) async fn lookup_user(name: &str) -> Result<(u32, u32), String> {
    let passwd = tokio::fs::read("/etc/passwd")
        .await
        .map_err(|e| format!("cannot read /etc/passwd: {e}"))?;
    for line in String::from_utf8_lossy(&passwd).lines() {
        let fields: Vec<&str> = line.split(':').collect();
        if fields.len() > 3 && fields[0] == name {
            let uid = fields[2].parse().map_err(|_| format!("bad uid for user '{name}'"))?;
            let gid = fields[3].parse().map_err(|_| format!("bad gid for user '{name}'"))?;
            return Ok((uid, gid));
        }
    }
    Err(format!("unknown user '{name}'"))
}

async fn other_processes() -> Result<Vec<(Pid, String)>, SupervisorError> {
    let scan = tokio::task::spawn_blocking(|| {
        let me = rustix::process::getpid();
        let mut found = Vec::new();
        for entry in std::fs::read_dir("/proc")? {
            let name = entry?.file_name();
            let Some(pid) = name.to_str().and_then(|s| s.parse().ok()).and_then(Pid::from_raw)
            else {
                continue;
            };
            if pid.as_raw_pid() == 1 || pid == me {
                continue;
            }
            let stat = match std::fs::read(format!("/proc/{}/stat", pid.as_raw_pid())) {
                Ok(stat) => String::from_utf8_lossy(&stat).into_owned(),
                Err(e) if e.kind() == ErrorKind::NotFound => continue,
                Err(e) if e.raw_os_error() == Some(rustix::io::Errno::SRCH.raw_os_error()) => {
                    continue;
                }
                Err(e) => return Err(e),
            };
            let Some((head, tail)) = stat.rsplit_once(')') else {
                continue;
            };
            if tail.trim_start().starts_with('Z') {
                continue;
            }
            let comm = head.split_once('(').map_or("", |(_, comm)| comm);
            found.push((pid, comm.to_string()));
        }
        Ok::<_, std::io::Error>(found)
    });
    scan.await
        .map_err(|e| SupervisorError(format!("process scan failed: {e}")))?
        .map_err(|e| SupervisorError(format!("cannot scan /proc: {e}")))
}

fn describe(processes: &[(Pid, String)]) -> String {
    let names: Vec<String> =
        processes.iter().map(|(pid, comm)| format!("{} ({comm})", pid.as_raw_pid())).collect();
    names.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_hung_quiet_command_times_out() {
        let supervisor = Supervisor::new(BTreeMap::new()).unwrap();
        let quiet = Quiet { command: vec!["sleep".into(), "30".into()], timeout_s: 1 };
        let started = std::time::Instant::now();
        let error = supervisor.wait_quiet(Some(&quiet)).await.unwrap_err();
        assert_eq!(error.0, "not quiet after 1 s");
        assert!(started.elapsed() < Duration::from_secs(5), "took {:?}", started.elapsed());
    }

    #[tokio::test]
    async fn a_command_past_its_limit_is_killed() {
        let supervisor = Supervisor::new(BTreeMap::new()).unwrap();
        let hung = ["sleep".to_string(), "30".to_string()];
        let started = std::time::Instant::now();
        let error = supervisor.run_within(&hung, &[], Duration::from_secs(1)).await.unwrap_err();
        assert_eq!(error.0, "sleep 30: timed out after 1 s");
        assert!(started.elapsed() < Duration::from_secs(5), "took {:?}", started.elapsed());
    }

    #[tokio::test]
    async fn quiet_needs_two_passes() {
        let supervisor = Supervisor::new(BTreeMap::new()).unwrap();
        let quiet = Quiet { command: vec!["true".into()], timeout_s: 5 };
        let started = std::time::Instant::now();
        supervisor.wait_quiet(Some(&quiet)).await.unwrap();
        assert!(started.elapsed() >= QUIET_INTERVAL, "two passes, 200 ms apart");
    }
}
