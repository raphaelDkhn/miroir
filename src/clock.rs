//! The task clock: `wait` moves the data into the past as if time passed, fires the events
//! due, and scores milestones.

use std::collections::{BTreeMap, BTreeSet};

use jiff::Timestamp;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::db::Move;
use crate::env::{Catalog, Env, labelled};
use crate::grade::Unread;
use crate::manifest::{Manifest, Persona, ensure, is_short_name};
use crate::task::{Answer, Check, GoldenStep, Task};
use crate::tools::ToolError;

const WAIT_QUIET_FAULT: &str = "the quiet command never passed before a wait";

/// Minutes in a day.
pub const DAY: u32 = 1_440;
/// What an event's request is recorded as.
pub const EVENT: &str = "event";

/// The waiting tool's name.
pub const WAIT: &str = "wait";
/// Longest single wait: a week.
pub const MAX_WAIT_MINUTES: u32 = 10_080;
/// Latest clock minute: a year.
pub const MAX_CLOCK_MINUTES: u32 = 525_600;

/// A request sent at a clock minute, as someone other than the agent.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Event {
    pub name: String,
    /// Clock minute it is sent at.
    pub at: u32,
    /// Headers of whoever sends it, by app.
    #[serde(default)]
    pub headers: BTreeMap<String, BTreeMap<String, String>>,
    /// The request: `app`, `method`, `path` and `body`.
    pub request: Map<String, Value>,
}

impl Event {
    pub fn persona(&self) -> Persona {
        Persona { description: String::new(), headers: self.headers.clone() }
    }
}

/// Check a task's events, and that its clock can run in this env.
pub fn validate(manifest: &Manifest, task: &Task) -> Result<(), String> {
    let mut names = BTreeSet::new();
    for event in &task.events {
        let at = format!("event '{}'", event.name);
        ensure(
            is_short_name(&event.name),
            format!("{at}: a name is 1 to 64 lowercase letters, digits, '_' or '-'"),
        )?;
        ensure(names.insert(event.name.as_str()), format!("{at} is listed twice"))?;
        ensure(
            (1..=MAX_CLOCK_MINUTES).contains(&event.at),
            format!("{at}: at must be a minute from 1 to {MAX_CLOCK_MINUTES}"),
        )?;
        manifest.validate_headers(&at, &event.headers)?;
        manifest.validate_request(&format!("{at} request"), &event.request)?;
    }
    if !task.has_clock() {
        return Ok(());
    }
    ensure(
        task.answer != Some(Answer::Date),
        "a task with a clock can't take a date answer: waits move the dates, and submit's \
         normal form doesn't follow them",
    )?;
    ensure(
        manifest.time.hook.is_none(),
        "a task with a clock can't run in an env with time.hook: a wait moves the databases, \
         not what the hook moves",
    )
}

pub fn validate_waits(what: &str, steps: &[GoldenStep], clock: bool) -> Result<(), String> {
    for (i, step) in steps.iter().enumerate().filter(|(_, step)| step.tool == WAIT) {
        let at = format!("{what} step {}", i + 1);
        ensure(
            clock,
            format!("{at}: {WAIT} needs a task with a clock: events, or a check with by"),
        )?;
        let minutes = step.arguments.get("minutes").and_then(Value::as_u64);
        let whole = minutes.is_some_and(|m| (1..=u64::from(MAX_WAIT_MINUTES)).contains(&m));
        ensure(
            whole && step.arguments.len() == 1,
            format!(
                "{at}: {WAIT} takes exactly minutes, a whole number from 1 to {MAX_WAIT_MINUTES}"
            ),
        )?;
    }
    Ok(())
}

/// The episode's clock: its minute, when it moved, the events fired and the
/// milestones scored.
#[derive(Debug, Default)]
pub struct Clock {
    pub minute: u32,
    pub moves: Vec<(Timestamp, u32)>,
    pub fired: Vec<Fired>,
    pub scored: BTreeMap<usize, (Value, f64)>,
}

/// An event that fired, and the status it got.
#[derive(Debug)]
pub struct Fired {
    pub name: String,
    pub minute: u32,
    pub status: Option<u64>,
}

/// A minute at which milestones are scored or events fire.
#[derive(Debug, PartialEq, Eq)]
pub struct Stop {
    pub minute: u32,
    pub milestones: Vec<usize>,
    pub events: Vec<usize>,
}

/// The stops after `from` up to `to`; with `wake`, only up to the first with events.
pub fn stops(task: &Task, from: u32, to: u32, wake: bool) -> Vec<Stop> {
    let mut due: BTreeMap<u32, Stop> = BTreeMap::new();
    for (index, check) in task.checks.iter().enumerate() {
        if let Some(by) = check.by.filter(|by| from < *by && *by <= to) {
            stop_at(&mut due, by).milestones.push(index);
        }
    }
    for (index, event) in task.events.iter().enumerate() {
        if from < event.at && event.at <= to {
            stop_at(&mut due, event.at).events.push(index);
        }
    }
    let mut stops = Vec::new();
    for stop in due.into_values() {
        let wakes = wake && !stop.events.is_empty();
        stops.push(stop);
        if wakes {
            break;
        }
    }
    stops
}

fn stop_at(due: &mut BTreeMap<u32, Stop>, minute: u32) -> &mut Stop {
    due.entry(minute).or_insert_with(|| Stop { minute, milestones: Vec::new(), events: Vec::new() })
}

/// Minute of the last event or milestone, which the clock runs to before grading.
pub fn end(task: &Task) -> u32 {
    let milestones = task.checks.iter().filter_map(|check| check.by);
    task.events.iter().map(|event| event.at).chain(milestones).max().unwrap_or(0)
}

/// How far the data moves back when the clock goes from `from` to `to`.
pub fn move_between(from: u32, to: u32) -> Move {
    Move {
        seconds: -60 * (i64::from(to) - i64::from(from)),
        dates: -(i64::from(to / DAY) - i64::from(from / DAY)),
    }
}

impl Env {
    /// The `wait` tool.
    pub(crate) async fn wait(
        &self,
        arguments: &Map<String, Value>,
        task: &Task,
        clock: &mut Clock,
        catalog: &Catalog,
    ) -> Result<Value, ToolError> {
        if !task.has_clock() {
            let message = "unknown tool 'wait': this task has no clock, so no time passes in it";
            return Err(ToolError { error_type: "tool_not_found", message: message.into() });
        }
        let minutes = wait_minutes(arguments)?;
        self.quiet_for_the_clock().await;
        let from = clock.minute;
        let until = from.saturating_add(minutes);
        let reached = self.run_clock(task, clock, catalog, until, true).await?;
        Ok(json!({"waited_minutes": reached - from, "elapsed_minutes": reached}))
    }

    /// Run the clock to its end before grading.
    pub(crate) async fn run_out(&self, task: &Task, clock: &mut Clock, catalog: &Catalog) {
        self.quiet_for_the_clock().await;
        if self.run_clock(task, clock, catalog, end(task), false).await.is_err() {
            tracing::warn!("the clock could not run out, so the episode has no grade");
        }
    }

    async fn run_clock(
        &self,
        task: &Task,
        clock: &mut Clock,
        catalog: &Catalog,
        until: u32,
        wake: bool,
    ) -> Result<u32, ToolError> {
        for stop in stops(task, clock.minute, until, wake) {
            self.move_clock(clock, stop.minute).await?;
            if !stop.milestones.is_empty() {
                self.score_milestones(task, catalog, clock, &stop.milestones).await;
            }
            for &index in &stop.events {
                self.fire(task, index, clock).await?;
            }
            if !stop.events.is_empty() {
                self.quiet_for_the_clock().await;
                if wake {
                    return Ok(clock.minute);
                }
            }
        }
        if clock.minute < until {
            self.move_clock(clock, until).await?;
        }
        Ok(clock.minute)
    }

    async fn score_milestones(
        &self,
        task: &Task,
        catalog: &Catalog,
        clock: &mut Clock,
        due: &[usize],
    ) {
        let checks: Vec<&Check> = due.iter().map(|&index| &task.checks[index]).collect();
        let scored = match self.score(catalog, &checks).await {
            Ok((_, scored)) => scored,
            Err(Unread::Invalid(why)) => {
                self.edb.lose(&why);
                return;
            }
            Err(Unread::Fault(fault)) => checks
                .iter()
                .map(|check| {
                    let report =
                        json!({"name": check.name, "by": check.by, "error": fault, "score": 0.0});
                    (report, 0.0)
                })
                .collect(),
        };
        for (&index, frozen) in due.iter().zip(scored) {
            clock.scored.insert(index, frozen);
        }
    }

    async fn quiet_for_the_clock(&self) {
        if self.supervisor.wait_quiet(self.manifest.quiet.as_ref()).await.is_err() {
            self.edb.refuse(WAIT_QUIET_FAULT.into());
        }
    }

    async fn fire(&self, task: &Task, index: usize, clock: &mut Clock) -> Result<(), ToolError> {
        let event = &task.events[index];
        let arguments = json!({"event": event.name});
        let (status, failed) =
            self.send_as(&event.persona(), &event.request, EVENT, EVENT, arguments).await;
        clock.fired.push(Fired { name: event.name.clone(), minute: clock.minute, status });
        let Some(why) = failed else { return Ok(()) };
        let message = format!("event '{}' failed: {why}", event.name);
        self.edb.lose(&message);
        Err(ToolError { error_type: "execution_error", message })
    }

    async fn move_clock(&self, clock: &mut Clock, to: u32) -> Result<(), ToolError> {
        let by = move_between(clock.minute, to);
        for (app, db) in &self.dbs {
            if let Err(e) = db.age(by).await {
                let message = format!("the clock could not move: {}", labelled(&self.dbs, app, e));
                self.edb.lose(&message);
                return Err(ToolError { error_type: "execution_error", message });
            }
        }
        clock.minute = to;
        clock.moves.push((Timestamp::now(), to));
        Ok(())
    }
}

fn wait_minutes(arguments: &Map<String, Value>) -> Result<u32, ToolError> {
    if let Some(extra) = arguments.keys().find(|key| *key != "minutes") {
        return Err(ToolError::invalid(format!("unexpected argument '{extra}'")));
    }
    let minutes = arguments.get("minutes").and_then(Value::as_u64);
    minutes
        .and_then(|minutes| u32::try_from(minutes).ok())
        .filter(|minutes| (1..=MAX_WAIT_MINUTES).contains(minutes))
        .ok_or_else(|| {
            ToolError::invalid(format!(
                "minutes must be a whole number from 1 to {MAX_WAIT_MINUTES}"
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(events: &[(&str, u32)], milestones: &[u32]) -> Task {
        let listed: Vec<String> = events
            .iter()
            .map(|(name, at)| {
                format!("  - {{name: {name}, at: {at}, request: {{method: GET, path: /x}}}}")
            })
            .collect();
        let checks: Vec<String> = milestones
            .iter()
            .enumerate()
            .map(|(i, by)| format!("  - {{name: m{i}, weight: 0.5, by: {by}, sql: SELECT 1}}"))
            .collect();
        let yaml = format!(
            "instruction: x\nevents:\n{}\nchecks:\n  - {{name: a, weight: 1, sql: SELECT 1}}\n\
             {}\nany_change: true\ngolden: [{{tool: submit}}]\n",
            listed.join("\n"),
            checks.join("\n")
        );
        serde_saphyr::from_str(&yaml).unwrap()
    }

    fn stop(minute: u32, milestones: &[usize], events: &[usize]) -> Stop {
        Stop { minute, milestones: milestones.to_vec(), events: events.to_vec() }
    }

    #[test]
    fn a_wait_wakes_at_the_first_minute_events_are_due() {
        let task = task(&[("late", 200), ("first", 30), ("one", 90), ("two", 90)], &[]);
        assert_eq!(stops(&task, 0, 100, false), [stop(30, &[], &[1]), stop(90, &[], &[2, 3])]);
        assert_eq!(stops(&task, 0, 100, true), [stop(30, &[], &[1])], "a wait wakes at the first");
        assert_eq!(stops(&task, 30, 100, true), [stop(90, &[], &[2, 3])], "what fired is past");
        assert_eq!(stops(&task, 90, 199, true), [], "nothing more until 200");
        assert_eq!(stops(&task, 0, 200, false).len(), 3);
        assert_eq!(end(&task), 200);
    }

    #[test]
    fn milestones_come_before_the_events_due_with_them_and_never_wake_a_wait() {
        let task = task(&[("first", 30), ("second", 300)], &[30, 200, 500]);
        assert_eq!(stops(&task, 0, 1_000, true), [stop(30, &[1], &[0])], "scored, then fired");
        assert_eq!(
            stops(&task, 30, 1_000, true),
            [stop(200, &[2], &[]), stop(300, &[], &[1])],
            "a milestone alone doesn't wake it"
        );
        assert_eq!(stops(&task, 300, 1_000, false), [stop(500, &[3], &[])]);
        assert_eq!(end(&task), 500, "the last milestone comes after the last event");
        assert_eq!(end(&self::task(&[], &[])), 0);
    }

    #[test]
    fn dates_move_at_whole_days_of_the_clock_whatever_the_time_of_day() {
        assert_eq!(move_between(0, 720), Move { seconds: -43_200, dates: 0 }, "half a day");
        assert_eq!(move_between(720, 1_440), Move { seconds: -43_200, dates: -1 }, "and the rest");
        assert_eq!(move_between(1_439, 1_441), Move { seconds: -120, dates: -1 });
        assert_eq!(move_between(100, 3_000), Move { seconds: -174_000, dates: -2 });
        let halves = [move_between(0, 720), move_between(720, 1_440)];
        assert_eq!(halves.iter().map(|m| m.dates).sum::<i64>(), move_between(0, 1_440).dates);
    }
}
