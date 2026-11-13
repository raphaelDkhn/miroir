//! Coworkers: colleagues a model plays, who read what the agent writes them in the app and
//! reply there.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::env::{Env, EnvError};
use crate::manifest::{Manifest, Persona, Pick, ensure, is_short_name};
use crate::task::{GoldenStep, Task};
use crate::tools::percent_encode;

/// What a coworker's reply is recorded `via`.
pub const VIA: &str = "coworker";
/// Most characters of a message or reply.
pub const TEXT_LIMIT: usize = 4_000;
const WINDOW: usize = 20;

/// The golden step that stands for a coworker's answer.
pub const COWORKER_REPLY: &str = "coworker_reply";

/// A coworker: who they are, what they know, where their messages are and how they
/// reply.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Coworker {
    pub name: String,
    /// Who they are, as their prompt says.
    pub description: String,
    /// What they know, and how they act.
    pub script: String,
    /// Their own headers, by app, which their replies send.
    #[serde(default)]
    pub headers: BTreeMap<String, BTreeMap<String, String>>,
    /// Where the messages to them are.
    pub inbox: Inbox,
    /// Request that posts a reply: `{text}` is the answer, other `{field}`s come from the
    /// message.
    pub reply: Map<String, Value>,
}

impl Coworker {
    /// Their persona, to post as them.
    pub fn persona(&self) -> Persona {
        Persona { description: self.description.clone(), headers: self.headers.clone() }
    }
}

/// A query for the messages to a coworker.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Inbox {
    pub app: Option<String>,
    /// Scalar SQL returning a JSON array of messages, each with an `id` and a `text`.
    pub sql: String,
}

/// Check a task's coworkers; a golden step must answer for each.
pub fn validate(manifest: &Manifest, task: &Task) -> Result<(), String> {
    let mut names = BTreeSet::new();
    for coworker in &task.coworkers {
        let at = format!("coworker '{}'", coworker.name);
        ensure(
            is_short_name(&coworker.name),
            format!("{at}: a name is 1 to 64 lowercase letters, digits, '_' or '-'"),
        )?;
        ensure(names.insert(coworker.name.as_str()), format!("{at} is listed twice"))?;
        ensure(!coworker.description.trim().is_empty(), format!("{at} needs a description"))?;
        ensure(!coworker.script.trim().is_empty(), format!("{at} needs a script"))?;
        manifest.validate_headers(&at, &coworker.headers)?;
        ensure(!coworker.inbox.sql.trim().is_empty(), format!("{at}: inbox needs sql"))?;
        let inbox_at = format!("{at} inbox");
        manifest.check_app(&inbox_at, coworker.inbox.app.as_deref(), Pick::Database, true)?;
        validate_reply(manifest, &format!("{at} reply"), &coworker.reply)?;
        let answered = task.golden.iter().any(|step| answers_for(step, &coworker.name));
        ensure(
            answered,
            format!(
                "{at}: no golden step answers for them; add a coworker_reply step after the \
                 call that reaches them"
            ),
        )?;
    }
    Ok(())
}

fn validate_reply(manifest: &Manifest, at: &str, reply: &Map<String, Value>) -> Result<(), String> {
    manifest.validate_request(at, reply)?;
    let path = reply.get("path").and_then(Value::as_str);
    let mut texts = path.into_iter().collect::<Vec<_>>();
    if let Some(body) = reply.get("body") {
        strings_in(body, &mut texts);
    }
    let fills_text = texts.iter().any(|text| placeholders(text).contains(&"text"));
    ensure(fills_text, format!("{at}: {{text}}, the answer, appears nowhere in path or body"))
}

pub fn validate_replies(
    what: &str,
    steps: &[GoldenStep],
    coworkers: &[Coworker],
) -> Result<(), String> {
    for (i, step) in steps.iter().enumerate().filter(|(_, step)| step.tool == COWORKER_REPLY) {
        let at = format!("{what} step {}", i + 1);
        ensure(
            !coworkers.is_empty(),
            format!("{at}: {COWORKER_REPLY} needs a task with coworkers"),
        )?;
        let mut keys: Vec<&str> = step.arguments.keys().map(String::as_str).collect();
        keys.sort_unstable();
        ensure(
            keys == ["coworker", "text"],
            format!("{at}: {COWORKER_REPLY} takes exactly a coworker and a text"),
        )?;
        let name = step.arguments.get("coworker").and_then(Value::as_str).unwrap_or_default();
        ensure(
            coworkers.iter().any(|coworker| coworker.name == name),
            format!("{at}: the task has no coworker '{name}'"),
        )?;
        let text = step.arguments.get("text").and_then(Value::as_str);
        ensure(
            text.is_some_and(|text| !text.trim().is_empty()),
            format!("{at}: {COWORKER_REPLY} needs a text that isn't empty"),
        )?;
    }
    Ok(())
}

fn answers_for(step: &GoldenStep, coworker: &str) -> bool {
    step.tool == COWORKER_REPLY
        && step.arguments.get("coworker").and_then(Value::as_str) == Some(coworker)
}

fn strings_in<'a>(value: &'a Value, into: &mut Vec<&'a str>) {
    match value {
        Value::String(text) => into.push(text),
        Value::Array(items) => items.iter().for_each(|item| strings_in(item, into)),
        Value::Object(map) => map.values().for_each(|item| strings_in(item, into)),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// A message to a coworker; its fields fill the reply's placeholders.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub id: String,
    pub text: String,
    pub fields: Map<String, Value>,
}

/// Parse an inbox query's value.
pub fn parse_inbox(value: Option<&str>) -> Result<Vec<Message>, String> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let parsed: Value =
        serde_json::from_str(value).map_err(|e| format!("the inbox is not JSON: {e}"))?;
    let Value::Array(items) = parsed else {
        return Err("the inbox must be one JSON array of messages".into());
    };
    let mut messages = Vec::with_capacity(items.len());
    for (i, item) in items.into_iter().enumerate() {
        let Value::Object(fields) = item else {
            return Err(format!("inbox message {} is not an object", i + 1));
        };
        let id = match fields.get("id") {
            Some(Value::String(id)) => id.clone(),
            Some(Value::Number(id)) => id.to_string(),
            _ => return Err(format!("inbox message {} has no string or number id", i + 1)),
        };
        let Some(Value::String(text)) = fields.get("text") else {
            return Err(format!("inbox message {} has no text", i + 1));
        };
        messages.push(Message { id, text: cut(text), fields: fields.clone() });
    }
    Ok(messages)
}

#[derive(Debug, Default)]
struct Heard {
    seen: BTreeSet<String>,
    said: Vec<(bool, String)>,
    owed: bool,
    last: Map<String, Value>,
}

/// What each coworker heard and said this episode.
#[derive(Debug, Default)]
pub struct Coworkers {
    by_name: BTreeMap<String, Heard>,
}

impl Coworkers {
    /// Start with the messages already there at reset, which they never answer.
    pub fn start(inboxes: impl IntoIterator<Item = (String, Vec<Message>)>) -> Coworkers {
        let mut coworkers = Coworkers::default();
        for (name, messages) in inboxes {
            let heard = coworkers.by_name.entry(name).or_default();
            heard.seen.extend(messages.into_iter().map(|m| m.id));
        }
        coworkers
    }

    /// Take a coworker's new messages; true when any came.
    pub fn hear(&mut self, name: &str, messages: Vec<Message>) -> bool {
        let heard = self.by_name.entry(name.to_string()).or_default();
        let mut any = false;
        for message in messages {
            if heard.seen.insert(message.id.clone()) {
                heard.said.push((true, message.text));
                heard.last = message.fields;
                heard.owed = true;
                any = true;
            }
        }
        any
    }

    /// Whether the coworker owes a reply.
    pub fn owes(&self, name: &str) -> bool {
        self.by_name.get(name).is_some_and(|heard| heard.owed)
    }

    /// Fields of the last message they heard.
    pub fn last(&self, name: &str) -> Option<&Map<String, Value>> {
        self.by_name.get(name).map(|heard| &heard.last)
    }

    pub fn answered(&mut self, name: &str, text: &str) {
        let heard = self.by_name.entry(name.to_string()).or_default();
        heard.said.push((false, text.to_string()));
        heard.owed = false;
    }

    /// Mark the messages a reply itself made as heard.
    pub fn sent_while(&mut self, name: &str, before: &BTreeSet<String>, after: Vec<Message>) {
        let heard = self.by_name.entry(name.to_string()).or_default();
        heard.seen.extend(after.into_iter().map(|m| m.id).filter(|id| !before.contains(id)));
    }

    /// The chat a model answers as the coworker: a system prompt, then the last 20
    /// messages.
    pub fn prompt(&self, coworker: &Coworker, agent: &str) -> Value {
        let said = self.by_name.get(&coworker.name).map_or(&[][..], |heard| &heard.said[..]);
        let mut window = &said[said.len().saturating_sub(WINDOW)..];
        while let Some(((false, _), rest)) = window.split_first() {
            window = rest;
        }
        let mut messages = vec![json!({"role": "system", "content": system(coworker, agent)})];
        let mut turns: Vec<(bool, String)> = Vec::new();
        for (from_agent, text) in window {
            match turns.last_mut() {
                Some((true, joined)) if *from_agent => {
                    joined.push_str("\n\n");
                    joined.push_str(text);
                }
                _ => turns.push((*from_agent, text.clone())),
            }
        }
        for (from_agent, text) in turns {
            let role = if from_agent { "user" } else { "assistant" };
            messages.push(json!({"role": role, "content": text}));
        }
        Value::Array(messages)
    }
}

fn system(coworker: &Coworker, agent: &str) -> String {
    format!(
        "You are {}. {} has written to you at work, and you answer in the same place.\n\n\
         What you know, and how you act:\n{}\n\n\
         Reply with your answer only, as you would write it back: briefly, in the first person, \
         using only what you know, and saying so when you don't know something. You can only \
         answer; you do nothing else for them.",
        coworker.description.trim(),
        agent.trim(),
        coworker.script.trim(),
    )
}

/// The first `TEXT_LIMIT` characters.
pub fn cut(text: &str) -> String {
    match text.char_indices().nth(TEXT_LIMIT) {
        Some((end, _)) => text[..end].to_string(),
        None => text.to_string(),
    }
}

/// Fill a reply's placeholders: `{text}` with the answer, others from the message's
/// fields.
pub fn fill(
    reply: &Map<String, Value>,
    fields: &Map<String, Value>,
    answer: &str,
) -> Result<Map<String, Value>, String> {
    let mut filled = reply.clone();
    if let Some(Value::String(path)) = reply.get("path") {
        filled.insert("path".into(), Value::String(fill_text(path, fields, answer, true)?));
    }
    if let Some(body) = reply.get("body") {
        filled.insert("body".into(), fill_value(body, fields, answer)?);
    }
    Ok(filled)
}

fn fill_value(value: &Value, fields: &Map<String, Value>, answer: &str) -> Result<Value, String> {
    Ok(match value {
        Value::String(text) => Value::String(fill_text(text, fields, answer, false)?),
        Value::Array(items) => Value::Array(
            items.iter().map(|item| fill_value(item, fields, answer)).collect::<Result<_, _>>()?,
        ),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, item)| Ok((key.clone(), fill_value(item, fields, answer)?)))
                .collect::<Result<_, String>>()?,
        ),
        Value::Null | Value::Bool(_) | Value::Number(_) => value.clone(),
    })
}

fn fill_text(
    text: &str,
    fields: &Map<String, Value>,
    answer: &str,
    in_path: bool,
) -> Result<String, String> {
    let mut filled = String::new();
    for piece in pieces(text) {
        match piece {
            Piece::Text(text) => filled.push_str(text),
            Piece::Field("text") => filled.push_str(&encoded(answer, in_path)),
            Piece::Field(name) => {
                let value = match fields.get(name) {
                    Some(Value::String(value)) => value.clone(),
                    Some(value @ (Value::Number(_) | Value::Bool(_))) => value.to_string(),
                    Some(_) => {
                        return Err(format!(
                            "the message's field '{name}' isn't a string or a number"
                        ));
                    }
                    None => return Err(format!("the message has no field '{name}' for the reply")),
                };
                filled.push_str(&encoded(&value, in_path));
            }
        }
    }
    Ok(filled)
}

fn encoded(text: &str, in_path: bool) -> String {
    if in_path { percent_encode(text) } else { text.to_string() }
}

/// The `{name}` placeholders in a text.
pub fn placeholders(text: &str) -> Vec<&str> {
    pieces(text)
        .into_iter()
        .filter_map(|piece| match piece {
            Piece::Field(name) => Some(name),
            Piece::Text(_) => None,
        })
        .collect()
}

#[derive(Debug, PartialEq, Eq)]
enum Piece<'a> {
    Text(&'a str),
    Field(&'a str),
}

fn pieces(text: &str) -> Vec<Piece<'_>> {
    let mut pieces = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find('{') {
        let after = &rest[open + 1..];
        let name_len = after
            .char_indices()
            .find(|&(i, c)| !(c == '_' || c.is_ascii_alphabetic() || (i > 0 && c.is_ascii_digit())))
            .map_or(after.len(), |(i, _)| i);
        if name_len > 0 && after[name_len..].starts_with('}') {
            if open > 0 {
                pieces.push(Piece::Text(&rest[..open]));
            }
            pieces.push(Piece::Field(&after[..name_len]));
            rest = &after[name_len + 1..];
        } else {
            pieces.push(Piece::Text(&rest[..=open]));
            rest = after;
        }
    }
    if !rest.is_empty() {
        pieces.push(Piece::Text(rest));
    }
    pieces
}

impl Env {
    /// Hear every inbox, then return the turns owed.
    pub(crate) async fn owed_turns(
        &self,
        task: &Task,
        coworkers: &mut Coworkers,
    ) -> Result<Value, EnvError> {
        for (name, messages) in self.inboxes(task).await.map_err(|e| self.lost(e))? {
            coworkers.hear(&name, messages);
        }
        let agent = self.persona_description(task);
        let turns: Vec<Value> = task
            .coworkers
            .iter()
            .filter(|coworker| coworkers.owes(&coworker.name))
            .map(|coworker| {
                let messages = coworkers.prompt(coworker, agent);
                json!({"coworker": coworker.name, "messages": messages})
            })
            .collect();
        Ok(json!({"turns": turns}))
    }

    /// Post a coworker's reply as them.
    pub(crate) async fn send_reply(
        &self,
        task: &Task,
        coworkers: &mut Coworkers,
        name: &str,
        text: &str,
    ) -> Result<Value, EnvError> {
        let Some(coworker) = task.coworkers.iter().find(|coworker| coworker.name == name) else {
            return Err(EnvError::Invalid(format!("the task has no coworker '{name}'")));
        };
        if !coworkers.owes(name) {
            let message = format!("{name} has no message to answer; send coworkers first");
            return Err(EnvError::Invalid(message));
        }
        let text = cut(text.trim());
        if text.is_empty() {
            return Err(EnvError::Invalid("a coworker's answer needs text".into()));
        }
        let before = self.inboxes(task).await.map_err(|e| self.lost(e))?;
        let fields = coworkers.last(name).cloned().unwrap_or_default();
        let arguments = fill(&coworker.reply, &fields, &text)
            .map_err(|e| self.lost(format!("coworker '{name}''s reply: {e}")))?;
        let said = json!({"coworker": name, "text": text});
        let (status, failed) =
            self.send_as(&coworker.persona(), &arguments, COWORKER_REPLY, VIA, said).await;
        if let Some(why) = failed {
            return Err(self.lost(format!("coworker '{name}''s reply: {why}")));
        }
        coworkers.answered(name, &text);
        let after = self.inboxes(task).await.map_err(|e| self.lost(e))?;
        for ((name, before), (_, after)) in before.iter().zip(after) {
            let ids: BTreeSet<String> = before.iter().map(|m| m.id.clone()).collect();
            coworkers.sent_while(name, &ids, after);
        }
        Ok(json!({"status": status}))
    }

    /// Each coworker's messages, as their inbox query reads them.
    pub(crate) async fn inboxes(&self, task: &Task) -> Result<Vec<(String, Vec<Message>)>, String> {
        let mut inboxes = Vec::with_capacity(task.coworkers.len());
        for coworker in &task.coworkers {
            let app = coworker.inbox.app.as_deref().or_else(|| self.manifest.only_database());
            let Some(db) = app.and_then(|app| self.dbs.get(app)) else {
                return Err(format!("coworker '{}': no database for the inbox", coworker.name));
            };
            let value = db.value(&coworker.inbox.sql).await;
            let messages = value
                .map_err(|e| e.0)
                .and_then(|value| parse_inbox(value.as_deref()))
                .map_err(|e| format!("coworker '{}''s inbox: {e}", coworker.name))?;
            inboxes.push((coworker.name.clone(), messages));
        }
        Ok(inboxes)
    }

    fn lost(&self, why: String) -> EnvError {
        self.edb.lose(&why);
        EnvError::Other(why)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(id: &str, text: &str) -> Message {
        let fields = json!({"id": id, "text": text, "root_id": format!("root-{id}")});
        Message { id: id.into(), text: text.into(), fields: fields.as_object().unwrap().clone() }
    }

    fn coworker(name: &str) -> Coworker {
        serde_json::from_value(json!({
            "name": name,
            "description": format!("{name}, a developer"),
            "script": "Erin should take it.",
            "inbox": {"sql": "SELECT 1"},
            "reply": {"method": "POST", "path": "/posts", "body": {"message": "{text}"}},
        }))
        .unwrap()
    }

    fn roles(prompt: &Value) -> Vec<(String, String)> {
        prompt
            .as_array()
            .unwrap()
            .iter()
            .skip(1)
            .map(|m| (m["role"].as_str().unwrap().into(), m["content"].as_str().unwrap().into()))
            .collect()
    }

    #[test]
    fn an_inbox_is_one_array_of_messages_with_ids_and_texts() {
        assert_eq!(parse_inbox(None).unwrap(), []);
        let parsed = parse_inbox(Some(r#"[{"id": 7, "text": "hi", "channel": "c"}]"#)).unwrap();
        assert_eq!((parsed[0].id.as_str(), parsed[0].text.as_str()), ("7", "hi"));
        assert_eq!(parsed[0].fields["channel"], "c");
        for (value, expected) in [
            ("nope", "not JSON"),
            (r#"{"id": 1, "text": "a"}"#, "one JSON array"),
            ("[1]", "message 1 is not an object"),
            (r#"[{"id": 1, "text": "a"}, {"text": "b"}]"#, "message 2 has no string or number id"),
            (r#"[{"id": null, "text": "b"}]"#, "no string or number id"),
            (r#"[{"id": "x", "text": 3}]"#, "message 1 has no text"),
        ] {
            let err = parse_inbox(Some(value)).unwrap_err();
            assert!(err.contains(expected), "{value}: {err}");
        }
    }

    #[test]
    fn long_texts_are_cut_by_chars() {
        let long = "é".repeat(TEXT_LIMIT + 5);
        let parsed = parse_inbox(Some(&json!([{"id": "1", "text": long}]).to_string())).unwrap();
        assert_eq!(parsed[0].text.chars().count(), TEXT_LIMIT);
        assert_eq!(cut("short"), "short");
    }

    #[test]
    fn messages_seen_at_reset_never_come_back() {
        let mut coworkers = Coworkers::start([("dan".into(), vec![message("old", "before")])]);
        assert!(!coworkers.hear("dan", vec![message("old", "before")]));
        assert!(!coworkers.owes("dan"));
        assert!(coworkers.hear("dan", vec![message("old", "before"), message("new", "who?")]));
        assert!(coworkers.owes("dan"));
        assert_eq!(coworkers.last("dan").unwrap()["root_id"], "root-new");
        assert_eq!(
            roles(&coworkers.prompt(&coworker("dan"), "Maya")),
            [("user".into(), "who?".into())]
        );
    }

    #[test]
    fn each_coworker_hears_and_answers_alone() {
        let mut coworkers = Coworkers::start([("dan".into(), vec![]), ("priya".into(), vec![])]);
        coworkers.hear("dan", vec![message("1", "for dan")]);
        assert!(coworkers.owes("dan") && !coworkers.owes("priya"));
        coworkers.answered("dan", "Erin.");
        coworkers.hear("priya", vec![message("2", "for priya")]);
        assert!(!coworkers.owes("dan") && coworkers.owes("priya"));
        let dan = roles(&coworkers.prompt(&coworker("dan"), "Maya"));
        assert_eq!(dan, [("user".into(), "for dan".into()), ("assistant".into(), "Erin.".into())]);
    }

    #[test]
    fn a_turn_not_answered_gains_the_messages_that_come_after_it() {
        let mut coworkers = Coworkers::start([("dan".into(), vec![])]);
        coworkers.hear("dan", vec![message("1", "who?")]);
        coworkers.hear("dan", vec![message("1", "who?"), message("2", "anyone?")]);
        assert!(coworkers.owes("dan"));
        let prompt = coworkers.prompt(&coworker("dan"), "Maya Chen (maya)");
        assert_eq!(roles(&prompt), [("user".into(), "who?\n\nanyone?".into())]);
        let system = prompt[0]["content"].as_str().unwrap();
        assert!(system.starts_with("You are dan, a developer. Maya Chen (maya) has written"));
        assert!(system.contains("Erin should take it."));
    }

    #[test]
    fn only_what_appeared_while_a_reply_was_sent_counts_as_its_own() {
        let mut coworkers = Coworkers::start([("dan".into(), vec![])]);
        coworkers.hear("dan", vec![message("1", "who?")]);
        coworkers.answered("dan", "Erin.");
        let before: BTreeSet<String> = ["1".into(), "2".into()].into();
        coworkers.sent_while(
            "dan",
            &before,
            vec![message("1", ""), message("2", ""), message("3", "")],
        );
        assert!(coworkers.hear("dan", vec![message("2", "and?"), message("3", "Erin.")]));
        let said = roles(&coworkers.prompt(&coworker("dan"), "Maya"));
        assert_eq!(said.last().unwrap(), &("user".to_string(), "and?".to_string()));
    }

    #[test]
    fn the_prompt_reads_the_latest_entries_starting_with_the_agents() {
        let mut coworkers = Coworkers::start([("dan".into(), vec![])]);
        for i in 0..15 {
            coworkers.hear("dan", vec![message(&i.to_string(), &format!("q{i}"))]);
            coworkers.answered("dan", &format!("a{i}"));
        }
        let said = roles(&coworkers.prompt(&coworker("dan"), "Maya"));
        assert_eq!(said.len(), 20, "the last 20 of 30 entries");
        assert_eq!(said[0], ("user".into(), "q5".into()));
        assert_eq!(said[19], ("assistant".into(), "a14".into()));
        coworkers.hear("dan", vec![message("15", "q15")]);
        let said = roles(&coworkers.prompt(&coworker("dan"), "Maya"));
        assert_eq!(said[0], ("user".into(), "q6".into()), "an answer never opens the window");
        assert_eq!(said.len(), 19);
    }

    #[test]
    fn fill_puts_the_answer_and_fields_in_one_pass() {
        let reply = json!({
            "app": "chat",
            "method": "POST",
            "path": "/deals/{deal}/notes",
            "body": {"root_id": "{root_id}", "message": "{text}", "tags": ["{deal}", 3, null]},
        });
        let fields = json!({"deal": 1100, "root_id": "p1", "id": "n9"});
        let filled =
            fill(reply.as_object().unwrap(), fields.as_object().unwrap(), "See {root_id}.")
                .unwrap();
        assert_eq!(filled["path"], "/deals/1100/notes");
        assert_eq!(filled["body"]["message"], "See {root_id}.", "an answer's braces are its own");
        assert_eq!(filled["body"]["root_id"], "p1");
        assert_eq!(filled["body"]["tags"], json!(["1100", 3, null]));
        assert_eq!(
            (filled["app"].as_str(), filled["method"].as_str()),
            (Some("chat"), Some("POST"))
        );
    }

    #[test]
    fn fill_encodes_path_values_and_refuses_unknown_fields() {
        let reply =
            json!({"method": "POST", "path": "/c/{channel}/{text}", "body": {"m": "{text}"}});
        let fields = json!({"channel": "a/b c", "list": [1]});
        let filled = fill(reply.as_object().unwrap(), fields.as_object().unwrap(), "é?").unwrap();
        assert_eq!(filled["path"], "/c/a%2Fb%20c/%C3%A9%3F");
        assert_eq!(filled["body"]["m"], "é?");
        let missing = json!({"method": "POST", "path": "/x/{deal}", "body": "{text}"});
        let err = fill(missing.as_object().unwrap(), fields.as_object().unwrap(), "a").unwrap_err();
        assert!(err.contains("no field 'deal'"), "{err}");
        let listed = json!({"method": "POST", "path": "/x", "body": "{list}{text}"});
        let err = fill(listed.as_object().unwrap(), fields.as_object().unwrap(), "a").unwrap_err();
        assert!(err.contains("'list' isn't a string or a number"), "{err}");
    }

    #[test]
    fn placeholders_are_names_in_braces_and_other_braces_are_text() {
        assert_eq!(placeholders("/deals/{deal}/notes"), ["deal"]);
        assert_eq!(placeholders("{text}"), ["text"]);
        assert_eq!(placeholders("{a}{b_2} and {_c}"), ["a", "b_2", "_c"]);
        let none: [&str; 0] = [];
        for text in ["{}", "{ text }", "{2x}", "{text", "text}", "{{", "a {b-c} d", ""] {
            assert_eq!(placeholders(text), none, "{text}");
        }
        assert_eq!(placeholders("{{text}}"), ["text"], "a brace before one is text");
    }

    #[test]
    fn pieces_keep_every_character() {
        let text = "a {b} {c d} {e}";
        let rebuilt: String = pieces(text)
            .iter()
            .map(|piece| match piece {
                Piece::Text(t) => (*t).to_string(),
                Piece::Field(name) => format!("{{{name}}}"),
            })
            .collect();
        assert_eq!(rebuilt, text);
    }
}
