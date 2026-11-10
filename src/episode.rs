//! Episodes: reset, step, state and record, and the coworker frames.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use jiff::Timestamp;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::sync::OwnedMutexGuard;
use tracing::Instrument;
use uuid::Uuid;

use crate::clock::{Clock, WAIT};
use crate::coworker::Coworkers;
use crate::db::Shift;
use crate::edb::ActionRow;
use crate::env::{Catalog, Env, EnvError, Restored, reset_failed, rounded_seconds};
use crate::grade::{NetChange, Outcome, crashed};
use crate::manifest::{MAX_TIMEOUT_S, Persona};
use crate::record::{self, Window};
use crate::task::{SPLITS, Task};
use crate::tools::{Failure, ToolError};

/// The env's one episode.
#[derive(Default)]
pub struct Episode {
    id: Option<String>,
    task: Option<usize>,
    step_count: u32,
    done: bool,
    reward: Option<f64>,
    success: Option<bool>,
    shift: Option<Shift>,
    catalog: Catalog,
    started: Option<Timestamp>,
    reset_s: Value,
    deadline: Option<Deadline>,
    ended: Option<Ended>,
    workspace: String,
    coworkers: Coworkers,
    clock: Clock,
}

#[derive(Clone, Copy)]
struct Deadline {
    at: Instant,
    seconds: u64,
}

struct Ended {
    metadata: Value,
    at: Timestamp,
    grading_s: f64,
    effects_seq: i64,
}

/// A reset frame's data, as `schemas/reset.json` describes it.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResetArgs {
    pub task_id: Option<String>,
    pub split: Option<String>,
    pub index: Option<usize>,
    pub seed: Option<u64>,
    pub episode_id: Option<String>,
    pub max_seconds: Option<u64>,
    #[serde(default)]
    pub coworkers: bool,
}

impl Env {
    /// Restore the task's world and start its episode; returns the reset observation.
    pub async fn reset(&self, args: ResetArgs) -> Result<Value, EnvError> {
        self.refuse_leaked()?;
        let named = args.task_id.as_ref().and_then(|id| self.tasks.iter().find(|t| &t.id == id));
        if let Some(task) = named.filter(|task| !task.coworkers.is_empty() && !args.coworkers) {
            return Err(EnvError::Invalid(format!(
                "task '{}' has coworkers: reset it with coworkers: true, from a client that \
                 answers them (both adapters do once given a coworker model)",
                task.id
            )));
        }
        let (observation, _) = self.reset_with(args, None, false).await?;
        self.look_for_leaks().await?;
        Ok(observation)
    }

    /// Reset at a given shift, or the current one with `None`, fingerprinting the
    /// databases if asked.
    pub async fn reset_with(
        &self,
        args: ResetArgs,
        shift: Option<Shift>,
        fingerprint: bool,
    ) -> Result<(Value, Option<BTreeMap<String, String>>), EnvError> {
        let index = self.pick_task(&args)?;
        let task = &self.tasks[index];
        let id = args.episode_id.unwrap_or_else(|| Uuid::new_v4().to_string());
        if id.is_empty() || id.len() > 255 {
            return Err(EnvError::Invalid("episode_id must be 1 to 255 characters".into()));
        }
        if let Some(seconds) = args.max_seconds
            && !(1..=MAX_TIMEOUT_S).contains(&seconds)
        {
            return Err(EnvError::Invalid(format!("max_seconds must be 1 to {MAX_TIMEOUT_S}")));
        }
        let (task_id, world) = (&task.id, &task.world);
        let span = tracing::info_span!("episode", episode_id = %id, %task_id, %world);
        let mut episode = self.episode.lock().await;
        *episode = Episode::default();
        let started = Timestamp::now();
        let reuse = shift.is_none();
        let shift = shift.unwrap_or_else(|| self.shift_at(started));
        let Restored { print, seconds, catalog } = self
            .restore(&task.world, task.setup.as_deref(), shift, fingerprint, reuse)
            .instrument(span.clone())
            .await?;
        let inboxes = self.inboxes(task).await.map_err(|e| reset_failed("coworkers", e))?;
        let seconds = Value::Object(seconds);
        let deadline = args
            .max_seconds
            .map(|seconds| Deadline { at: Instant::now() + Duration::from_secs(seconds), seconds });
        *episode = Episode {
            id: Some(id),
            task: Some(index),
            shift: Some(shift),
            catalog,
            started: Some(Timestamp::now()),
            reset_s: seconds.clone(),
            deadline,
            workspace: Uuid::new_v4().to_string(),
            coworkers: Coworkers::start(inboxes),
            ..Episode::default()
        };
        span.in_scope(|| tracing::info!(days = shift.timestamps, %seconds, "reset done"));
        Ok((self.reset_observation(task, &seconds, started), print))
    }

    fn pick_task(&self, args: &ResetArgs) -> Result<usize, EnvError> {
        if let Some(id) = &args.task_id {
            return self
                .tasks
                .iter()
                .position(|t| &t.id == id)
                .ok_or_else(|| EnvError::Invalid(format!("unknown task '{id}'")));
        }
        let split = args.split.as_deref().unwrap_or("train");
        if !SPLITS.contains(&split) {
            return Err(EnvError::Invalid(format!("unknown split '{split}'")));
        }
        let drawn =
            |task: &Task| task.split == split && (args.coworkers || task.coworkers.is_empty());
        let candidates: Vec<usize> =
            (0..self.tasks.len()).filter(|&i| drawn(&self.tasks[i])).collect();
        let count = u64::try_from(candidates.len()).unwrap_or(u64::MAX);
        if count == 0 {
            let with = self.tasks.iter().filter(|t| t.split == split && !t.coworkers.is_empty());
            let hint = match with.count() {
                0 => String::new(),
                n => {
                    format!(" without coworkers; reset with coworkers: true for its {n} with them")
                }
            };
            return Err(EnvError::Invalid(format!("split '{split}' has no tasks{hint}")));
        }
        if let Some(index) = args.index {
            return candidates.get(index).copied().ok_or_else(|| {
                EnvError::Invalid(format!("split '{split}' has only {count} tasks, no {index}"))
            });
        }
        let draw = args.seed.unwrap_or_else(|| {
            u64::try_from(Uuid::new_v4().as_u128() % u128::from(count)).unwrap_or(0)
        });
        let pick = usize::try_from(draw % count).unwrap_or(0);
        Ok(candidates[pick])
    }

    fn reset_observation(&self, task: &Task, reset_seconds: &Value, now: Timestamp) -> Value {
        let now = Timestamp::from_second(now.as_second()).unwrap_or(now);
        json!({
            "observation": {
                "task_id": task.id,
                "instruction": task.instruction.trim(),
                "persona": self.persona_json(task),
                "now": now.to_string(),
                "max_steps": task.max_steps,
                "metadata": coworker_names(json!({"reset_s": reset_seconds}), task),
            },
            "reward": null,
            "done": false,
        })
    }

    /// The episode's state frame.
    pub async fn state(&self) -> Value {
        let episode = self.episode.lock().await;
        json!({
            "episode_id": episode.id,
            "step_count": episode.step_count,
            "task_id": episode.task.map(|i| &self.tasks[i].id),
            "done": episode.done,
            "reward": episode.reward,
            "success": episode.success,
        })
    }
}

#[derive(Clone, Copy)]
enum Ending {
    Submit,
    MaxSteps,
    MaxSeconds,
    Void,
    Crash,
}

impl Ending {
    fn as_str(self) -> &'static str {
        match self {
            Ending::Submit => "submit",
            Ending::MaxSteps => "max_steps",
            Ending::MaxSeconds => "max_seconds",
            Ending::Void => "void",
            Ending::Crash => "crash",
        }
    }
}

/// A step: list the tools, or call one.
#[derive(Debug)]
pub enum Action {
    ListTools,
    CallTool { name: String, arguments: Map<String, Value>, episode_id: Option<String> },
}

impl Action {
    /// Parse a step frame's data; null arguments are dropped.
    pub fn parse(data: &Value) -> Result<Action, String> {
        let fields = data.as_object().ok_or("step data must be an object")?;
        let allowed: &[&str] = match fields.get("type").and_then(Value::as_str) {
            Some("list_tools") => &["type", "metadata"],
            Some("call_tool") => &["type", "tool_name", "arguments", "metadata"],
            _ => return Err("step data needs type list_tools or call_tool".into()),
        };
        if let Some(extra) = fields.keys().find(|k| !allowed.contains(&k.as_str())) {
            return Err(format!("unexpected field '{extra}' in step data"));
        }
        if fields["type"] == "list_tools" {
            return Ok(Action::ListTools);
        }
        let name = fields
            .get("tool_name")
            .and_then(Value::as_str)
            .ok_or("call_tool needs a string tool_name")?;
        let mut arguments = match fields.get("arguments") {
            None => Map::new(),
            Some(Value::Object(arguments)) => arguments.clone(),
            Some(_) => return Err("arguments must be an object".into()),
        };
        arguments.retain(|_, value| !value.is_null());
        let episode_id = fields
            .get("metadata")
            .and_then(|metadata| metadata.get("episode_id"))
            .and_then(Value::as_str)
            .map(str::to_string);
        Ok(Action::CallTool { name: name.to_string(), arguments, episode_id })
    }
}

impl Env {
    /// Run a step; the episode ends graded on submit, a step or time limit, or a crash.
    pub async fn step(self: &Arc<Self>, action: Action) -> Result<Value, EnvError> {
        let episode = self.episode.clone().lock_owned().await;
        let env = self.clone();
        tokio::spawn(async move { env.step_holding(episode, action).await })
            .await
            .map_err(|e| EnvError::Other(format!("the step failed: {e}")))?
    }

    async fn step_holding(
        &self,
        mut episode: OwnedMutexGuard<Episode>,
        action: Action,
    ) -> Result<Value, EnvError> {
        let (Some(index), Some(id)) = (episode.task, episode.id.clone()) else {
            return Err(EnvError::Other("no episode is running; send reset first".into()));
        };
        if let Action::CallTool { episode_id: Some(wanted), .. } = &action
            && *wanted != id
        {
            let message = format!("episode '{wanted}' is over; this env is running '{id}'");
            return Err(EnvError::Invalid(message));
        }
        if episode.done {
            return Err(EnvError::Other("the episode is done; send reset to start another".into()));
        }
        let task = &self.tasks[index];
        let (task_id, world) = (&task.id, &task.world);
        let span = tracing::info_span!("episode", episode_id = %id, %task_id, %world);
        let late = episode.deadline.filter(|deadline| {
            matches!(&action, Action::CallTool { .. }) && Instant::now() >= deadline.at
        });
        if late.is_none() {
            episode.step_count += 1;
        }
        let mut observation = match (&action, late) {
            (Action::ListTools, _) => {
                json!({"tools": self.tools.list(false, task.has_clock()), "metadata": {}})
            }
            (Action::CallTool { name, .. }, Some(deadline)) => {
                let seconds = deadline.seconds;
                let message =
                    format!("the episode's {seconds} s ran out; it was graded as it stood");
                let error = json!({"error_type": "timeout", "message": message});
                json!({"tool_name": name, "result": null, "error": error, "metadata": {}})
            }
            (Action::CallTool { name, arguments, .. }, None) => {
                let (result, error) = self
                    .call_tool(name, arguments, task, &mut episode)
                    .instrument(span.clone())
                    .await;
                json!({"tool_name": name, "result": result, "error": error, "metadata": {}})
            }
        };
        let called = |tool: &str| matches!(&action, Action::CallTool { name, .. } if name == tool);
        let ending = if self.supervisor.crash().is_some() {
            Ending::Crash
        } else if called("submit") && observation["error"].is_null() {
            Ending::Submit
        } else if called(WAIT) && self.edb.lost().is_some() {
            Ending::Void
        } else if late.is_some() {
            Ending::MaxSeconds
        } else if matches!(&action, Action::CallTool { .. }) && episode.step_count >= task.max_steps
        {
            Ending::MaxSteps
        } else {
            return Ok(envelope(observation, Some(0.0), false, &json!({})));
        };
        let outcome = self.finish(task, &mut episode, ending).instrument(span).await;
        observation["metadata"] = outcome.metadata.clone();
        Ok(envelope(observation, outcome.reward, true, &outcome.metadata))
    }

    async fn finish(&self, task: &Task, episode: &mut Episode, ending: Ending) -> Outcome {
        let grading = Instant::now();
        let mut outcome = if let Some(crash) = self.supervisor.crash() {
            crashed(task, &crash)
        } else {
            if task.has_clock() && self.edb.lost().is_none() {
                self.run_out(task, &mut episode.clock, &episode.catalog).await;
            }
            let outcome = self.grade(task, &episode.catalog, &episode.clock.scored).await;
            self.supervisor.crash().map_or(outcome, |crash| crashed(task, &crash))
        };
        tracing::info!(reward = outcome.reward, success = outcome.success, "episode done");
        let crashed = outcome.metadata["crashed"].is_object();
        let termination = if crashed { Ending::Crash } else { ending };
        outcome.metadata["termination"] = json!(termination.as_str());
        outcome.metadata["world"] = json!(task.world);
        outcome.metadata["task_hash"] = json!(task.hash);
        outcome.metadata["manifest_hash"] = json!(self.manifest_hash);
        outcome.metadata["miroir_version"] = json!(env!("CARGO_PKG_VERSION"));
        if episode.shift != Some(self.shift()) {
            outcome.metadata["clock_rolled"] = Value::Bool(true);
        }
        if task.has_clock() {
            let fired = episode.clock.fired.iter();
            let events: Vec<Value> = fired
                .map(|f| json!({"name": f.name, "minute": f.minute, "status": f.status}))
                .collect();
            outcome.metadata["clock"] = json!({"minutes": episode.clock.minute, "events": events});
        }
        let grading_s = rounded_seconds(grading.elapsed());
        let effects_seq = self.edb.last_effect().await.unwrap_or_else(|e| {
            tracing::warn!(
                "cannot count the effects, so the record keeps any that come later: {e}"
            );
            i64::MAX
        });
        episode.done = true;
        episode.reward = outcome.reward;
        episode.success = Some(outcome.success);
        let metadata = outcome.metadata.clone();
        episode.ended = Some(Ended { metadata, at: Timestamp::now(), grading_s, effects_seq });
        outcome
    }

    async fn call_tool(
        &self,
        name: &str,
        arguments: &Map<String, Value>,
        task: &Task,
        episode: &mut Episode,
    ) -> (Value, Value) {
        let persona = task.persona.as_deref().and_then(|p| self.manifest.personas.get(p));
        let dates = episode.shift.map_or(0, |shift| shift.dates);
        let started = Instant::now();
        let outcome = match name {
            "api_request" => match persona {
                Some(persona) => self.tools.api_request(arguments, persona).await,
                None => Err(ToolError {
                    error_type: "execution_error",
                    message: "the task has no persona".into(),
                }),
            },
            "api_docs" => self.tools.api_docs(arguments),
            "submit" => self.submit(arguments, task, dates).await,
            "run_code" => self.run_code(arguments, task, &episode.workspace).await,
            WAIT => self.wait(arguments, task, &mut episode.clock, &episode.catalog).await,
            _ => self.call_mcp(name, arguments, persona).await,
        };
        let latency_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
        let text = |key: &str| arguments.get(key).and_then(Value::as_str);
        let (app, method, path) = match name {
            "api_request" => (text("app"), text("method"), text("path")),
            "api_docs" => (text("app"), None, None),
            "run_code" | WAIT => (None, None, None),
            _ => (self.mcp_app(name), None, None),
        };
        let status = outcome.as_ref().ok().and_then(|r| r.get("status")).and_then(Value::as_u64);
        let (result, error) = match outcome {
            Ok(result) => (result, Value::Null),
            Err(e) => (Value::Null, e.to_json()),
        };
        let recorded = if error.is_null() { &result } else { &error };
        self.edb
            .add_action(&ActionRow {
                tool: name,
                app,
                method,
                path,
                status,
                latency_ms,
                arguments: &Value::Object(arguments.clone()),
                result: recorded,
                via: None,
            })
            .await;
        (result, error)
    }

    /// Send a request as someone other than the agent, recorded `via` them.
    pub(crate) async fn send_as(
        &self,
        persona: &Persona,
        request: &Map<String, Value>,
        tool: &str,
        via: &str,
        arguments: Value,
    ) -> (Option<u64>, Option<String>) {
        let started = Instant::now();
        let outcome = self.tools.api_request(request, persona).await;
        let latency_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
        let status = outcome.as_ref().ok().and_then(|r| r.get("status")).and_then(Value::as_u64);
        let (recorded, failed) = match &outcome {
            Ok(_) => {
                let failed = status.filter(|code| *code >= 500).map(|code| format!("HTTP {code}"));
                (json!({"status": status}), failed)
            }
            Err(e) => (e.to_json(), Some(e.message.clone())),
        };
        let text = |key: &str| request.get(key).and_then(Value::as_str);
        self.edb
            .add_action(&ActionRow {
                tool,
                app: text("app"),
                method: text("method"),
                path: text("path"),
                status,
                latency_ms,
                arguments: &arguments,
                result: &recorded,
                via: Some(via),
            })
            .await;
        (status, failed)
    }

    /// The tool error of a failure, voiding the grade when the env is at fault.
    pub(crate) fn fail(&self, failure: Failure) -> ToolError {
        if failure.void {
            self.edb.lose(&failure.error.message);
        }
        failure.error
    }

    /// Whether to list `wait`: the named episode's task has a clock, or else any task does.
    pub async fn offers_wait(&self, episode_id: Option<&str>) -> bool {
        let episode = self.episode.lock().await;
        let named = episode.id.as_deref().filter(|id| episode_id == Some(*id));
        match (named, episode.task) {
            (Some(_), Some(index)) => self.tasks[index].has_clock(),
            _ => self.tasks.iter().any(Task::has_clock),
        }
    }

    async fn submit(
        &self,
        arguments: &Map<String, Value>,
        task: &Task,
        dates: i64,
    ) -> Result<Value, ToolError> {
        if let Some(extra) = arguments.keys().find(|k| *k != "answer") {
            return Err(ToolError::invalid(format!("unexpected argument '{extra}'")));
        }
        let answer = match arguments.get("answer") {
            None => "",
            Some(Value::String(answer)) => answer.as_str(),
            Some(_) => return Err(ToolError::invalid("answer must be a string")),
        };
        let answer = match task.answer {
            Some(form) => form.normalize(answer, dates).map_err(ToolError::invalid)?,
            None => answer.to_string(),
        };
        self.edb
            .set_answer(&answer)
            .await
            .map_err(|e| ToolError { error_type: "execution_error", message: e })?;
        Ok(json!({"submitted": true}))
    }

    /// Net changes since the reset.
    pub async fn changes_so_far(&self) -> Result<Vec<NetChange>, String> {
        let catalog = self.episode.lock().await.catalog.clone();
        self.changes_since(&catalog).await
    }

    /// The coworkers who owe a reply, each with the chat to answer.
    pub async fn coworker_turns(&self) -> Result<Value, EnvError> {
        let mut episode = self.episode.lock().await;
        let Some(index) = episode.task else {
            return Err(EnvError::Other("no episode is running; send reset first".into()));
        };
        if episode.done {
            return Ok(json!({"turns": []}));
        }
        self.owed_turns(&self.tasks[index], &mut episode.coworkers).await
    }

    /// Post a coworker's reply in the app.
    pub async fn coworker_reply(
        self: &Arc<Self>,
        coworker: String,
        text: String,
    ) -> Result<Value, EnvError> {
        let mut episode = self.episode.clone().lock_owned().await;
        let env = self.clone();
        let replying = async move {
            let Some(index) = episode.task else {
                return Err(EnvError::Other("no episode is running; send reset first".into()));
            };
            if episode.done {
                let message = "the episode is done; coworkers answer only while it runs";
                return Err(EnvError::Invalid(message.into()));
            }
            env.send_reply(&env.tasks[index], &mut episode.coworkers, &coworker, &text).await
        };
        tokio::spawn(replying)
            .await
            .map_err(|e| EnvError::Other(format!("the reply failed: {e}")))?
    }

    pub(crate) fn persona_description(&self, task: &Task) -> &str {
        let persona = task.persona.as_deref().unwrap_or_default();
        self.manifest.personas.get(persona).map_or("", |p| p.description.as_str())
    }

    pub(crate) fn persona_json(&self, task: &Task) -> Value {
        let name = task.persona.as_deref().unwrap_or_default();
        json!({"name": name, "description": self.persona_description(task)})
    }

    /// The finished episode's record: calls, changes, effects, signature, grade and
    /// timings.
    pub async fn record(&self, episode_id: Option<&str>) -> Result<Value, EnvError> {
        let episode = self.episode.lock().await;
        let (Some(index), Some(id)) = (episode.task, episode.id.as_deref()) else {
            return Err(EnvError::Invalid("no episode has run since the last reset".into()));
        };
        if let Some(wanted) = episode_id.filter(|wanted| *wanted != id) {
            let message = format!("episode '{wanted}' is over; this env is running '{id}'");
            return Err(EnvError::Invalid(message));
        }
        let (Some(started), Some(ended)) = (episode.started, &episode.ended) else {
            return Err(EnvError::Invalid("the record is ready once the episode is done".into()));
        };
        let task = &self.tasks[index];
        let unread = |e: sqlx::Error| EnvError::Other(format!("cannot read the episode DB: {e}"));
        let actions = self.edb.actions().await.map_err(unread)?;
        let changes = self.edb.changes().await.map_err(unread)?;
        let window = Window::new(started, &episode.clock.moves, ended.at);
        let mut builder = record::Builder::new(window, &actions, &changes);
        let mut after = 0;
        loop {
            let page = self.edb.effects(after, ended.effects_seq).await.map_err(unread)?;
            let Some((last, _, _)) = page.last() else { break };
            after = *last;
            for (_, kind, data) in &page {
                builder.effect(kind, data);
            }
        }
        let answer = if task.asks_for_answer() {
            Some(self.edb.answer().await.unwrap_or_default())
        } else {
            None
        };
        let graded = ended.metadata["fault"].is_null() && ended.metadata["invalid"].is_null();
        let mut record = builder.finish(answer.as_deref(), graded);
        let episode_s = Duration::try_from(ended.at.duration_since(started)).unwrap_or_default();
        record["episode_id"] = json!(id);
        record["task_id"] = json!(task.id);
        record["reward"] = json!(episode.reward);
        record["null_max"] = json!(task.null_max);
        record["metadata"] = ended.metadata.clone();
        record["timings"] = json!({
            "reset_s": episode.reset_s,
            "episode_s": rounded_seconds(episode_s),
            "grading_s": ended.grading_s,
        });
        Ok(record)
    }
}

fn envelope(observation: Value, reward: Option<f64>, done: bool, metadata: &Value) -> Value {
    let mut data = Map::new();
    data.insert("observation".into(), observation);
    data.insert("reward".into(), json!(reward));
    data.insert("done".into(), Value::Bool(done));
    if metadata.as_object().is_some_and(|m| !m.is_empty()) {
        data.insert("metadata".into(), metadata.clone());
    }
    Value::Object(data)
}

/// Add the names of the task's coworkers to a listing or metadata.
pub fn coworker_names(mut value: Value, task: &Task) -> Value {
    if !task.coworkers.is_empty() {
        let names: Vec<&str> = task.coworkers.iter().map(|c| c.name.as_str()).collect();
        value["coworkers"] = json!(names);
    }
    value
}
