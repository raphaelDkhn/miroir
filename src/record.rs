//! The episode record: calls, changes and effects within a size budget, and a signature
//! that identifies the end state.

use std::collections::{BTreeMap, BTreeSet};

use jiff::civil::{Date, DateTime};
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};
use serde_json::{Map, Value, json};

use crate::clock::{DAY, EVENT};
use crate::edb::{StoredAction, StoredChange};
use crate::manifest::content_hash;

/// Most bytes of arguments, results, changes and effects a record keeps.
pub const RECORD_LIMIT: usize = 4 * 1024 * 1024;
const CLOCK_SLACK: SignedDuration = SignedDuration::from_secs(60);

#[derive(Clone, Copy)]
struct Stretch {
    from: Timestamp,
    to: Timestamp,
    first_day: Date,
    last_day: Date,
    minute: u32,
}

impl Stretch {
    fn new(from: Timestamp, to: Timestamp, minute: u32, last: u32) -> Stretch {
        let back = SignedDuration::from_mins(i64::from(last.saturating_sub(minute)));
        let days_back = i64::from(last / DAY) - i64::from(minute / DAY);
        let from = from.checked_sub(CLOCK_SLACK).unwrap_or(from);
        let to = to.checked_add(CLOCK_SLACK).unwrap_or(to);
        let day = |at: Timestamp| {
            let date = at.to_zoned(TimeZone::UTC).date();
            let span = jiff::Span::new().try_days(days_back).ok();
            span.and_then(|span| date.checked_sub(span).ok()).unwrap_or(date)
        };
        Stretch {
            from: from.checked_sub(back).unwrap_or(from),
            to: to.checked_sub(back).unwrap_or(to),
            first_day: day(from),
            last_day: day(to),
            minute,
        }
    }

    fn holds_epoch(&self, n: i64) -> bool {
        let (from, to) = (self.from.as_second(), self.to.as_second());
        let millis = from.saturating_mul(1000)..=to.saturating_mul(1000).saturating_add(999);
        (from..=to).contains(&n) || millis.contains(&n)
    }

    fn now(&self) -> String {
        match self.minute {
            0 => "<now>".into(),
            minute => format!("<now+{minute}>"),
        }
    }

    fn today(&self) -> String {
        match self.minute / DAY {
            0 => "<today>".into(),
            day => format!("<today+{day}>"),
        }
    }
}

/// The episode's span in real and clock time, to read the times it wrote as tokens.
#[derive(Clone)]
pub struct Window {
    stretches: Vec<Stretch>,
    real: Stretch,
}

impl Window {
    pub fn new(started: Timestamp, moves: &[(Timestamp, u32)], ended: Timestamp) -> Window {
        let last = moves.last().map_or(0, |(_, minute)| *minute);
        let starts = std::iter::once((started, 0)).chain(moves.iter().copied());
        let ends = moves.iter().map(|(at, _)| *at).chain(std::iter::once(ended));
        let stretches = starts
            .zip(ends)
            .map(|((from, minute), to)| Stretch::new(from, to, minute, last))
            .collect();
        Window { stretches, real: Stretch::new(started, ended, 0, 0) }
    }
}

fn token(stretches: &[Stretch], text: &str) -> Option<String> {
    if text.len() <= "YYYY-MM-DD".len() {
        let date: Date = text.parse().ok()?;
        let held = stretches.iter().find(|s| (s.first_day..=s.last_day).contains(&date));
        return held.map(Stretch::today);
    }
    let at = match text.parse::<Timestamp>() {
        Ok(at) => at,
        Err(_) => TimeZone::UTC.to_timestamp(text.parse::<DateTime>().ok()?).ok()?,
    };
    stretches.iter().find(|s| s.from <= at && at <= s.to).map(Stretch::now)
}

fn normalize(value: &mut Value, stretches: &[Stretch]) {
    match value {
        Value::String(text) => {
            if let Some(token) = token(stretches, text) {
                *text = token;
            }
        }
        Value::Number(number) => {
            let held = number.as_i64().and_then(|n| stretches.iter().find(|s| s.holds_epoch(n)));
            if let Some(stretch) = held {
                *value = Value::String(stretch.now());
            }
        }
        Value::Array(items) => {
            for item in items {
                normalize(item, stretches);
            }
        }
        Value::Object(fields) => {
            for item in fields.values_mut() {
                normalize(item, stretches);
            }
        }
        Value::Null | Value::Bool(_) => {}
    }
}

/// A hash of the end state, with the episode's times as tokens, so rollouts that end
/// alike share it.
pub struct Signature {
    window: Window,
    items: Vec<String>,
}

impl Signature {
    pub fn new(window: Window) -> Signature {
        Signature { window, items: Vec::new() }
    }

    pub fn change(&mut self, change: &Value) {
        let mut key = change_key(change);
        normalize(&mut key, &self.window.stretches);
        self.items.push(content_hash(format!("change {key}").as_bytes()));
    }

    pub fn effect(&mut self, kind: &str, data: &Value) {
        let mut data = data.clone();
        if kind == "http"
            && let Some(fields) = data.as_object_mut()
        {
            fields.remove("headers");
        }
        normalize(&mut data, std::slice::from_ref(&self.window.real));
        self.items.push(content_hash(format!("effect {kind} {data}").as_bytes()));
    }

    pub fn finish(mut self, answer: Option<&str>) -> String {
        self.items.sort_unstable();
        let mut text = self.items.join("\n");
        if let Some(answer) = answer {
            text.push_str("\nanswer ");
            text.push_str(&answer.split_whitespace().collect::<Vec<_>>().join(" "));
        }
        content_hash(text.as_bytes())
    }
}

fn change_key(change: &Value) -> Value {
    let (tbl, op) = (&change["tbl"], change["op"].as_str().unwrap_or_default());
    let mut key = match op {
        "update" => {
            let columns = change["columns"].as_array().map_or(&[][..], Vec::as_slice);
            let set: Map<String, Value> = columns
                .iter()
                .filter_map(Value::as_str)
                .map(|column| (column.to_string(), change["after"][column].clone()))
                .collect();
            json!({"tbl": tbl, "op": op, "pk": change["pk"], "set": set})
        }
        "insert" => json!({"tbl": tbl, "op": op, "row": change["after"]}),
        "delete" if change["pk"].as_object().is_some_and(|pk| !pk.is_empty()) => {
            json!({"tbl": tbl, "op": op, "pk": change["pk"]})
        }
        "delete" => json!({"tbl": tbl, "op": op, "row": change["before"]}),
        _ => change.clone(),
    };
    if let (Some(app), Some(fields)) = (change.get("app"), key.as_object_mut()) {
        fields.insert("app".into(), app.clone());
    }
    key
}

/// Counts of the agent's calls, client and server errors, repeats and docs lookups.
pub fn diagnostics(actions: &[StoredAction]) -> Value {
    let mut seen = BTreeSet::new();
    let (mut client_errors, mut server_errors, mut repeated, mut docs) = (0_u32, 0_u32, 0, 0);
    let (mut code_requests, mut coworker_replies, mut events) = (0, 0, 0);
    for action in actions {
        match action.via.as_deref() {
            Some("coworker") => {
                coworker_replies += 1;
                continue;
            }
            Some(EVENT) => {
                events += 1;
                continue;
            }
            Some(_) | None => {}
        }
        match action.status {
            Some(400..=499) => client_errors += 1,
            Some(500..) => server_errors += 1,
            Some(_) | None => {}
        }
        if action.via.is_some() {
            code_requests += 1;
            continue;
        }
        docs += usize::from(action.tool == "api_docs");
        repeated += usize::from(!seen.insert((action.tool.as_str(), action.arguments.as_str())));
    }
    json!({
        "calls": actions.len() - code_requests - coworker_replies - events,
        "client_errors": client_errors,
        "server_errors": server_errors,
        "repeated_calls": repeated,
        "docs_calls": docs,
        "code_requests": code_requests,
        "coworker_replies": coworker_replies,
        "events": events,
    })
}

struct Budget {
    left: usize,
    cut: BTreeMap<&'static str, u64>,
}

impl Budget {
    fn take(&mut self, what: &'static str, bytes: usize) -> bool {
        if bytes <= self.left {
            self.left -= bytes;
            return true;
        }
        *self.cut.entry(what).or_default() += 1;
        false
    }

    fn fit(&mut self, what: &'static str, text: &str) -> Value {
        if self.take(what, text.len()) { parse(text) } else { json!({"omitted_bytes": text.len()}) }
    }
}

fn parse(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_string()))
}

/// Builds a record within `RECORD_LIMIT`, cutting results first.
pub struct Builder<'a> {
    actions: &'a [StoredAction],
    arguments: Vec<Value>,
    changes: Vec<Value>,
    effects: Vec<Value>,
    signature: Signature,
    budget: Budget,
}

impl<'a> Builder<'a> {
    pub fn new(
        window: Window,
        actions: &'a [StoredAction],
        changes: &[StoredChange],
    ) -> Builder<'a> {
        let mut budget = Budget { left: RECORD_LIMIT, cut: BTreeMap::new() };
        let mut arguments = vec![Value::Null; actions.len()];
        for (what, code) in [("arguments", false), ("code_arguments", true)] {
            for (slot, action) in arguments.iter_mut().zip(actions) {
                if (action.via.as_deref() == Some("run_code")) == code {
                    *slot = budget.fit(what, &action.arguments);
                }
            }
        }
        let mut signature = Signature::new(window);
        let mut kept = Vec::new();
        for change in changes {
            let texts = [
                Some(&change.pk),
                change.before.as_ref(),
                change.after.as_ref(),
                change.columns.as_ref(),
            ];
            let bytes = texts.iter().flatten().map(|text| text.len()).sum();
            let mut value = json!({
                "tbl": change.tbl,
                "op": change.op,
                "pk": parse(&change.pk),
                "before": change.before.as_deref().map(parse),
                "after": change.after.as_deref().map(parse),
                "columns": change.columns.as_deref().map(parse),
            });
            if let Some(app) = &change.app {
                value["app"] = json!(app);
            }
            signature.change(&value);
            if budget.take("changes", bytes) {
                kept.push(value);
            }
        }
        Builder { actions, arguments, changes: kept, effects: Vec::new(), signature, budget }
    }

    pub fn effect(&mut self, kind: &str, data: &str) {
        let parsed = parse(data);
        self.signature.effect(kind, &parsed);
        if self.budget.take("effects", data.len()) {
            self.effects.push(json!({"kind": kind, "data": parsed}));
        }
    }

    /// The record; it has a signature only when the episode was graded.
    pub fn finish(self, answer: Option<&str>, graded: bool) -> Value {
        let Builder { actions, arguments, changes, effects, signature, mut budget } = self;
        let calls: Vec<Value> = actions
            .iter()
            .zip(arguments)
            .map(|(action, arguments)| {
                let result = action.result.as_deref().map(|text| budget.fit("results", text));
                let mut call = json!({
                    "tool": action.tool,
                    "method": action.method,
                    "path": action.path,
                    "status": action.status,
                    "latency_ms": action.latency_ms,
                    "arguments": arguments,
                    "result": result,
                });
                if let Some(app) = &action.app {
                    call["app"] = json!(app);
                }
                if let Some(via) = &action.via {
                    call["via"] = json!(via);
                }
                call
            })
            .collect();
        let mut record = json!({
            "signature": graded.then(|| signature.finish(answer)),
            "diagnostics": diagnostics(actions),
            "actions": calls,
            "changes": changes,
            "effects": effects,
            "limit_bytes": RECORD_LIMIT,
        });
        if !budget.cut.is_empty() {
            record["truncated"] = json!(budget.cut);
        }
        record
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> Timestamp {
        text.parse().unwrap()
    }

    fn window_on(day: &str) -> Window {
        Window::new(at(&format!("{day}T10:00:00Z")), &[], at(&format!("{day}T10:05:00Z")))
    }

    fn signature(window: &Window, changes: &[Value], effects: &[(&str, Value)]) -> String {
        let mut signature = Signature::new(window.clone());
        for change in changes {
            signature.change(change);
        }
        for (kind, data) in effects {
            signature.effect(kind, data);
        }
        signature.finish(None)
    }

    fn assigned(assignee: i64, updated: i64) -> Value {
        json!({"tbl": "issue", "op": "update", "pk": {"id": 36},
               "before": {"id": 36, "assignee_id": 0, "updated_unix": 1_600_000_000},
               "after": {"id": 36, "assignee_id": assignee, "updated_unix": updated},
               "columns": ["assignee_id", "updated_unix"]})
    }

    fn commented(created: [&str; 3], millis: i64) -> Value {
        json!({"tbl": "comment", "op": "insert", "pk": {"id": 41}, "before": null,
               "after": {"id": 41, "issue_id": 36, "content": "Done", "created_at": created[0],
                         "edited_at": created[1], "mysql_at": created[2], "seen_ms": millis},
               "columns": null})
    }

    #[test]
    fn rollouts_that_end_alike_share_a_signature_whatever_their_clock_wrote() {
        let window = window_on("2026-11-04");
        let first = [
            assigned(6, at("2026-11-04T10:00:31Z").as_second()),
            commented(
                [
                    "2026-11-04T10:01:02.123456+00:00",
                    "2026-11-04T10:01:02.123456",
                    "2026-11-04 10:01:02.000000",
                ],
                at("2026-11-04T10:01:02Z").as_millisecond(),
            ),
        ];
        let second = [
            commented(
                ["2026-11-04T10:04:59Z", "2026-11-04T10:04:59", "2026-11-04 10:04:59"],
                at("2026-11-04T10:04:59Z").as_millisecond() + 999,
            ),
            assigned(6, at("2026-11-04T09:59:30Z").as_second()),
        ];
        assert_eq!(signature(&window, &first, &[]), signature(&window, &second, &[]));
        let other_assignee =
            [assigned(7, at("2026-11-04T10:00:31Z").as_second()), first[1].clone()];
        assert_ne!(signature(&window, &first, &[]), signature(&window, &other_assignee, &[]));
        let due = |day: &str| [assigned(6, at(&format!("{day}T00:00:00Z")).as_second())];
        assert_ne!(
            signature(&window, &due("2026-12-01"), &[]),
            signature(&window, &due("2026-12-15"), &[]),
            "a time the agent set, far from the episode's clock, counts"
        );
        assert_ne!(signature(&window, &first, &[]), signature(&window, &first[..1], &[]));
    }

    fn waited(minute: u32) -> Window {
        let moved = [(at("2026-11-04T10:02:00Z"), minute)];
        Window::new(at("2026-11-04T10:00:00Z"), &moved, at("2026-11-04T10:05:00Z"))
    }

    fn token_for(window: &Window, value: Value) -> Value {
        let mut value = value;
        normalize(&mut value, &window.stretches);
        value
    }

    #[test]
    fn a_write_before_a_wait_and_one_after_it_differ_but_runs_alike_match() {
        let window = waited(600);
        let before = at("2026-11-04T10:01:00Z").as_second() - 600 * 60;
        let also_before = at("2026-11-04T10:01:30Z").as_second() - 600 * 60;
        let after = at("2026-11-04T10:03:00Z").as_second();
        assert_eq!(token_for(&window, json!(before)), json!("<now>"));
        assert_eq!(token_for(&window, json!(after)), json!("<now+600>"));
        assert_eq!(
            signature(&window, &[assigned(6, before)], &[]),
            signature(&window, &[assigned(6, also_before)], &[]),
            "two runs that wrote before the wait"
        );
        assert_ne!(
            signature(&window, &[assigned(6, before)], &[]),
            signature(&window, &[assigned(6, after)], &[]),
            "the same write, before the wait and after it"
        );
    }

    #[test]
    fn a_date_reads_by_the_day_of_the_clock_it_was_stamped_on() {
        let window = waited(1_500);
        assert_eq!(token_for(&window, json!("2026-11-03")), json!("<today>"), "moved a day back");
        assert_eq!(token_for(&window, json!("2026-11-04")), json!("<today+1>"));
        assert_eq!(token_for(&window, json!("2026-11-02")), json!("2026-11-02"));
    }

    #[test]
    fn effects_keep_the_real_window_since_no_move_touches_them() {
        let window = waited(600);
        let sent = |at: &str| signature(&window, &[], &[("http", json!({"body": {"at": at}}))]);
        assert_eq!(sent("2026-11-04T10:00:30Z"), sent("2026-11-04T10:04:00Z"));
    }

    #[test]
    fn a_date_counts_relative_to_the_episodes_day() {
        let due = |day: &str| {
            [json!({"tbl": "milestone", "op": "update", "pk": {"id": 2},
                    "after": {"due": day}, "columns": ["due"]})]
        };
        let (today, tomorrow) = (window_on("2026-11-04"), window_on("2026-11-05"));
        assert_eq!(
            signature(&today, &due("2026-11-04"), &[]),
            signature(&tomorrow, &due("2026-11-05"), &[]),
            "set due today, on two different days"
        );
        assert_ne!(
            signature(&today, &due("2026-11-04"), &[]),
            signature(&tomorrow, &due("2026-11-04"), &[])
        );
    }

    #[test]
    fn the_answer_counts_only_when_the_task_asks_for_one() {
        let finish = |answer: Option<&str>| Signature::new(window_on("2026-11-04")).finish(answer);
        assert_eq!(finish(Some(" 7 ")), finish(Some("7")));
        assert_eq!(finish(Some("7  open\nbugs")), finish(Some("7 open bugs")));
        assert_ne!(finish(Some("7")), finish(Some("8")));
        assert_ne!(finish(Some("")), finish(None), "asked, and none given");
    }

    #[test]
    fn an_http_calls_headers_do_not_count_but_its_body_does() {
        let window = window_on("2026-11-04");
        let call = |delivery: &str, body: Value| {
            json!({"method": "POST", "path": "/hooks/ci", "query": {},
                   "headers": {"x-gitea-delivery": delivery}, "body": body})
        };
        let sent = |data: Value| signature(&window, &[], &[("http", data)]);
        let pushed = json!({"ref": "main", "updated_at": "2026-11-04T10:02:00Z"});
        let later = json!({"ref": "main", "updated_at": "2026-11-04T10:03:00Z"});
        assert_eq!(sent(call("a1", pushed.clone())), sent(call("b2", later)));
        assert_ne!(sent(call("a1", pushed)), sent(call("a1", json!({"ref": "dev"}))));
        let email = |to: &str| json!({"to": [to], "subject": "Deal 2001 is now yours"});
        assert_ne!(
            signature(&window, &[], &[("email", email("marcus@acme.test"))]),
            signature(&window, &[], &[("email", email("dana@acme.test"))])
        );
    }

    #[test]
    fn creating_a_table_and_dropping_it_differ() {
        let window = window_on("2026-11-04");
        let schema = |before: Value, after: Value| {
            [json!({"tbl": "custom", "op": "schema", "pk": {}, "before": before,
                    "after": after, "columns": null})]
        };
        let shape = json!({"id": "integer"});
        assert_ne!(
            signature(&window, &schema(Value::Null, shape.clone()), &[]),
            signature(&window, &schema(shape, Value::Null), &[])
        );
    }

    #[test]
    fn json_keys_serialize_sorted_so_signatures_are_canonical() {
        assert_eq!(
            json!({"b": 1, "a": {"d": 2, "c": 3}}).to_string(),
            r#"{"a":{"c":3,"d":2},"b":1}"#
        );
    }

    fn action(tool: &str, status: Option<i64>, arguments: &str, result: &str) -> StoredAction {
        StoredAction {
            tool: tool.into(),
            app: None,
            method: None,
            path: None,
            status,
            latency_ms: 5,
            arguments: arguments.into(),
            result: Some(result.into()),
            via: None,
        }
    }

    fn requested(status: i64, arguments: &str) -> StoredAction {
        StoredAction {
            via: Some("run_code".into()),
            ..action("api_request", Some(status), arguments, "{}")
        }
    }

    #[test]
    fn diagnostics_count_failed_repeated_and_docs_calls() {
        let get = r#"{"method":"GET","path":"/deals"}"#;
        let actions = [
            action("api_request", Some(200), get, "{}"),
            action("api_request", Some(404), r#"{"method":"GET","path":"/nope"}"#, "{}"),
            action("api_request", Some(503), get, "{}"),
            action(
                "api_request",
                None,
                r#"{"method":"TRACE"}"#,
                r#"{"error_type":"invalid_args"}"#,
            ),
            action("api_docs", None, "{}", "[]"),
            action("api_docs", None, "{}", "[]"),
            action("submit", None, "{}", r#"{"submitted":true}"#),
        ];
        assert_eq!(
            diagnostics(&actions),
            json!({"calls": 7, "client_errors": 1, "server_errors": 1, "repeated_calls": 2,
                   "docs_calls": 2, "code_requests": 0, "coworker_replies": 0, "events": 0})
        );
    }

    #[test]
    fn the_codes_requests_count_apart_from_the_calls_and_their_errors_count_too() {
        let get = r#"{"method":"GET","path":"/deals"}"#;
        let actions = [
            requested(200, get),
            requested(200, get),
            requested(404, r#"{"method":"GET","path":"/nope"}"#),
            requested(502, get),
            action("run_code", None, r#"{"command":"curl $API_CRM/deals"}"#, "{}"),
            action("submit", None, "{}", r#"{"submitted":true}"#),
        ];
        assert_eq!(
            diagnostics(&actions),
            json!({"calls": 2, "client_errors": 1, "server_errors": 1, "repeated_calls": 0,
                   "docs_calls": 0, "code_requests": 4, "coworker_replies": 0, "events": 0})
        );
    }

    fn replied(status: i64, text: &str) -> StoredAction {
        let arguments = json!({"coworker": "dan", "text": text}).to_string();
        StoredAction {
            via: Some("coworker".into()),
            ..action("coworker_reply", Some(status), &arguments, "{}")
        }
    }

    #[test]
    fn coworkers_replies_are_none_of_the_agents_calls_or_errors() {
        let actions = [
            action("api_request", Some(201), r#"{"method":"POST","path":"/posts"}"#, "{}"),
            replied(201, "Erin."),
            replied(404, "Erin?"),
            replied(201, "Erin."),
            action("submit", None, "{}", r#"{"submitted":true}"#),
        ];
        assert_eq!(
            diagnostics(&actions),
            json!({"calls": 2, "client_errors": 0, "server_errors": 0, "repeated_calls": 0,
                   "docs_calls": 0, "code_requests": 0, "coworker_replies": 3, "events": 0})
        );
    }

    #[test]
    fn events_are_none_of_the_agents_calls_or_errors() {
        let event = |status: i64| StoredAction {
            via: Some(EVENT.into()),
            ..action(EVENT, Some(status), r#"{"event":"report-1"}"#, "{}")
        };
        let actions = [
            event(201),
            action("wait", None, r#"{"minutes":600}"#, r#"{"waited_minutes":47}"#),
            event(404),
            action("submit", None, "{}", r#"{"submitted":true}"#),
        ];
        assert_eq!(
            diagnostics(&actions),
            json!({"calls": 2, "client_errors": 0, "server_errors": 0, "repeated_calls": 0,
                   "docs_calls": 0, "code_requests": 0, "coworker_replies": 0, "events": 2})
        );
    }

    #[test]
    fn a_chatty_run_cannot_crowd_a_coworkers_answer_out_of_the_record() {
        let mib = 1024 * 1024;
        let mut actions: Vec<StoredAction> = (0..5).map(|_| requested(200, &quoted(mib))).collect();
        actions.push(replied(201, &"y".repeat(mib)));
        actions.push(action("submit", None, "{}", r#"{"submitted":true}"#));
        let record = Builder::new(window_on("2026-11-04"), &actions, &[]).finish(None, true);
        assert!(
            record["truncated"]["code_arguments"].as_u64() > Some(0),
            "{}",
            record["truncated"]
        );
        assert!(record["truncated"].get("arguments").is_none(), "{}", record["truncated"]);
        assert_eq!(record["actions"][5]["arguments"]["text"].as_str().map(str::len), Some(mib));
        assert_eq!(record["actions"][5]["via"], "coworker");
    }

    #[test]
    fn a_chatty_run_cannot_crowd_its_command_out_of_the_record() {
        let mib = 1024 * 1024;
        let mut actions: Vec<StoredAction> = (0..4).map(|_| requested(200, &quoted(mib))).collect();
        let command = json!({"command": "x".repeat(mib)}).to_string();
        actions.push(action("run_code", None, &command, "{}"));
        actions.push(action("submit", None, "{}", r#"{"submitted":true}"#));
        let record = Builder::new(window_on("2026-11-04"), &actions, &[]).finish(None, true);
        assert_eq!(record["truncated"], json!({"code_arguments": 2}), "{}", record["truncated"]);
        assert_eq!(record["actions"][4]["arguments"]["command"].as_str().map(str::len), Some(mib));
        assert_eq!(record["actions"][0]["via"], "run_code");
        assert!(record["actions"][4].get("via").is_none(), "the run itself is a tool call");
        assert_eq!(record["actions"][3]["arguments"], json!({"omitted_bytes": mib + 2}));
    }

    fn quoted(bytes: usize) -> String {
        json!("x".repeat(bytes)).to_string()
    }

    #[test]
    fn a_record_past_the_limit_keeps_the_arguments_and_cuts_the_results_first() {
        let mib = 1024 * 1024;
        let actions = [
            action("api_request", Some(200), &quoted(mib), &quoted(3 * mib)),
            action("submit", None, "{}", r#"{"submitted":true}"#),
        ];
        let changes = [StoredChange {
            app: None,
            tbl: "notes".into(),
            op: "insert".into(),
            pk: r#"{"id":1}"#.into(),
            before: None,
            after: Some(format!(r#"{{"id":1,"text":{}}}"#, quoted(mib))),
            columns: None,
        }];
        let mut builder = Builder::new(window_on("2026-11-04"), &actions, &changes);
        builder.effect("http", &quoted(mib + mib / 2));
        builder.effect("email", r#"{"to":["a@b.test"]}"#);
        let record = builder.finish(None, true);
        assert_eq!(record["truncated"], json!({"results": 1}), "{}", record["truncated"]);
        assert_eq!(record["actions"][0]["arguments"].as_str().map(str::len), Some(mib));
        assert_eq!(record["actions"][0]["result"], json!({"omitted_bytes": 3 * mib + 2}));
        assert_eq!(record["actions"][1]["result"], json!({"submitted": true}), "small ones fit");
        assert_eq!(record["changes"].as_array().map(Vec::len), Some(1));
        assert_eq!(record["effects"].as_array().map(Vec::len), Some(2));
        assert_eq!(record["limit_bytes"], RECORD_LIMIT);

        let huge = [action("api_request", Some(200), &quoted(5 * mib), "{}")];
        let record = Builder::new(window_on("2026-11-04"), &huge, &[]).finish(None, false);
        assert_eq!(record["truncated"], json!({"arguments": 1}));
        assert_eq!(record["actions"][0]["arguments"], json!({"omitted_bytes": 5 * mib + 2}));
        assert_eq!(record["signature"], Value::Null, "an end state grading never read");
        assert_eq!(record["diagnostics"]["calls"], 1);
    }

    #[test]
    fn a_record_that_fits_says_nothing_was_cut() {
        let actions = [action("submit", None, r#"{"answer":"7"}"#, r#"{"submitted":true}"#)];
        let record = Builder::new(window_on("2026-11-04"), &actions, &[]).finish(Some("7"), true);
        assert!(record.get("truncated").is_none(), "{record}");
        assert_eq!(record["actions"][0]["arguments"], json!({"answer": "7"}));
        assert_eq!(record["signature"].as_str().map(str::len), Some(16));
    }

    #[test]
    fn one_change_in_two_apps_is_two_end_states() {
        let window = Window::new(Timestamp::UNIX_EPOCH, &[], Timestamp::UNIX_EPOCH);
        let signature = |app: Option<&str>| {
            let mut change = json!({"tbl": "deals", "op": "delete", "pk": {"id": 1}});
            if let Some(app) = app {
                change["app"] = json!(app);
            }
            let mut signature = Signature::new(window.clone());
            signature.change(&change);
            signature.finish(None)
        };
        assert_ne!(signature(Some("east")), signature(Some("west")));
        assert_eq!(signature(Some("east")), signature(Some("east")));
        assert_ne!(signature(None), signature(Some("east")), "one database's signature is its own");
    }
}
