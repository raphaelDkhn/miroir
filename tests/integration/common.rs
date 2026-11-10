use std::path::Path;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const ENV_IMAGE: &str = "acme-crm-env";
pub const MYSQL_ENV_IMAGE: &str = "mini-shop-env";
pub const APPS_IMAGE: &str = "acme-apps-env";
pub const SANDBOX_IMAGE: &str = "miroir-sandbox";

pub fn docker(args: &[&str]) -> Output {
    Command::new("docker").args(args).output().expect("the docker CLI runs")
}

pub fn docker_ok(args: &[&str]) -> String {
    let out = docker(args);
    assert!(
        out.status.success(),
        "docker {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

const PID_LABEL: &str = "miroir-test-pid";

fn pid_label() -> String {
    format!("{PID_LABEL}={}", std::process::id())
}

fn remove_orphans() {
    let filter = format!("label={PID_LABEL}");
    let format = "{{.ID}} {{.Label \"miroir-test-pid\"}}";
    for kind in [["ps", "-a"], ["network", "ls"]] {
        let listed = docker_ok(&[kind[0], kind[1], "--filter", &filter, "--format", format]);
        for line in listed.lines() {
            let Some((id, pid)) = line.split_once(' ') else { continue };
            let pid = pid.parse().ok().and_then(rustix::process::Pid::from_raw);
            let gone = pid.is_some_and(|pid| {
                rustix::process::test_kill_process(pid) == Err(rustix::io::Errno::SRCH)
            });
            if gone && kind[0] == "ps" {
                let _ = docker(&["rm", "-f", id]);
            } else if gone {
                let _ = docker(&["network", "rm", id]);
            }
        }
    }
}

pub fn build_images() {
    static BUILT: OnceLock<()> = OnceLock::new();
    BUILT.get_or_init(|| {
        remove_orphans();
        bake(&[]);
    });
}

pub fn bake(args: &[&str]) -> String {
    let (built, text) = bake_outcome(args);
    assert!(built, "docker buildx bake {} failed:\n{text}", args.join(" "));
    text
}

pub fn bake_outcome(args: &[&str]) -> (bool, String) {
    let out = Command::new("docker")
        .args(["buildx", "bake", "--progress=plain"])
        .args(args)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("the docker CLI runs");
    let text =
        format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    (out.status.success(), text)
}

const MAX_CONTAINERS: usize = 4;
static SLOTS: (Mutex<usize>, Condvar) = (Mutex::new(0), Condvar::new());

struct Permit;

impl Permit {
    fn acquire() -> Permit {
        let (count, freed) = &SLOTS;
        let mut count = count.lock().unwrap();
        while *count >= MAX_CONTAINERS {
            count = freed.wait(count).unwrap();
        }
        *count += 1;
        Permit
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        let (count, freed) = &SLOTS;
        *count.lock().unwrap() -= 1;
        freed.notify_one();
    }
}

pub struct Container {
    pub id: String,
    pub port: u16,
    pub sandbox: Option<Sandbox>,
    _permit: Option<Permit>,
}

#[derive(Default)]
pub struct Options<'a> {
    pub image: &'a str,
    pub docker_args: &'a [&'a str],
    pub command: &'a [&'a str],
    pub files: &'a [(&'a str, &'a str)],
    pub sandbox: bool,
}

pub struct Sandbox {
    pub id: String,
    pub network: String,
    pub port: u16,
}

impl Sandbox {
    pub fn start() -> Sandbox {
        static STARTED: AtomicUsize = AtomicUsize::new(0);
        build_images();
        let label = pid_label();
        let count = STARTED.fetch_add(1, Ordering::SeqCst);
        let network = format!("miroir-test-{}-{count}", std::process::id());
        docker_ok(&["network", "create", "--label", &label, &network]);
        let id = docker_ok(&[
            "run",
            "-d",
            "--label",
            &label,
            "--network",
            &network,
            "--network-alias",
            "sandbox",
            "-p",
            "127.0.0.1::8000",
            SANDBOX_IMAGE,
        ]);
        let port = published_port(&id);
        Sandbox { id, network, port }
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = docker(&["rm", "-f", &self.id]);
        let _ = docker(&["network", "rm", &self.network]);
    }
}

impl Container {
    pub fn start(options: &Options) -> Container {
        Container::start_with(options, true)
    }

    pub fn start_with(options: &Options, capped: bool) -> Container {
        build_images();
        let permit = capped.then(Permit::acquire);
        let label = pid_label();
        let sandbox = options.sandbox.then(Sandbox::start);
        let mut args = vec!["create", "--label", &label, "-p", "127.0.0.1::8000"];
        if let Some(sandbox) = &sandbox {
            args.extend([
                "--network",
                &sandbox.network,
                "-e",
                "MIROIR_SANDBOX=http://sandbox:8000",
            ]);
        }
        args.extend_from_slice(options.docker_args);
        args.push(if options.image.is_empty() { ENV_IMAGE } else { options.image });
        args.extend_from_slice(options.command);
        let id = docker_ok(&args);
        let scratch = std::env::temp_dir().join(format!("miroir-test-{id}"));
        for (target, contents) in options.files {
            let source = scratch.join(target.trim_start_matches('/'));
            std::fs::create_dir_all(source.parent().unwrap()).unwrap();
            std::fs::write(&source, contents).unwrap();
            let parent = Path::new(target).parent().unwrap().to_str().unwrap();
            docker_ok(&["cp", source.to_str().unwrap(), &format!("{id}:{parent}/")]);
        }
        let _ = std::fs::remove_dir_all(&scratch);
        docker_ok(&["start", &id]);
        let container = Container { port: published_port(&id), id, sandbox, _permit: permit };
        retry("/health to answer", || try_get(&container.url("/health")));
        container
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    pub fn logs(&self) -> String {
        let out = docker(&["logs", &self.id]);
        format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
    }

    pub fn exec(&self, command: &[&str]) -> Output {
        let mut args = vec!["exec", self.id.as_str()];
        args.extend_from_slice(command);
        docker(&args)
    }

    pub fn exec_ok(&self, command: &[&str]) -> String {
        let out = self.exec(command);
        assert!(
            out.status.success(),
            "{command:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    pub fn wait_healthy(&self) {
        retry("/health to pass", || try_get(&self.url("/health")).filter(|(code, _)| *code == 200));
    }

    pub fn wait_for_log(&self, needle: &str, limit: Duration) {
        let deadline = Instant::now() + limit;
        while !self.logs().contains(needle) {
            assert!(Instant::now() < deadline, "no '{needle}' in the logs:\n{}", self.logs());
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    pub fn wait_exit(&self) -> i32 {
        docker_ok(&["wait", &self.id]).parse().unwrap()
    }
}

pub fn retry<T>(what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(value) = probe() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn published_port(id: &str) -> u16 {
    retry("a published port", || {
        let mapped =
            String::from_utf8_lossy(&docker(&["port", id, "8000/tcp"]).stdout).trim().to_string();
        mapped.rsplit(':').next().and_then(|port| port.parse().ok())
    })
}

impl Drop for Container {
    fn drop(&mut self) {
        let _ = docker(&["rm", "-f", &self.id]);
    }
}

pub fn get(url: &str) -> (u16, String) {
    try_get(url).unwrap_or_else(|| panic!("GET {url} failed"))
}

pub fn try_get(url: &str) -> Option<(u16, String)> {
    use std::io::{Read, Write};
    let rest = url.strip_prefix("http://")?;
    let (host, path) =
        rest.split_once('/').map_or((rest, "/".to_string()), |(h, p)| (h, format!("/{p}")));
    let mut stream = std::net::TcpStream::connect(host).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(30))).ok()?;
    write!(stream, "GET {path} HTTP/1.0\r\nHost: {host}\r\n\r\n").ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;
    let (head, body) = response.split_once("\r\n\r\n")?;
    Some((head.split_whitespace().nth(1)?.parse().ok()?, body.to_string()))
}

pub fn manifest_with_processes(processes: &str) -> String {
    format!(
        r#"miroir: 1
name: acme-crm
description: test manifest
processes:
  - name: db
    command: [postgres, -D, /var/lib/postgresql/data]
    user: postgres
    datastore: true
    ready: {{tcp: 5432}}
{processes}
state: [/var/lib/postgresql/data, /var/lib/redis, /app/uploads]
apps:
  crm:
    api: {{base_url: "http://127.0.0.1:3000"}}
    mcp: {{url: "http://127.0.0.1:3100/mcp"}}
    database: {{url: "postgres://postgres@127.0.0.1:5432/crm"}}
personas:
  sales_manager: {{description: Dana, headers: {{crm: {{Authorization: Bearer seed-token-dana}}}}}}
"#
    )
}

pub fn baked_at(image: &str) -> jiff::Timestamp {
    build_images();
    let info = docker_ok(&["run", "--rm", "--entrypoint", "cat", image, "/miroir/bake.json"]);
    let info: serde_json::Value = serde_json::from_str(&info).unwrap();
    info["baked_at"].as_str().unwrap().parse().unwrap()
}

impl Container {
    pub fn psql(&self, sql: &str) -> String {
        self.psql_in("crm", sql)
    }

    pub fn psql_in(&self, database: &str, sql: &str) -> String {
        self.exec_ok(&["psql", "-h", "127.0.0.1", "-U", "postgres", "-d", database, "-tAqc", sql])
    }

    pub fn product_get(&self, path: &str) -> serde_json::Value {
        let headers = "{headers: {authorization: 'Bearer seed-token-dana'}}";
        let script = format!(
            "fetch('http://127.0.0.1:3000{path}', {headers})\
             .then(r => r.text()).then(t => process.stdout.write(t))"
        );
        serde_json::from_str(&self.exec_ok(&["node", "-e", &script])).unwrap()
    }
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

pub struct Session {
    socket: Socket,
}

impl Session {
    pub async fn connect(env: &Container) -> Session {
        Session::connect_url(&format!("ws://127.0.0.1:{}/ws", env.port)).await
    }

    pub async fn connect_url(url: &str) -> Session {
        let (socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        Session { socket }
    }

    pub async fn send(&mut self, frame: serde_json::Value) -> serde_json::Value {
        use futures_util::SinkExt;
        let text = tokio_tungstenite::tungstenite::Message::Text(frame.to_string().into());
        self.socket.send(text).await.unwrap();
        self.receive().await.expect("a reply frame")
    }

    pub async fn ping_during(&mut self, frame: serde_json::Value) -> Duration {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        self.socket.send(Message::Text(frame.to_string().into())).await.unwrap();
        self.socket.send(Message::Ping(Vec::new().into())).await.unwrap();
        let started = Instant::now();
        loop {
            match self.socket.next().await.expect("the socket stays open").unwrap() {
                Message::Pong(_) => return started.elapsed(),
                Message::Text(text) => panic!("a reply before the pong: {text}"),
                _ => {}
            }
        }
    }

    pub async fn receive(&mut self) -> Option<serde_json::Value> {
        use futures_util::StreamExt;
        while let Some(message) = self.socket.next().await {
            match message.ok()? {
                tokio_tungstenite::tungstenite::Message::Text(text) => {
                    return Some(serde_json::from_str(text.as_str()).unwrap());
                }
                tokio_tungstenite::tungstenite::Message::Close(_) => return None,
                _ => {}
            }
        }
        None
    }

    pub async fn close(mut self) {
        use futures_util::SinkExt;
        let text = tokio_tungstenite::tungstenite::Message::Text(
            serde_json::json!({"type": "close"}).to_string().into(),
        );
        self.socket.send(text).await.unwrap();
        assert!(self.receive().await.is_none(), "the server closes the session");
    }

    pub async fn reset(&mut self, data: serde_json::Value) -> serde_json::Value {
        let reply = self.send(serde_json::json!({"type": "reset", "data": data})).await;
        assert_eq!(reply["type"], "observation", "reset failed: {reply}");
        reply["data"].clone()
    }

    pub async fn step(&mut self, data: serde_json::Value) -> serde_json::Value {
        self.send(serde_json::json!({"type": "step", "data": data})).await
    }

    pub async fn state(&mut self) -> serde_json::Value {
        let reply = self.send(serde_json::json!({"type": "state"})).await;
        assert_eq!(reply["type"], "state", "{reply}");
        reply["data"].clone()
    }
}

pub fn run_check(
    image: &str,
    files: &[(&str, &str)],
    tasks: &[(&str, &str)],
) -> (i32, serde_json::Value, String) {
    run_check_with(image, files, tasks, "")
}

pub fn run_check_with(
    image: &str,
    files: &[(&str, &str)],
    tasks: &[(&str, &str)],
    flags: &str,
) -> (i32, serde_json::Value, String) {
    check_beside(image, files, tasks, flags, None)
}

pub fn run_check_with_sandbox(
    image: &str,
    tasks: &[(&str, &str)],
) -> (i32, serde_json::Value, String) {
    let sandbox = Sandbox::start();
    check_beside(image, &[], tasks, "", Some(&sandbox))
}

pub struct Imported {
    pub code: i32,
    pub log: String,
    pub out: std::collections::BTreeMap<String, Vec<u8>>,
    pub listing: String,
}

pub fn run_import(image: &str, dir: &str, inputs: &[(&str, &[u8])], flags: &[&str]) -> Imported {
    let parent = dir.strip_suffix("/in").expect("the inputs go in a directory named in");
    let parent = if parent.is_empty() { "/" } else { parent };
    build_images();
    let _permit = Permit::acquire();
    let label = pid_label();
    let run = format!("chown 4242:4242 /out && exec tini -- miroir import {}", flags.join(" "));
    let create = ["create", "--label", &label, "--network", "none", "--entrypoint", "sh"];
    let id = docker_ok(&[&create[..], &[image, "-c", &run]].concat());
    let scratch = std::env::temp_dir().join(format!("miroir-import-{id}"));
    std::fs::create_dir_all(scratch.join("in")).unwrap();
    std::fs::create_dir_all(scratch.join("out")).unwrap();
    for (name, contents) in inputs {
        std::fs::write(scratch.join("in").join(name), contents).unwrap();
    }
    docker_ok(&["cp", scratch.join("in").to_str().unwrap(), &format!("{id}:{parent}")]);
    docker_ok(&["cp", scratch.join("out").to_str().unwrap(), &format!("{id}:/")]);
    docker_ok(&["start", &id]);
    let code: i32 = docker_ok(&["wait", &id]).parse().unwrap();
    let logs = docker(&["logs", &id]);
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&logs.stdout),
        String::from_utf8_lossy(&logs.stderr)
    );
    let back = scratch.join("back");
    docker_ok(&["cp", &format!("{id}:/out"), back.to_str().unwrap()]);
    let mut out = std::collections::BTreeMap::new();
    for entry in std::fs::read_dir(&back).unwrap() {
        let path = entry.unwrap().path();
        out.insert(
            path.file_name().unwrap().to_string_lossy().into_owned(),
            std::fs::read(&path).unwrap(),
        );
    }
    let tar = docker(&["cp", &format!("{id}:/out"), "-"]).stdout;
    let mut list = Command::new("tar")
        .args(["-tvf", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(list.stdin.as_mut().unwrap(), &tar).unwrap();
    let listing = String::from_utf8_lossy(&list.wait_with_output().unwrap().stdout).into_owned();
    let _ = docker(&["rm", "-f", &id]);
    let _ = std::fs::remove_dir_all(&scratch);
    Imported { code, log, out, listing }
}

fn check_beside(
    image: &str,
    files: &[(&str, &str)],
    tasks: &[(&str, &str)],
    flags: &str,
    sandbox: Option<&Sandbox>,
) -> (i32, serde_json::Value, String) {
    build_images();
    let _permit = Permit::acquire();
    let replace = format!(
        "rm -rf /miroir/tasks/* && cp /miroir/test-tasks/*.yaml /miroir/tasks/ \
         && exec tini -- miroir check --json {flags}"
    );
    let label = pid_label();
    let mut args = vec!["create", "--label", &label];
    match sandbox {
        Some(sandbox) => {
            args.extend([
                "--network",
                &sandbox.network,
                "-e",
                "MIROIR_SANDBOX=http://sandbox:8000",
            ]);
        }
        None => args.extend(["--network", "none"]),
    }
    if tasks.is_empty() {
        args.extend([image, "check", "--json"]);
        args.extend(flags.split_whitespace());
    } else {
        args.extend(["--entrypoint", "sh", image, "-c", &replace]);
    }
    let id = docker_ok(&args);
    let scratch = std::env::temp_dir().join(format!("miroir-check-{id}"));
    let renamed: Vec<(String, &str)> = tasks
        .iter()
        .map(|(name, yaml)| (format!("/miroir/test-tasks/{name}.yaml"), *yaml))
        .collect();
    let all = files.iter().map(|(p, c)| ((*p).to_string(), *c)).chain(renamed);
    for (target, contents) in all {
        let source = scratch.join(target.trim_start_matches('/'));
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(&source, contents).unwrap();
        let parent = Path::new(&target).parent().unwrap().to_str().unwrap().to_string();
        if parent == "/miroir/test-tasks" {
            docker_ok(&[
                "cp",
                scratch.join("miroir/test-tasks").to_str().unwrap(),
                &format!("{id}:/miroir/"),
            ]);
        } else {
            docker_ok(&["cp", source.to_str().unwrap(), &format!("{id}:{parent}/")]);
        }
    }
    let _ = std::fs::remove_dir_all(&scratch);
    docker_ok(&["start", &id]);
    let code: i32 = docker_ok(&["wait", &id]).parse().unwrap();
    let out = docker(&["logs", &id]);
    let _ = docker(&["rm", "-f", &id]);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let report = serde_json::from_str(&stdout).unwrap_or(serde_json::Value::Null);
    (code, report, String::from_utf8_lossy(&out.stderr).to_string())
}
