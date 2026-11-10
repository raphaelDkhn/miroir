//! Task files, `/miroir/tasks/*.yaml`: their types, answer forms and validation.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::clock::{self, Event, MAX_CLOCK_MINUTES, WAIT};
use crate::coworker::{self, COWORKER_REPLY, Coworker};
use crate::manifest::{Manifest, ManifestError, Pick, content_hash, ensure, error, parse_file};

/// Splits a task can be in.
pub const SPLITS: [&str; 3] = ["train", "validation", "test"];
/// Tools miroir itself provides, whose names an MCP server can't reuse.
pub const TOOLS: [&str; 6] =
    ["api_request", "api_docs", "submit", "run_code", WAIT, COWORKER_REPLY];

/// A task: an instruction, the checks that grade it, and a golden run that passes them.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Task {
    /// Defaults to the file name.
    #[serde(default)]
    pub id: String,
    #[serde(default = "default_split")]
    pub split: String,
    /// World it resets to; defaults to the only one.
    #[serde(default)]
    pub world: String,
    /// Persona the agent acts as; defaults to the only one.
    pub persona: Option<String>,
    pub instruction: String,
    /// Normal form a submitted answer takes.
    pub answer: Option<Answer>,
    /// Colleagues a model plays, whom the agent can ask.
    #[serde(default)]
    pub coworkers: Vec<Coworker>,
    /// Requests that arrive at clock minutes while the agent waits.
    #[serde(default)]
    pub events: Vec<Event>,
    /// Tool calls the episode allows; 50 by default.
    #[serde(default = "default_max_steps")]
    pub max_steps: u32,
    /// SQL run on the only database at each reset.
    pub setup: Option<String>,
    /// Weighted checks and gates that grade the end state.
    pub checks: Vec<Check>,
    /// The changes the task may make; any other is harm.
    pub allowed_changes: Option<Vec<AllowedChange>>,
    /// Allow every change, in place of `allowed_changes`.
    #[serde(default)]
    pub any_change: bool,
    /// Reward for harm, from -1 to 0; 0 by default.
    #[serde(default)]
    pub harm_reward: f64,
    /// Most an immediate empty submit may earn, which `check` tests.
    #[serde(default)]
    pub null_max: f64,
    /// A reference run, ending with submit, that must earn 1.
    pub golden: Vec<GoldenStep>,
    /// Runs whose reward must stay within bounds, which `check` replays.
    #[serde(default)]
    pub probes: Vec<Probe>,
    /// Hash of the task file.
    #[serde(skip)]
    pub hash: String,
}

impl Task {
    /// Whether time passes in the task: it has events, or checks scored `by` a minute.
    pub fn has_clock(&self) -> bool {
        !self.events.is_empty() || self.checks.iter().any(|check| check.by.is_some())
    }

    /// Whether the golden run submits a non-empty answer.
    pub fn asks_for_answer(&self) -> bool {
        let answer = self.golden.last().and_then(|step| step.arguments.get("answer"));
        answer.and_then(Value::as_str).is_some_and(|answer| !answer.trim().is_empty())
    }
}

/// Normal form of a submitted answer, so equal answers compare equal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Answer {
    Integer,
    Ids,
    Date,
}

impl Answer {
    /// Normalize an answer; a date moves back `days`, to how the task file writes it.
    pub fn normalize(self, answer: &str, days: i64) -> Result<String, String> {
        let answer = answer.trim();
        if answer.is_empty() {
            return Ok(String::new());
        }
        let (normal, form) = match self {
            Answer::Integer => {
                (answer.parse::<i64>().ok().map(|n| n.to_string()), "a whole number, such as 42")
            }
            Answer::Ids => (normal_ids(answer), "ids separated by commas, such as 12, 15, 18"),
            Answer::Date => (normal_date(answer, days), "a date as YYYY-MM-DD, such as 2026-11-04"),
        };
        normal.ok_or_else(|| format!("the answer must be {form}"))
    }
}

/// Move a `YYYY-MM-DD` date by whole days.
pub fn move_date(date: &str, days: i64) -> Option<String> {
    let date: jiff::civil::Date = date.parse().ok()?;
    let span = jiff::Span::new().try_days(days).ok()?;
    Some(date.checked_add(span).ok()?.to_string())
}

fn normal_ids(answer: &str) -> Option<String> {
    let mut ids: Vec<&str> =
        answer.split(|c: char| c == ',' || c.is_whitespace()).filter(|id| !id.is_empty()).collect();
    let id_char = |c: char| c.is_alphanumeric() || "-_.:/@".contains(c);
    if !ids.iter().all(|id| id.chars().all(id_char)) {
        return None;
    }
    if ids.iter().all(|id| id.parse::<u64>().is_ok()) {
        ids.sort_by_key(|id| (id.parse::<u64>().unwrap_or_default(), *id));
    } else {
        ids.sort_unstable();
    }
    ids.dedup();
    Some(ids.join(","))
}

fn normal_date(answer: &str, days: i64) -> Option<String> {
    let shaped =
        |(i, b): (usize, u8)| if i == 4 || i == 7 { b == b'-' } else { b.is_ascii_digit() };
    if answer.len() != 10 || !answer.bytes().enumerate().all(shaped) {
        return None;
    }
    move_date(answer, days.checked_neg()?)
}

/// A run whose reward must stay within `min_reward` and `max_reward`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Probe {
    pub name: String,
    pub steps: Vec<GoldenStep>,
    pub max_reward: Option<f64>,
    pub min_reward: Option<f64>,
}

/// A check, scored 0 to 1.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Check {
    pub name: String,
    /// A scalar query that returns a boolean or a number.
    pub sql: Option<String>,
    /// A command that scores what it prints on exit 0 (1 if nothing), and 0 otherwise; it
    /// gets `MIROIR_EPISODE_DB` and `MIROIR_DATABASE_URL`.
    pub command: Option<Vec<String>>,
    /// App whose database it reads, when several have one.
    pub app: Option<String>,
    /// `episode` runs `sql` on the episode database instead.
    pub db: Option<CheckDb>,
    /// Share of the reward; the weights sum to 1.
    pub weight: Option<f64>,
    /// Must score 1, or the episode earns `harm_reward`.
    #[serde(default)]
    pub gate: bool,
    /// Clock minute it is scored at, as a milestone.
    pub by: Option<u32>,
}

/// The episode database: actions, effects, net changes and the answer.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CheckDb {
    Episode,
}

/// A change the task may make.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AllowedChange {
    pub app: Option<String>,
    pub table: String,
    pub op: ChangeOp,
    /// Columns an update may change; any when unset.
    pub columns: Option<Vec<String>>,
}

/// Kind of a net change.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ChangeOp {
    Insert,
    Update,
    Delete,
    Truncate,
    Schema,
}

/// A tool call of a golden run or probe.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoldenStep {
    pub tool: String,
    #[serde(default)]
    pub arguments: Map<String, Value>,
}

fn default_split() -> String {
    "train".into()
}
fn default_max_steps() -> u32 {
    50
}

impl ChangeOp {
    pub fn as_str(self) -> &'static str {
        match self {
            ChangeOp::Insert => "insert",
            ChangeOp::Update => "update",
            ChangeOp::Delete => "delete",
            ChangeOp::Truncate => "truncate",
            ChangeOp::Schema => "schema",
        }
    }
}

impl Manifest {
    /// Load and validate a directory's tasks, sorted by world then id, with the probes in
    /// `probes/`.
    pub fn load_tasks(&self, dir: &Path) -> Result<Vec<Task>, ManifestError> {
        let mut tasks: Vec<Task> = Vec::new();
        for path in yaml_files(dir)? {
            let mut task: Task = parse_file(&path)?;
            let text = std::fs::read(&path).map_err(|e| error(&path, e.to_string()))?;
            task.hash = content_hash(&text);
            if task.id.is_empty() {
                task.id = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
            }
            self.fill_defaults(&mut task);
            self.validate_task(&task).map_err(|message| error(&path, message))?;
            if let Some(other) = tasks.iter().find(|t| t.id == task.id) {
                return Err(error(&path, format!("task id '{}' is used twice", other.id)));
            }
            tasks.push(task);
        }
        ensure(!tasks.is_empty(), "no task files (*.yaml, *.yml)").map_err(|m| error(dir, m))?;
        let rank = |task: &Task| self.worlds.iter().position(|world| *world == task.world);
        tasks.sort_by(|a, b| (rank(a), &a.id).cmp(&(rank(b), &b.id)));
        self.validate_held_out(&tasks).map_err(|m| error(dir, m))?;
        let probes = dir.join("probes");
        if probes.is_dir() {
            self.add_probes(&probes, &mut tasks)?;
        }
        Ok(tasks)
    }

    fn add_probes(&self, dir: &Path, tasks: &mut [Task]) -> Result<(), ManifestError> {
        for path in yaml_files(dir)? {
            let id = path.file_stem().unwrap_or_default().to_string_lossy();
            let Some(task) = tasks.iter_mut().find(|task| task.id == id) else {
                return Err(error(&path, format!("no task has the id '{id}'")));
            };
            let probes: Vec<Probe> = parse_file(&path)?;
            task.probes.extend(probes);
            self.validate_probes(task).map_err(|message| error(&path, message))?;
        }
        Ok(())
    }

    /// Check each step's `app` argument against the apps.
    pub(crate) fn validate_step_apps(
        &self,
        what: &str,
        steps: &[GoldenStep],
    ) -> Result<(), String> {
        for (i, step) in steps.iter().enumerate() {
            let pick = match step.tool.as_str() {
                "api_request" => Pick::Api,
                "api_docs" => Pick::Docs,
                _ => continue,
            };
            let at = format!("{what} step {}", i + 1);
            let app = match step.arguments.get("app") {
                None => None,
                Some(Value::String(name)) => Some(name.as_str()),
                Some(_) => return Err(format!("{at}: app must be a string")),
            };
            self.check_app(&at, app, pick, true)?;
        }
        Ok(())
    }

    fn fill_defaults(&self, task: &mut Task) {
        if task.persona.is_none() && self.personas.len() == 1 {
            task.persona = self.personas.keys().next().cloned();
        }
        if task.world.is_empty() && self.worlds.len() == 1 {
            task.world.clone_from(&self.worlds[0]);
        }
    }

    fn validate_task(&self, task: &Task) -> Result<(), String> {
        ensure(!task.id.is_empty(), "id must not be empty")?;
        ensure(SPLITS.contains(&task.split.as_str()), "split must be train, validation or test")?;
        match &task.persona {
            Some(name) => {
                ensure(self.personas.contains_key(name), format!("unknown persona '{name}'"))?;
            }
            None => return Err("persona is required when the manifest has several".into()),
        }
        ensure(!task.world.is_empty(), "world is required when the manifest has several")?;
        ensure(self.worlds.contains(&task.world), format!("unknown world '{}'", task.world))?;
        ensure(!task.instruction.trim().is_empty(), "instruction must not be empty")?;
        ensure(task.max_steps >= 1, "max_steps must be at least 1")?;
        ensure((0.0..=1.0).contains(&task.null_max), "null_max must be between 0 and 1")?;
        validate_checks(&task.checks)?;
        self.validate_scope(task)?;
        ensure((-1.0..=0.0).contains(&task.harm_reward), "harm_reward must be between -1 and 0")?;
        coworker::validate(self, task)?;
        clock::validate(self, task)?;
        self.validate_run("golden", &task.golden, task)?;
        self.validate_probes(task)
    }

    fn validate_scope(&self, task: &Task) -> Result<(), String> {
        for check in &task.checks {
            let at = format!("check '{}'", check.name);
            if check.db.is_some() {
                ensure(
                    check.app.is_none(),
                    format!("{at}: db: episode and app can't be combined"),
                )?;
            } else {
                self.check_app(&at, check.app.as_deref(), Pick::Database, check.sql.is_some())?;
            }
        }
        for allowed in task.allowed_changes.iter().flatten() {
            let at = format!("allowed_changes entry for '{}'", allowed.table);
            self.check_app(&at, allowed.app.as_deref(), Pick::Database, true)?;
            ensure(!allowed.table.is_empty(), "allowed_changes entries need a table")?;
            let columns_ok = allowed.columns.is_none() || allowed.op == ChangeOp::Update;
            ensure(columns_ok, "allowed_changes columns apply only to op: update")?;
        }
        ensure(
            task.setup.is_none() || self.databases().nth(1).is_none(),
            "setup runs on the only database, so it can't be used when several apps have one",
        )?;
        ensure(
            task.allowed_changes.is_some() != task.any_change,
            "set exactly one of allowed_changes or any_change: true",
        )
    }

    fn validate_run(&self, what: &str, steps: &[GoldenStep], task: &Task) -> Result<(), String> {
        validate_steps(what, steps, task.answer, self.mcp().is_some())?;
        self.validate_step_apps(what, steps)?;
        coworker::validate_replies(what, steps, &task.coworkers)?;
        clock::validate_waits(what, steps, task.has_clock())
    }

    fn validate_probes(&self, task: &Task) -> Result<(), String> {
        let mut names = BTreeSet::new();
        for probe in &task.probes {
            let at = format!("probe '{}'", probe.name);
            ensure(!probe.name.is_empty(), "every probe needs a name")?;
            ensure(names.insert(&probe.name), format!("{at} is listed twice"))?;
            let bounded = probe.max_reward.is_some() || probe.min_reward.is_some();
            ensure(bounded, format!("{at} needs max_reward or min_reward"))?;
            self.validate_run(&at, &probe.steps, task)?;
        }
        Ok(())
    }

    fn validate_held_out(&self, tasks: &[Task]) -> Result<(), String> {
        if self.worlds.len() < 2 {
            return Ok(());
        }
        for world in &self.worlds {
            let in_world = || tasks.iter().filter(|task| task.world == *world);
            let test = in_world().find(|task| task.split == "test");
            let other = in_world().find(|task| task.split != "test");
            if let (Some(test), Some(other)) = (test, other) {
                return Err(format!(
                    "world '{world}' has test tasks, such as '{}', and {} tasks, such as '{}': \
                     hold whole worlds out for test, so a test score comes from worlds training \
                     never saw",
                    test.id, other.split, other.id
                ));
            }
        }
        Ok(())
    }
}

fn yaml_files(dir: &Path) -> Result<Vec<PathBuf>, ManifestError> {
    let entries = std::fs::read_dir(dir).map_err(|e| error(dir, e.to_string()))?;
    let mut paths = Vec::new();
    for entry in entries {
        let path = entry.map_err(|e| error(dir, e.to_string()))?.path();
        if path.extension().is_some_and(|ext| ext == "yaml" || ext == "yml") {
            paths.push(path);
        }
    }
    Ok(paths)
}

fn validate_checks(checks: &[Check]) -> Result<(), String> {
    ensure(!checks.is_empty(), "checks must list at least one check")?;
    let mut names = BTreeSet::new();
    let mut total = 0.0;
    for check in checks {
        let at = format!("check '{}'", check.name);
        ensure(!check.name.is_empty(), "every check needs a name")?;
        ensure(check.name != "scope", "check name 'scope' is reserved for allowed_changes")?;
        ensure(names.insert(&check.name), format!("{at} is listed twice"))?;
        ensure(
            check.sql.is_some() != check.command.is_some(),
            format!("{at} needs exactly one of sql or command"),
        )?;
        ensure(
            check.db.is_none() || check.sql.is_some(),
            format!("{at}: db applies only to sql checks"),
        )?;
        if let Some(command) = &check.command {
            ensure(!command.is_empty(), format!("{at} has an empty command"))?;
        }
        if let Some(by) = check.by {
            ensure(
                !check.gate,
                format!(
                    "{at}: by applies only to weighted checks; a gate says what must not happen \
                     by the end"
                ),
            )?;
            ensure(
                (1..=MAX_CLOCK_MINUTES).contains(&by),
                format!("{at}: by must be a minute from 1 to {MAX_CLOCK_MINUTES}"),
            )?;
        }
        match (check.weight, check.gate) {
            (Some(weight), false) => {
                ensure(weight > 0.0 && weight <= 1.0, format!("{at}: weight must be in (0, 1]"))?;
                total += weight;
            }
            (None, true) => {}
            _ => return Err(format!("{at} needs exactly one of weight or gate: true")),
        }
    }
    ensure((total - 1.0_f64).abs() <= 1e-6, format!("check weights sum to {total}, not 1"))
}

fn validate_steps(
    what: &str,
    steps: &[GoldenStep],
    answer: Option<Answer>,
    mcp: bool,
) -> Result<(), String> {
    ensure(!steps.is_empty(), format!("{what} must list its tool calls"))?;
    for (i, step) in steps.iter().enumerate() {
        ensure(
            TOOLS.contains(&step.tool.as_str()) || (mcp && is_tool_name(&step.tool)),
            format!("{what} step {}: unknown tool '{}'", i + 1, step.tool),
        )?;
        let last = i + 1 == steps.len();
        ensure(
            (step.tool == "submit") == last,
            format!("{what} must end with submit, and only there"),
        )?;
    }
    let submitted = steps.last().and_then(|step| step.arguments.get("answer"));
    if let (Some(form), Some(text)) = (answer, submitted.and_then(Value::as_str)) {
        form.normalize(text, 0).map_err(|e| format!("{what}: {e}"))?;
    }
    Ok(())
}

/// 1 to 64 ASCII letters, digits, `_` or `-`, as model APIs accept.
pub fn is_tool_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

#[cfg(test)]
mod tests {
    use proptest::strategy::Strategy;

    use super::*;
    use crate::manifest::tests::{MANIFEST, manifest};

    const TASK: &str = r"
instruction: Do it.
checks:
  - {name: a, weight: 0.6, sql: SELECT 1}
  - {name: b, weight: 0.3, db: episode, sql: SELECT 1}
  - {name: c, weight: 0.1, command: [true]}
  - {name: d, gate: true, sql: SELECT 1}
any_change: true
golden:
  - {tool: api_request, arguments: {method: GET, path: /x}}
  - {tool: submit, arguments: {answer: done}}
probes:
  - {name: guess, steps: [{tool: submit, arguments: {answer: nope}}], max_reward: 0}
";

    const COWORKER_TASK: &str = r#"
instruction: Ask Priya.
coworkers:
  - name: priya
    description: Priya Shah, account executive
    script: The deal is at proposal.
    headers: {crm: {Authorization: Bearer priya}}
    inbox: {sql: SELECT 1}
    reply: {method: POST, path: "/deals/{deal}/notes", body: {note: {text: ["{text}"]}}}
checks:
  - {name: a, weight: 1, sql: SELECT 1}
any_change: true
golden:
  - {tool: api_request, arguments: {method: POST, path: /deals/1/notes, body: {text: "@priya?"}}}
  - {tool: coworker_reply, arguments: {coworker: priya, text: Proposal.}}
  - {tool: submit}
probes:
  - {name: hi, steps: [{tool: coworker_reply, arguments: {coworker: priya, text: Hi.}}, {tool: submit}], max_reward: 0}
"#;

    const CLOCK_TASK: &str = r"
instruction: Watch the deal.
events:
  - name: signed
    at: 90
    headers: {crm: {Authorization: Bearer priya}}
    request: {method: POST, path: /deals/1/notes, body: {text: Signed.}}
checks:
  - {name: a, weight: 1, by: 210, sql: SELECT 1}
  - {name: g, gate: true, sql: SELECT 1}
any_change: true
golden:
  - {tool: wait, arguments: {minutes: 600}}
  - {tool: submit}
probes:
  - {name: late, steps: [{tool: wait, arguments: {minutes: 300}}, {tool: submit}], max_reward: 0}
";

    const REVIEWED: &str = r#"# should fail: 3 rollouts earned 1.0; episode e1, 2026-11-04
- name: review-3f9a2c1e0b7d
  steps:
    - {"tool": "api_request", "arguments": {"method": "GET", "path": "/y"}}
    - {"tool": "submit", "arguments": {}}
  max_reward: 0.0
"#;

    fn task(yaml: &str) -> Result<Task, String> {
        task_in(MANIFEST, yaml)
    }

    fn task_in(manifest_yaml: &str, yaml: &str) -> Result<Task, String> {
        let manifest = manifest(manifest_yaml)?;
        let mut task: Task = serde_saphyr::from_str(yaml).map_err(|e| e.to_string())?;
        task.id = "t".into();
        manifest.fill_defaults(&mut task);
        manifest.validate_task(&task)?;
        Ok(task)
    }

    fn task_err(from: &str, to: &str) -> String {
        assert!(TASK.contains(from), "test patch does not apply: {from}");
        task(&TASK.replacen(from, to, 1)).expect_err("task should be rejected")
    }

    #[test]
    fn valid_manifest_and_task_pass() {
        let m = manifest(MANIFEST).unwrap();
        assert_eq!(m.processes[0].ready.as_ref().unwrap().timeout_s, 60);
        assert_eq!(m.sinks.responses["/hooks/*"].status, 204);
        let t = task(TASK).unwrap();
        assert_eq!(
            (t.split.as_str(), t.max_steps, t.persona.as_deref()),
            ("train", 50, Some("manager"))
        );
        assert_eq!(t.checks[1].db, Some(CheckDb::Episode));
        assert_eq!(
            (m.worlds.as_slice(), t.world.as_str()),
            (&["default".to_string()][..], "default")
        );
    }

    #[test]
    fn a_task_names_its_world_when_there_are_several() {
        let one = task_in(MANIFEST, &format!("world: default\n{TASK}")).unwrap();
        assert_eq!(one.world, "default");
        let unknown = task(&format!("world: elsewhere\n{TASK}")).unwrap_err();
        assert!(unknown.contains("unknown world 'elsewhere'"), "{unknown}");
        let several = format!("{MANIFEST}worlds: [north, south]\n");
        let missing = task_in(&several, TASK).unwrap_err();
        assert!(missing.contains("world is required when the manifest has several"), "{missing}");
        assert_eq!(task_in(&several, &format!("world: south\n{TASK}")).unwrap().world, "south");
    }

    #[test]
    fn tasks_come_by_world_then_id_and_test_worlds_hold_only_test_tasks() {
        let dir = std::env::temp_dir().join(format!("miroir-worlds-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let in_world = |world: &str, split: &str| format!("world: {world}\nsplit: {split}\n{TASK}");
        std::fs::write(dir.join("a.yaml"), in_world("south", "test")).unwrap();
        std::fs::write(dir.join("b.yaml"), in_world("north", "train")).unwrap();
        std::fs::write(dir.join("c.yaml"), in_world("north", "validation")).unwrap();
        let m = manifest(&format!("{MANIFEST}worlds: [north, south]\n")).unwrap();
        let order: Vec<(String, String)> =
            m.load_tasks(&dir).unwrap().into_iter().map(|t| (t.world, t.id)).collect();
        let expected = [("north", "b"), ("north", "c"), ("south", "a")];
        assert_eq!(order, expected.map(|(w, id)| (w.to_string(), id.to_string())));
        std::fs::write(dir.join("d.yaml"), in_world("south", "train")).unwrap();
        let err = m.load_tasks(&dir).unwrap_err().to_string();
        assert!(
            err.contains("world 'south' has test tasks, such as 'a', and train tasks, such as 'd'"),
            "{err}"
        );
        let single = manifest(MANIFEST).unwrap();
        for file in ["a.yaml", "b.yaml", "c.yaml", "d.yaml"] {
            let yaml = std::fs::read_to_string(dir.join(file)).unwrap();
            std::fs::write(
                dir.join(file),
                yaml.replacen("world: north\n", "", 1).replacen("world: south\n", "", 1),
            )
            .unwrap();
        }
        assert_eq!(single.load_tasks(&dir).unwrap().len(), 4, "one world holds out nothing");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_task_names_its_app_only_when_there_are_several() {
        let add = |app: &str| MANIFEST.replacen("apps:\n", &format!("apps:\n  {app}\n"), 1);
        let described =
            |yaml: String| yaml.replacen("  crm:\n", "  crm:\n    description: CRM\n", 1);
        let two_apis = described(add("east: {description: East, api: {base_url: \"http://x:1\"}}"))
            .replacen("http://x:1", "http://127.0.0.1:3001", 1);
        let two_dbs = add("ledger: {database: {url: \"postgres://postgres@127.0.0.1/ledger\"}}");
        let call =
            |app: &str| TASK.replacen("{method: GET,", &format!("{{app: {app}, method: GET,"), 1);
        let on_check = |task: &str, check: &str, app: &str| {
            task.replacen(&format!("{{name: {check},"), &format!("{{name: {check}, app: {app},"), 1)
        };
        let refused = |manifest: &str, task: &str, expected: &str| {
            let err = task_in(manifest, task).unwrap_err();
            assert!(err.contains(expected), "expected '{expected}', got '{err}'");
        };

        refused(MANIFEST, &on_check(TASK, "a", "crm"), "check 'a': app applies only when several");
        refused(
            MANIFEST,
            &call("crm"),
            "golden step 1: app applies only when several apps have an api",
        );

        refused(
            &two_apis,
            TASK,
            "golden step 1: app is required, since several apps have an api: crm, east",
        );
        for app in ["crm", "east"] {
            assert!(task_in(&two_apis, &call(app)).is_ok(), "{app}");
        }
        refused(&two_apis, &call("ghost"), "unknown app 'ghost'");
        refused(&two_apis, &call("3"), "golden step 1: app must be a string");
        refused(
            &two_apis,
            &on_check(&call("crm"), "a", "crm"),
            "applies only when several apps have a database",
        );
        let submit = "{tool: submit, arguments: {answer: done}}";
        let docs = call("crm").replacen(
            submit,
            &format!("{{tool: api_docs, arguments: {{app: east}}}}\n  - {submit}"),
            1,
        );
        refused(&two_apis, &docs, "golden step 2: app 'east' has no api.openapi");

        refused(
            &two_dbs,
            TASK,
            "check 'a': app is required, since several apps have a database: crm, ledger",
        );
        let both = on_check(&on_check(TASK, "a", "crm"), "d", "ledger");
        assert!(
            task_in(&two_dbs, &both).is_ok(),
            "a database-only app is checked; a command needs none"
        );
        refused(
            &two_dbs,
            &on_check(&both, "b", "crm"),
            "check 'b': db: episode and app can't be combined",
        );
        refused(&two_dbs, &on_check(&both, "c", "ghost"), "check 'c': unknown app 'ghost'");
        refused(
            &two_dbs,
            &both.replacen("{method: GET,", "{app: crm, method: GET,", 1),
            "golden step 1: app applies only",
        );
        let scoped = |entry: &str| {
            both.replacen("any_change: true", &format!("allowed_changes: [{entry}]"), 1)
        };
        refused(
            &two_dbs,
            &scoped("{table: deals, op: insert}"),
            "allowed_changes entry for 'deals': app is required",
        );
        assert!(task_in(&two_dbs, &scoped("{app: ledger, table: entries, op: insert}")).is_ok());
        refused(&two_dbs, &format!("setup: SELECT 1\n{both}"), "setup runs on the only database");
        assert!(task(&format!("setup: SELECT 1\n{TASK}")).is_ok(), "one database takes setup");

        let three = add("ledger: {database: {url: \"postgres://postgres@127.0.0.1/ledger\"}}");
        let three = described(three.replacen(
            "apps:\n",
            "apps:\n  east: {description: East, api: {base_url: \"http://127.0.0.1:3001\"}}\n",
            1,
        ));
        refused(
            &three,
            &on_check(&on_check(&call("ledger"), "a", "crm"), "d", "crm"),
            "golden step 1: app 'ledger' has no api",
        );

        let dir = std::env::temp_dir().join(format!("miroir-task-apps-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("probes")).unwrap();
        std::fs::write(dir.join("a-task.yaml"), call("crm")).unwrap();
        let unnamed = "- {name: x, max_reward: 0, steps: [\
                       {tool: api_request, arguments: {method: GET, path: /y}}, {tool: submit}]}\n";
        std::fs::write(dir.join("probes/a-task.yaml"), unnamed).unwrap();
        let err = manifest(&two_apis).unwrap().load_tasks(&dir).unwrap_err().to_string();
        assert!(
            err.contains("probe 'x' step 1: app is required") && err.contains("a-task.yaml"),
            "{err}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_repos_manifests_and_tasks_load() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        for dir in ["fixtures/acme-crm", "fixtures/acme-apps", "fixtures/mini-shop"] {
            let manifest = Manifest::load(&root.join(dir).join("miroir.yaml"));
            let manifest = manifest.unwrap_or_else(|e| panic!("{dir}: {e}"));
            let tasks = manifest.load_tasks(&root.join(dir).join("tasks"));
            assert!(
                tasks.is_ok(),
                "{dir}: {}",
                tasks.err().map(|e| e.to_string()).unwrap_or_default()
            );
        }
    }

    #[test]
    fn steps_may_call_an_mcp_servers_tools_when_the_env_has_one() {
        let crm_api = "    api: {base_url: \"http://127.0.0.1:3000\"}\n";
        let with_mcp = MANIFEST.replacen(
            crm_api,
            &format!("{crm_api}    mcp: {{url: \"http://127.0.0.1:8080/mcp\"}}\n"),
            1,
        );
        let mcp_step = TASK.replacen("{tool: api_request,", "{tool: move_deal,", 1);
        assert!(task_in(&with_mcp, &mcp_step).is_ok());
        let err = task(&mcp_step).unwrap_err();
        assert!(err.contains("golden step 1: unknown tool 'move_deal'"), "{err}");
        for name in ["crm.move", "", &"x".repeat(65)] {
            let odd = TASK.replacen("{tool: api_request,", &format!("{{tool: \"{name}\","), 1);
            let err = task_in(&with_mcp, &odd).unwrap_err();
            assert!(err.contains("unknown tool"), "{name}: {err}");
        }
        let probe = TASK.replacen(
            "steps: [{tool: submit,",
            "steps: [{tool: list_deals}, {tool: submit,",
            1,
        );
        assert!(task_in(&with_mcp, &probe).is_ok());
        assert!(task(&probe).unwrap_err().contains("probe 'guess' step 1: unknown tool"));
        assert!(is_tool_name("list_deals") && is_tool_name("a-b_9") && !is_tool_name("a b"));
    }

    #[test]
    fn task_rules() {
        let cases = [
            ("weight: 0.1, command", "weight: 0.2, command", "sum to"),
            (
                "{name: d, gate: true, sql: SELECT 1}",
                "{name: d, gate: true, weight: 0.5, sql: SELECT 1}",
                "exactly one of weight or gate",
            ),
            (
                "{name: d, gate: true, sql: SELECT 1}",
                "{name: d, sql: SELECT 1}",
                "exactly one of weight or gate",
            ),
            (
                "command: [true]}",
                "command: [true], sql: SELECT 1}",
                "exactly one of sql or command",
            ),
            ("command: [true]}", "command: [true], db: episode}", "db applies only to sql"),
            ("db: episode, sql", "db: app, sql", "unknown variant"),
            ("{name: b,", "{name: a,", "listed twice"),
            ("{name: d,", "{name: scope,", "reserved"),
            ("instruction: Do it.", "instruction: Do it.\nsplit: dev", "split must be"),
            ("instruction: Do it.", "instruction: Do it.\npersona: ghost", "unknown persona"),
            ("instruction: Do it.", "instruction: Do it.\nmax_steps: 0", "max_steps"),
            ("instruction: Do it.", "instruction: Do it.\nnull_max: 2", "null_max"),
            (
                "instruction: Do it.",
                "instruction: Do it.\nallowed_changes: [{table: deals, op: insert, columns: [a]}]",
                "only to op: update",
            ),
            ("{tool: submit, arguments: {answer: done}}", "{tool: api_docs}", "end with submit"),
            ("{tool: api_request,", "{tool: submit,", "end with submit"),
            ("{tool: api_request,", "{tool: shell,", "unknown tool"),
            ("instruction: Do it.", "instruction: \" \"", "instruction must not be empty"),
            ("checks:", "check:", "unknown field"),
            ("any_change: true", "any_change: false", "exactly one of allowed_changes"),
            ("any_change: true", "any_change: true\nallowed_changes: []", "exactly one of"),
            ("instruction: Do it.", "instruction: Do it.\nharm_reward: 0.5", "harm_reward"),
            ("instruction: Do it.", "instruction: Do it.\nharm_reward: -2", "harm_reward"),
            (
                "instruction: Do it.",
                "instruction: Do it.\nanswer: integer",
                "golden: the answer must be a whole number",
            ),
            ("instruction: Do it.", "instruction: Do it.\nanswer: number", "number"),
            ("{name: guess,", "{name: \"\",", "every probe needs a name"),
            (
                "max_reward: 0}",
                "max_reward: 0}\n  - {name: guess, steps: [{tool: submit}], min_reward: 1}",
                "listed twice",
            ),
            (", max_reward: 0}", "}", "needs max_reward or min_reward"),
            (
                "steps: [{tool: submit, arguments: {answer: nope}}]",
                "steps: []",
                "must list its tool calls",
            ),
            (
                "steps: [{tool: submit, arguments: {answer: nope}}]",
                "steps: [{tool: api_docs}]",
                "must end with submit",
            ),
        ];
        for (from, to, expected) in cases {
            let err = task_err(from, to);
            assert!(err.contains(expected), "{to}: expected '{expected}', got '{err}'");
        }
    }

    #[test]
    fn coworker_rules() {
        let t = task(COWORKER_TASK).unwrap();
        assert_eq!(t.coworkers[0].name, "priya");
        assert_eq!(t.coworkers[0].headers["crm"]["Authorization"], "Bearer priya");
        let refused = |from: &str, to: &str, expected: &str| {
            assert!(COWORKER_TASK.contains(from), "test patch does not apply: {from}");
            let err = task(&COWORKER_TASK.replacen(from, to, 1)).unwrap_err();
            assert!(err.contains(expected), "{to}: expected '{expected}', got '{err}'");
        };
        let twin = "coworkers:\n  - {name: priya, description: d, script: s, inbox: {sql: x}, \
                    reply: {method: POST, path: /n, body: {text: \"{text}\"}}}\n";
        let cases = [
            ("name: priya", "name: Priya", "a name is 1 to 64 lowercase"),
            ("coworkers:\n", twin, "coworker 'priya' is listed twice"),
            ("description: Priya Shah, account executive", "description: \" \"", "description"),
            ("script: The deal is at proposal.", "script: \"\"", "needs a script"),
            ("headers: {crm:", "headers: {ledger:", "'ledger', which is no app with an api"),
            ("inbox: {sql: SELECT 1}", "inbox: {sql: \" \"}", "inbox needs sql"),
            (
                "inbox: {sql: SELECT 1}",
                "inbox: {app: crm, sql: SELECT 1}",
                "inbox: app applies only when several apps have a database",
            ),
            ("reply: {method", "reply: {headers: {}, method", "'headers' is not one of"),
            (
                "reply: {method",
                "reply: {app: crm, method",
                "reply: app applies only when several apps have an api",
            ),
            ("{method: POST, path: \"/deals", "{method: HEAD, path: \"/deals", "method must be"),
            ("path: \"/deals/{deal}", "path: \"deals/{deal}", "path must start with /"),
            ("[\"{text}\"]", "[\"{answer}\", \"{ text }\"]", "{text}, the answer, appears nowhere"),
            (
                "  - {tool: coworker_reply, arguments: {coworker: priya, text: Proposal.}}\n",
                "",
                "no golden step answers for them",
            ),
            ("{coworker: priya, text: Hi.}", "{coworker: dana, text: Hi.}", "no coworker 'dana'"),
            (
                "{coworker: priya, text: Hi.}",
                "{coworker: priya, text: Hi., to: all}",
                "probe 'hi' step 1: coworker_reply takes exactly a coworker and a text",
            ),
            ("{coworker: priya, text: Hi.}", "{coworker: priya}", "takes exactly"),
            ("{coworker: priya, text: Hi.}", "{coworker: priya, text: \" \"}", "isn't empty"),
            ("{coworker: priya, text: Proposal.}", "{coworker: priya, text: 3}", "isn't empty"),
        ];
        for (from, to, expected) in cases {
            refused(from, to, expected);
        }
        let in_plain_task = task_err(
            "  - {tool: submit, arguments: {answer: done}}",
            "  - {tool: coworker_reply, arguments: {coworker: priya, text: hi}}\n  \
             - {tool: submit, arguments: {answer: done}}",
        );
        assert!(
            in_plain_task.contains("golden step 2: coworker_reply needs a task with coworkers")
        );
        let last = task_err("{tool: submit, arguments: {answer: done}}", "{tool: coworker_reply}");
        assert!(last.contains("end with submit"), "{last}");
    }

    #[test]
    fn clock_rules() {
        let t = task(CLOCK_TASK).unwrap();
        assert!(t.has_clock());
        assert_eq!(
            (t.events[0].name.as_str(), t.events[0].at, t.checks[0].by),
            ("signed", 90, Some(210))
        );
        assert!(!task(TASK).unwrap().has_clock());
        let milestone_only =
            TASK.replacen("{name: a, weight: 0.6,", "{name: a, weight: 0.6, by: 5,", 1);
        assert!(task(&milestone_only).unwrap().has_clock(), "a milestone alone makes a clock");
        let refused = |from: &str, to: &str, expected: &str| {
            assert!(CLOCK_TASK.contains(from), "test patch does not apply: {from}");
            let err = task(&CLOCK_TASK.replacen(from, to, 1)).unwrap_err();
            assert!(err.contains(expected), "{to}: expected '{expected}', got '{err}'");
        };
        let twin = "events:\n  - {name: signed, at: 5, request: {method: GET, path: /x}}\n";
        let cases = [
            ("name: signed", "name: Signed", "event 'Signed': a name is 1 to 64 lowercase"),
            ("events:\n", twin, "event 'signed' is listed twice"),
            ("at: 90", "at: 0", "at must be a minute from 1 to 525600"),
            ("at: 90", "at: 525601", "at must be a minute from 1 to 525600"),
            ("headers: {crm:", "headers: {ledger:", "'ledger', which is no app with an api"),
            ("request: {method", "request: {headers: {}, method", "'headers' is not one of"),
            (
                "request: {method",
                "request: {app: crm, method",
                "event 'signed' request: app applies only when several apps have an api",
            ),
            ("{method: POST, path: /deals", "{method: HEAD, path: /deals", "method must be"),
            ("path: /deals/1/notes", "path: deals/1/notes", "path must start with /"),
            ("by: 210", "by: 0", "check 'a': by must be a minute from 1 to 525600"),
            ("{name: g, gate: true,", "{name: g, gate: true, by: 30,", "only to weighted checks"),
            ("instruction: Watch the deal.", "instruction: W.\nanswer: date", "date answer"),
            ("{minutes: 600}", "{minutes: 0}", "golden step 1: wait takes exactly minutes"),
            ("{minutes: 600}", "{minutes: 10081}", "a whole number from 1 to 10080"),
            ("{minutes: 600}", "{minutes: 1.5}", "takes exactly minutes"),
            ("{minutes: 600}", "{minutes: '60'}", "takes exactly minutes"),
            ("{minutes: 600}", "{minutes: 60, at: 3}", "takes exactly minutes"),
            ("{minutes: 300}", "{}", "probe 'late' step 1: wait takes exactly minutes"),
        ];
        for (from, to, expected) in cases {
            refused(from, to, expected);
        }
        let waits_without_a_clock = task_err(
            "  - {tool: submit, arguments: {answer: done}}",
            "  - {tool: wait, arguments: {minutes: 5}}\n  - {tool: submit, arguments: {answer: done}}",
        );
        assert!(
            waits_without_a_clock.contains("golden step 2: wait needs a task with a clock"),
            "{waits_without_a_clock}"
        );
        let hooked = MANIFEST.replacen("personas:\n", "time: {hook: [\"true\"]}\npersonas:\n", 1);
        assert!(task_in(&hooked, TASK).is_ok(), "a hook is fine without a clock");
        let err = task_in(&hooked, CLOCK_TASK).unwrap_err();
        assert!(err.contains("can't run in an env with time.hook"), "{err}");
    }

    #[test]
    fn a_reviewed_wait_needs_a_task_with_a_clock() {
        let dir = std::env::temp_dir().join(format!("miroir-waits-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("probes")).unwrap();
        std::fs::write(dir.join("a-task.yaml"), TASK).unwrap();
        std::fs::write(dir.join("b-task.yaml"), CLOCK_TASK).unwrap();
        let waits = "- {name: waits, steps: [{tool: wait, arguments: {minutes: 30}}, {tool: submit}], \
                     max_reward: 0}\n";
        std::fs::write(dir.join("probes/b-task.yaml"), waits).unwrap();
        let m = manifest(MANIFEST).unwrap();
        let tasks = m.load_tasks(&dir).unwrap();
        assert_eq!(tasks[1].probes.len(), 2, "the clock task takes the reviewed wait");
        std::fs::write(dir.join("probes/a-task.yaml"), waits).unwrap();
        let err = m.load_tasks(&dir).unwrap_err().to_string();
        assert!(err.contains("probe 'waits' step 1: wait needs a task with a clock"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_coworker_names_its_apps_only_when_there_are_several() {
        let east = "east: {description: East, api: {base_url: \"http://127.0.0.1:3001\"}, \
                    database: {url: \"postgres://postgres@127.0.0.1/east\"}}";
        let two = MANIFEST.replacen("apps:\n", &format!("apps:\n  {east}\n"), 1).replacen(
            "  crm:\n",
            "  crm:\n    description: CRM\n",
            1,
        );
        let named = COWORKER_TASK
            .replacen("{sql: SELECT 1}\n", "{app: crm, sql: SELECT 1}\n", 1)
            .replacen("reply: {method", "reply: {app: crm, method", 1)
            .replacen("{method: POST, path: /deals/1", "{app: crm, method: POST, path: /deals/1", 1)
            .replacen("{name: a, weight: 1, sql", "{name: a, app: crm, weight: 1, sql", 1);
        assert!(task_in(&two, &named).is_ok(), "{:?}", task_in(&two, &named).err());
        let without = |from: &str, to: &str, expected: &str| {
            let err = task_in(&two, &named.replacen(from, to, 1)).unwrap_err();
            assert!(err.contains(expected), "expected '{expected}', got '{err}'");
        };
        without("{app: crm, sql: SELECT 1}\n", "{sql: SELECT 1}\n", "inbox: app is required");
        without("reply: {app: crm, method", "reply: {method", "reply: app is required");
        without("reply: {app: crm,", "reply: {app: ghost,", "unknown app 'ghost'");
        let headers = named.replacen("headers: {crm:", "headers: {east:", 1);
        assert!(task_in(&two, &headers).is_ok(), "headers may be for any app with an api");
    }

    fn integer_task() -> String {
        TASK.replacen("instruction: Do it.", "instruction: Do it.\nanswer: integer", 1)
            .replacen("{answer: done}", "{answer: ' 3 '}", 1)
            .replacen("{answer: nope}", "{answer: '4'}", 1)
    }

    #[test]
    fn answers_take_their_normal_form() {
        let taken = [
            (Answer::Integer, " 007\n", 0, "7"),
            (Answer::Integer, "+42", 0, "42"),
            (Answer::Integer, "-3", 0, "-3"),
            (Answer::Integer, "-0", 0, "0"),
            (Answer::Integer, "  ", 0, ""),
            (Answer::Integer, "7", 30, "7"),
            (Answer::Ids, "18 12\n15,12", 0, "12,15,18"),
            (Answer::Ids, "10, 9", 0, "9,10"),
            (Answer::Ids, "b, a, INV-04001, acme/infra", 0, "INV-04001,a,acme/infra,b"),
            (Answer::Ids, ", ,", 0, ""),
            (Answer::Date, " 2026-11-04 ", 0, "2026-11-04"),
            (Answer::Date, "2028-02-29", 0, "2028-02-29"),
            (Answer::Date, "2026-11-07", 3, "2026-11-04"),
            (Answer::Date, "2027-01-02", 3, "2026-12-30"),
            (Answer::Date, "2026-11-04", -1, "2026-11-05"),
            (Answer::Date, "", 3, ""),
        ];
        for (form, answer, days, normal) in taken {
            let got = form.normalize(answer, days);
            assert_eq!(got.as_deref(), Ok(normal), "{form:?} {answer:?} {days}");
        }
        assert_eq!(move_date("2028-02-28", 1).as_deref(), Some("2028-02-29"));
        assert_eq!(move_date("9999-12-31", 1), None, "past the calendar");
        let refused = [
            (Answer::Integer, "7 open bugs"),
            (Answer::Integer, "7.0"),
            (Answer::Integer, "1,234"),
            (Answer::Integer, "**7**"),
            (Answer::Integer, "99999999999999999999"),
            (Answer::Ids, "[12, 15]"),
            (Answer::Ids, "#12, #15"),
            (Answer::Ids, "12; 15"),
            (Answer::Date, "2026-02-30"),
            (Answer::Date, "2026-11-4"),
            (Answer::Date, "11/04/2026"),
            (Answer::Date, "2026-11-04T00:00:00Z"),
            (Answer::Date, "20261104"),
        ];
        for (form, answer) in refused {
            let err = form.normalize(answer, 0).unwrap_err();
            assert!(err.starts_with("the answer must be "), "{form:?} {answer:?}: {err}");
        }
    }

    proptest::proptest! {
        #[test]
        fn a_normal_answer_is_its_own_normal_form(
            answer in proptest::prop_oneof![
                r"[ +\-0-9]{0,22}",
                r"[0-9]{4}-[01][0-9]-[0-3][0-9]",
                r"[ -~\n]{0,30}",
            ]
        ) {
            for form in [Answer::Integer, Answer::Ids, Answer::Date] {
                if let Ok(normal) = form.normalize(&answer, 0) {
                    proptest::prop_assert_eq!(form.normalize(&normal, 0), Ok(normal.clone()));
                }
            }
        }

        #[test]
        fn ids_normalize_alike_in_any_order_and_with_repeats(
            (ids, shuffled) in proptest::collection::vec("[a-z0-9]{1,5}", 1..8).prop_flat_map(
                |ids| (proptest::strategy::Just(ids.clone()), proptest::strategy::Just(ids).prop_shuffle())
            )
        ) {
            let once = Answer::Ids.normalize(&ids.join(", "), 0);
            let again =
                Answer::Ids.normalize(&format!("{}\n{}", shuffled.join(" "), ids.join(",")), 0);
            proptest::prop_assert_eq!(once, again);
        }

        #[test]
        fn a_date_sent_at_any_shift_is_stored_as_the_task_file_wrote_it(
            date in r"(19|20)[0-9]{2}-(0[1-9]|1[0-2])-(0[1-9]|1[0-9]|2[0-8])",
            days in -1000_i64..1000,
        ) {
            let sent = move_date(&date, days).unwrap();
            proptest::prop_assert_eq!(Answer::Date.normalize(&sent, days), Ok(date));
        }
    }

    #[test]
    fn a_typed_answer_holds_golden_and_probe_answers_to_its_form() {
        assert_eq!(task(&integer_task()).unwrap().answer, Some(Answer::Integer));
        let worded = integer_task().replacen("{answer: '4'}", "{answer: four}", 1);
        let err = task(&worded).unwrap_err();
        assert!(err.contains("probe 'guess': the answer must be a whole number"), "{err}");
        let dir = std::env::temp_dir().join(format!("miroir-typed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("probes")).unwrap();
        std::fs::write(dir.join("a-task.yaml"), integer_task()).unwrap();
        let reviewed = "- {name: words, steps: [{tool: submit, arguments: {answer: three}}], \
                        max_reward: 0}\n";
        std::fs::write(dir.join("probes/a-task.yaml"), reviewed).unwrap();
        let err = manifest(MANIFEST).unwrap().load_tasks(&dir).unwrap_err().to_string();
        assert!(err.contains("probe 'words': the answer must be a whole number"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_tasks_defaults_ids_and_rejects_duplicates() {
        let dir = std::env::temp_dir().join(format!("miroir-tasks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("b-task.yaml"), TASK).unwrap();
        std::fs::write(dir.join("a-task.yml"), TASK).unwrap();
        std::fs::write(dir.join("notes.txt"), "ignored").unwrap();
        let m = manifest(MANIFEST).unwrap();
        let ids: Vec<String> = m.load_tasks(&dir).unwrap().into_iter().map(|t| t.id).collect();
        assert_eq!(ids, ["a-task", "b-task"]);
        std::fs::write(dir.join("c.yaml"), format!("id: a-task\n{TASK}")).unwrap();
        let err = m.load_tasks(&dir).unwrap_err().to_string();
        assert!(err.contains("used twice"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn probes_in_the_probes_folder_join_their_task() {
        let dir = std::env::temp_dir().join(format!("miroir-probes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("probes")).unwrap();
        std::fs::write(dir.join("a-task.yaml"), TASK).unwrap();
        let task_hash = |m: &Manifest| m.load_tasks(&dir).unwrap()[0].hash.clone();
        let m = manifest(MANIFEST).unwrap();
        let before = task_hash(&m);
        std::fs::write(dir.join("probes/a-task.yaml"), REVIEWED).unwrap();
        std::fs::write(dir.join("probes/labels.jsonl"), "{}\n").unwrap();
        let tasks = m.load_tasks(&dir).unwrap();
        let names: Vec<&str> = tasks[0].probes.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["guess", "review-3f9a2c1e0b7d"]);
        assert_eq!(tasks[0].probes[1].steps.len(), 2);
        assert_eq!(task_hash(&m), before, "probes test the grading; they don't change it");

        let refused = |file: &str, yaml: &str, expected: &str| {
            std::fs::write(dir.join("probes").join(file), yaml).unwrap();
            let err = m.load_tasks(&dir).unwrap_err().to_string();
            assert!(err.contains(expected) && err.contains(file), "{yaml}: {err}");
            std::fs::remove_file(dir.join("probes").join(file)).unwrap();
        };
        let guess = "- {name: guess, steps: [{tool: submit}], max_reward: 0}\n";
        refused("b-task.yaml", guess, "no task has the id 'b-task'");
        std::fs::remove_file(dir.join("probes/a-task.yaml")).unwrap();
        refused("a-task.yaml", guess, "listed twice");
        refused(
            "a-task.yml",
            "- {name: x, steps: [{tool: api_docs}], max_reward: 0}\n",
            "end with",
        );
        refused(
            "a-task.yaml",
            "- {name: y, steps: [{tool: coworker_reply, arguments: {coworker: a, text: b}}, \
             {tool: submit}], max_reward: 0}\n",
            "probe 'y' step 1: coworker_reply needs a task with coworkers",
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
