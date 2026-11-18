//! What the integration tests drive: worlds booted from the fixture's world image, the agent's
//! container running a task's solution against one, the grader image, and the evidence a world
//! leaves. Every container and network carries the label `miroir-test` and is removed when its
//! handle drops. The images come from `docker buildx bake`; these tests never build them.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use serde_json::Value;

/// The fixture's world image.
pub const WORLD: &str = "acme-crm-env:dev";
/// The grader: Harbor's separate verifier runs its `/tests/test.sh`.
const GRADER: &str = "miroir:dev";
/// The agent's container, where Harbor's oracle runs a task's solution.
const MAIN: &str = "miroir-main:dev";
const LABEL: &str = "miroir-test";
/// Worlds alive at once: each runs Postgres, Redis and two Node processes.
const MAX_WORLDS: usize = 4;
/// Harbor's healthcheck waits 300 s for the ready marker; a test world gets less.
const BOOT_LIMIT: Duration = Duration::from_secs(120);

/// Run the Docker CLI.
fn docker(args: &[&str]) -> Output {
    Command::new("docker").args(args).output().expect("the docker CLI runs")
}

/// Run the Docker CLI, which must succeed; its trimmed stdout.
pub fn docker_ok(args: &[&str]) -> String {
    let out = docker(args);
    assert!(out.status.success(), "docker {} failed:\n{}", args.join(" "), text(&out));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// stdout, then stderr.
fn text(out: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
}

/// Fail once, with the fix, when `docker buildx bake` has not built the images.
fn images() {
    static CHECKED: OnceLock<()> = OnceLock::new();
    CHECKED.get_or_init(|| {
        for image in [WORLD, GRADER, MAIN] {
            let found = docker(&["image", "inspect", "--format", "{{.Id}}", image]);
            assert!(found.status.success(), "no image {image}: run `docker buildx bake` first");
        }
    });
}

/// A path of the fixture, `fixtures/acme-crm/<path>`.
fn fixture(path: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/acme-crm").join(path)
}

/// The fixture's manifest, with `from`, which it holds once, replaced by `to`.
pub fn manifest_with(from: &str, to: &str) -> String {
    let manifest = std::fs::read_to_string(fixture("miroir.yaml")).unwrap();
    assert_eq!(manifest.matches(from).count(), 1, "the fixture's miroir.yaml has one {from:?}");
    manifest.replacen(from, to, 1)
}

/// The `MIROIR_SPEC` of a fixture task.
pub fn spec(task: &str) -> String {
    let toml = std::fs::read_to_string(fixture(&format!("tasks/{task}/task.toml"))).unwrap();
    let task: toml::Table = toml::from_str(&toml).unwrap();
    task["verifier"]["env"]["MIROIR_SPEC"].as_str().unwrap().to_string()
}

/// A new directory under the temp directory, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        static MADE: AtomicUsize = AtomicUsize::new(0);
        let made = MADE.fetch_add(1, Ordering::SeqCst);
        let dir = format!("{LABEL}-{}-{made}-{name}", std::process::id());
        let dir = std::env::temp_dir().join(dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }

    /// `path` under the directory, with its parent directories made.
    fn file(&self, path: &str) -> PathBuf {
        let file = self.0.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        file
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A container, removed when dropped.
struct Container(String);

impl Container {
    /// `docker create` with `args`: options, the image, then its command.
    fn create(args: &[&str]) -> Container {
        Container(docker_ok(&[&["create", "--label", LABEL], args].concat()))
    }

    /// Copy the contents of the directory `from` to `/`.
    fn copy_in(&self, from: &Path) {
        docker_ok(&["cp", &format!("{}/.", from.display()), &format!("{}:/", self.0)]);
    }

    /// Start it and wait for its end: its exit status, stdout and stderr.
    fn run(&self) -> (i32, String, String) {
        let out = docker(&["start", "--attach", &self.0]);
        let (stdout, stderr) = (&out.stdout, &out.stderr);
        let (stdout, stderr) = (String::from_utf8_lossy(stdout), String::from_utf8_lossy(stderr));
        (out.status.code().unwrap(), stdout.into_owned(), stderr.into_owned())
    }

    /// Copy `path` out to `to`; false when there is no such path.
    fn copy_out(&self, path: &str, to: &Path) -> bool {
        docker(&["cp", &format!("{}:{path}", self.0), &to.display().to_string()]).status.success()
    }
}

impl Drop for Container {
    fn drop(&mut self) {
        let _ = docker(&["rm", "--force", "--volumes", &self.0]);
    }
}

/// A Docker network, removed when dropped.
struct Network(String);

impl Drop for Network {
    fn drop(&mut self) {
        let _ = docker(&["network", "rm", &self.0]);
    }
}

/// One of `MAX_WORLDS` places for a live world.
struct Permit;

static PLACES: (Mutex<usize>, Condvar) = (Mutex::new(0), Condvar::new());

impl Permit {
    fn take() -> Permit {
        let (taken, freed) = &PLACES;
        let mut taken = taken.lock().unwrap_or_else(PoisonError::into_inner);
        while *taken >= MAX_WORLDS {
            taken = freed.wait(taken).unwrap_or_else(PoisonError::into_inner);
        }
        *taken += 1;
        Permit
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        let (taken, freed) = &PLACES;
        *taken.lock().unwrap_or_else(PoisonError::into_inner) -= 1;
        freed.notify_one();
    }
}

/// A container of the world image.
pub struct World {
    // Fields drop in order: the network goes once the container is gone.
    container: Container,
    network: Option<Network>,
    _permit: Permit,
}

impl World {
    /// A world with `files` (absolute path, contents) written over the image's, created with
    /// `options` before the image name; not yet started. Each file is copied alone, so the
    /// directories above it keep their owner and mode.
    pub fn create(files: &[(&str, &str)], options: &[&str]) -> World {
        images();
        let permit = Permit::take();
        let container = Container::create(&[options, &[WORLD]].concat());
        let scratch = Scratch::new("world");
        for (n, (path, contents)) in files.iter().enumerate() {
            let source = scratch.file(&n.to_string());
            std::fs::write(&source, contents).unwrap();
            let to = format!("{}:{path}", container.0);
            docker_ok(&["cp", &source.display().to_string(), &to]);
        }
        World { container, network: None, _permit: permit }
    }

    /// The stock world, booted until ready.
    pub fn boot() -> World {
        World::boot_with(&[], &[])
    }

    /// A world created as by [`World::create`], booted until ready.
    pub fn boot_with(files: &[(&str, &str)], options: &[&str]) -> World {
        let world = World::create(files, options);
        world.start();
        world.wait_ready();
        world
    }

    pub fn id(&self) -> &str {
        &self.container.0
    }

    pub fn start(&self) {
        docker_ok(&["start", self.id()]);
    }

    /// Wait for the ready marker, as the compose healthcheck does.
    pub fn wait_ready(&self) {
        let deadline = Instant::now() + BOOT_LIMIT;
        while !self.exec(&["test", "-e", "/miroir/run/ready"]).status.success() {
            assert!(self.running(), "the world stopped before it was ready:\n{}", self.logs());
            assert!(Instant::now() < deadline, "the world was not ready in time:\n{}", self.logs());
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    fn running(&self) -> bool {
        docker_ok(&["inspect", "--format", "{{.State.Running}}", self.id()]) == "true"
    }

    /// Wait for the world's container to end; its exit status. A world that becomes ready
    /// instead, or still runs after the boot limit, fails the test.
    pub fn wait_exit(&self) -> i32 {
        let deadline = Instant::now() + BOOT_LIMIT;
        while self.running() {
            let ready = self.exec(&["test", "-e", "/miroir/run/ready"]).status.success();
            assert!(!ready, "the world booted instead of stopping:\n{}", self.logs());
            assert!(Instant::now() < deadline, "the world did not stop in time:\n{}", self.logs());
            std::thread::sleep(Duration::from_millis(200));
        }
        docker_ok(&["inspect", "--format", "{{.State.ExitCode}}", self.id()]).parse().unwrap()
    }

    /// Wait until the world's log holds `text`.
    pub fn wait_log(&self, text: &str) {
        let deadline = Instant::now() + BOOT_LIMIT;
        while !self.logs().contains(text) {
            assert!(Instant::now() < deadline, "no {text:?} in time:\n{}", self.logs());
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Everything `miroir run` and the processes printed.
    pub fn logs(&self) -> String {
        text(&docker(&["logs", self.id()]))
    }

    pub fn exec(&self, argv: &[&str]) -> Output {
        docker(&[&["exec", self.id()], argv].concat())
    }

    /// Run `argv` in the world, which must succeed; its trimmed stdout.
    pub fn exec_ok(&self, argv: &[&str]) -> String {
        let out = self.exec(argv);
        assert!(out.status.success(), "{argv:?} failed in the world:\n{}", text(&out));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Run `sql` against the graded database as its superuser; the rows, `|`-separated.
    pub fn psql(&self, sql: &str) -> String {
        let psql = ["psql", "-h", "127.0.0.1", "-U", "postgres", "-d", "crm"];
        self.exec_ok(&[&psql[..], &["-v", "ON_ERROR_STOP=1", "-tAqc", sql]].concat())
    }

    /// Run the collect hook as Harbor does, `sh -c 'miroir collect'` with the exec's bare
    /// environment. It must succeed; its summary line.
    pub fn collect(&self) -> String {
        self.exec_ok(&["sh", "-c", "miroir collect"])
    }

    /// The evidence file collect wrote, copied out of the world.
    pub fn evidence(&self) -> Evidence {
        let scratch = Scratch::new("evidence");
        let path = scratch.file("evidence.db");
        let copied = self.container.copy_out("/miroir/evidence/evidence.db", &path);
        assert!(copied, "no evidence in the world:\n{}", self.logs());
        Evidence { path, _scratch: scratch }
    }

    /// Run a fixture task's golden solution, or its probe `n`, as Harbor's oracle does: in a
    /// container of the agent's image, on a network where the world answers as `acme-crm`. It
    /// must succeed; the answer it wrote, if any.
    pub fn solve(&mut self, task: &str, probe: Option<u32>) -> Option<String> {
        let network = self.network();
        let solution = probe.map(|n| format!("SOLUTION={n}"));
        let mut args = vec!["--network", &network];
        if let Some(solution) = &solution {
            args.extend(["--env", solution]);
        }
        args.extend([MAIN, "bash", "-c", "mkdir -p /logs/artifacts && bash /solution/solve.sh"]);
        let main = Container::create(&args);
        let from = fixture(&format!("tasks/{task}/solution")).display().to_string();
        docker_ok(&["cp", &from, &format!("{}:/solution", main.0)]);
        let (status, stdout, stderr) = main.run();
        assert_eq!(status, 0, "{task} {solution:?} failed:\n{stdout}{stderr}");
        let scratch = Scratch::new("answer");
        let answer = scratch.file("answer.txt");
        main.copy_out("/logs/artifacts/answer.txt", &answer)
            .then(|| std::fs::read_to_string(answer).unwrap())
    }

    /// The world's own network, made on first use.
    fn network(&mut self) -> String {
        if let Some(Network(network)) = &self.network {
            return network.clone();
        }
        let name = format!("{LABEL}-{}", &self.id()[..12]);
        docker_ok(&["network", "create", "--label", LABEL, &name]);
        self.network = Some(Network(name.clone()));
        docker_ok(&["network", "connect", "--alias", "acme-crm", &name, self.id()]);
        name
    }
}

/// An evidence file copied out of a world.
pub struct Evidence {
    path: PathBuf,
    _scratch: Scratch,
}

impl Evidence {
    /// Each row of `sql`, a query whose one column is JSON text, read with the library's own
    /// evidence reader.
    pub fn rows(&self, sql: &'static str) -> Vec<Value> {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let mut conn = miroir::evidence::open(&self.path, false).await.unwrap();
            let rows: Vec<String> = sqlx::query_scalar(sql).fetch_all(&mut conn).await.unwrap();
            rows.iter().map(|row| serde_json::from_str(row).unwrap()).collect()
        })
    }

    /// The `(tbl, op)` of every row of `changes`, in order.
    pub fn changes(&self) -> Vec<(String, String)> {
        let rows = self.rows("SELECT json_array(tbl, op) FROM changes ORDER BY id");
        rows.iter()
            .map(|row| (row[0].as_str().unwrap().into(), row[1].as_str().unwrap().into()))
            .collect()
    }

    /// The `kind` of every row of `faults`, in order.
    pub fn faults(&self) -> Vec<String> {
        let rows = self.rows("SELECT json_array(kind) FROM faults ORDER BY seq");
        rows.iter().map(|row| row[0].as_str().unwrap().to_string()).collect()
    }
}

/// What the grader did with a trial.
pub struct Graded {
    pub exit: i32,
    /// The summary, or `void: <reason>`.
    pub stdout: String,
    /// `/logs/verifier/reward.json`, when it was written.
    pub reward: Option<Value>,
    /// `/logs/verifier/reward-details.json`, when it was written.
    pub details: Option<Value>,
}

/// Grade as Harbor's separate verifier does: `/tests/test.sh` in the grader image, with `spec`
/// in `MIROIR_SPEC`, and the evidence and answer restored where collect and the agent left them.
/// The grader must print nothing on stderr.
pub fn grade(spec: &str, evidence: Option<&Evidence>, answer: Option<&str>) -> Graded {
    images();
    let inputs = Scratch::new("verifier");
    let restored = inputs.file("miroir/evidence/evidence.db");
    if let Some(evidence) = evidence {
        std::fs::copy(&evidence.path, restored).unwrap();
    }
    let answered = inputs.file("logs/artifacts/answer.txt");
    if let Some(answer) = answer {
        std::fs::write(answered, answer).unwrap();
    }
    let variable = format!("MIROIR_SPEC={spec}");
    let grader = Container::create(&["--env", &variable, GRADER, "/tests/test.sh"]);
    grader.copy_in(&inputs.0);
    let (exit, stdout, stderr) = grader.run();
    assert_eq!(stderr, "", "the grader writes nothing on stderr");
    let outputs = Scratch::new("verifier-out");
    let verifier = outputs.0.join("verifier");
    grader.copy_out("/logs/verifier", &verifier);
    let read = |name: &str| {
        let bytes = std::fs::read(verifier.join(name)).ok()?;
        Some(serde_json::from_slice(&bytes).unwrap())
    };
    Graded { exit, stdout, reward: read("reward.json"), details: read("reward-details.json") }
}
