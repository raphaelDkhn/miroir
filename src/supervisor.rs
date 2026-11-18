//! The world's processes: start in order until ready, wait for quiet, record crashes, stop in
//! reverse order.

use std::collections::BTreeMap;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{ExitStatus, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use rustix::process::{Pid, Signal};
use serde_json::json;
use tokio::process::{Child, Command};
use tokio::sync::watch;
use tokio::time::{Instant, sleep, sleep_until, timeout};

use crate::evidence::fault;
use crate::journal;
use crate::manifest::{Process, Ready};

/// How long a process's ready probe may take to pass.
pub const READY_LIMIT: Duration = Duration::from_secs(60);
/// How long two passes of the `quiet` command in a row may take.
pub const QUIET_LIMIT: Duration = Duration::from_secs(30);

const PROBE_INTERVAL: Duration = Duration::from_millis(20);
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// The least time between the starts of two quiet runs.
const QUIET_INTERVAL: Duration = Duration::from_millis(200);
/// How long a process may take to die once it got SIGKILL.
const KILL_GRACE: Duration = Duration::from_secs(5);

/// A process that exited while the world ran.
#[derive(Debug, Clone)]
pub struct Crash {
    pub process: String,
    /// `status N`, or `signal N` when a signal killed it.
    pub status: String,
}

/// A supervisor failure. Each message names the process or command and the fix.
#[derive(Debug, thiserror::Error)]
pub enum SupervisorError {
    #[error("{0}")]
    Spawn(String),
    #[error("{0}")]
    NotReady(String),
    #[error("{0}")]
    Exited(String),
    #[error("{0}")]
    Command(String),
    #[error("{0}")]
    QuietTimeout(String),
    #[error("{0}")]
    Stop(String),
}

/// A started process.
#[derive(Debug)]
struct Running {
    name: String,
    /// Also its process group: each process leads its own.
    pid: Pid,
    /// Its exit status, once it has exited.
    exited: watch::Receiver<Option<String>>,
}

/// Starts, watches and stops the world's processes, and runs its commands.
#[derive(Debug)]
pub struct Supervisor {
    env: BTreeMap<String, String>,
    faults: Option<PathBuf>,
    running: Mutex<Vec<Running>>,
    crash: Arc<Mutex<Option<Crash>>>,
    stopping: Arc<AtomicBool>,
}

impl Supervisor {
    /// `env` is layered over this process's environment for every child and command; a
    /// process's own `env` goes on top. Crashes are appended to `faults` when it is set.
    pub fn new(env: BTreeMap<String, String>, faults: Option<PathBuf>) -> Supervisor {
        Supervisor {
            env,
            faults,
            running: Mutex::new(Vec::new()),
            crash: Arc::new(Mutex::new(None)),
            stopping: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Start `processes` in order, each once the previous one is ready: its ready probe passed
    /// within `ready_within`. Fails when a started process exits while a probe is pending; an
    /// exit after that is a crash, which [`Supervisor::crash`] returns.
    pub async fn start(
        &self,
        processes: &[&Process],
        ready_within: Duration,
    ) -> Result<(), SupervisorError> {
        let probe = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| SupervisorError::NotReady(format!("cannot build the ready probe: {e}")))?;
        for process in processes {
            self.spawn(process).await?;
            if let Some(ready) = &process.ready {
                self.wait_ready(&probe, process, ready, ready_within).await?;
            }
            tracing::info!(process = %process.name, "ready");
        }
        Ok(())
    }

    async fn spawn(&self, process: &Process) -> Result<(), SupervisorError> {
        let failed = |message: String| {
            SupervisorError::Spawn(format!("process '{}' cannot start: {message}", process.name))
        };
        let mut command = self.command(&process.command);
        command.envs(&process.env).kill_on_drop(true);
        if let Some(cwd) = &process.cwd {
            command.current_dir(cwd);
        }
        if let Some(user) = &process.user {
            let (uid, gid, home) = user_ids(user).await.map_err(failed)?;
            command.uid(uid).gid(gid);
            // As su-exec, gosu and Docker's USER do; a HOME in either env still wins.
            if !self.env.contains_key("HOME") && !process.env.contains_key("HOME") {
                command.env("HOME", home);
            }
        }
        let child =
            command.spawn().map_err(|e| failed(format!("{e}; check its command, cwd and user")))?;
        let pid = pid(&child).ok_or_else(|| failed("it has no process id".into()))?;
        deprioritize(pid).map_err(|e| failed(format!("cannot set its oom_score_adj: {e}")))?;
        let (exit, exited) = watch::channel(None);
        let running = Running { name: process.name.clone(), pid, exited };
        self.running.lock().unwrap_or_else(PoisonError::into_inner).push(running);
        self.watch(child, exit, process.name.clone());
        tracing::info!(process = %process.name, pid = pid.as_raw_pid(), "started");
        Ok(())
    }

    /// Reap `child` when it exits; an exit before `stop` is a crash, recorded once in `crash`
    /// and as a fault line each time.
    fn watch(&self, mut child: Child, exit: watch::Sender<Option<String>>, name: String) {
        let (crash, stopping, faults) =
            (self.crash.clone(), self.stopping.clone(), self.faults.clone());
        tokio::spawn(async move {
            let status = match child.wait().await {
                Ok(status) => described(status),
                Err(e) => format!("an unknown status ({e})"),
            };
            if !stopping.load(Ordering::SeqCst) {
                tracing::error!(process = %name, "exited unexpectedly with {status}");
                if let Some(faults) = faults {
                    record_crash(faults, &name, &status).await;
                }
                let mut first = crash.lock().unwrap_or_else(PoisonError::into_inner);
                first.get_or_insert(Crash { process: name, status: status.clone() });
            }
            exit.send_replace(Some(status));
        });
    }

    async fn wait_ready(
        &self,
        probe: &reqwest::Client,
        process: &Process,
        ready: &Ready,
        within: Duration,
    ) -> Result<(), SupervisorError> {
        let deadline = Instant::now() + within;
        loop {
            if let Some(crash) = self.crash() {
                return Err(exited_while_starting(&crash, &process.name));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if ready_passes(probe, ready, remaining.min(PROBE_TIMEOUT)).await {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(SupervisorError::NotReady(format!(
                    "process '{}' was not ready within {} s: {} never passed; check its \
                     command, its output above and its ready probe",
                    process.name,
                    within.as_secs(),
                    probe_text(ready),
                )));
            }
            sleep(PROBE_INTERVAL).await;
        }
    }

    /// Run one command to completion within `limit`; past it, its process group is killed.
    pub async fn run_within(
        &self,
        argv: &[String],
        limit: Duration,
    ) -> Result<(), SupervisorError> {
        let shown = argv.join(" ");
        match bounded(self.command(argv), limit).await {
            Err(e) => Err(SupervisorError::Spawn(format!(
                "cannot run `{shown}`: {e}; check that its program is installed and on PATH"
            ))),
            Ok(None) => Err(SupervisorError::Command(format!(
                "`{shown}` did not finish within {} s, so it was killed",
                limit.as_secs()
            ))),
            Ok(Some(output)) if output.status.success() => Ok(()),
            Ok(Some(output)) => Err(SupervisorError::Command(format!(
                "`{shown}` exited with {}; its output is above",
                described(output.status)
            ))),
        }
    }

    /// Wait until `quiet` passes twice in a row, within `within`, its runs starting at least
    /// 200 ms apart. Without a quiet command there is nothing to wait for.
    pub async fn wait_quiet(
        &self,
        quiet: Option<&[String]>,
        within: Duration,
    ) -> Result<(), SupervisorError> {
        let Some(argv) = quiet else { return Ok(()) };
        let deadline = Instant::now() + within;
        let mut passes = 0;
        loop {
            let started = Instant::now();
            let limit = deadline.saturating_duration_since(started).max(QUIET_INTERVAL);
            let failure = self.quiet_once(argv, limit).await?;
            passes = if failure.is_none() { passes + 1 } else { 0 };
            if passes == 2 {
                return Ok(());
            }
            if Instant::now() >= deadline {
                let last = failure.unwrap_or_else(|| "its last run passed".into());
                return Err(SupervisorError::QuietTimeout(format!(
                    "the quiet command `{}` did not pass twice in a row within {} s ({last}): \
                     the world's background work never settled; check the command and the \
                     processes' output above",
                    argv.join(" "),
                    within.as_secs(),
                )));
            }
            sleep_until(started + QUIET_INTERVAL).await;
        }
    }

    /// One run of the quiet command: `None` when it passed, else why it did not.
    async fn quiet_once(
        &self,
        argv: &[String],
        limit: Duration,
    ) -> Result<Option<String>, SupervisorError> {
        let mut command = self.command(argv);
        // Its stderr says why it failed; a child left holding it open makes the run time out.
        command.stdout(Stdio::null()).stderr(Stdio::piped());
        match bounded(command, limit).await {
            Err(e) => Err(SupervisorError::Spawn(format!(
                "cannot run the quiet command `{}`: {e}; check that its program is installed \
                 and on PATH",
                argv.join(" ")
            ))),
            Ok(Some(output)) if output.status.success() => Ok(None),
            Ok(Some(output)) => Ok(Some(format!(
                "its last run exited with {}{}",
                described(output.status),
                last_line(&output.stderr)
            ))),
            Ok(None) => Ok(Some(
                "its last run was killed at the limit, still running or leaving a background \
                 process that holds its output open"
                    .into(),
            )),
        }
    }

    /// The first process that exited unexpectedly, if any.
    pub fn crash(&self) -> Option<Crash> {
        self.crash.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Stop every process in reverse order: SIGTERM to its process group, then SIGKILL once
    /// `limit` has passed since the stop began. Their exits are no crash. A process that
    /// survives SIGKILL is named in the error, after every other one was stopped.
    pub async fn stop(&self, limit: Duration) -> Result<(), SupervisorError> {
        self.stopping.store(true, Ordering::SeqCst);
        let deadline = Instant::now() + limit;
        let running =
            std::mem::take(&mut *self.running.lock().unwrap_or_else(PoisonError::into_inner));
        let mut stuck = Vec::new();
        for process in running.iter().rev() {
            signal(process.pid, Signal::TERM);
            let mut exited = process.exited.clone();
            let remaining = deadline.saturating_duration_since(Instant::now());
            if timeout(remaining, exited.wait_for(Option::is_some)).await.is_ok() {
                continue;
            }
            tracing::warn!(process = %process.name, "still running after SIGTERM; sending SIGKILL");
            signal(process.pid, Signal::KILL);
            if timeout(KILL_GRACE, exited.wait_for(Option::is_some)).await.is_err() {
                stuck.push(format!("'{}'", process.name));
            }
        }
        if stuck.is_empty() {
            return Ok(());
        }
        Err(SupervisorError::Stop(format!(
            "process {} still running {} s after SIGKILL; the kernel may be stuck on its I/O",
            stuck.join(", "),
            KILL_GRACE.as_secs()
        )))
    }

    /// A command with the layered environment, its output on stderr, and a process group of
    /// its own, so a signal reaches its children too.
    fn command(&self, argv: &[String]) -> Command {
        let (program, args) = argv.split_first().map_or(("", &[][..]), |(p, a)| (p.as_str(), a));
        let mut command = Command::new(program);
        command
            .args(args)
            .envs(&self.env)
            .stdin(Stdio::null())
            .stdout(std::io::stderr())
            .stderr(std::io::stderr())
            .process_group(0);
        command
    }
}

fn exited_while_starting(crash: &Crash, starting: &str) -> SupervisorError {
    let Crash { process, status } = crash;
    let when = if process == starting {
        "before it was ready".to_string()
    } else {
        format!("while '{starting}' was starting")
    };
    SupervisorError::Exited(format!(
        "process '{process}' exited with {status} {when}; its output is above"
    ))
}

async fn ready_passes(probe: &reqwest::Client, ready: &Ready, limit: Duration) -> bool {
    let attempt = async {
        match ready {
            Ready::Tcp(port) => tokio::net::TcpStream::connect(("127.0.0.1", *port)).await.is_ok(),
            Ready::Http(url) => probe.get(url).send().await.is_ok_and(|response| {
                response.status().is_success() || response.status().is_redirection()
            }),
        }
    };
    timeout(limit, attempt).await.unwrap_or(false)
}

fn probe_text(ready: &Ready) -> String {
    match ready {
        Ready::Tcp(port) => format!("a TCP connection to 127.0.0.1:{port}"),
        Ready::Http(url) => format!("GET {url} answering 2xx or 3xx"),
    }
}

async fn record_crash(faults: PathBuf, name: &str, status: &str) {
    let line = json!({"kind": fault::CRASH, "detail": format!("{name} exited with {status}")});
    let written = tokio::task::spawn_blocking(move || journal::append(&faults, &line)).await;
    match written {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::error!(process = %name, "cannot record the crash as a fault: {e}"),
        Err(e) => tracing::error!(process = %name, "cannot record the crash as a fault: {e}"),
    }
}

/// Run `command` until it exits or `limit` passes; then its process group is killed and the
/// result is `None`. Only the streams given `Stdio::piped()` are read into the output.
async fn bounded(mut command: Command, limit: Duration) -> std::io::Result<Option<Output>> {
    let child = command.kill_on_drop(true).spawn()?;
    let pid = pid(&child);
    if let Some(pid) = pid {
        deprioritize(pid)?;
    }
    if let Ok(output) = timeout(limit, child.wait_with_output()).await {
        return output.map(Some);
    }
    if let Some(pid) = pid {
        signal(pid, Signal::KILL);
    }
    Ok(None)
}

fn pid(child: &Child) -> Option<Pid> {
    child.id().and_then(|id| i32::try_from(id).ok()).and_then(Pid::from_raw)
}

/// Signal a process group; one that is already gone is no error.
fn signal(group: Pid, signal: Signal) {
    if let Err(e) = rustix::process::kill_process_group(group, signal)
        && e != rustix::io::Errno::SRCH
    {
        tracing::warn!(group = group.as_raw_pid(), "cannot signal a process group: {e}");
    }
}

/// Have the kernel's OOM killer pick this child before `miroir` (Linux only). Call it before
/// the child is reaped, so its `/proc` entry still exists. A child that already exited needs
/// nothing: its `/proc` entry then belongs to root, so a `miroir` that is not root cannot write
/// it, and that failure is no error.
fn deprioritize(pid: Pid) -> std::io::Result<()> {
    if !cfg!(target_os = "linux") {
        return Ok(());
    }
    let proc = format!("/proc/{}", pid.as_raw_pid());
    match std::fs::write(format!("{proc}/oom_score_adj"), "1000") {
        Err(_) if std::fs::read_to_string(format!("{proc}/stat")).map_or(true, |s| exited(&s)) => {
            Ok(())
        }
        written => written,
    }
}

/// Whether a `/proc/<pid>/stat` line is a process that exited: a zombie (`Z`) or dead (`X`).
/// The state follows the command name, which may hold spaces and `)`, so it is read after the
/// last `)`.
fn exited(stat: &str) -> bool {
    stat.rsplit_once(')').is_some_and(|(_, rest)| rest.trim_start().starts_with(['Z', 'X']))
}

/// `: <the last line>` of what a command printed, or nothing when it printed none.
fn last_line(printed: &[u8]) -> String {
    let text = String::from_utf8_lossy(printed);
    let last = text.lines().map(str::trim).rfind(|line| !line.is_empty());
    last.map(|line| format!(": {line}")).unwrap_or_default()
}

/// `status N`, or `signal N` when a signal ended the process.
fn described(status: ExitStatus) -> String {
    match (status.code(), status.signal()) {
        (Some(code), _) => format!("status {code}"),
        (None, Some(signal)) => format!("signal {signal}"),
        (None, None) => status.to_string(),
    }
}

/// The uid, gid and home directory of `user`, from `/etc/passwd`.
async fn user_ids(user: &str) -> Result<(u32, u32, String), String> {
    let passwd = tokio::fs::read("/etc/passwd")
        .await
        .map_err(|e| format!("cannot read /etc/passwd to find user '{user}': {e}"))?;
    passwd_ids(&String::from_utf8_lossy(&passwd), user)
}

fn passwd_ids(passwd: &str, user: &str) -> Result<(u32, u32, String), String> {
    for line in passwd.lines() {
        let fields: Vec<&str> = line.split(':').collect();
        let [name, _, uid, gid, _, home, ..] = fields.as_slice() else { continue };
        if *name != user {
            continue;
        }
        let number = |id: &str| {
            id.parse().map_err(|_| format!("user '{user}' has '{id}' as an id in /etc/passwd"))
        };
        return Ok((number(uid)?, number(gid)?, (*home).to_string()));
    }
    Err(format!("no user '{user}' in /etc/passwd; create it in the world's Dockerfile"))
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::path::Path;

    use super::*;
    use crate::bake::tests::temp_root;

    const LONG: Duration = Duration::from_secs(10);

    fn sh(script: &str) -> Vec<String> {
        vec!["sh".into(), "-c".into(), script.into()]
    }

    fn process(name: &str, script: &str, ready: Option<Ready>) -> Process {
        Process {
            name: name.into(),
            command: sh(script),
            user: None,
            cwd: None,
            env: BTreeMap::new(),
            datastore: false,
            ready,
        }
    }

    fn supervisor() -> Supervisor {
        Supervisor::new(BTreeMap::new(), None)
    }

    /// A loopback TCP port that nothing listens on.
    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap_or_default()
    }

    /// Wait until `done`, for at most `LONG`.
    async fn eventually(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + LONG;
        while !done() {
            assert!(Instant::now() < deadline, "never happened: {what}");
            sleep(Duration::from_millis(20)).await;
        }
    }

    /// A loopback HTTP server answering every request with `status`.
    async fn http_server(status: &'static str) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0_u8; 1024];
                let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut request).await;
                let response = format!("HTTP/1.1 {status}\r\ncontent-length: 0\r\n\r\n");
                let _ = tokio::io::AsyncWriteExt::write_all(&mut stream, response.as_bytes()).await;
            }
        });
        port
    }

    fn pids(supervisor: &Supervisor) -> Vec<Pid> {
        supervisor.running.lock().unwrap().iter().map(|running| running.pid).collect()
    }

    fn is_gone(pid: Pid) -> bool {
        rustix::process::test_kill_process(pid) == Err(rustix::io::Errno::SRCH)
    }

    #[tokio::test]
    async fn processes_start_in_order_each_after_the_previous_one_is_ready() {
        let dir = temp_root("order");
        let order = dir.join("order");
        let port = free_port();
        let first = process(
            "first",
            &format!("echo first >> '{}'; exec sleep 30", order.display()),
            Some(Ready::Tcp(port)),
        );
        let second = process(
            "second",
            &format!("echo second >> '{}'; exec sleep 30", order.display()),
            None,
        );
        let binder = std::thread::spawn({
            let order = order.clone();
            move || {
                while !read(&order).contains("first") {
                    std::thread::sleep(Duration::from_millis(10));
                }
                std::thread::sleep(Duration::from_millis(300));
                let mut file = std::fs::OpenOptions::new().append(true).open(&order).unwrap();
                file.write_all(b"ready\n").unwrap();
                std::net::TcpListener::bind(("127.0.0.1", port)).unwrap()
            }
        });
        let supervisor = supervisor();
        supervisor.start(&[&first, &second], LONG).await.unwrap();
        let _listener = binder.join().unwrap();
        eventually("second started", || read(&order).contains("second")).await;
        assert_eq!(read(&order), "first\nready\nsecond\n");
        supervisor.stop(LONG).await.unwrap();
    }

    #[tokio::test]
    async fn a_ready_probe_that_never_passes_fails_naming_the_process() {
        let port = free_port();
        let web = process("web", "exec sleep 30", Some(Ready::Tcp(port)));
        let supervisor = supervisor();
        let started = Instant::now();
        let error = supervisor.start(&[&web], Duration::from_secs(1)).await.unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(4), "took {:?}", started.elapsed());
        let SupervisorError::NotReady(message) = &error else { panic!("{error:?}") };
        let expected = format!(
            "process 'web' was not ready within 1 s: a TCP connection to 127.0.0.1:{port} never \
             passed; check its command, its output above and its ready probe"
        );
        assert_eq!(*message, expected);
        supervisor.stop(LONG).await.unwrap();
    }

    #[tokio::test]
    async fn an_http_probe_passes_on_2xx_or_3xx_only() {
        let ok = http_server("204 No Content").await;
        let moved = http_server("302 Found").await;
        let failing = http_server("500 Internal Server Error").await;
        let url = |port: u16| Ready::Http(format!("http://127.0.0.1:{port}/healthz"));
        let supervisor = supervisor();
        let web = process("web", "exec sleep 30", Some(url(ok)));
        let api = process("api", "exec sleep 30", Some(url(moved)));
        supervisor.start(&[&web, &api], LONG).await.unwrap();
        let broken = process("broken", "exec sleep 30", Some(url(failing)));
        let error = supervisor.start(&[&broken], Duration::from_secs(1)).await.unwrap_err();
        let expected =
            format!("GET http://127.0.0.1:{failing}/healthz answering 2xx or 3xx never passed");
        assert!(error.to_string().contains(&expected), "{error}");
        supervisor.stop(LONG).await.unwrap();
    }

    #[tokio::test]
    async fn a_process_that_exits_during_start_fails_it_naming_it_and_its_status() {
        let flaky = process("flaky", "exit 3", Some(Ready::Tcp(free_port())));
        let supervisor = supervisor();
        let started = Instant::now();
        let error = supervisor.start(&[&flaky], LONG).await.unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(4), "took {:?}", started.elapsed());
        let SupervisorError::Exited(message) = &error else { panic!("{error:?}") };
        assert_eq!(
            message,
            "process 'flaky' exited with status 3 before it was ready; its output is above"
        );

        let worker = process("worker", "sleep 0.2; exit 5", None);
        let web = process("web", "exec sleep 30", Some(Ready::Tcp(free_port())));
        let supervisor = self::supervisor();
        let error = supervisor.start(&[&worker, &web], LONG).await.unwrap_err();
        assert_eq!(
            error.to_string(),
            "process 'worker' exited with status 5 while 'web' was starting; its output is above"
        );
        supervisor.stop(LONG).await.unwrap();
    }

    #[tokio::test]
    async fn a_process_that_cannot_start_names_itself() {
        let mut missing = process("web", "", None);
        missing.command = vec!["/nonexistent/miroir-web".into()];
        let error = supervisor().start(&[&missing], LONG).await.unwrap_err();
        let SupervisorError::Spawn(message) = &error else { panic!("{error:?}") };
        assert!(message.starts_with("process 'web' cannot start: "), "{message}");
        assert!(message.ends_with("; check its command, cwd and user"), "{message}");

        let mut stranger = process("web", "exec sleep 30", None);
        stranger.user = Some("no-such-user-miroir".into());
        let error = supervisor().start(&[&stranger], LONG).await.unwrap_err();
        assert_eq!(
            error.to_string(),
            "process 'web' cannot start: no user 'no-such-user-miroir' in /etc/passwd; create it \
             in the world's Dockerfile"
        );
    }

    #[test]
    fn users_are_read_from_passwd() {
        let passwd = "root:x:0:0:root:/root:/bin/sh\n\
                      # comment\n\
                      short:x:1\n\
                      postgres:x:70:71:PostgreSQL:/var/lib/postgresql:/bin/sh\n\
                      broken:x:abc:1::/:/bin/sh\n";
        assert_eq!(passwd_ids(passwd, "root"), Ok((0, 0, "/root".into())));
        assert_eq!(passwd_ids(passwd, "postgres"), Ok((70, 71, "/var/lib/postgresql".into())));
        let error = passwd_ids(passwd, "broken").unwrap_err();
        assert_eq!(error, "user 'broken' has 'abc' as an id in /etc/passwd");
        let error = passwd_ids(passwd, "short").unwrap_err();
        assert!(error.starts_with("no user 'short' in /etc/passwd"), "{error}");
        assert!(passwd_ids(passwd, "post").is_err(), "names match whole");
    }

    #[tokio::test]
    async fn the_manifest_env_overrides_the_parents_and_a_process_env_overrides_both() {
        let dir = temp_root("env");
        let out = dir.join("out");
        let env: BTreeMap<String, String> =
            [("HOME", "/manifest-home"), ("BOTH", "manifest"), ("ONLY", "m")]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
        let supervisor = Supervisor::new(env, None);
        let check =
            sh(r#"test "$HOME" = /manifest-home && test "$BOTH" = manifest && test -n "$PATH""#);
        supervisor.run_within(&check, LONG).await.unwrap();
        let mut web = process(
            "web",
            &format!("echo \"$BOTH $ONLY $HOME $(pwd)\" > '{}'; exec sleep 30", out.display()),
            None,
        );
        web.env.insert("BOTH".into(), "process".into());
        web.cwd = Some(dir.to_path_buf());
        supervisor.start(&[&web], LONG).await.unwrap();
        eventually("web wrote its environment", || read(&out).ends_with('\n')).await;
        let pwd = dir.canonicalize().unwrap();
        assert_eq!(read(&out), format!("process m /manifest-home {}\n", pwd.display()));
        supervisor.stop(LONG).await.unwrap();
    }

    #[tokio::test]
    async fn a_crash_appends_one_fault_line() {
        let dir = temp_root("crash");
        let faults = dir.join("faults.jsonl");
        let supervisor = Supervisor::new(BTreeMap::new(), Some(faults.clone()));
        let worker = process("worker", "sleep 0.1; exit 1", None);
        let killed = process("killed", "sleep 0.7; kill -9 $$", None);
        supervisor.start(&[&worker, &killed], LONG).await.unwrap();
        eventually("both crashed", || journal::read(&faults).unwrap().len() == 2).await;
        let crash = supervisor.crash().unwrap();
        assert_eq!((crash.process.as_str(), crash.status.as_str()), ("worker", "status 1"));
        assert_eq!(
            journal::read(&faults).unwrap(),
            [
                json!({"kind": "crash", "detail": "worker exited with status 1"}),
                json!({"kind": "crash", "detail": "killed exited with signal 9"}),
            ]
        );
        supervisor.stop(LONG).await.unwrap();
    }

    #[tokio::test]
    async fn a_crash_is_kept_when_its_fault_line_cannot_be_written() {
        let faults = PathBuf::from("/nonexistent/miroir/faults.jsonl");
        let supervisor = Supervisor::new(BTreeMap::new(), Some(faults));
        supervisor.start(&[&process("worker", "exit 2", None)], LONG).await.unwrap();
        eventually("the crash was kept", || supervisor.crash().is_some()).await;
        assert_eq!(supervisor.crash().unwrap().status, "status 2");
    }

    #[tokio::test]
    async fn stop_ends_every_child_in_reverse_order_and_records_no_crash() {
        let dir = temp_root("stop");
        let (order, faults) = (dir.join("order"), dir.join("faults.jsonl"));
        let script = |name: &str| {
            format!(
                "trap 'echo {name} >> \"{0}\"; exit 0' TERM; echo trapped > '{1}'; \
                 while :; do sleep 0.05; done",
                order.display(),
                dir.join(name).display(),
            )
        };
        let first = process("first", &script("first"), None);
        let second = process("second", &script("second"), None);
        let supervisor = Supervisor::new(BTreeMap::new(), Some(faults.clone()));
        supervisor.start(&[&first, &second], LONG).await.unwrap();
        let started = pids(&supervisor);
        let trapped = || ["first", "second"].iter().all(|name| dir.join(name).exists());
        eventually("both trap SIGTERM", trapped).await;
        supervisor.stop(LONG).await.unwrap();
        assert_eq!(read(&order), "second\nfirst\n");
        assert!(started.into_iter().all(is_gone), "every process exited");
        sleep(Duration::from_millis(100)).await;
        assert!(supervisor.crash().is_none());
        assert_eq!(journal::read(&faults).unwrap(), Vec::<serde_json::Value>::new());
    }

    #[tokio::test]
    async fn stop_kills_what_ignores_sigterm_past_the_limit() {
        let dir = temp_root("stubborn");
        let trapped = dir.join("trapped");
        let script = format!(
            "trap '' TERM; echo trapped > '{}'; while :; do sleep 0.05; done",
            trapped.display()
        );
        let stubborn = process("stubborn", &script, None);
        let quick = process("quick", "exec sleep 30", None);
        let supervisor = supervisor();
        supervisor.start(&[&quick, &stubborn], LONG).await.unwrap();
        let started_pids = pids(&supervisor);
        eventually("stubborn ignores SIGTERM", || trapped.exists()).await;
        let started = Instant::now();
        supervisor.stop(Duration::from_secs(1)).await.unwrap();
        let elapsed = started.elapsed();
        assert!(elapsed >= Duration::from_secs(1), "SIGKILL came early: {elapsed:?}");
        assert!(elapsed < Duration::from_secs(4), "took {elapsed:?}");
        assert!(started_pids.into_iter().all(is_gone));
        assert!(supervisor.crash().is_none());
    }

    #[tokio::test]
    async fn a_command_past_its_limit_is_killed_with_its_children() {
        let dir = temp_root("limit");
        let child = dir.join("child");
        let hung = sh(&format!("sleep 30 & echo $! > '{}'; wait", child.display()));
        let started = Instant::now();
        let error = supervisor().run_within(&hung, Duration::from_secs(1)).await.unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(4), "took {:?}", started.elapsed());
        let SupervisorError::Command(message) = &error else { panic!("{error:?}") };
        assert!(message.ends_with("` did not finish within 1 s, so it was killed"), "{message}");
        let pid: i32 = read(&child).trim().parse().unwrap();
        let pid = Pid::from_raw(pid).unwrap();
        eventually("the command's child was killed", || is_gone(pid)).await;
    }

    #[tokio::test]
    async fn a_command_that_fails_or_cannot_start_names_itself() {
        let error = supervisor().run_within(&sh("exit 4"), LONG).await.unwrap_err();
        let SupervisorError::Command(message) = &error else { panic!("{error:?}") };
        assert_eq!(message, "`sh -c exit 4` exited with status 4; its output is above");
        let missing = vec!["/nonexistent/miroir-seed".to_string(), "--all".into()];
        let error = supervisor().run_within(&missing, LONG).await.unwrap_err();
        let SupervisorError::Spawn(message) = &error else { panic!("{error:?}") };
        assert!(message.starts_with("cannot run `/nonexistent/miroir-seed --all`: "), "{message}");
        assert!(message.ends_with("check that its program is installed and on PATH"), "{message}");
        supervisor().run_within(&sh("exit 0"), LONG).await.unwrap();
    }

    #[tokio::test]
    async fn quiet_needs_two_passes_in_a_row() {
        let dir = temp_root("quiet");
        let runs = dir.join("runs");
        let quiet = sh(&format!(
            "n=$(cat '{0}' 2>/dev/null || echo 0); n=$((n + 1)); echo $n > '{0}'; [ $n -ne 2 ]",
            runs.display()
        ));
        let started = Instant::now();
        supervisor().wait_quiet(Some(&quiet), LONG).await.unwrap();
        assert_eq!(read(&runs), "4\n", "pass, fail, then two passes in a row");
        assert!(started.elapsed() >= QUIET_INTERVAL * 3, "runs start 200 ms apart");
    }

    #[tokio::test]
    async fn without_a_quiet_command_the_world_is_quiet() {
        let started = Instant::now();
        supervisor().wait_quiet(None, LONG).await.unwrap();
        assert!(started.elapsed() < QUIET_INTERVAL);
    }

    #[tokio::test]
    async fn a_hung_quiet_command_times_out() {
        let started = Instant::now();
        let hung = vec!["sleep".to_string(), "30".to_string()];
        let error = supervisor().wait_quiet(Some(&hung), Duration::from_secs(1)).await.unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(4), "took {:?}", started.elapsed());
        let SupervisorError::QuietTimeout(message) = &error else { panic!("{error:?}") };
        assert_eq!(
            message,
            "the quiet command `sleep 30` did not pass twice in a row within 1 s (its last run \
             was killed at the limit, still running or leaving a background process that holds \
             its output open): the world's background work never settled; check the command \
             and the processes' output above"
        );
    }

    #[tokio::test]
    async fn a_failing_quiet_command_times_out_with_its_status() {
        let error =
            supervisor().wait_quiet(Some(&sh("exit 3")), Duration::from_secs(1)).await.unwrap_err();
        assert!(error.to_string().contains("(its last run exited with status 3)"), "{error}");
        let said = sh("echo starting >&2; echo 'queue unreachable ' >&2; echo >&2; exit 3");
        let error = supervisor().wait_quiet(Some(&said), Duration::from_secs(1)).await.unwrap_err();
        let reason = "(its last run exited with status 3: queue unreachable)";
        assert!(error.to_string().contains(reason), "{error}");
        let missing = vec!["/nonexistent/miroir-quiet".to_string()];
        let started = Instant::now();
        let error = supervisor().wait_quiet(Some(&missing), LONG).await.unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(2), "a missing program fails at once");
        let SupervisorError::Spawn(message) = &error else { panic!("{error:?}") };
        assert!(message.starts_with("cannot run the quiet command `/nonexistent/miroir-quiet`: "));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn every_child_is_the_oom_killers_first_pick() {
        let supervisor = supervisor();
        supervisor.start(&[&process("web", "exec sleep 30", None)], LONG).await.unwrap();
        let pid = pids(&supervisor)[0].as_raw_pid();
        let adj = std::fs::read_to_string(format!("/proc/{pid}/oom_score_adj")).unwrap();
        assert_eq!(adj.trim(), "1000");
        let check = sh(r#"sleep 0.3; test "$(cat /proc/self/oom_score_adj)" = 1000"#);
        supervisor.run_within(&check, LONG).await.unwrap();
        supervisor.stop(LONG).await.unwrap();
    }

    /// A child can exit before its `oom_score_adj` is written; when `miroir` is not root, its
    /// `/proc` entry then belongs to root and refuses the write.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_child_that_already_exited_needs_no_deprioritizing() {
        let mut child = std::process::Command::new("sh").args(["-c", "exit 0"]).spawn().unwrap();
        let pid = Pid::from_raw(i32::try_from(child.id()).unwrap()).unwrap();
        let stat = format!("/proc/{}/stat", pid.as_raw_pid());
        while !exited(&std::fs::read_to_string(&stat).unwrap()) {
            std::thread::sleep(Duration::from_millis(10));
        }
        deprioritize(pid).unwrap();
        child.wait().unwrap();
        deprioritize(pid).unwrap();
    }

    #[test]
    fn a_stat_line_says_whether_its_process_exited() {
        assert!(exited("42 (sh) Z 1 42 42 0 -1"));
        assert!(exited("42 (a) b) X 1"), "the name may hold ')'");
        assert!(!exited("42 (sleep) S 1 42 42 0 -1"));
        assert!(!exited("42 (Z) R 1"), "a name is not a state");
        assert!(!exited("no state"));
    }
}
