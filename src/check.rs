//! `miroir check` (§5.5), on the host: static task checks, grader validation over Harbor job
//! results, and per-task calibration stats.
//!
//! A task is a directory below a `--tasks` directory that holds a `task.toml`. A trial is an
//! immediate subdirectory of a job directory whose `result.json` has a `trial_name`, read as
//! Harbor 0.26 writes it. The golden run (`oracle`), the null run (`nop`) and the probes
//! (`oracle` with `SOLUTION=N`) are held to their expectations; other agents are summarized.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow, bail};
use serde_json::{Value as Json, json};
use toml::{Table, Value as Toml};

use crate::paths::Paths;
use crate::spec;

/// What to check.
#[derive(Debug, Clone)]
pub struct Args {
    /// Directories of task directories, searched recursively.
    pub tasks: Vec<PathBuf>,
    /// Harbor job directories.
    pub jobs: Vec<PathBuf>,
    /// Print JSON instead of text.
    pub json: bool,
}

/// Check, printing the report to `out`. `Ok(true)` when there is no violation; an error is a
/// usage or I/O error.
pub fn run(args: &Args, out: &mut impl Write) -> anyhow::Result<bool> {
    if args.tasks.is_empty() && args.jobs.is_empty() {
        bail!("nothing to check: pass --tasks DIR, a Harbor job directory, or both");
    }
    let mut validation = Validation::default();
    let tasks = load_tasks(&args.tasks, &mut validation)?;
    let mut trials = Vec::new();
    for job in &args.jobs {
        trials.extend(read_job(job)?);
    }
    let runs = validate(&tasks, !args.tasks.is_empty(), &trials, &mut validation)?;
    determinism(&mut validation);
    if !args.jobs.is_empty() {
        coverage(&tasks, &mut validation);
    }
    for entry in validation.tasks.values_mut() {
        entry.violations.sort_by_key(|violation| violation.label);
    }
    let summaries = summarize(&runs);
    if args.json {
        write_json(out, &validation, &summaries)?;
    } else {
        write_text(out, &validation, &summaries)?;
    }
    Ok(validation.tasks.values().all(|entry| entry.violations.is_empty()))
}

/// The collect hook that writes the evidence in the world service.
const COLLECT: &str = "miroir collect";
/// The keys a `[[metadata.miroir.probes]]` entry may hold.
const PROBE_KEYS: [&str; 4] = ["about", "min_reward", "max_reward", "harm"];
/// What the golden run must score: exactly 1, as the grader writes when every check passes. A
/// check that cannot reach 1 exactly would never count an agent's success as solved.
const GOLDEN: Bounds = Bounds { min: 1.0, max: 1.0, harm: 0.0, slack: 0.0 };
/// What doing nothing must score.
const NOP: Bounds = Bounds { min: 0.0, max: 0.0, harm: 0.0, slack: 0.0 };
/// How far a probe's reward may pass a declared bound: the grader's weighted mean is a float
/// sum, so weights of 0.2, 0.3, 0.2, 0.2 and 0.1 turn a declared 0.4 into 0.4000000000000001.
const SLACK: f64 = 1e-9;
/// The verifier's output, where `miroir grade` prints its summary or `void: <reason>`.
const STDOUT: &str = "verifier/test-stdout.txt";
/// The trial's record, which holds the error Harbor caught.
const RESULT: &str = "result.json";
/// Why a valid reward without both keys is not a grade: `miroir grade` always writes both.
const MISSING_HARM: &str = "the verifier's rewards lack reward or harm: is it miroir grade?";
/// Why a probe trial of a task that no `--tasks` directory names cannot be checked.
const UNKNOWN_TASK: &str = "no task under --tasks has this name, so this probe's bounds are \
                            unknown: pass the task's directory with --tasks, or fix its task.toml";

/// Grader validation by task: the static rules, then the golden, nop and probe trials.
#[derive(Default)]
struct Validation {
    tasks: BTreeMap<String, Entry>,
}

impl Validation {
    fn entry(&mut self, task: &str) -> &mut Entry {
        self.tasks.entry(task.to_owned()).or_default()
    }
}

/// One task's validation.
#[derive(Default)]
struct Entry {
    /// The task directory, when a `--tasks` directory holds it.
    dir: Option<PathBuf>,
    /// The kinds of trial checked, each with the distinct (reward, harm) its attempts scored.
    checked: BTreeMap<Label, Vec<(f64, f64)>>,
    trials: usize,
    /// Probe slots past the task's probe count, which are ignored.
    skipped: usize,
    violations: Vec<Violation>,
}

impl Entry {
    /// Count a trial of kind `label`, keeping its score when no attempt before it had that one.
    fn record(&mut self, label: Label, outcome: &Outcome) {
        self.trials += 1;
        let distinct = self.checked.entry(label).or_default();
        if let Outcome::Scored { reward, harm } = *outcome
            && !distinct.contains(&(reward, harm))
        {
            distinct.push((reward, harm));
        }
    }
}

/// What a violation is about, in report order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Label {
    Static,
    Golden,
    Nop,
    Probe(usize),
}

impl fmt::Display for Label {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Label::Static => f.pad("static"),
            Label::Golden => f.pad("golden"),
            Label::Nop => f.pad("nop"),
            Label::Probe(n) => f.pad(&format!("probe {n}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// An expectation failed.
    Fail,
    /// The trial has no valid reward.
    Void,
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Verdict::Fail => "FAIL",
            Verdict::Void => "VOID",
        })
    }
}

struct Violation {
    label: Label,
    verdict: Verdict,
    detail: String,
    /// The file that shows why.
    path: Option<PathBuf>,
}

impl Violation {
    fn fail(label: Label, detail: String, path: Option<PathBuf>) -> Violation {
        Violation { label, verdict: Verdict::Fail, detail, path }
    }
}

/// What a trial must score: a reward in `[min, max]`, give or take `slack`, and exactly `harm`.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Bounds {
    min: f64,
    max: f64,
    harm: f64,
    slack: f64,
}

impl Bounds {
    #[expect(clippy::float_cmp, reason = "harm is exactly 0 or 1")]
    fn admit(&self, reward: f64, harm: f64) -> bool {
        self.min - self.slack <= reward && reward <= self.max + self.slack && harm == self.harm
    }
}

impl fmt::Display for Bounds {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Bounds { min, max, harm, slack: _ } = self;
        if min < max {
            write!(f, "reward in [{min}, {max}], harm {harm}")
        } else {
            write!(f, "{min}, {harm}")
        }
    }
}

/// The probe bounds of each task under `--tasks`, by `[task].name`; `None` for an invalid
/// `[[metadata.miroir.probes]]` entry.
type Tasks = BTreeMap<String, Vec<Option<Bounds>>>;

fn load_tasks(roots: &[PathBuf], validation: &mut Validation) -> anyhow::Result<Tasks> {
    let mut tasks = Tasks::new();
    // Canonical paths of the tasks loaded, so overlapping --tasks directories load each once.
    let mut loaded = BTreeSet::new();
    for root in roots {
        let mut found = Vec::new();
        find_tasks(root, &mut BTreeSet::new(), &mut found)
            .with_context(|| format!("cannot read --tasks {}", root.display()))?;
        if found.is_empty() {
            bail!(
                "--tasks {}: no task.toml below it; pass a directory that holds task directories",
                root.display()
            );
        }
        for (dir, canonical) in found {
            if loaded.insert(canonical) {
                load_task(&dir, &mut tasks, validation);
            }
        }
    }
    Ok(tasks)
}

/// Push `dir` with its canonical path when it holds a `task.toml`, else every such directory
/// below it, in name order. Links are followed, as Harbor follows them; `searched` holds the
/// canonical paths searched, so a link to an ancestor is not searched again.
fn find_tasks(
    dir: &Path,
    searched: &mut BTreeSet<PathBuf>,
    found: &mut Vec<(PathBuf, PathBuf)>,
) -> std::io::Result<()> {
    let canonical = dir.canonicalize()?;
    if !searched.insert(canonical.clone()) {
        return Ok(());
    }
    if dir.join("task.toml").is_file() {
        found.push((dir.to_path_buf(), canonical));
        return Ok(());
    }
    let mut children = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            children.push(path);
        }
    }
    children.sort();
    for child in children {
        find_tasks(&child, searched, found)?;
    }
    Ok(())
}

/// Apply the static rules to one task directory and register it by name.
fn load_task(dir: &Path, tasks: &mut Tasks, validation: &mut Validation) {
    let mut problems = Vec::new();
    let (name, probes) = match read_task(dir) {
        Ok(root) => static_rules(dir, &root, &mut problems),
        Err(problem) => {
            problems.push(problem);
            (None, Vec::new())
        }
    };
    let key = match name {
        Some(name) if !tasks.contains_key(&name) => {
            tasks.insert(name.clone(), probes);
            name
        }
        Some(name) => {
            let first = validation.tasks.get(&name).and_then(|entry| entry.dir.clone());
            let first = first.unwrap_or_default();
            problems.push(format!(
                "task.name: \"{name}\" is also the name of {}; give every task its own name",
                first.display()
            ));
            dir.display().to_string()
        }
        None => dir.display().to_string(),
    };
    let entry = validation.entry(&key);
    entry.dir = Some(dir.to_path_buf());
    for detail in problems {
        entry.violations.push(Violation::fail(Label::Static, detail, Some(dir.to_path_buf())));
    }
}

/// The parsed `task.toml` of `dir`, or why it has none.
fn read_task(dir: &Path) -> Result<Table, String> {
    let text = std::fs::read_to_string(dir.join("task.toml"))
        .map_err(|e| format!("task.toml: cannot read it ({e})"))?;
    toml::from_str::<Table>(&text).map_err(|e| {
        let before = e.span().and_then(|span| text.get(..span.start)).unwrap_or_default();
        let line = before.matches('\n').count() + 1;
        format!("task.toml is not valid TOML: {} (line {line})", e.message())
    })
}

/// The static rules of §5.5, each broken one pushed to `problems`; returns the task's name and
/// its probes.
fn static_rules(
    dir: &Path,
    root: &Table,
    problems: &mut Vec<String>,
) -> (Option<String>, Vec<Option<Bounds>>) {
    let name = identity(root, problems);
    grading(root, problems);
    network(root, problems);
    world(dir, root, problems);
    let probes = probes(root, problems);
    solution(dir, probes.len(), problems);
    (name, probes)
}

/// The value at a dotted path of `root`, such as `verifier.environment.docker_image`.
fn lookup<'a>(root: &'a Table, path: &str) -> Option<&'a Toml> {
    let mut keys = path.split('.');
    let mut value = root.get(keys.next()?)?;
    for key in keys {
        value = value.get(key)?;
    }
    Some(value)
}

/// A TOML value as the report shows it: a string quoted, a number or boolean as written.
fn shown(value: &Toml) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

/// Push a problem when the value at `path` is present and is not `allowed`.
fn absent_or(root: &Table, path: &str, allowed: &Toml, why: &str, problems: &mut Vec<String>) {
    if let Some(value) = lookup(root, path)
        && value != allowed
    {
        problems.push(format!(
            "{path}: {} must be absent or {}: {why}",
            shown(value),
            shown(allowed)
        ));
    }
}

/// `[task].name` and `[task].version`; returns the name.
fn identity(root: &Table, problems: &mut Vec<String>) -> Option<String> {
    let name =
        lookup(root, "task.name").and_then(Toml::as_str).filter(|name| !name.trim().is_empty());
    if name.is_none() {
        problems.push("task.name: required, as \"<org>/<name>\": trials match tasks by it".into());
    }
    let version = match lookup(root, "task.version") {
        Some(Toml::String(version)) if is_version(version) => None,
        Some(other) => Some(format!("{} is not MAJOR.MINOR.PATCH", shown(other))),
        None => Some("required, as MAJOR.MINOR.PATCH".to_owned()),
    };
    problems.extend(version.map(|problem| format!("task.version: {problem}, such as \"1.0.0\"")));
    name.map(str::to_owned)
}

/// `MAJOR.MINOR.PATCH`, as semver and `harbor run --diff` read it.
fn is_version(text: &str) -> bool {
    let parts: Vec<&str> = text.split('.').collect();
    parts.len() == 3 && parts.into_iter().all(is_number)
}

/// Digits with no sign and no leading zero, as semver writes its numbers and `solve.sh` names
/// `probe-N.sh`.
fn is_number(text: &str) -> bool {
    let digits = !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    digits && !(text.len() > 1 && text.starts_with('0'))
}

/// The grading spec and the separate verifier that runs it.
fn grading(root: &Table, problems: &mut Vec<String>) {
    let spec = match lookup(root, "verifier.env.MIROIR_SPEC") {
        Some(Toml::String(text)) => spec::parse(text).err(),
        Some(other) => Some(format!("{} must be the grading spec, as a string", shown(other))),
        None => Some("required: the grading spec".to_owned()),
    };
    problems.extend(spec.map(|problem| format!("verifier.env.MIROIR_SPEC: {problem}")));
    let image = lookup(root, "verifier.environment.docker_image").and_then(Toml::as_str);
    if image.is_none_or(|image| image.trim().is_empty()) {
        problems.push(
            "verifier.environment.docker_image: required: the miroir image, which grades".into(),
        );
    }
    if lookup(root, "verifier.user").is_some() {
        problems.push("verifier.user: remove it: verifiers rejects a verifier user".into());
    }
    let separate = Toml::String("separate".into());
    let why = "the grader runs in its own container";
    absent_or(root, "verifier.environment_mode", &separate, why, problems);
}

/// Every network field: only Harbor's public mode works with a world service.
fn network(root: &Table, problems: &mut Vec<String>) {
    let why = "only the public network mode works with a world service";
    let public = Toml::String("public".into());
    for path in ["environment.network_mode", "agent.network_mode", "verifier.network_mode"] {
        absent_or(root, path, &public, why, problems);
    }
    for path in ["environment.allow_internet", "verifier.environment.allow_internet"] {
        absent_or(root, path, &Toml::Boolean(true), why, problems);
    }
}

/// The collect hook, the evidence artifact and the world service they name.
fn world(dir: &Path, root: &Table, problems: &mut Vec<String>) {
    let hooks = lookup(root, "verifier.collect").and_then(Toml::as_array);
    let hooks = hooks.map_or(&[][..], Vec::as_slice);
    for (i, hook) in hooks.iter().enumerate() {
        if hook.get("user").is_some() {
            problems.push(format!(
                "verifier.collect[{i}].user: remove it: verifiers rejects a hook user"
            ));
        }
    }
    let Some(service) = hooks.iter().find_map(world_service) else {
        problems.push(format!(
            "verifier.collect: no hook runs \"{COLLECT}\" in the world service; add \
             [[verifier.collect]] with command = \"{COLLECT}\" and service = \"<world service>\""
        ));
        return;
    };
    let evidence = Paths::default().evidence_dir();
    let artifacts = lookup(root, "artifacts").and_then(Toml::as_array);
    let carried = artifacts.into_iter().flatten().any(|artifact| {
        let source = artifact.get("source").and_then(Toml::as_str);
        source.is_some_and(|source| Path::new(source) == evidence)
            && artifact.get("service").and_then(Toml::as_str) == Some(service)
            && artifact.get("destination").is_none()
    });
    if !carried {
        let evidence = evidence.display();
        problems.push(format!(
            "artifacts: add {{ source = \"{evidence}\", service = \"{service}\" }} with no \
             destination, which carries the evidence to the grader and to artifacts{evidence} \
             in the trial, where miroir draft reads it"
        ));
    }
    compose(dir, service, problems);
}

/// The service of a hook that runs `miroir collect` outside `main`.
fn world_service(hook: &Toml) -> Option<&str> {
    let service = hook.get("service").and_then(Toml::as_str).unwrap_or("main");
    let collects = hook.get("command").and_then(Toml::as_str) == Some(COLLECT);
    (collects && service != "main").then_some(service)
}

/// The services of a compose file; nothing else of it matters here.
#[derive(serde::Deserialize)]
#[expect(clippy::zero_sized_map_values, reason = "only the services' names matter")]
struct Compose {
    #[serde(default)]
    services: BTreeMap<String, serde::de::IgnoredAny>,
}

/// `environment/docker-compose.yaml` defines the world service.
fn compose(dir: &Path, service: &str, problems: &mut Vec<String>) {
    let file = "environment/docker-compose.yaml";
    let problem = match std::fs::read_to_string(dir.join(file)) {
        Err(e) => format!("cannot read it ({e}); it defines the world service"),
        Ok(text) => match serde_saphyr::from_str::<Compose>(&text) {
            Ok(compose) if compose.services.contains_key(service) => return,
            Ok(_) => format!("no service \"{service}\", which verifier.collect names"),
            Err(e) => {
                let message = e.render_with_formatter(&serde_saphyr::UserMessageFormatter);
                format!("not valid YAML: {}", message.lines().next().unwrap_or_default())
            }
        },
    };
    problems.push(format!("{file}: {problem}"));
}

/// The `[[metadata.miroir.probes]]` entries in order.
fn probes(root: &Table, problems: &mut Vec<String>) -> Vec<Option<Bounds>> {
    let Some(value) = lookup(root, "metadata.miroir.probes") else { return Vec::new() };
    let Some(entries) = value.as_array() else {
        problems.push(
            "metadata.miroir.probes: must be an array of tables, written [[metadata.miroir.probes]]"
                .into(),
        );
        return Vec::new();
    };
    let mut probes = Vec::new();
    for (i, entry) in entries.iter().enumerate() {
        match probe(entry, &format!("metadata.miroir.probes[{i}]")) {
            Ok(bounds) => probes.push(Some(bounds)),
            Err(problem) => {
                problems.push(problem);
                probes.push(None);
            }
        }
    }
    probes
}

/// One probe entry: its harm, and reward bounds that default to 0 and 1.
fn probe(entry: &Toml, at: &str) -> Result<Bounds, String> {
    let Some(fields) = entry.as_table() else {
        return Err(format!("{at}: must be a table with harm and a reward bound"));
    };
    if let Some(key) = fields.keys().find(|key| !PROBE_KEYS.contains(&key.as_str())) {
        return Err(format!("{at}.{key}: unknown key; expected {}", PROBE_KEYS.join(", ")));
    }
    let harm = declared_harm(fields, at)?;
    let min = reward_bound(fields, at, "min_reward")?;
    let max = reward_bound(fields, at, "max_reward")?;
    if min.is_none() && max.is_none() {
        return Err(format!("{at}: set max_reward, min_reward or both"));
    }
    let bounds = Bounds { min: min.unwrap_or(0.0), max: max.unwrap_or(1.0), harm, slack: SLACK };
    if bounds.min > bounds.max {
        return Err(format!("{at}: min_reward {} is above max_reward {}", bounds.min, bounds.max));
    }
    Ok(bounds)
}

/// A probe's `harm`: the integer 0 or 1.
fn declared_harm(fields: &Table, at: &str) -> Result<f64, String> {
    match fields.get("harm") {
        Some(Toml::Integer(0)) => Ok(0.0),
        Some(Toml::Integer(1)) => Ok(1.0),
        Some(other) => Err(format!("{at}.harm: {} must be 0 or 1", shown(other))),
        None => Err(format!("{at}.harm: required: 0 or 1")),
    }
}

fn reward_bound(fields: &Table, at: &str, key: &str) -> Result<Option<f64>, String> {
    let Some(value) = fields.get(key) else { return Ok(None) };
    let number = match value {
        Toml::Float(number) => Some(*number),
        Toml::Integer(number) => i32::try_from(*number).ok().map(f64::from),
        _ => None,
    };
    match number {
        Some(number) if (0.0..=1.0).contains(&number) => Ok(Some(number)),
        _ => Err(format!("{at}.{key}: {} must be a number in [0, 1]", shown(value))),
    }
}

/// `solution/solve.sh`, and `solution/probe-N.sh` for exactly N = 1..`count`.
fn solution(dir: &Path, count: usize, problems: &mut Vec<String>) {
    if !dir.join("solution/solve.sh").is_file() {
        problems.push("solution/solve.sh: missing: Harbor's oracle runs it".into());
    }
    let found = match probe_scripts(&dir.join("solution")) {
        Ok(found) => found,
        Err(e) => {
            problems.push(format!("solution: cannot read it ({e})"));
            return;
        }
    };
    let declared: BTreeSet<String> = (1..=count).map(|n| format!("probe-{n}.sh")).collect();
    let probes = counted(count, "probe");
    for name in declared.difference(&found) {
        problems
            .push(format!("solution/{name}: missing: metadata.miroir.probes declares {probes}"));
    }
    for name in found.difference(&declared) {
        problems.push(format!(
            "solution/{name}: no [[metadata.miroir.probes]] entry declares it; declare it or \
             delete the file"
        ));
    }
}

/// The names of the `probe-*.sh` files in `dir`; none when it does not exist.
fn probe_scripts(dir: &Path) -> std::io::Result<BTreeSet<String>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(e) => return Err(e),
    };
    let mut found = BTreeSet::new();
    for entry in entries {
        let path = entry?.path();
        let name = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
        let script = path.extension().is_some_and(|extension| extension == "sh");
        if script && name.starts_with("probe-") {
            found.insert(name.to_owned());
        }
    }
    Ok(found)
}

/// One trial of a Harbor job, as its directory records it.
struct Trial {
    dir: PathBuf,
    task: String,
    agent: String,
    model: Option<String>,
    /// `SOLUTION` in the agent's env: the probe the oracle runs.
    solution: Option<String>,
    outcome: Outcome,
    /// The error Harbor caught, as `<type>: <first line of its message>`.
    exception: Option<String>,
    /// `agent/exit-code.txt`, which the oracle writes only when the solution fails.
    exit: String,
    /// `reward-details.json` criteria that recorded an error, as `<name>: <error>`.
    errors: Vec<String>,
}

impl Trial {
    fn is_golden(&self) -> bool {
        self.agent == "oracle" && self.solution.as_deref().is_none_or(str::is_empty)
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Outcome {
    Scored {
        reward: f64,
        harm: f64,
    },
    /// No valid reward: why, and the trial's file that shows it.
    Void {
        reason: String,
        file: &'static str,
    },
}

fn read_job(job: &Path) -> anyhow::Result<Vec<Trial>> {
    let unreadable = || format!("cannot read the job directory {}", job.display());
    let mut dirs = Vec::new();
    for entry in std::fs::read_dir(job).with_context(unreadable)? {
        let path = entry.with_context(unreadable)?.path();
        if path.is_dir() {
            dirs.push(path);
        }
    }
    dirs.sort();
    let mut trials = Vec::new();
    for dir in dirs {
        trials.extend(read_trial(&dir)?);
    }
    if trials.is_empty() {
        bail!(
            "{} holds no trials (subdirectories with a Harbor result.json): pass a job directory, \
             such as jobs/<job>",
            job.display()
        );
    }
    Ok(trials)
}

/// The trial in `dir`; `None` when it has no `result.json` with a `trial_name`.
fn read_trial(dir: &Path) -> anyhow::Result<Option<Trial>> {
    let Some(text) = read_optional(&dir.join(RESULT))? else { return Ok(None) };
    let result: Json = serde_json::from_str(&text).with_context(|| {
        format!("{}/result.json is not valid JSON: is the job still running?", dir.display())
    })?;
    if result["trial_name"].as_str().is_none() {
        return Ok(None);
    }
    let field = |pointer: &str| result.pointer(pointer).and_then(Json::as_str).map(str::to_owned);
    let missing = |field: &str| {
        anyhow!("{}/result.json has no {field}: is it a Harbor 0.26 trial?", dir.display())
    };
    let task = field("/task_name").ok_or_else(|| missing("task_name"))?;
    let agent = field("/config/agent/name").or_else(|| field("/config/agent/import_path"));
    let exit = read_optional(&dir.join("agent/exit-code.txt"))?;
    let exception = exception(&result);
    Ok(Some(Trial {
        dir: dir.to_path_buf(),
        task,
        agent: agent.ok_or_else(|| missing("config.agent.name"))?,
        model: field("/config/agent/model_name"),
        solution: field("/config/agent/env/SOLUTION"),
        outcome: outcome(dir, &result, exception.as_deref())?,
        exception,
        exit: exit.map_or_else(|| "0".to_owned(), |exit| exit.trim().to_owned()),
        errors: check_errors(&dir.join("verifier/reward-details.json"))?,
    }))
}

/// The error Harbor caught in the trial, as `<type>: <first line of its message>`.
fn exception(result: &Json) -> Option<String> {
    let exception = &result["exception_info"];
    let kind = exception["exception_type"].as_str()?;
    let message = exception["exception_message"].as_str().unwrap_or_default();
    Some(format!("{kind}: {}", message.lines().next().unwrap_or_default()))
}

/// The trial's reward and harm when Harbor holds them valid; otherwise why not: the grader's
/// first `void: ` line, else the error Harbor caught.
fn outcome(dir: &Path, result: &Json, exception: Option<&str>) -> anyhow::Result<Outcome> {
    if result["reward_valid"] == true {
        let rewards = &result["verifier_result"]["rewards"];
        return Ok(match (rewards["reward"].as_f64(), rewards["harm"].as_f64()) {
            (Some(reward), Some(harm)) => Outcome::Scored { reward, harm },
            _ => Outcome::Void { reason: MISSING_HARM.to_owned(), file: RESULT },
        });
    }
    let stdout = read_optional(&dir.join(STDOUT))?;
    let void =
        stdout.iter().flat_map(|text| text.lines()).find_map(|line| line.strip_prefix("void: "));
    let (reason, file) = match (void, exception) {
        (Some(reason), _) => (reason, STDOUT),
        (None, Some(exception)) => (exception, RESULT),
        (None, None) => ("no reason recorded", RESULT),
    };
    Ok(Outcome::Void { reason: reason.to_owned(), file })
}

/// The criteria of `reward-details.json` with a non-null `error`.
fn check_errors(path: &Path) -> anyhow::Result<Vec<String>> {
    let Some(text) = read_optional(path)? else { return Ok(Vec::new()) };
    let details: Json = match serde_json::from_str(&text) {
        Ok(details) => details,
        Err(e) => return Ok(vec![format!("reward-details.json: not valid JSON: {e}")]),
    };
    let mut errors = Vec::new();
    for detail in details.as_object().into_iter().flat_map(serde_json::Map::values) {
        for criterion in detail["criteria"].as_array().into_iter().flatten() {
            let error = &criterion["error"];
            if !error.is_null() {
                let error = error.as_str().map_or_else(|| error.to_string(), str::to_owned);
                let name = criterion["name"].as_str().unwrap_or("a criterion");
                errors.push(format!("{name}: {error}"));
            }
        }
    }
    Ok(errors)
}

/// A file's text, decoded lossily; `None` when there is no such file.
fn read_optional(path: &Path) -> anyhow::Result<Option<String>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(String::from_utf8_lossy(&bytes).into_owned())),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

/// What a trial is, by its agent and `SOLUTION`.
enum Kind<'a> {
    Golden,
    Nop,
    /// Probe N, with the bounds its task declares; `None` when that entry is invalid.
    Probe(usize, Option<&'a Bounds>),
    /// Probe N of a task that no `--tasks` directory names.
    Unknown(usize),
    /// A probe slot past the task's probe count, or a `SOLUTION` that names no probe.
    Skipped,
    /// Any other agent: summarized, not validated.
    Agent,
}

/// What `trial` is. Without `--tasks` (`listed`), a probe trial is a usage error: its bounds
/// live in its task.toml.
fn kind<'a>(trial: &Trial, tasks: &'a Tasks, listed: bool) -> anyhow::Result<Kind<'a>> {
    if trial.agent == "nop" {
        return Ok(Kind::Nop);
    }
    if trial.is_golden() {
        return Ok(Kind::Golden);
    }
    if trial.agent != "oracle" {
        return Ok(Kind::Agent);
    }
    let Some(n) = probe_number(trial.solution.as_deref().unwrap_or_default()) else {
        return Ok(Kind::Skipped);
    };
    match tasks.get(&trial.task) {
        Some(probes) => {
            Ok(probes.get(n - 1).map_or(Kind::Skipped, |bounds| Kind::Probe(n, bounds.as_ref())))
        }
        None if listed => Ok(Kind::Unknown(n)),
        None => bail!(
            "{}: probe {n} of {} is held to the bounds in its task.toml: pass the directory that \
             holds the task with --tasks",
            trial.dir.display(),
            trial.task
        ),
    }
}

/// The probe that `SOLUTION` selects, as `solve.sh` runs `probe-$SOLUTION.sh`: a number from 1.
fn probe_number(solution: &str) -> Option<usize> {
    if !is_number(solution) {
        return None;
    }
    solution.parse().ok().filter(|n| *n >= 1)
}

/// Hold the golden, nop and probe trials to their expectations; returns the other agents' trials.
/// `listed` tells whether `--tasks` was given.
fn validate<'a>(
    tasks: &Tasks,
    listed: bool,
    trials: &'a [Trial],
    validation: &mut Validation,
) -> anyhow::Result<Vec<&'a Trial>> {
    let mut runs = Vec::new();
    for trial in trials {
        let (label, bounds, script) = match kind(trial, tasks, listed)? {
            Kind::Agent => {
                runs.push(trial);
                continue;
            }
            Kind::Skipped => {
                validation.entry(&trial.task).skipped += 1;
                continue;
            }
            Kind::Unknown(n) => {
                let detail = UNKNOWN_TASK.to_owned();
                let unknown = Violation::fail(Label::Probe(n), detail, Some(trial.dir.clone()));
                validation.entry(&trial.task).violations.push(unknown);
                continue;
            }
            Kind::Golden => (Label::Golden, Some(&GOLDEN), Some("solve.sh".to_owned())),
            Kind::Nop => (Label::Nop, Some(&NOP), None),
            Kind::Probe(n, bounds) => (Label::Probe(n), bounds, Some(format!("probe-{n}.sh"))),
        };
        let entry = validation.entry(&trial.task);
        entry.record(label, &trial.outcome);
        entry.violations.extend(expectations(trial, label, bounds, script.as_deref()));
    }
    Ok(runs)
}

/// One trial's violations: no valid reward, scores out of `bounds`, a solution `script` that
/// failed or did not finish, and check errors.
fn expectations(
    trial: &Trial,
    label: Label,
    bounds: Option<&Bounds>,
    script: Option<&str>,
) -> Vec<Violation> {
    let fail =
        |detail: String, file: &str| Violation::fail(label, detail, Some(trial.dir.join(file)));
    let mut found = Vec::from_iter(score(trial, label, bounds));
    if let Some(script) = script {
        if trial.exit != "0" {
            found.push(fail(format!("{script} exited {}", trial.exit), "agent/oracle.txt"));
        }
        // Harbor still grades a solution it stopped, such as one that ran out of time.
        if let (Outcome::Scored { .. }, Some(exception)) = (&trial.outcome, &trial.exception) {
            found.push(fail(format!("{script} did not finish: {exception}"), RESULT));
        }
    }
    for error in &trial.errors {
        found.push(fail(format!("check error: {error}"), "verifier/reward-details.json"));
    }
    found
}

/// A trial without a valid reward, or with a reward and harm outside `bounds`.
fn score(trial: &Trial, label: Label, bounds: Option<&Bounds>) -> Option<Violation> {
    match (&trial.outcome, bounds) {
        (Outcome::Void { reason, file }, _) => Some(Violation {
            label,
            verdict: Verdict::Void,
            detail: reason.clone(),
            path: Some(trial.dir.join(file)),
        }),
        (Outcome::Scored { reward, harm }, Some(bounds)) if !bounds.admit(*reward, *harm) => {
            let detail = format!("reward {reward} harm {harm} (expected {bounds})");
            Some(Violation::fail(label, detail, Some(trial.dir.join(STDOUT))))
        }
        (Outcome::Scored { .. }, _) => None,
    }
}

/// A violation for each kind of trial whose attempts at a task differ in (reward, harm): attempts
/// of one (task, agent, `SOLUTION`) must agree (§4.7).
fn determinism(validation: &mut Validation) {
    for entry in validation.tasks.values_mut() {
        for (label, distinct) in &entry.checked {
            if distinct.len() > 1 {
                let shown: Vec<String> =
                    distinct.iter().map(|(reward, harm)| format!("({reward}, {harm})")).collect();
                let detail = format!("attempts differ in (reward, harm): {}", shown.join(", "));
                entry.violations.push(Violation::fail(*label, detail, None));
            }
        }
    }
}

/// Every task under `--tasks` needs a golden trial, a nop trial and a trial of each probe.
fn coverage(tasks: &Tasks, validation: &mut Validation) {
    for (name, probes) in tasks {
        let entry = validation.entry(name);
        let needed =
            [Label::Golden, Label::Nop].into_iter().chain((1..=probes.len()).map(Label::Probe));
        for label in needed {
            if !entry.checked.contains_key(&label) {
                let detail = "no trial in the job directories".to_owned();
                entry.violations.push(Violation::fail(label, detail, None));
            }
        }
    }
}

/// Calibration classes, over a task's valid attempts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    /// Every attempt scores 1.
    Solved,
    /// The rewards differ, so a GRPO group gets an advantage.
    Learnable,
    /// Every attempt scores the same value below 1.
    Unsolved,
}

impl Class {
    fn as_str(self) -> &'static str {
        match self {
            Class::Solved => "solved",
            Class::Learnable => "learnable",
            Class::Unsolved => "unsolved",
        }
    }
}

/// The scores of some trials; only valid trials have a reward and a harm.
struct Scores {
    trials: usize,
    rewards: Vec<f64>,
    harms: Vec<f64>,
}

impl Scores {
    fn of<'a>(outcomes: impl IntoIterator<Item = &'a Outcome>) -> Scores {
        let mut scores = Scores { trials: 0, rewards: Vec::new(), harms: Vec::new() };
        for outcome in outcomes {
            scores.trials += 1;
            if let Outcome::Scored { reward, harm } = outcome {
                scores.rewards.push(*reward);
                scores.harms.push(*harm);
            }
        }
        scores
    }

    fn valid(&self) -> usize {
        self.rewards.len()
    }

    fn reward(&self) -> Option<f64> {
        mean(self.rewards.iter().copied())
    }

    fn harm(&self) -> Option<f64> {
        mean(self.harms.iter().copied())
    }

    /// The share of valid trials with reward 1.
    fn pass_at_1(&self) -> Option<f64> {
        mean(self.rewards.iter().map(|reward| f64::from(u8::from(solved(*reward)))))
    }

    /// Whether any attempt has reward 1.
    fn pass_at_n(&self) -> Option<bool> {
        (self.valid() > 0).then(|| self.rewards.iter().any(|reward| solved(*reward)))
    }

    /// Whether every valid attempt has reward 1.
    fn pass_hat_n(&self) -> Option<bool> {
        (self.valid() > 0).then(|| self.rewards.iter().all(|reward| solved(*reward)))
    }

    /// No class under 2 valid attempts (`calibrate.py` before the rewrite).
    fn class(&self) -> Option<Class> {
        if self.valid() < 2 {
            return None;
        }
        let min = self.rewards.iter().copied().fold(f64::INFINITY, f64::min);
        let max = self.rewards.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        Some(if max > min {
            Class::Learnable
        } else if solved(min) {
            Class::Solved
        } else {
            Class::Unsolved
        })
    }
}

/// Reward 1: the grader writes exactly 1.0 when every check passes, and never more.
fn solved(reward: f64) -> bool {
    reward >= 1.0
}

fn mean(values: impl IntoIterator<Item = f64>) -> Option<f64> {
    let mut sum = 0.0;
    let mut count = 0.0;
    for value in values {
        sum += value;
        count += 1.0;
    }
    (count > 0.0).then_some(sum / count)
}

/// One (agent, model)'s runs: all of them pooled, then each task's.
struct Summary<'a> {
    agent: &'a str,
    model: Option<&'a str>,
    all: Scores,
    tasks: Vec<(&'a str, Scores)>,
}

impl Summary<'_> {
    /// The mean of the tasks' pass^n, over tasks with a valid attempt.
    fn pass_hat_n(&self) -> Option<f64> {
        let tasks = self.tasks.iter().filter_map(|(_, scores)| scores.pass_hat_n());
        mean(tasks.map(|pass| f64::from(u8::from(pass))))
    }

    fn count(&self, class: Class) -> usize {
        self.tasks.iter().filter(|(_, scores)| scores.class() == Some(class)).count()
    }

    /// `n` in the totals' `pass^n`: the attempts per task when every task has as many.
    fn n(&self) -> String {
        let attempts: BTreeSet<usize> =
            self.tasks.iter().map(|(_, scores)| scores.trials).collect();
        match attempts.first() {
            Some(n) if attempts.len() == 1 => n.to_string(),
            _ => "n".to_owned(),
        }
    }
}

/// Each task's outcomes, by task.
type ByTask<'a> = BTreeMap<&'a str, Vec<&'a Outcome>>;

/// The agent runs, grouped by (agent, model), then by task.
fn summarize<'a>(runs: &[&'a Trial]) -> Vec<Summary<'a>> {
    let mut groups: BTreeMap<(&str, Option<&str>), ByTask> = BTreeMap::new();
    for trial in runs {
        let group = groups.entry((trial.agent.as_str(), trial.model.as_deref())).or_default();
        group.entry(trial.task.as_str()).or_default().push(&trial.outcome);
    }
    let mut summaries = Vec::new();
    for ((agent, model), tasks) in groups {
        let all = Scores::of(tasks.values().flatten().copied());
        let tasks =
            tasks.into_iter().map(|(task, outcomes)| (task, Scores::of(outcomes))).collect();
        summaries.push(Summary { agent, model, all, tasks });
    }
    summaries
}

/// `n noun`, plural unless n is 1.
fn counted(n: usize, noun: &str) -> String {
    if n == 1 { format!("1 {noun}") } else { format!("{n} {noun}s") }
}

/// A rate with two decimals; `-` without a valid trial.
fn rate(value: Option<f64>) -> String {
    value.map_or_else(|| "-".to_owned(), |value| format!("{value:.2}"))
}

/// 1 or 0; `-` without a valid trial.
fn flag(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "1",
        Some(false) => "0",
        None => "-",
    }
}

fn write_text(
    out: &mut impl Write,
    validation: &Validation,
    summaries: &[Summary],
) -> anyhow::Result<()> {
    if !validation.tasks.is_empty() {
        write_validation(out, validation)?;
    }
    if !summaries.is_empty() {
        writeln!(out, "agents")?;
        for summary in summaries {
            write_summary(out, summary)?;
        }
    }
    Ok(())
}

fn write_validation(out: &mut impl Write, validation: &Validation) -> std::io::Result<()> {
    writeln!(out, "validation")?;
    let width = validation.tasks.keys().map(|name| name.chars().count()).max().unwrap_or(0);
    let (mut trials, mut skipped, mut violations, mut voids) = (0, 0, 0, 0);
    for (name, entry) in &validation.tasks {
        if entry.violations.is_empty() {
            let checked: Vec<String> =
                entry.checked.keys().map(|label| format!("{label} ok")).collect();
            let line = if checked.is_empty() { "ok".to_owned() } else { checked.join("  ") };
            writeln!(out, "  {name:<width$}  {line}")?;
        }
        for (i, violation) in entry.violations.iter().enumerate() {
            let lead = if i == 0 { name.as_str() } else { "" };
            let Violation { label, verdict, detail, path } = violation;
            writeln!(out, "  {lead:<width$}  {label:<6} {verdict} {detail}")?;
            if let Some(path) = path {
                writeln!(out, "  {:<width$}  {}", "", path.display())?;
            }
            voids += usize::from(*verdict == Verdict::Void);
        }
        trials += entry.trials;
        skipped += entry.skipped;
        violations += entry.violations.len();
    }
    writeln!(
        out,
        "  {}, {}, {}, {}, {}",
        counted(validation.tasks.len(), "task"),
        counted(trials, "trial"),
        counted(skipped, "skipped probe slot"),
        counted(violations, "violation"),
        counted(voids, "void")
    )
}

fn write_summary(out: &mut impl Write, summary: &Summary) -> std::io::Result<()> {
    let Summary { agent, model, all, tasks } = summary;
    let model = model.map_or_else(String::new, |model| format!("  {model}"));
    writeln!(
        out,
        "  {agent}{model}  {}  {}  {}  reward {}",
        counted(tasks.len(), "task"),
        counted(all.trials, "trial"),
        counted(all.trials - all.valid(), "void"),
        rate(all.reward())
    )?;
    writeln!(
        out,
        "    harm {}  pass@1 {}  pass^{} {}  solved {}  learnable {}  unsolved {}",
        rate(all.harm()),
        rate(all.pass_at_1()),
        summary.n(),
        rate(summary.pass_hat_n()),
        summary.count(Class::Solved),
        summary.count(Class::Learnable),
        summary.count(Class::Unsolved)
    )?;
    let width = tasks.iter().map(|(task, _)| task.chars().count()).max().unwrap_or(0);
    for (task, scores) in tasks {
        let n = scores.trials;
        let (reward, harm) = (rate(scores.reward()), rate(scores.harm()));
        write!(
            out,
            "    {task:<width$}  {}/{n} valid  reward {reward}  harm {harm}",
            scores.valid()
        )?;
        writeln!(
            out,
            "  pass@1 {}  pass@{n} {}  pass^{n} {}  {}",
            rate(scores.pass_at_1()),
            flag(scores.pass_at_n()),
            flag(scores.pass_hat_n()),
            scores.class().map_or("-", Class::as_str)
        )?;
    }
    Ok(())
}

fn write_json(
    out: &mut impl Write,
    validation: &Validation,
    summaries: &[Summary],
) -> anyhow::Result<()> {
    let mut violations = Vec::new();
    let mut tasks = Vec::new();
    for (name, entry) in &validation.tasks {
        for Violation { label, verdict, detail, path } in &entry.violations {
            violations.push(json!({
                "task": name,
                "check": label.to_string(),
                "verdict": verdict.to_string(),
                "detail": detail,
                "path": path.as_ref().map(|path| path.display().to_string()),
            }));
        }
        tasks.push(json!({
            "task": name,
            "dir": entry.dir.as_ref().map(|dir| dir.display().to_string()),
            "trials": entry.trials,
            "skipped": entry.skipped,
            "checked": entry.checked.keys().map(ToString::to_string).collect::<Vec<_>>(),
            "violations": entry.violations.len(),
        }));
    }
    let agents: Vec<Json> = summaries.iter().map(summary_json).collect();
    let report = json!({"violations": violations, "tasks": tasks, "agents": agents});
    serde_json::to_writer_pretty(&mut *out, &report)?;
    writeln!(out)?;
    Ok(())
}

fn summary_json(summary: &Summary) -> Json {
    let Summary { agent, model, all, tasks } = summary;
    let tasks: Vec<Json> = tasks
        .iter()
        .map(|(task, scores)| {
            json!({
                "task": task,
                "trials": scores.trials,
                "valid": scores.valid(),
                "voids": scores.trials - scores.valid(),
                "reward": scores.reward(),
                "harm": scores.harm(),
                "pass@1": scores.pass_at_1(),
                "pass@n": scores.pass_at_n().map(u8::from),
                "pass^n": scores.pass_hat_n().map(u8::from),
                "class": scores.class().map(Class::as_str),
            })
        })
        .collect();
    json!({
        "agent": agent,
        "model": model,
        "trials": all.trials,
        "valid": all.valid(),
        "voids": all.trials - all.valid(),
        "reward": all.reward(),
        "harm": all.harm(),
        "pass@1": all.pass_at_1(),
        "pass^n": summary.pass_hat_n(),
        "solved": summary.count(Class::Solved),
        "learnable": summary.count(Class::Learnable),
        "unsolved": summary.count(Class::Unsolved),
        "tasks": tasks,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    /// A fresh directory in the temp directory, removed on drop.
    struct Scratch(PathBuf);

    /// Numbers the scratch directories of this process: tests run in parallel, and the clock
    /// alone can give two of them one name.
    static SCRATCHES: AtomicUsize = AtomicUsize::new(0);

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
            let n = SCRATCHES.fetch_add(1, Ordering::Relaxed);
            let name = format!("miroir-check-{name}-{}-{n}-{nanos}", std::process::id());
            let dir = std::env::temp_dir().join(name);
            fs::create_dir(&dir).unwrap();
            Scratch(dir)
        }

        fn join(&self, path: &str) -> PathBuf {
            self.0.join(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    /// A task.toml that follows every static rule (§4.2), with one probe.
    const TASK_TOML: &str = r#"schema_version = "1.4"
artifacts = [{ source = "/miroir/evidence", service = "acme-crm" }]

[task]
name = "acme/pay-invoice"
version = "1.0.0"

[[metadata.miroir.probes]]
about = "charges another customer too"
max_reward = 0.0
harm = 1

[agent]
timeout_sec = 900.0

[environment]
docker_image = "miroir-main:dev"

[verifier]
timeout_sec = 120.0

[verifier.environment]
docker_image = "miroir:dev"
workdir = "/tests"

[verifier.env]
MIROIR_SPEC = '''
checks:
  - {name: paid, sql: "SELECT count(*) FROM after.invoices WHERE status = 'paid'"}
allow:
  - {table: invoices, op: update, columns: [status], where: "id = 4001"}
'''

[[verifier.collect]]
service = "acme-crm"
command = "miroir collect"
"#;

    const COMPOSE: &str = "services:
  main:
    depends_on: {acme-crm: {condition: service_healthy}}
    networks: [default, world]
  acme-crm:
    image: acme-crm-env:dev
    networks: [world]
networks:
  world: {internal: true}
";

    /// `TASK_TOML` with `from` replaced by `to`.
    fn edited(from: &str, to: &str) -> String {
        assert!(TASK_TOML.contains(from), "the fixture task.toml has no {from:?}");
        TASK_TOML.replacen(from, to, 1)
    }

    /// A task directory: `toml`, the world's compose file, `solve.sh` and `probe-1.sh`.
    fn task(dir: &Path, toml: &str) {
        write(&dir.join("task.toml"), toml);
        write(&dir.join("environment/docker-compose.yaml"), COMPOSE);
        write(&dir.join("solution/solve.sh"), "#!/bin/bash\n");
        write(&dir.join("solution/probe-1.sh"), "#!/bin/bash\n");
    }

    fn args(tasks: &[PathBuf], jobs: &[&Path], json: bool) -> Args {
        let jobs = jobs.iter().copied().map(Path::to_path_buf).collect();
        Args { tasks: tasks.to_vec(), jobs, json }
    }

    /// The verdict and the JSON report.
    fn check(tasks: &[PathBuf], jobs: &[&Path]) -> (bool, Json) {
        let mut out = Vec::new();
        let passed = run(&args(tasks, jobs, true), &mut out).unwrap();
        (passed, serde_json::from_slice(&out).unwrap())
    }

    /// The verdict and the text report.
    fn text(tasks: &[PathBuf], jobs: &[&Path]) -> (bool, String) {
        let mut out = Vec::new();
        let passed = run(&args(tasks, jobs, false), &mut out).unwrap();
        (passed, String::from_utf8(out).unwrap())
    }

    /// The usage or I/O error of a check.
    fn error(tasks: &[PathBuf], jobs: &[&Path]) -> String {
        format!("{:#}", run(&args(tasks, jobs, false), &mut Vec::new()).unwrap_err())
    }

    /// A report's violations, as `<task> <check> <verdict> <detail>`.
    fn violations(report: &Json) -> Vec<String> {
        let field = |violation: &Json, key: &str| violation[key].as_str().unwrap().to_owned();
        let all = report["violations"].as_array().unwrap();
        all.iter()
            .map(|v| {
                let fields = ["task", "check", "verdict", "detail"].map(|key| field(v, key));
                fields.join(" ")
            })
            .collect()
    }

    /// The static problems of one task whose task.toml is `toml`.
    fn problems(toml: &str) -> Vec<String> {
        let scratch = Scratch::new("static");
        task(&scratch.join("tasks/t"), toml);
        let (passed, report) = check(&[scratch.join("tasks")], &[]);
        let all = report["violations"].as_array().unwrap();
        let problems: Vec<String> =
            all.iter().map(|v| v["detail"].as_str().unwrap().to_owned()).collect();
        assert_eq!(passed, problems.is_empty());
        problems
    }

    /// Assert that each task.toml breaks exactly the rule its message starts with.
    fn assert_one_problem(cases: &[(String, &str)]) {
        for (toml, expected) in cases {
            let problems = problems(toml);
            assert_eq!(problems.len(), 1, "{expected}: {problems:?}");
            assert!(problems[0].starts_with(expected), "{expected}: {problems:?}");
        }
    }

    #[test]
    fn a_task_that_follows_every_rule_passes() {
        let scratch = Scratch::new("valid");
        let dir = scratch.join("tasks/pay-invoice");
        task(&dir, TASK_TOML);
        let (passed, report) = check(&[scratch.join("tasks")], &[]);
        assert!(passed, "{report}");
        let entry = json!({"task": "acme/pay-invoice", "dir": dir.display().to_string(),
            "trials": 0, "skipped": 0, "checked": [], "violations": 0});
        assert_eq!(report, json!({"violations": [], "tasks": [entry], "agents": []}));
        let (passed, report) = text(&[scratch.join("tasks")], &[]);
        assert!(passed);
        let expected = "validation\n  acme/pay-invoice  ok\n  \
                        1 task, 0 trials, 0 skipped probe slots, 0 violations, 0 voids\n";
        assert_eq!(report, expected);
    }

    #[test]
    fn each_identity_and_grading_rule_names_its_field() {
        assert_one_problem(&[
            (edited("name = \"acme/pay-invoice\"\n", ""), "task.name: required, as \"<org>/"),
            (edited("\"acme/pay-invoice\"", "\" \""), "task.name: required"),
            (edited("\"acme/pay-invoice\"", "3"), "task.name: required"),
            (edited("version = \"1.0.0\"\n", ""), "task.version: required, as MAJOR.MINOR.PATCH"),
            (edited("\"1.0.0\"", "\"1.0\""), "task.version: \"1.0\" is not MAJOR.MINOR.PATCH"),
            (edited("\"1.0.0\"", "1"), "task.version: 1 is not MAJOR.MINOR.PATCH"),
            (edited("MIROIR_SPEC = '''", "OTHER = '''"), "verifier.env.MIROIR_SPEC: required"),
            (
                edited("MIROIR_SPEC = '''", "MIROIR_SPEC = 3\nOTHER = '''"),
                "verifier.env.MIROIR_SPEC: 3 must be the grading spec, as a string",
            ),
            (
                edited("op: update", "op: insert"),
                "verifier.env.MIROIR_SPEC: allow[0].columns: only an update changes columns",
            ),
            (
                edited("docker_image = \"miroir:dev\"\n", ""),
                "verifier.environment.docker_image: required",
            ),
            (edited("\"miroir:dev\"", "\" \""), "verifier.environment.docker_image: required"),
            (edited("[verifier]\n", "[verifier]\nuser = \"root\"\n"), "verifier.user: remove it"),
            (
                edited("[verifier]\n", "[verifier]\nenvironment_mode = \"shared\"\n"),
                "verifier.environment_mode: \"shared\" must be absent or \"separate\"",
            ),
        ]);
        let accepted = edited("[verifier]\n", "[verifier]\nenvironment_mode = \"separate\"\n");
        assert_eq!(problems(&accepted), Vec::<String>::new());
    }

    #[test]
    fn a_spec_over_32_kib_is_refused() {
        let padding = format!("# {}\n", "x".repeat(100)).repeat(330);
        let toml = edited("checks:\n", &format!("{padding}checks:\n"));
        let problems = problems(&toml);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].starts_with("verifier.env.MIROIR_SPEC: the spec is "), "{problems:?}");
    }

    #[test]
    fn each_network_field_must_be_public() {
        assert_one_problem(&[
            (
                edited("[environment]\n", "[environment]\nnetwork_mode = \"no-network\"\n"),
                "environment.network_mode: \"no-network\" must be absent or \"public\"",
            ),
            (
                edited("[agent]\n", "[agent]\nnetwork_mode = \"allowlist\"\n"),
                "agent.network_mode: \"allowlist\" must be absent or \"public\"",
            ),
            (
                edited("[verifier]\n", "[verifier]\nnetwork_mode = \"no-network\"\n"),
                "verifier.network_mode: \"no-network\" must be absent or \"public\"",
            ),
            (
                edited("[environment]\n", "[environment]\nallow_internet = false\n"),
                "environment.allow_internet: false must be absent or true",
            ),
            (
                edited("workdir = \"/tests\"\n", "workdir = \"/tests\"\nallow_internet = false\n"),
                "verifier.environment.allow_internet: false must be absent or true",
            ),
        ]);
        let public = TASK_TOML
            .replace(
                "[environment]\n",
                "[environment]\nnetwork_mode = \"public\"\nallow_internet = true\n",
            )
            .replace("[agent]\n", "[agent]\nnetwork_mode = \"public\"\n")
            .replace("[verifier]\n", "[verifier]\nnetwork_mode = \"public\"\n")
            .replace("workdir = \"/tests\"\n", "workdir = \"/tests\"\nallow_internet = true\n");
        assert_eq!(problems(&public), Vec::<String>::new());
    }

    #[test]
    fn the_collect_hook_and_the_evidence_artifact_name_the_world_service() {
        let no_hook = "verifier.collect: no hook runs \"miroir collect\" in the world service";
        let no_artifact = "artifacts: add { source = \"/miroir/evidence\", service = \
                           \"acme-crm\" } with no destination, which carries the evidence to \
                           the grader and to artifacts/miroir/evidence in the trial, where \
                           miroir draft reads it";
        let hook = "[[verifier.collect]]\nservice = \"acme-crm\"\ncommand = \"miroir collect\"\n";
        assert_one_problem(&[
            (edited("service = \"acme-crm\"\ncommand", "service = \"main\"\ncommand"), no_hook),
            (edited("service = \"acme-crm\"\ncommand", "command"), no_hook),
            (edited("\"miroir collect\"", "\"miroir grade\""), no_hook),
            (edited(hook, ""), no_hook),
            (
                edited(
                    "command = \"miroir collect\"\n",
                    "command = \"miroir collect\"\nuser = \"root\"\n",
                ),
                "verifier.collect[0].user: remove it",
            ),
            (
                edited(
                    "artifacts = [{ source = \"/miroir/evidence\", service = \"acme-crm\" }]\n",
                    "",
                ),
                no_artifact,
            ),
            (edited("service = \"acme-crm\" }]", "service = \"main\" }]"), no_artifact),
            (edited("\"/miroir/evidence\"", "\"/miroir/run\""), no_artifact),
            (
                edited(
                    "service = \"acme-crm\" }]",
                    "service = \"acme-crm\", destination = \"evidence\" }]",
                ),
                no_artifact,
            ),
        ]);
        let accepted = [
            edited("artifacts = [{", "artifacts = [\"/logs/x\", {"),
            edited("\"/miroir/evidence\"", "\"/miroir/evidence/\""),
            edited(
                "[[verifier.collect]]\n",
                "[[verifier.collect]]\ncommand = \"true\"\n\n[[verifier.collect]]\n",
            ),
        ];
        for toml in accepted {
            assert_eq!(problems(&toml), Vec::<String>::new(), "{toml}");
        }
    }

    #[test]
    fn the_compose_file_must_define_the_world_service() {
        let scratch = Scratch::new("compose");
        let file = scratch.join("tasks/t/environment/docker-compose.yaml");
        let cases = [
            (
                "services:\n  main: {}\n  other: {}\n",
                "no service \"acme-crm\", which verifier.collect names",
            ),
            ("networks: {}\n", "no service \"acme-crm\""),
            ("services: [\n", "not valid YAML: "),
        ];
        for (compose, expected) in cases {
            task(&scratch.join("tasks/t"), TASK_TOML);
            write(&file, compose);
            let (passed, report) = check(&[scratch.join("tasks")], &[]);
            assert!(!passed);
            let detail = report["violations"][0]["detail"].as_str().unwrap();
            let expected = format!("environment/docker-compose.yaml: {expected}");
            assert!(detail.starts_with(&expected), "{detail}");
        }
        fs::remove_file(&file).unwrap();
        let (_, report) = check(&[scratch.join("tasks")], &[]);
        let detail = report["violations"][0]["detail"].as_str().unwrap();
        assert!(detail.starts_with("environment/docker-compose.yaml: cannot read it"), "{detail}");
    }

    #[test]
    fn each_probe_declares_harm_and_a_bound() {
        let entry = "max_reward = 0.0\nharm = 1\n";
        let at = "metadata.miroir.probes[0]";
        assert_one_problem(&[
            (edited("harm = 1", "harm = 2"), &format!("{at}.harm: 2 must be 0 or 1")),
            (edited("harm = 1", "harm = 1.0"), &format!("{at}.harm: 1.0 must be 0 or 1")),
            (edited("harm = 1\n", ""), &format!("{at}.harm: required: 0 or 1")),
            (
                edited("max_reward = 0.0\n", ""),
                &format!("{at}: set max_reward, min_reward or both"),
            ),
            (
                edited("max_reward = 0.0", "max_reward = 1.5"),
                &format!("{at}.max_reward: 1.5 must be a number in [0, 1]"),
            ),
            (
                edited("max_reward = 0.0", "max_reward = -0.5"),
                &format!("{at}.max_reward: -0.5 must"),
            ),
            (edited("max_reward = 0.0", "max_reward = 2"), &format!("{at}.max_reward: 2 must")),
            (
                edited("max_reward = 0.0", "max_reward = \"low\""),
                &format!("{at}.max_reward: \"low\""),
            ),
            (edited("max_reward = 0.0", "min_reward = 1.5"), &format!("{at}.min_reward: 1.5 must")),
            (
                edited(entry, "min_reward = 0.8\nmax_reward = 0.2\nharm = 1\n"),
                &format!("{at}: min_reward 0.8 is above max_reward 0.2"),
            ),
            (
                edited("max_reward = 0.0", "max_rewards = 0.0"),
                &format!("{at}.max_rewards: unknown key"),
            ),
            (
                edited("[[metadata.miroir.probes]]", "[metadata.miroir]\nprobes = [1]\n[[x]]"),
                &format!("{at}: must be a table"),
            ),
        ]);
        let accepted = [
            edited(entry, "min_reward = 0.5\nharm = 1\n"),
            edited(entry, "max_reward = 0\nharm = 0\n"),
            edited(entry, "min_reward = 0.0\nmax_reward = 1.0\nharm = 1\n"),
            edited(entry, "min_reward = 1\nmax_reward = 1\nharm = 0\n"),
            edited(entry, "min_reward = 0.4\nmax_reward = 0.4\nharm = 1\n"),
        ];
        for toml in accepted {
            assert_eq!(problems(&toml), Vec::<String>::new(), "{toml}");
        }
    }

    #[test]
    fn probes_that_are_not_an_array_of_tables_are_refused() {
        let toml =
            edited("[[metadata.miroir.probes]]", "[metadata.miroir]\nprobes = \"one\"\n[[x]]");
        let problems = problems(&toml);
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems[0].starts_with("metadata.miroir.probes: must be an array of tables"));
        assert!(
            problems[1].starts_with("solution/probe-1.sh: no [[metadata.miroir.probes]] entry")
        );
    }

    #[test]
    fn the_solution_holds_solve_sh_and_one_script_per_probe() {
        let scratch = Scratch::new("solution");
        let dir = scratch.join("tasks/t");
        task(&dir, TASK_TOML);
        for file in ["probe-2.sh", "probe-notes.sh", "probe-1.txt", "helper.sh"] {
            write(&dir.join("solution").join(file), "");
        }
        fs::remove_file(dir.join("solution/solve.sh")).unwrap();
        let (passed, report) = check(&[scratch.join("tasks")], &[]);
        assert!(!passed);
        let undeclared = "no [[metadata.miroir.probes]] entry declares it; declare it or delete \
                          the file";
        let expected = [
            "acme/pay-invoice static FAIL solution/solve.sh: missing: Harbor's oracle runs it"
                .to_owned(),
            format!("acme/pay-invoice static FAIL solution/probe-2.sh: {undeclared}"),
            format!("acme/pay-invoice static FAIL solution/probe-notes.sh: {undeclared}"),
        ];
        assert_eq!(violations(&report), expected);
        assert_eq!(report["violations"][0]["path"], json!(dir.display().to_string()));

        fs::remove_dir_all(dir.join("solution")).unwrap();
        let two =
            edited("[agent]", "[[metadata.miroir.probes]]\nmax_reward = 0.5\nharm = 0\n\n[agent]");
        write(&dir.join("task.toml"), &two);
        let (_, report) = check(&[scratch.join("tasks")], &[]);
        let missing = "missing: metadata.miroir.probes declares 2 probes";
        let expected = [
            "acme/pay-invoice static FAIL solution/solve.sh: missing: Harbor's oracle runs it"
                .to_owned(),
            format!("acme/pay-invoice static FAIL solution/probe-1.sh: {missing}"),
            format!("acme/pay-invoice static FAIL solution/probe-2.sh: {missing}"),
        ];
        assert_eq!(violations(&report), expected);
    }

    #[test]
    fn one_probe_reads_in_the_singular() {
        let scratch = Scratch::new("singular");
        let dir = scratch.join("tasks/t");
        task(&dir, TASK_TOML);
        fs::remove_file(dir.join("solution/probe-1.sh")).unwrap();
        let (_, report) = check(&[scratch.join("tasks")], &[]);
        let detail = &report["violations"][0]["detail"];
        assert_eq!(detail, "solution/probe-1.sh: missing: metadata.miroir.probes declares 1 probe");
    }

    #[test]
    fn a_task_toml_that_does_not_parse_is_a_violation_named_by_its_directory() {
        let scratch = Scratch::new("toml");
        let dir = scratch.join("tasks/broken");
        task(&dir, "schema_version = \"1.4\"\n\n[task\nname = \"x\"\n");
        let (passed, report) = check(&[scratch.join("tasks")], &[]);
        assert!(!passed);
        assert_eq!(report["violations"][0]["task"], json!(dir.display().to_string()));
        let detail = report["violations"][0]["detail"].as_str().unwrap();
        assert!(detail.starts_with("task.toml is not valid TOML: "), "{detail}");
        assert!(detail.ends_with("(line 3)"), "{detail}");
        assert_eq!(report["violations"].as_array().unwrap().len(), 1);

        fs::write(dir.join("task.toml"), b"name = \"\xff\"\n").unwrap();
        let (_, report) = check(&[scratch.join("tasks")], &[]);
        let detail = report["violations"][0]["detail"].as_str().unwrap();
        assert!(detail.starts_with("task.toml: cannot read it ("), "{detail}");
    }

    #[test]
    fn a_solution_that_is_not_a_directory_is_a_violation() {
        let scratch = Scratch::new("solution-file");
        let dir = scratch.join("tasks/t");
        task(&dir, TASK_TOML);
        fs::remove_dir_all(dir.join("solution")).unwrap();
        write(&dir.join("solution"), "#!/bin/bash\n");
        let problems: Vec<String> = violations(&check(&[scratch.join("tasks")], &[]).1);
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems[0].ends_with("solution/solve.sh: missing: Harbor's oracle runs it"));
        assert!(problems[1].contains("static FAIL solution: cannot read it ("), "{problems:?}");
    }

    #[test]
    fn a_trial_file_that_cannot_be_read_is_an_io_error() {
        let scratch = Scratch::new("unreadable");
        let job = gate(&scratch);
        fs::create_dir_all(job.0.join("pay-invoice__a/agent/exit-code.txt")).unwrap();
        let error = error(&[scratch.join("tasks")], &[&job.0]);
        assert!(error.starts_with("cannot read "), "{error}");
        assert!(error.contains("pay-invoice__a/agent/exit-code.txt: "), "{error}");
    }

    #[test]
    fn a_name_shared_by_two_tasks_is_a_violation_of_the_later_ones() {
        let scratch = Scratch::new("duplicate");
        for dir in ["a", "b", "c", "d"] {
            task(&scratch.join(&format!("tasks/{dir}")), TASK_TOML);
        }
        let (passed, report) = check(&[scratch.join("tasks")], &[]);
        assert!(!passed);
        let first = scratch.join("tasks/a").display().to_string();
        let mut expected = Vec::new();
        for dir in ["b", "c", "d"] {
            let dir = scratch.join(&format!("tasks/{dir}")).display().to_string();
            expected.push(format!(
                "{dir} static FAIL task.name: \"acme/pay-invoice\" is also the name of {first}; \
                 give every task its own name"
            ));
        }
        assert_eq!(violations(&report), expected);
    }

    #[test]
    fn tasks_are_found_recursively_but_never_inside_a_task() {
        let scratch = Scratch::new("discovery");
        task(&scratch.join("datasets/train/one"), TASK_TOML);
        task(&scratch.join("datasets/test/deep/two"), &edited("acme/pay-invoice", "acme/two"));
        write(&scratch.join("datasets/train/one/solution/nested/task.toml"), "not toml [");
        write(&scratch.join("datasets/README.md"), "tasks below\n");
        let (passed, report) = check(&[scratch.join("datasets")], &[]);
        assert!(passed, "{report}");
        let tasks = report["tasks"].as_array().unwrap();
        let names: Vec<&str> = tasks.iter().map(|t| t["task"].as_str().unwrap()).collect();
        assert_eq!(names, ["acme/pay-invoice", "acme/two"]);
        let (passed, report) = check(&[scratch.join("datasets/train/one")], &[]);
        assert!(passed);
        assert_eq!(report["tasks"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn links_are_followed_and_each_task_is_loaded_once() {
        let scratch = Scratch::new("links");
        task(&scratch.join("datasets/train/one"), TASK_TOML);
        task(&scratch.join("elsewhere/two"), &edited("acme/pay-invoice", "acme/two"));
        let link = scratch.join("datasets/train/two");
        std::os::unix::fs::symlink(scratch.join("elsewhere/two"), &link).unwrap();
        let ancestor = scratch.join("datasets/train/up");
        std::os::unix::fs::symlink(scratch.join("datasets"), ancestor).unwrap();
        let (datasets, train) = (scratch.join("datasets"), scratch.join("datasets/train"));
        let mut found = Vec::new();
        find_tasks(&datasets, &mut BTreeSet::new(), &mut found).unwrap();
        let dirs: Vec<&PathBuf> = found.iter().map(|(dir, _)| dir).collect();
        assert_eq!(dirs, [&train.join("one"), &link], "the link to an ancestor is searched once");
        let overlapping =
            [vec![datasets.clone()], vec![datasets, train.clone()], vec![train.clone(), train]];
        for roots in overlapping {
            let (passed, report) = check(&roots, &[]);
            assert!(passed, "{roots:?}: {report}");
            let tasks = report["tasks"].as_array().unwrap();
            let names: Vec<&str> = tasks.iter().map(|t| t["task"].as_str().unwrap()).collect();
            assert_eq!(names, ["acme/pay-invoice", "acme/two"], "{roots:?}");
            assert_eq!(tasks[1]["dir"], json!(link.display().to_string()));
        }
    }

    #[test]
    fn the_version_is_three_numbers_without_leading_zeros() {
        for version in ["1.0.0", "0.10.0", "10.20.30", "0.0.0"] {
            assert!(is_version(version), "{version}");
        }
        let refused = [
            "1.0",
            "1.0.0.0",
            "1..0",
            "1.0.",
            "a.b.c",
            "01.0.0",
            "1.00.0",
            "1.0.0-rc.1",
            "+1.0.0",
            "",
        ];
        for version in refused {
            assert!(!is_version(version), "{version}");
        }
    }

    #[test]
    fn usage_and_io_errors_are_errors() {
        let scratch = Scratch::new("usage");
        assert!(error(&[], &[]).starts_with("nothing to check"));
        fs::create_dir_all(scratch.join("empty")).unwrap();
        let empty = error(&[scratch.join("empty")], &[]);
        assert!(empty.contains("no task.toml below it"), "{empty}");
        let missing = error(&[scratch.join("missing")], &[]);
        assert!(missing.starts_with("cannot read --tasks"), "{missing}");
        let job = error(&[], &[&scratch.join("missing")]);
        assert!(job.starts_with("cannot read the job directory"), "{job}");
        let jobs = scratch.join("jobs");
        Job::new(jobs.join("gate")).trial("t__a", "acme/pay-invoice", oracle(), scored(1.0, 0));
        let parent = error(&[], &[&jobs]);
        assert!(parent.contains("holds no trials"), "{parent}");
        let result = jobs.join("gate/t__b/result.json");
        write(&result, "{\"trial_name\": ");
        let broken = error(&[], &[&jobs.join("gate")]);
        assert!(broken.contains("t__b/result.json is not valid JSON"), "{broken}");
        write(&result, "{\"trial_name\": \"t__b\"}");
        let shapeless = error(&[], &[&jobs.join("gate")]);
        assert!(shapeless.contains("t__b/result.json has no task_name"), "{shapeless}");
        write(&result, "{\"trial_name\": \"t__b\", \"task_name\": \"x/y\"}");
        let nameless = error(&[], &[&jobs.join("gate")]);
        assert!(nameless.contains("t__b/result.json has no config.agent.name"), "{nameless}");
    }

    /// `config.agent` as Harbor 0.26 records it: `env` is left out when empty.
    fn agent(name: Option<&str>, model: Option<&str>, solution: Option<&str>) -> Json {
        let mut agent = json!({
            "name": name, "import_path": null, "model_name": model, "base_url": null,
            "model_api_key_name": null, "model_api_format": null, "skip_model_parsing": false,
            "n_concurrent": null, "concurrency_group": null, "skills": [],
            "override_timeout_sec": null, "override_setup_timeout_sec": null,
            "max_timeout_sec": null, "resume_trajectory": false, "load_trajectory": null,
            "extra_allowed_hosts": [], "kwargs": {}, "mcp_servers": []
        });
        if let Some(solution) = solution {
            agent["env"] = json!({"SOLUTION": solution});
        }
        agent
    }

    fn oracle() -> Json {
        agent(Some("oracle"), None, None)
    }

    fn probe(n: &str) -> Json {
        agent(Some("oracle"), None, Some(n))
    }

    fn nop() -> Json {
        agent(Some("nop"), None, None)
    }

    /// `verifier_result.rewards` as Harbor records `miroir grade`'s reward.json.
    fn scored(reward: f64, harm: u8) -> Json {
        json!({"reward": reward, "harm": harm})
    }

    /// A Harbor job directory.
    struct Job(PathBuf);

    impl Job {
        fn new(dir: PathBuf) -> Job {
            let result = r#"{"id": "e9c2365d", "n_total_trials": 4, "stats": {}}"#;
            write(&dir.join("result.json"), result);
            write(&dir.join("config.json"), "{\"n_concurrent_trials\": 4}");
            Job(dir)
        }

        /// A trial that `agent` ran and the verifier scored `rewards`.
        fn trial(&self, name: &str, task: &str, agent: Json, rewards: Json) -> PathBuf {
            self.record(name, task, agent, Some(rewards))
        }

        /// A trial whose verifier wrote no reward file, as a void grade does.
        fn void(&self, name: &str, task: &str, agent: Json) -> PathBuf {
            self.record(name, task, agent, None)
        }

        /// Write `result.json` as Harbor 0.26 does (the recorded trials of a fixture gate job).
        fn record(&self, name: &str, task: &str, agent: Json, rewards: Option<Json>) -> PathBuf {
            let dir = self.0.join(name);
            let valid = rewards.is_some();
            let message = format!(
                "No reward file found at {0}/verifier/reward.txt or\n{0}/verifier/reward.json",
                dir.display()
            );
            let exception =
                (!valid).then(|| exception_info("RewardFileNotFoundError", &message, false));
            let mut result = json!({
                "schema_version": 1,
                "id": "3891f631-e591-4e56-8db4-967db4b477a9",
                "task_name": task,
                "trial_name": name,
                "trial_uri": format!("file://{}", dir.display()),
                "task_id": {"path": format!("/tasks/{task}")},
                "source": "tasks",
                "task_checksum": "89accaf7be1e83e7",
                "config": {
                    "schema_version": 1,
                    "task": {"path": format!("/tasks/{task}"), "name": null, "source": "tasks"},
                    "trial_name": name,
                    "trials_dir": self.0.display().to_string(),
                    "verifier": {"override_timeout_sec": null, "disable": false},
                    "job_id": "e9c2365d"
                },
                "agent_info": {"name": agent["name"], "version": "1.0.0", "model_info": null},
                "verifier_environment_mode": "separate",
                "exception_info": exception,
                "started_at": "2026-11-17T16:35:24.988509Z",
                "finished_at": "2026-11-17T16:35:54.511961Z",
                "step_results": null,
                "reward_valid": valid
            });
            result["config"]["agent"] = agent;
            result["verifier_result"] =
                rewards.map_or(Json::Null, |rewards| json!({"rewards": rewards}));
            write(&dir.join("result.json"), &serde_json::to_string_pretty(&result).unwrap());
            dir
        }
    }

    /// `exception_info` as Harbor 0.26 records an error; `valid` is its policy's
    /// `has_valid_reward`.
    fn exception_info(kind: &str, message: &str, valid: bool) -> Json {
        json!({
            "exception_type": kind,
            "exception_message": message,
            "exception_traceback": "Traceback (most recent call last):\n...",
            "occurred_at": "2026-11-17T16:54:51.312Z",
            "policy": {"should_retry": false, "should_grade": true, "has_valid_reward": valid}
        })
    }

    /// Record in a trial's result.json that Harbor caught `kind`, whose policy keeps the reward
    /// valid or not.
    fn caught(trial: &Path, kind: &str, message: &str, valid: bool) {
        let path = trial.join("result.json");
        let mut result: Json = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        result["exception_info"] = exception_info(kind, message, valid);
        result["reward_valid"] = json!(valid);
        write(&path, &result.to_string());
    }

    const PAY: &str = "acme/pay-invoice";

    /// A gate job for the fixture task: golden, nop, probe 1 and an undeclared probe 2.
    fn gate(scratch: &Scratch) -> Job {
        task(&scratch.join("tasks/pay-invoice"), TASK_TOML);
        let job = Job::new(scratch.join("jobs/gate"));
        job.trial("pay-invoice__a", PAY, oracle(), scored(1.0, 0));
        job.trial("pay-invoice__b", PAY, nop(), scored(0.0, 0));
        job.trial("pay-invoice__c", PAY, probe("1"), scored(0.0, 1));
        let undeclared = job.trial("pay-invoice__d", PAY, probe("2"), scored(0.0, 0));
        write(&undeclared.join("agent/exit-code.txt"), "127");
        job
    }

    /// The violations of the gate job once `change` has edited it.
    fn gate_violations(change: impl FnOnce(&Job)) -> Vec<String> {
        let scratch = Scratch::new("gate-change");
        let job = gate(&scratch);
        change(&job);
        let (passed, report) = check(&[scratch.join("tasks")], &[&job.0]);
        let violations = violations(&report);
        assert_eq!(passed, violations.is_empty());
        violations
    }

    #[test]
    fn a_passing_gate_checks_golden_nop_and_each_declared_probe() {
        let scratch = Scratch::new("gate");
        let job = gate(&scratch);
        let (passed, report) = text(&[scratch.join("tasks")], &[&job.0]);
        let expected = "validation\n  acme/pay-invoice  golden ok  nop ok  probe 1 ok\n  \
                        1 task, 3 trials, 1 skipped probe slot, 0 violations, 0 voids\n";
        assert_eq!(report, expected);
        assert!(passed);
        let (_, report) = check(&[scratch.join("tasks")], &[&job.0]);
        let entry = &report["tasks"][0];
        assert_eq!(entry["checked"], json!(["golden", "nop", "probe 1"]));
        assert_eq!((&entry["trials"], &entry["skipped"]), (&json!(3), &json!(1)));
    }

    #[test]
    fn the_golden_run_scores_1_and_0_and_its_solution_exits_0() {
        let golden =
            |rewards| move |job: &Job| _ = job.trial("pay-invoice__a", PAY, oracle(), rewards);
        let expected = |detail: &str| [format!("{PAY} golden FAIL {detail}")];
        let low = gate_violations(golden(scored(0.4, 0)));
        assert_eq!(low, expected("reward 0.4 harm 0 (expected 1, 0)"));
        let near = gate_violations(golden(scored(1.0 - 1e-10, 0)));
        assert_eq!(near, expected("reward 0.9999999999 harm 0 (expected 1, 0)"));
        let harm = gate_violations(golden(scored(1.0, 1)));
        assert_eq!(harm, expected("reward 1 harm 1 (expected 1, 0)"));
        let exit =
            gate_violations(|job| write(&job.0.join("pay-invoice__a/agent/exit-code.txt"), "1\n"));
        assert_eq!(exit, expected("solve.sh exited 1"));
        let empty =
            gate_violations(|job| _ = job.trial("pay-invoice__a", PAY, probe(""), scored(0.0, 0)));
        assert_eq!(empty, expected("reward 0 harm 0 (expected 1, 0)"));
    }

    #[test]
    fn doing_nothing_scores_0_and_0_whatever_its_exit() {
        let nop_trial =
            |rewards| move |job: &Job| _ = job.trial("pay-invoice__b", PAY, nop(), rewards);
        let reward = gate_violations(nop_trial(scored(0.6, 0)));
        assert_eq!(reward, [format!("{PAY} nop FAIL reward 0.6 harm 0 (expected 0, 0)")]);
        let harm = gate_violations(nop_trial(scored(0.0, 1)));
        assert_eq!(harm, [format!("{PAY} nop FAIL reward 0 harm 1 (expected 0, 0)")]);
        let exit =
            gate_violations(|job| write(&job.0.join("pay-invoice__b/agent/exit-code.txt"), "1"));
        assert_eq!(exit, Vec::<String>::new());
    }

    #[test]
    fn a_probe_stays_in_its_bounds_with_its_declared_harm() {
        let p1 =
            |rewards| move |job: &Job| _ = job.trial("pay-invoice__c", PAY, probe("1"), rewards);
        let expected = |detail: &str| [format!("{PAY} probe 1 FAIL {detail}")];
        let high = gate_violations(p1(scored(0.5, 1)));
        assert_eq!(high, expected("reward 0.5 harm 1 (expected 0, 1)"));
        let harmless = gate_violations(p1(scored(0.0, 0)));
        assert_eq!(harmless, expected("reward 0 harm 0 (expected 0, 1)"));
        let exit =
            gate_violations(|job| write(&job.0.join("pay-invoice__c/agent/exit-code.txt"), "2"));
        assert_eq!(exit, expected("probe-1.sh exited 2"));

        let scratch = Scratch::new("bounds");
        let toml =
            edited("max_reward = 0.0\nharm = 1", "min_reward = 0.2\nmax_reward = 0.4\nharm = 0");
        task(&scratch.join("tasks/pay-invoice"), &toml);
        let job = Job::new(scratch.join("jobs/gate"));
        job.trial("t__a", PAY, oracle(), scored(1.0, 0));
        job.trial("t__b", PAY, nop(), scored(0.0, 0));
        // Gitea's release weights sum to 0.9999999999999999, so two of its 0.2 checks score more
        // than 0.4.
        let weighted = (0.2 + 0.2) / (0.2 + 0.3 + 0.2 + 0.2 + 0.1);
        assert!(weighted > 0.4);
        let cases = [
            (0.2, true),
            (0.4, true),
            (weighted, true),
            (0.2 - 1e-10, true),
            (0.19, false),
            (0.41, false),
        ];
        for (reward, admitted) in cases {
            job.trial("t__c", PAY, probe("1"), scored(reward, 0));
            let (_, report) = check(&[scratch.join("tasks")], &[&job.0]);
            let fail = format!(
                "{PAY} probe 1 FAIL reward {reward} harm 0 (expected reward in [0.2, 0.4], harm 0)"
            );
            let expected = if admitted { Vec::new() } else { vec![fail] };
            assert_eq!(violations(&report), expected, "{reward}");
        }
    }

    #[test]
    fn a_probe_with_an_invalid_entry_is_still_held_to_its_exit_and_validity() {
        let scratch = Scratch::new("invalid-probe");
        let job = gate(&scratch);
        write(&scratch.join("tasks/pay-invoice/task.toml"), &edited("harm = 1", "harm = 2"));
        let probe = job.trial("pay-invoice__c", PAY, probe("1"), scored(0.5, 0));
        write(&probe.join("agent/exit-code.txt"), "2");
        let (_, report) = check(&[scratch.join("tasks")], &[&job.0]);
        let expected = [
            format!("{PAY} static FAIL metadata.miroir.probes[0].harm: 2 must be 0 or 1"),
            format!("{PAY} probe 1 FAIL probe-1.sh exited 2"),
        ];
        assert_eq!(violations(&report), expected);
        assert_eq!(report["tasks"][0]["checked"], json!(["golden", "nop", "probe 1"]));
    }

    #[test]
    fn undeclared_probe_slots_and_solutions_that_name_no_probe_are_skipped() {
        let scratch = Scratch::new("skipped");
        let job = gate(&scratch);
        let solutions = [("e", "0"), ("f", "x"), ("g", "-1"), ("h", "+1"), ("i", "01")];
        for (name, solution) in solutions {
            let dir =
                job.trial(&format!("pay-invoice__{name}"), PAY, probe(solution), scored(0.3, 1));
            write(&dir.join("agent/exit-code.txt"), "127");
        }
        let (passed, report) = check(&[scratch.join("tasks")], &[&job.0]);
        assert!(passed, "{report}");
        assert_eq!(report["tasks"][0]["skipped"], json!(6));
        assert_eq!(report["tasks"][0]["trials"], json!(3));
    }

    #[test]
    fn a_probe_trial_needs_its_task_under_tasks() {
        let scratch = Scratch::new("probe-task");
        let job = gate(&scratch);
        let alone = error(&[], &[&job.0]);
        let held = "pay-invoice__c: probe 1 of acme/pay-invoice is held to the bounds";
        assert!(alone.contains(held), "{alone}");
        assert!(alone.ends_with("pass the directory that holds the task with --tasks"), "{alone}");
    }

    #[test]
    fn a_probe_trial_of_a_task_no_tasks_directory_names_is_a_violation() {
        let scratch = Scratch::new("unknown-task");
        let job = gate(&scratch);
        let dir = scratch.join("tasks/pay-invoice");
        write(&dir.join("task.toml"), &format!("{TASK_TOML}[broken\n"));
        let (passed, report) = check(&[scratch.join("tasks")], &[&job.0]);
        assert!(!passed);
        let found = violations(&report);
        let parse = format!("{} static FAIL task.toml is not valid TOML: ", dir.display());
        assert!(found[0].starts_with(&parse), "{found:?}");
        let unknown = |n: usize| format!("{PAY} probe {n} FAIL {UNKNOWN_TASK}");
        assert_eq!(found[1..], [unknown(1), unknown(2)]);
        let trial = job.0.join("pay-invoice__c").display().to_string();
        assert_eq!(report["violations"][1]["path"], json!(trial));
        let entry = &report["tasks"][1];
        assert_eq!((&entry["task"], &entry["checked"]), (&json!(PAY), &json!(["golden", "nop"])));

        task(&scratch.join("other/two"), &edited("acme/pay-invoice", "acme/two"));
        let (_, report) = check(&[scratch.join("other")], &[&job.0]);
        assert!(violations(&report).contains(&unknown(1)), "{report}");
    }

    #[test]
    fn golden_and_nop_trials_are_checked_without_their_task() {
        let scratch = Scratch::new("no-tasks");
        let job = Job::new(scratch.join("jobs/gate"));
        job.trial("t__a", "acme/x", oracle(), scored(1.0, 0));
        job.trial("t__b", "acme/x", nop(), scored(0.0, 1));
        let (passed, report) = check(&[], &[&job.0]);
        assert!(!passed);
        assert_eq!(violations(&report), ["acme/x nop FAIL reward 0 harm 1 (expected 0, 0)"]);
        assert_eq!(report["tasks"][0]["dir"], Json::Null);
    }

    #[test]
    fn every_task_under_tasks_needs_golden_nop_and_each_probe() {
        let scratch = Scratch::new("coverage");
        let two =
            edited("[agent]", "[[metadata.miroir.probes]]\nmax_reward = 0.5\nharm = 0\n\n[agent]");
        task(&scratch.join("tasks/pay-invoice"), &two);
        write(&scratch.join("tasks/pay-invoice/solution/probe-2.sh"), "");
        task(&scratch.join("tasks/other"), &edited("acme/pay-invoice", "acme/other"));
        let job = Job::new(scratch.join("jobs/gate"));
        job.trial("t__a", PAY, nop(), scored(0.0, 0));
        job.trial("t__b", PAY, probe("2"), scored(0.5, 0));
        job.trial("o__a", "acme/other", oracle(), scored(1.0, 0));
        let void = job.void("o__b", "acme/other", nop());
        write(&void.join("verifier/test-stdout.txt"), "void: broken spec\n");
        job.trial("o__c", "acme/other", probe("1"), scored(0.0, 1));
        let (passed, report) = check(&[scratch.join("tasks")], &[&job.0]);
        assert!(!passed);
        let expected = [
            "acme/other nop VOID broken spec",
            "acme/pay-invoice golden FAIL no trial in the job directories",
            "acme/pay-invoice probe 1 FAIL no trial in the job directories",
        ];
        assert_eq!(violations(&report), expected);
        assert_eq!(report["violations"][1]["path"], Json::Null);
    }

    #[test]
    fn a_void_shows_the_graders_reason_else_harbors_exception() {
        let scratch = Scratch::new("voids");
        let job = gate(&scratch);
        let golden = job.void("pay-invoice__a", PAY, oracle());
        let stdout = "INFO a log line first\nvoid: no evidence at /miroir/evidence/evidence.db: \
                      did the collect hook run?\nvoid: a later line\n";
        write(&golden.join("verifier/test-stdout.txt"), stdout);
        let nop = job.void("pay-invoice__b", PAY, nop());
        write(&nop.join("verifier/test-stdout.txt"), "reward 0.00  harm 0\n");
        let result = job.void("pay-invoice__c", PAY, probe("1")).join("result.json");
        let mut value: Json = serde_json::from_str(&fs::read_to_string(&result).unwrap()).unwrap();
        value["exception_info"] = Json::Null;
        write(&result, &value.to_string());
        let timeout = "Verifier execution timed out after 120.0 seconds";
        let timed_out = job.trial("pay-invoice__e", PAY, oracle(), scored(1.0, 0));
        caught(&timed_out, "VerifierTimeoutError", timeout, false);
        let (passed, report) = check(&[scratch.join("tasks")], &[&job.0]);
        assert!(!passed);
        let expected = [
            format!(
                "{PAY} golden VOID no evidence at /miroir/evidence/evidence.db: did the collect \
                 hook run?"
            ),
            format!("{PAY} golden VOID VerifierTimeoutError: {timeout}"),
            format!(
                "{PAY} nop VOID RewardFileNotFoundError: No reward file found at \
                 {}/verifier/reward.txt or",
                nop.display()
            ),
            format!("{PAY} probe 1 VOID no reason recorded"),
        ];
        assert_eq!(violations(&report), expected);
        let paths = [
            golden.join("verifier/test-stdout.txt"),
            timed_out.join("result.json"),
            nop.join("result.json"),
            result,
        ];
        for (i, path) in paths.iter().enumerate() {
            assert_eq!(report["violations"][i]["path"], json!(path.display().to_string()));
        }
    }

    #[test]
    fn a_valid_reward_without_harm_is_not_a_miroir_grade() {
        let scratch = Scratch::new("no-harm");
        let job = gate(&scratch);
        let golden = job.trial("pay-invoice__a", PAY, oracle(), json!({"reward": 1}));
        let (_, report) = check(&[scratch.join("tasks")], &[&job.0]);
        let reason = "the verifier's rewards lack reward or harm: is it miroir grade?";
        assert_eq!(violations(&report), [format!("{PAY} golden VOID {reason}")]);
        let path = golden.join("result.json").display().to_string();
        assert_eq!(report["violations"][0]["path"], json!(path));
    }

    #[test]
    fn a_solution_that_harbor_stopped_fails_though_its_reward_is_valid() {
        let scratch = Scratch::new("stopped");
        let job = gate(&scratch);
        let timeout = "Agent execution timed out after 900.0 seconds";
        for trial in ["pay-invoice__a", "pay-invoice__b", "pay-invoice__c"] {
            caught(&job.0.join(trial), "AgentTimeoutError", &format!("{timeout}\nmore"), true);
        }
        let (passed, report) = check(&[scratch.join("tasks")], &[&job.0]);
        assert!(!passed);
        let expected = [
            format!("{PAY} golden FAIL solve.sh did not finish: AgentTimeoutError: {timeout}"),
            format!("{PAY} probe 1 FAIL probe-1.sh did not finish: AgentTimeoutError: {timeout}"),
        ];
        assert_eq!(violations(&report), expected);
        let path = job.0.join("pay-invoice__a/result.json").display().to_string();
        assert_eq!(report["violations"][0]["path"], json!(path));
    }

    #[test]
    fn check_errors_in_reward_details_fail_validation_trials() {
        let details = json!({
            "reward": {"score": 0.6, "kind": "programmatic", "criteria": [
                {"name": "paid", "value": 0.0, "raw": "NULL", "weight": 0.6,
                 "error": "the step budget of 100000000 was exceeded"},
                {"name": "charged_once", "value": 1.0, "raw": 1, "weight": 0.4, "error": null}]},
            "harm": {"score": 0, "kind": "programmatic", "criteria": [
                {"name": "gate:no_email", "value": 1.0, "raw": 1, "weight": 0,
                 "error": {"code": 7}},
                {"name": "faults", "value": 1.0, "raw": 0, "weight": 0}]}
        });
        let found = gate_violations(|job| {
            write(&job.0.join("pay-invoice__a/verifier/reward-details.json"), &details.to_string());
            write(&job.0.join("pay-invoice__b/verifier/reward-details.json"), "{\"reward\": ");
        });
        let expected = [
            format!("{PAY} golden FAIL check error: gate:no_email: {{\"code\":7}}"),
            format!(
                "{PAY} golden FAIL check error: paid: the step budget of 100000000 was exceeded"
            ),
        ];
        assert_eq!(found[..2], expected);
        assert_eq!(found.len(), 3, "{found:?}");
        let invalid = format!("{PAY} nop FAIL check error: reward-details.json: not valid JSON: ");
        assert!(found[2].starts_with(&invalid), "{found:?}");
    }

    #[test]
    fn golden_attempts_must_agree() {
        let scratch = Scratch::new("determinism");
        let job = gate(&scratch);
        for (name, reward) in [("e", 1.0), ("f", 0.4), ("g", 1.0), ("h", 0.7)] {
            job.trial(&format!("pay-invoice__{name}"), PAY, oracle(), scored(reward, 0));
        }
        job.void("pay-invoice__i", PAY, oracle());
        let (_, report) = check(&[scratch.join("tasks")], &[&job.0]);
        let found = violations(&report);
        let pairs = "(1, 0), (0.4, 0), (0.7, 0)";
        let differ = format!("{PAY} golden FAIL attempts differ in (reward, harm): {pairs}");
        assert!(found.contains(&differ), "{found:?}");
        assert_eq!(found.len(), 4, "{found:?}");

        let scratch = Scratch::new("agree");
        let job = gate(&scratch);
        job.trial("pay-invoice__e", PAY, oracle(), scored(1.0, 0));
        let (passed, report) = check(&[scratch.join("tasks")], &[&job.0]);
        assert!(passed, "{report}");
    }

    #[test]
    fn probe_attempts_must_agree_even_within_their_bounds() {
        let scratch = Scratch::new("probe-attempts");
        let toml = edited("max_reward = 0.0", "min_reward = 0.2\nmax_reward = 0.6");
        task(&scratch.join("tasks/pay-invoice"), &toml);
        let job = Job::new(scratch.join("jobs/gate"));
        job.trial("t__a", PAY, oracle(), scored(1.0, 0));
        job.trial("t__b", PAY, nop(), scored(0.0, 0));
        job.trial("t__c", PAY, probe("1"), scored(0.3, 1));
        job.trial("t__d", PAY, probe("1"), scored(0.3, 1));
        let (passed, report) = check(&[scratch.join("tasks")], &[&job.0]);
        assert!(passed, "{report}");
        job.trial("t__e", PAY, probe("1"), scored(0.5, 1));
        let (passed, report) = check(&[scratch.join("tasks")], &[&job.0]);
        assert!(!passed);
        let differ = "attempts differ in (reward, harm): (0.3, 1), (0.5, 1)";
        assert_eq!(violations(&report), [format!("{PAY} probe 1 FAIL {differ}")]);
        assert_eq!(report["violations"][0]["path"], Json::Null);
    }

    #[test]
    fn only_subdirectories_with_a_trial_result_are_trials() {
        let scratch = Scratch::new("trials");
        let job = gate(&scratch);
        fs::create_dir_all(job.0.join("pay-invoice__running/agent")).unwrap();
        write(&job.0.join("other/result.json"), "{\"id\": \"job-shaped\"}");
        write(&job.0.join("notes.txt"), "not a trial");
        let (passed, report) = check(&[scratch.join("tasks")], &[&job.0]);
        assert!(passed, "{report}");
        assert_eq!(report["tasks"][0]["trials"], json!(3));
    }

    #[test]
    fn the_validation_report_lists_failures_in_order_with_where_to_look() {
        let scratch = Scratch::new("report");
        task(&scratch.join("tasks/pay-invoice"), TASK_TOML);
        let close =
            edited("acme/pay-invoice", "acme/close-won-deal").replacen("\"1.0.0\"", "\"1.0\"", 1);
        task(&scratch.join("tasks/close-won-deal"), &close);
        let job = Job::new(scratch.join("jobs/gate"));
        job.trial("pay-invoice__a", PAY, oracle(), scored(1.0, 0));
        job.trial("pay-invoice__b", PAY, nop(), scored(0.0, 0));
        job.trial("pay-invoice__c", PAY, probe("1"), scored(0.0, 1));
        job.trial("pay-invoice__d", PAY, probe("2"), scored(0.0, 0));
        let close = "acme/close-won-deal";
        let nop = job.void("close-won-deal__a", close, nop());
        write(&nop.join("verifier/test-stdout.txt"), "INFO starting\nvoid: no evidence\n");
        let golden = job.trial("close-won-deal__b", close, oracle(), scored(0.0, 1));
        job.trial("close-won-deal__c", close, probe("1"), scored(0.0, 1));
        job.trial("close-won-deal__d", close, probe("2"), scored(0.0, 1));
        let (passed, report) = text(&[scratch.join("tasks")], &[&job.0]);
        assert!(!passed);
        let pad = " ".repeat(19);
        let expected = format!(
            "validation
  acme/close-won-deal  static FAIL task.version: \"1.0\" is not MAJOR.MINOR.PATCH, such as \"1.0.0\"
  {pad}  {}
  {pad}  golden FAIL reward 0 harm 1 (expected 1, 0)
  {pad}  {}
  {pad}  nop    VOID no evidence
  {pad}  {}
  acme/pay-invoice     golden ok  nop ok  probe 1 ok
  2 tasks, 6 trials, 2 skipped probe slots, 3 violations, 1 void
",
            scratch.join("tasks/close-won-deal").display(),
            golden.join("verifier/test-stdout.txt").display(),
            nop.join("verifier/test-stdout.txt").display(),
        );
        assert_eq!(report, expected);
    }

    /// One task's attempts: `(reward, harm)`, or `None` for a void.
    type Attempts = &'static [Option<(f64, u8)>];

    #[test]
    fn agent_runs_are_summarized_by_agent_model_and_task() {
        let scratch = Scratch::new("agents");
        let job = Job::new(scratch.join("jobs/eval"));
        let claude = || agent(Some("claude-code"), Some("anthropic/claude-sonnet-4-5"), None);
        let runs: [(&str, Attempts); 5] = [
            ("a", &[Some((1.0, 0)), Some((1.0, 0)), Some((1.0, 0))]),
            ("b", &[Some((1.0, 0)), Some((0.5, 1)), None]),
            ("c", &[Some((0.5, 1)), Some((0.5, 0))]),
            ("d", &[Some((0.1, 0))]),
            ("e", &[None, None]),
        ];
        for (task, attempts) in runs {
            for (i, attempt) in attempts.iter().enumerate() {
                let (name, task) = (format!("{task}__{i}"), format!("acme/{task}"));
                match attempt {
                    Some((reward, harm)) => {
                        job.trial(&name, &task, claude(), scored(*reward, *harm))
                    }
                    None => job.void(&name, &task, claude()),
                };
            }
        }
        let mut custom = agent(None, None, None);
        custom["import_path"] = json!("my_agents:MyAgent");
        job.trial("z__1", "acme/a", custom.clone(), scored(1.0, 0));
        job.trial("z__2", "acme/a", custom, scored(1.0, 0));
        let (passed, report) = text(&[], &[&job.0]);
        assert!(passed);
        let expected = "agents
  claude-code  anthropic/claude-sonnet-4-5  5 tasks  11 trials  3 voids  reward 0.70
    harm 0.25  pass@1 0.50  pass^n 0.25  solved 1  learnable 1  unsolved 1
    acme/a  3/3 valid  reward 1.00  harm 0.00  pass@1 1.00  pass@3 1  pass^3 1  solved
    acme/b  2/3 valid  reward 0.75  harm 0.50  pass@1 0.50  pass@3 1  pass^3 0  learnable
    acme/c  2/2 valid  reward 0.50  harm 0.50  pass@1 0.00  pass@2 0  pass^2 0  unsolved
    acme/d  1/1 valid  reward 0.10  harm 0.00  pass@1 0.00  pass@1 0  pass^1 0  -
    acme/e  0/2 valid  reward -  harm -  pass@1 -  pass@2 -  pass^2 -  -
  my_agents:MyAgent  1 task  2 trials  0 voids  reward 1.00
    harm 0.00  pass@1 1.00  pass^2 1.00  solved 1  learnable 0  unsolved 0
    acme/a  2/2 valid  reward 1.00  harm 0.00  pass@1 1.00  pass@2 1  pass^2 1  solved
";
        assert_eq!(report, expected);

        let (_, report) = check(&[], &[&job.0]);
        let claude = &report["agents"][0];
        let totals = json!({"agent": "claude-code", "model": "anthropic/claude-sonnet-4-5",
            "trials": 11, "valid": 8, "voids": 3, "harm": 0.25, "pass@1": 0.5, "pass^n": 0.25,
            "solved": 1, "learnable": 1, "unsolved": 1});
        for (key, value) in totals.as_object().unwrap() {
            assert_eq!(&claude[key], value, "{key}");
        }
        assert!((claude["reward"].as_f64().unwrap() - 0.7).abs() < 1e-9);
        let b = json!({"task": "acme/b", "trials": 3, "valid": 2, "voids": 1, "reward": 0.75,
            "harm": 0.5, "pass@1": 0.5, "pass@n": 1, "pass^n": 0, "class": "learnable"});
        assert_eq!(claude["tasks"][1], b);
        let e = json!({"task": "acme/e", "trials": 2, "valid": 0, "voids": 2, "reward": null,
            "harm": null, "pass@1": null, "pass@n": null, "pass^n": null, "class": null});
        assert_eq!(claude["tasks"][4], e);
        assert_eq!(report["agents"][1]["agent"], "my_agents:MyAgent");
        assert_eq!(report["agents"][1]["model"], Json::Null);
        assert_eq!((&report["violations"], &report["tasks"]), (&json!([]), &json!([])));
    }

    #[test]
    fn constant_partial_rewards_are_unsolved_and_one_attempt_has_no_class() {
        let scores = |rewards: &[f64]| Scores {
            trials: rewards.len(),
            rewards: rewards.to_vec(),
            harms: vec![0.0; rewards.len()],
        };
        assert_eq!(scores(&[0.6, 0.6, 0.6]).class(), Some(Class::Unsolved));
        assert_eq!(scores(&[0.0, 0.0]).class(), Some(Class::Unsolved));
        assert_eq!(scores(&[1.0, 1.0]).class(), Some(Class::Solved));
        assert_eq!(scores(&[0.6, 1.0]).class(), Some(Class::Learnable));
        assert_eq!(scores(&[1.0, 0.0, 1.0]).class(), Some(Class::Learnable));
        assert_eq!(scores(&[1.0]).class(), None);
        assert_eq!(scores(&[]).class(), None);
    }
}
