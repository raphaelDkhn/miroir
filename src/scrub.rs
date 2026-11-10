//! Keyed scrubbing for `miroir import`: each original gets a deterministic fake of the same
//! shape, and a scan afterwards proves none is left.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write;

use hmac::{Hmac, KeyInit, Mac};
use percent_encoding::percent_decode_str;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::Sha256;

use crate::db::{Column, ColumnKind, Table};
use crate::tools::percent_encode;

type HmacSha256 = Hmac<Sha256>;

/// Shortest key the scrubber accepts.
pub const MIN_KEY_BYTES: usize = 32;
const ATTEMPTS: u32 = 64;
const NAME_WORD_MIN: usize = 3;
const PHONE_DIGITS_MIN: usize = 7;
const SECRET_MIN: usize = 8;
const FAKE_TLD: &str = "example";
const VOWELS: &[u8] = b"aeiou";
const CONSONANTS: &[u8] = b"bcdfghjklmnprstvwz";
const PLAIN: [&str; 15] = [
    "smallint",
    "integer",
    "int",
    "tinyint",
    "mediumint",
    "bigint",
    "numeric",
    "decimal",
    "real",
    "double",
    "float",
    "boolean",
    "bool",
    "bit",
    "uuid",
];
const STRINGS: [&str; 9] = [
    "text",
    "character",
    "varchar",
    "char",
    "citext",
    "tinytext",
    "mediumtext",
    "longtext",
    "name",
];
const JSON: [&str; 2] = ["json", "jsonb"];
const BINARY: [&str; 7] =
    ["bytea", "binary", "varbinary", "blob", "tinyblob", "mediumblob", "longblob"];

/// What a column or trace field holds, which says how it's scrubbed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Name,
    Email,
    Phone,
    Secret,
    Text,
    Blank,
    Keep,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Name => "name",
            Kind::Email => "email",
            Kind::Phone => "phone",
            Kind::Secret => "secret",
            Kind::Text => "text",
            Kind::Blank => "blank",
            Kind::Keep => "keep",
        }
    }
}

/// The scrub spec: each app's columns by table, and the trace fields, with their kinds.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    pub apps: BTreeMap<String, BTreeMap<String, BTreeMap<String, Kind>>>,
    #[serde(default)]
    pub fields: BTreeMap<String, Kind>,
}

impl Spec {
    /// Parse a spec; a field must hold a name, an email, a phone or a secret.
    pub fn parse(text: &str) -> Result<Spec, String> {
        let spec: Spec = serde_saphyr::from_str(text).map_err(|e| e.to_string())?;
        for (field, kind) in &spec.fields {
            match kind {
                Kind::Name | Kind::Email | Kind::Phone | Kind::Secret => {}
                Kind::Text | Kind::Blank | Kind::Keep => {
                    return Err(format!(
                        "fields: '{field}' is {}, but a field holds a name, an email, a phone or \
                         a secret; every other string of a call is scrubbed as text",
                        kind.as_str()
                    ));
                }
            }
        }
        Ok(spec)
    }
}

/// A column and how to scrub it.
#[derive(Debug, Clone)]
pub struct Planned {
    pub table: Table,
    pub column: String,
    pub kind: Kind,
}

impl Planned {
    pub fn label(&self) -> String {
        format!("{}.{}", self.table.name, self.column)
    }
}

/// Columns to scrub, and columns to scan for originals afterwards.
#[derive(Debug, Default)]
pub struct Plan {
    pub scrub: Vec<Planned>,
    pub scan: Vec<Planned>,
}

/// Plan an app's scrub; every column but numbers, truth values, times and uuids must
/// be listed.
pub fn plan(
    app: &str,
    tables: &[Table],
    rules: &BTreeMap<String, BTreeMap<String, Kind>>,
) -> Result<Plan, String> {
    let mut errors = Vec::new();
    for (table, columns) in rules {
        let Some(found) = tables.iter().find(|t| &t.name == table) else {
            errors.push(format!("table '{table}' is listed, but the dump has no such table"));
            continue;
        };
        for (column, kind) in columns {
            match found.columns.iter().find(|c| &c.name == column) {
                None => errors.push(format!("{table}.{column} is listed, but has no such column")),
                Some(c) => errors.extend(fits(*kind, c).err().map(|e| format!("{table}.{e}"))),
            }
        }
    }
    let mut plan = Plan::default();
    let mut missing = Vec::new();
    for table in tables {
        for column in &table.columns {
            let listed = rules.get(&table.name).and_then(|columns| columns.get(&column.name));
            let kind = match listed {
                Some(kind) => *kind,
                None if column.generated || plain(column) => Kind::Keep,
                None => {
                    missing.push(format!("{}.{} ({})", table.name, column.name, column.sql_type));
                    continue;
                }
            };
            let planned = Planned { table: table.clone(), column: column.name.clone(), kind };
            if kind != Kind::Keep {
                plan.scrub.push(planned.clone());
            }
            if !binary(column) && !timed(column) {
                plan.scan.push(planned);
            }
        }
    }
    if !missing.is_empty() {
        errors.push(format!(
            "list every column that isn't a number, a truth value, a timestamp or a uuid, with \
             its kind or keep; missing: {}",
            missing.join(", ")
        ));
    }
    if errors.is_empty() { Ok(plan) } else { Err(format!("app '{app}': {}", errors.join("; "))) }
}

fn fits(kind: Kind, column: &Column) -> Result<(), String> {
    let name = &column.name;
    if column.generated {
        return Err(format!("{name} is generated, so it recomputes from others: leave it out"));
    }
    let base = base_type(&column.sql_type);
    let (strings, json) = (STRINGS.contains(&base.as_str()), JSON.contains(&base.as_str()));
    let array = column.sql_type.trim_end().ends_with("[]");
    let ok = match kind {
        Kind::Name | Kind::Email | Kind::Phone | Kind::Secret => strings,
        Kind::Text => strings || json || array,
        Kind::Blank | Kind::Keep => true,
    };
    if ok {
        return Ok(());
    }
    Err(format!(
        "{name} ({}) can't be scrubbed as {}: name, email, phone and secret take text columns, \
         text also json and arrays, and other columns take keep or blank",
        column.sql_type,
        kind.as_str()
    ))
}

fn base_type(sql_type: &str) -> String {
    let lower = sql_type.trim().to_ascii_lowercase();
    lower.split(['(', ' ']).next().unwrap_or_default().to_string()
}

fn plain(column: &Column) -> bool {
    match column.kind {
        ColumnKind::Timestamp | ColumnKind::EpochSeconds | ColumnKind::UnixTime { .. } => true,
        ColumnKind::Date | ColumnKind::Other => {
            PLAIN.contains(&base_type(&column.sql_type).as_str())
        }
    }
}

fn binary(column: &Column) -> bool {
    BINARY.contains(&base_type(&column.sql_type).as_str())
}

fn timed(column: &Column) -> bool {
    match column.kind {
        ColumnKind::Date
        | ColumnKind::Timestamp
        | ColumnKind::EpochSeconds
        | ColumnKind::UnixTime { .. } => true,
        ColumnKind::Other => false,
    }
}

/// The originals fakes must avoid and scans look for.
#[derive(Debug, Default)]
pub struct Originals {
    words: BTreeSet<String>,
    names: BTreeSet<String>,
    phones: BTreeSet<String>,
    secrets: BTreeSet<String>,
}

impl Originals {
    pub fn add(&mut self, kind: Kind, value: &str) {
        match kind {
            Kind::Name => {
                let words = words_of(value);
                self.words.extend(words.iter().map(|w| w.to_lowercase()));
                if words.len() >= 2 {
                    self.names.insert(normal_name(&words));
                }
            }
            Kind::Phone => {
                let digits = digits_of(value);
                if !digits.is_empty() {
                    self.phones.insert(digits);
                }
            }
            Kind::Secret if !value.is_empty() => {
                self.secrets.insert(value.to_string());
            }
            Kind::Secret | Kind::Email | Kind::Text | Kind::Blank | Kind::Keep => {}
        }
    }
}

/// How many fakes the scrubber holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Counts {
    pub words: usize,
    pub phones: usize,
    pub secrets: usize,
}

/// Makes HMAC-keyed fakes: an original always gets the same fake, and no two share
/// one.
pub struct Scrubber {
    mac: HmacSha256,
    words: HashMap<String, String>,
    names: BTreeSet<String>,
    longest_name: usize,
    phones: HashMap<String, String>,
    secrets: HashMap<String, String>,
}

impl Scrubber {
    /// Draw a fake for every original; the key needs `MIN_KEY_BYTES`.
    pub fn new(key: &[u8], originals: &Originals) -> Result<Scrubber, String> {
        if key.len() < MIN_KEY_BYTES {
            return Err(format!(
                "the key has {} bytes; it needs {MIN_KEY_BYTES} or more, such as \
                 `head -c 32 /dev/urandom`",
                key.len()
            ));
        }
        let mac = HmacSha256::new_from_slice(key).map_err(|e| format!("the key: {e}"))?;
        let mut scrubber = Scrubber {
            mac,
            words: HashMap::new(),
            names: originals.names.clone(),
            longest_name: originals.names.iter().map(|n| n.split(' ').count()).max().unwrap_or(0),
            phones: HashMap::new(),
            secrets: HashMap::new(),
        };
        let (long, short): (BTreeSet<_>, BTreeSet<_>) =
            originals.words.iter().cloned().partition(|w| w.chars().count() >= NAME_WORD_MIN);
        scrubber.words = assign(&long, "name words", |w, n| scrubber.fake_word(w, n))?;
        scrubber.words.extend(scrubber.shuffle_short(&short));
        scrubber.phones = assign(&originals.phones, "phones", |d, n| scrubber.fake_digits(d, n))?;
        scrubber.secrets = assign(&originals.secrets, "secrets", |s, n| scrubber.fake_hex(s, n))?;
        Ok(scrubber)
    }

    pub fn counts(&self) -> Counts {
        Counts { words: self.words.len(), phones: self.phones.len(), secrets: self.secrets.len() }
    }

    /// Scrub a value as `kind`; `None` blanks it.
    pub fn scrub(&self, kind: Kind, value: &str) -> Option<String> {
        Some(match kind {
            Kind::Name => self.name(value),
            Kind::Email if is_address(value.trim()) => self.email(value.trim()),
            Kind::Email | Kind::Text => self.text(value),
            Kind::Phone => self.phone(value),
            Kind::Secret => self.secret(value),
            Kind::Blank => return None,
            Kind::Keep => value.to_string(),
        })
    }

    fn name(&self, value: &str) -> String {
        let mut out = String::with_capacity(value.len());
        for (piece, is_word) in split_words(value) {
            if !is_word {
                out.push_str(piece);
                continue;
            }
            let lower = piece.to_lowercase();
            let fake = match self.words.get(&lower) {
                Some(fake) => fake.clone(),
                None => self.fake_word(&lower, 0),
            };
            out.push_str(&cased(&fake, piece));
        }
        out
    }

    fn email(&self, address: &str) -> String {
        let lower = address.to_lowercase();
        let domain = lower.rsplit_once('@').map_or("", |(_, domain)| domain);
        let local = hex(&self.bytes("email", &lower, 0, 8));
        format!("{local}@{}.{FAKE_TLD}", hex(&self.bytes("domain", domain, 0, 4)))
    }

    fn phone(&self, value: &str) -> String {
        let digits = digits_of(value);
        match self.phones.get(&digits) {
            Some(fake) => with_digits(value, fake),
            None => with_digits(value, &self.fake_digits(&digits, 0)),
        }
    }

    fn secret(&self, value: &str) -> String {
        match self.secrets.get(value) {
            Some(fake) => fake.clone(),
            None => self.fake_hex(value, 0),
        }
    }

    /// Scrub free text: emails, known secrets and phones, then name words.
    pub fn text(&self, value: &str) -> String {
        let mut runs = vec![(value.to_string(), false)];
        runs = replace(runs, |s| {
            find_addresses(s).into_iter().map(|(a, b)| (a, b, Some(self.email(&s[a..b])))).collect()
        });
        runs = replace(runs, |s| {
            let known = |token: &str| self.secrets.get(token).cloned();
            secret_tokens(s).into_iter().map(|(a, b)| (a, b, known(&s[a..b]))).collect()
        });
        runs = replace(runs, |s| {
            let fake = |run: &str| self.phones.get(&digits_of(run)).map(|f| with_digits(run, f));
            phone_runs(s).into_iter().map(|(a, b)| (a, b, fake(&s[a..b]))).collect()
        });
        runs = replace(runs, |s| {
            let fake = |word: &str| self.words.get(&word.to_lowercase()).map(|f| cased(f, word));
            let long = |(a, b): &(usize, usize)| s[*a..*b].chars().count() >= NAME_WORD_MIN;
            word_spans(s).into_iter().filter(long).map(|(a, b)| (a, b, fake(&s[a..b]))).collect()
        });
        runs.into_iter().map(|(text, _)| text).collect()
    }

    /// Originals left in a value; single name words only with `words`.
    pub fn leaks(&self, value: &str, words: bool) -> BTreeSet<Leak> {
        let mut found = BTreeSet::new();
        let addresses = find_addresses(value);
        if addresses.iter().any(|&(a, b)| !is_fake_address(&value[a..b])) {
            found.insert(Leak::Email);
        }
        let mut masked = value.to_string();
        for &(a, b) in &addresses {
            masked.replace_range(a..b, &" ".repeat(b - a));
        }
        let value = masked.as_str();
        if phone_runs(value)
            .iter()
            .any(|&(a, b)| self.phones.contains_key(&digits_of(&value[a..b])))
        {
            found.insert(Leak::Phone);
        }
        if secret_tokens(value).iter().any(|&(a, b)| self.secrets.contains_key(&value[a..b])) {
            found.insert(Leak::Secret);
        }
        let spans = word_spans(value);
        let lower: Vec<String> = spans.iter().map(|&(a, b)| value[a..b].to_lowercase()).collect();
        if (2..=self.longest_name)
            .any(|n| lower.windows(n).any(|w| self.names.contains(&w.join(" "))))
        {
            found.insert(Leak::Name);
        }
        let long = |w: &&String| w.chars().count() >= NAME_WORD_MIN;
        if words && lower.iter().filter(long).any(|w| self.words.contains_key(w)) {
            found.insert(Leak::NameWord);
        }
        found
    }

    fn bytes(&self, kind: &str, value: &str, attempt: u32, n: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(n + 32);
        let mut block = 0u32;
        while out.len() < n {
            let mut mac = self.mac.clone();
            for part in [kind.as_bytes(), &[0], value.as_bytes(), &[0]] {
                mac.update(part);
            }
            mac.update(&attempt.to_le_bytes());
            mac.update(&block.to_le_bytes());
            out.extend_from_slice(&mac.finalize().into_bytes());
            block += 1;
        }
        out.truncate(n);
        out
    }

    fn fake_word(&self, word: &str, attempt: u32) -> String {
        let chars: Vec<char> = word.chars().collect();
        let bytes = self.bytes("word", word, attempt, chars.len() + 1);
        let mut vowel = bytes[0] & 1 == 1;
        let mut out = String::with_capacity(chars.len());
        for (c, b) in chars.iter().zip(&bytes[1..]) {
            if c.is_ascii_digit() {
                out.push(char::from(b'0' + b % 10));
                continue;
            }
            let pool = if vowel { VOWELS } else { CONSONANTS };
            out.push(char::from(pool[usize::from(*b) % pool.len()]));
            vowel = !vowel;
        }
        out
    }

    fn shuffle_short(&self, short: &BTreeSet<String>) -> HashMap<String, String> {
        let mut fakes = HashMap::new();
        for len in 1..NAME_WORD_MIN {
            let mut space = strings_of_len(len);
            space.sort_by_cached_key(|s| self.bytes("short", s, 0, 8));
            let words = short.iter().filter(|w| w.chars().count() == len);
            fakes.extend(words.cloned().zip(space));
        }
        fakes
    }

    fn fake_digits(&self, digits: &str, attempt: u32) -> String {
        let bytes = self.bytes("phone", digits, attempt, digits.len());
        bytes.iter().map(|b| char::from(b'0' + b % 10)).collect()
    }

    fn fake_hex(&self, secret: &str, attempt: u32) -> String {
        let len = secret.chars().count();
        let mut out = hex(&self.bytes("secret", secret, attempt, len.div_ceil(2)));
        out.truncate(len);
        out
    }
}

const CREDENTIALS: [&str; 3] = ["authorization", "cookie", "proxy-authorization"];
const CALL_TOOLS: [&str; 2] = ["api_request", "api_docs"];

/// A production agent trace: its request, API calls and answer, kept under a consent
/// basis.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Trace {
    pub id: String,
    #[serde(default)]
    pub consent: Option<String>,
    pub request: String,
    pub calls: Vec<Call>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persona: Option<String>,
}

/// A call of a trace.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Call {
    pub tool: String,
    #[serde(default)]
    pub arguments: Map<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
}

/// Read a JSONL trace file, with each trace's line number.
pub fn read_traces(text: &str) -> Result<Vec<(usize, Trace)>, String> {
    let mut traces = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let at = |e: String| format!("line {}: {e}", i + 1);
        let trace: Trace = serde_json::from_str(line).map_err(|e| at(e.to_string()))?;
        if trace.id.trim().is_empty() || trace.request.trim().is_empty() {
            return Err(at("`id` and `request` must hold text".into()));
        }
        if trace.calls.is_empty() {
            return Err(at("`calls` must list at least one call".into()));
        }
        if let Some(call) = trace.calls.iter().find(|c| !CALL_TOOLS.contains(&c.tool.as_str())) {
            return Err(at(format!(
                "a call's tool is '{}', not api_request or api_docs",
                call.tool
            )));
        }
        traces.push((i + 1, trace));
    }
    Ok(traces)
}

/// Whether the trace names a consent basis.
pub fn consented(trace: &Trace) -> bool {
    trace.consent.as_deref().is_some_and(|basis| !basis.trim().is_empty())
}

/// Add the originals the traces' fields hold, in bodies and query strings.
pub fn add_fields(
    originals: &mut Originals,
    traces: &[(usize, Trace)],
    fields: &BTreeMap<String, Kind>,
) {
    for (_, trace) in traces {
        for call in &trace.calls {
            walk_fields(&Value::Object(call.arguments.clone()), None, fields, &mut |kind, text| {
                originals.add(kind, text);
            });
            if let Some(path) = call.arguments.get("path").and_then(Value::as_str) {
                for (key, value) in query_pairs(path) {
                    if let Some(kind) = fields.get(&key) {
                        originals.add(*kind, &value);
                    }
                }
            }
        }
    }
}

fn walk_fields(
    value: &Value,
    key: Option<&str>,
    fields: &BTreeMap<String, Kind>,
    found: &mut impl FnMut(Kind, &str),
) {
    match value {
        Value::String(text) => {
            if let Some(kind) = key.and_then(|key| fields.get(key)) {
                found(*kind, text);
            }
        }
        Value::Array(items) => {
            for item in items {
                walk_fields(item, key, fields, found);
            }
        }
        Value::Object(map) => {
            for (key, item) in map {
                walk_fields(item, Some(key), fields, found);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

impl Scrubber {
    /// Scrub a trace, dropping credential headers; returns it and how many were dropped.
    pub fn trace(&self, trace: &Trace, fields: &BTreeMap<String, Kind>) -> (Trace, usize) {
        let mut dropped = 0;
        let calls = trace
            .calls
            .iter()
            .map(|call| {
                let mut arguments = Map::new();
                for (key, value) in &call.arguments {
                    let scrubbed = match key.as_str() {
                        "method" | "app" => value.clone(),
                        "path" => value
                            .as_str()
                            .map_or_else(|| value.clone(), |p| Value::String(self.path(p, fields))),
                        "headers" => self.headers(value, &mut dropped),
                        _ => self.json(value, Some(key), fields),
                    };
                    arguments.insert(key.clone(), scrubbed);
                }
                Call { tool: call.tool.clone(), arguments, status: call.status }
            })
            .collect();
        let scrubbed = Trace {
            id: self.text(&trace.id),
            consent: trace.consent.as_deref().map(|basis| self.text(basis)),
            request: self.text(&trace.request),
            calls,
            answer: trace.answer.as_deref().map(|answer| self.text(answer)),
            persona: trace.persona.clone(),
        };
        (scrubbed, dropped)
    }

    fn json(&self, value: &Value, key: Option<&str>, fields: &BTreeMap<String, Kind>) -> Value {
        match value {
            Value::String(text) => {
                let kind = key.and_then(|key| fields.get(key)).copied().unwrap_or(Kind::Text);
                self.scrub(kind, text).map_or(Value::Null, Value::String)
            }
            Value::Array(items) => {
                Value::Array(items.iter().map(|item| self.json(item, key, fields)).collect())
            }
            Value::Object(map) => Value::Object(
                map.iter().map(|(k, item)| (k.clone(), self.json(item, Some(k), fields))).collect(),
            ),
            Value::Null | Value::Bool(_) | Value::Number(_) => value.clone(),
        }
    }

    fn headers(&self, value: &Value, dropped: &mut usize) -> Value {
        let Value::Object(map) = value else { return self.json(value, None, &BTreeMap::new()) };
        let mut kept = Map::new();
        for (name, header) in map {
            if CREDENTIALS.contains(&name.to_ascii_lowercase().as_str()) {
                *dropped += 1;
                continue;
            }
            kept.insert(name.clone(), self.json(header, None, &BTreeMap::new()));
        }
        Value::Object(kept)
    }

    fn path(&self, path: &str, fields: &BTreeMap<String, Kind>) -> String {
        let (route, query) = path.split_once('?').map_or((path, None), |(r, q)| (r, Some(q)));
        let segments: Vec<String> = route
            .split('/')
            .map(|segment| {
                let plain = decode(segment, false);
                let scrubbed = self.text(&plain);
                if scrubbed == plain { segment.to_string() } else { percent_encode(&scrubbed) }
            })
            .collect();
        let mut out = segments.join("/");
        if let Some(query) = query {
            let pairs: Vec<String> = query
                .split('&')
                .map(|pair| {
                    let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
                    let (plain_key, plain) = (decode(key, true), decode(value, true));
                    let kind = fields.get(&plain_key).copied().unwrap_or(Kind::Text);
                    let scrubbed = self.scrub(kind, &plain).unwrap_or_default();
                    if scrubbed == plain {
                        pair.to_string()
                    } else {
                        format!("{key}={}", percent_encode(&scrubbed))
                    }
                })
                .collect();
            out = format!("{out}?{}", pairs.join("&"));
        }
        out
    }

    /// Originals left in a trace, by field.
    pub fn trace_leaks(&self, trace: &Trace) -> Vec<(String, Leak)> {
        let mut found = Vec::new();
        let mut check = |field: String, text: &str| {
            for leak in self.leaks(text, true) {
                found.push((field.clone(), leak));
            }
        };
        check("id".into(), &trace.id);
        check("consent".into(), trace.consent.as_deref().unwrap_or_default());
        check("request".into(), &trace.request);
        check("answer".into(), trace.answer.as_deref().unwrap_or_default());
        for (i, call) in trace.calls.iter().enumerate() {
            let mut strings = Vec::new();
            strings_of(&Value::Object(call.arguments.clone()), &mut strings);
            for text in strings {
                let plain = decode(&text, true);
                check(format!("calls[{i}]"), &plain);
            }
        }
        found.sort();
        found.dedup();
        found
    }
}

fn strings_of(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(text) => out.push(text.clone()),
        Value::Array(items) => {
            for item in items {
                strings_of(item, out);
            }
        }
        Value::Object(map) => {
            for (key, item) in map {
                out.push(key.clone());
                strings_of(item, out);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn query_pairs(path: &str) -> Vec<(String, String)> {
    let Some((_, query)) = path.split_once('?') else { return Vec::new() };
    query
        .split('&')
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (decode(key, true), decode(value, true))
        })
        .collect()
}

fn decode(text: &str, query: bool) -> String {
    let spaced = if query { text.replace('+', " ") } else { text.to_string() };
    percent_decode_str(&spaced).decode_utf8_lossy().into_owned()
}

/// An original a scan found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Leak {
    Email,
    Phone,
    Secret,
    Name,
    NameWord,
}

impl Leak {
    pub fn as_str(self) -> &'static str {
        match self {
            Leak::Email => "an email address that isn't a fake",
            Leak::Phone => "an original phone number",
            Leak::Secret => "an original secret",
            Leak::Name => "an original full name",
            Leak::NameWord => "an original name word",
        }
    }
}

fn assign(
    originals: &BTreeSet<String>,
    what: &str,
    draw: impl Fn(&str, u32) -> String,
) -> Result<HashMap<String, String>, String> {
    let mut fakes = HashMap::new();
    let mut taken: std::collections::HashSet<String> = std::collections::HashSet::new();
    for original in originals {
        let free = (0..ATTEMPTS)
            .map(|attempt| draw(original, attempt))
            .find(|fake| !originals.contains(fake) && !taken.contains(fake));
        let Some(fake) = free else {
            return Err(format!(
                "{ATTEMPTS} draws found no free fake for one of the {} {what}: their values fill \
                 the space of their length",
                originals.len()
            ));
        };
        taken.insert(fake.clone());
        fakes.insert(original.clone(), fake);
    }
    Ok(fakes)
}

fn strings_of_len(len: usize) -> Vec<String> {
    let mut all = vec![String::new()];
    for _ in 0..len {
        all = all.iter().flat_map(|s| ('a'..='z').map(move |c| format!("{s}{c}"))).collect();
    }
    all
}

fn replace(
    runs: Vec<(String, bool)>,
    find: impl Fn(&str) -> Vec<(usize, usize, Option<String>)>,
) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    for (text, written) in runs {
        if written {
            out.push((text, true));
            continue;
        }
        let mut at = 0;
        for (start, end, fake) in find(&text) {
            let Some(fake) = fake else { continue };
            out.push((text[at..start].to_string(), false));
            out.push((fake, true));
            at = end;
        }
        out.push((text[at..].to_string(), false));
    }
    out
}

fn cased(fake: &str, like: &str) -> String {
    let letters: Vec<char> = like.chars().filter(|c| c.is_alphabetic()).collect();
    if letters.len() > 1 && letters.iter().all(|c| c.is_uppercase()) {
        return fake.to_uppercase();
    }
    if like.chars().next().is_some_and(char::is_uppercase) {
        let mut chars = fake.chars();
        return chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default();
    }
    fake.to_string()
}

fn normal_name(words: &[&str]) -> String {
    words.iter().map(|w| w.to_lowercase()).collect::<Vec<_>>().join(" ")
}

fn words_of(value: &str) -> Vec<&str> {
    split_words(value).into_iter().filter(|(_, word)| *word).map(|(piece, _)| piece).collect()
}

fn split_words(value: &str) -> Vec<(&str, bool)> {
    let mut pieces = Vec::new();
    let mut start = 0;
    let mut current: Option<bool> = None;
    for (i, c) in value.char_indices() {
        let word = c.is_alphanumeric();
        if current.is_some_and(|w| w != word) {
            pieces.push((&value[start..i], !word));
            start = i;
        }
        current = Some(word);
    }
    if let Some(word) = current {
        pieces.push((&value[start..], word));
    }
    pieces
}

fn word_spans(value: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut at = 0;
    for (piece, word) in split_words(value) {
        if word {
            spans.push((at, at + piece.len()));
        }
        at += piece.len();
    }
    spans
}

fn digits_of(value: &str) -> String {
    value.chars().filter(char::is_ascii_digit).collect()
}

fn with_digits(value: &str, digits: &str) -> String {
    let mut fresh = digits.chars();
    value.chars().map(|c| if c.is_ascii_digit() { fresh.next().unwrap_or(c) } else { c }).collect()
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

fn local_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "._%+-".contains(c)
}

fn domain_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '.' || c == '-'
}

fn find_addresses(value: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut from = 0;
    for (at, _) in value.match_indices('@') {
        let start = value[from..at]
            .char_indices()
            .rev()
            .take_while(|(_, c)| local_char(*c))
            .last()
            .map_or(at, |(i, _)| from + i);
        let tail = &value[at + 1..];
        let mut end = at + 1 + tail.find(|c: char| !domain_char(c)).unwrap_or(tail.len());
        while end > at + 1 && value[..end].ends_with(['.', '-']) {
            end -= 1;
        }
        if start < at && is_address(&value[start..end]) {
            spans.push((start, end));
            from = end;
        }
    }
    spans
}

fn is_address(value: &str) -> bool {
    let Some((local, domain)) = value.rsplit_once('@') else { return false };
    let tld = domain.rsplit_once('.').map_or("", |(_, tld)| tld);
    !local.is_empty()
        && local.chars().all(local_char)
        && domain.chars().all(domain_char)
        && tld.len() >= 2
        && tld.chars().all(|c| c.is_ascii_alphabetic())
}

fn is_fake_address(address: &str) -> bool {
    let hexes = |s: &str, n: usize| s.len() == n && s.chars().all(|c| c.is_ascii_hexdigit());
    let Some((local, domain)) = address.split_once('@') else { return false };
    let Some((name, tld)) = domain.split_once('.') else { return false };
    hexes(local, 16) && hexes(name, 8) && tld == FAKE_TLD
}

fn secret_tokens(value: &str) -> Vec<(usize, usize)> {
    let secret_char = |c: char| c.is_ascii_alphanumeric() || "_-./+$=:~".contains(c);
    let mut spans = Vec::new();
    let mut start = None;
    for (i, c) in value.char_indices().chain([(value.len(), ' ')]) {
        match (start, secret_char(c) && i < value.len()) {
            (None, true) => start = Some(i),
            (Some(s), false) => {
                let end = s + value[s..i].trim_end_matches(['.', ':']).len();
                if end - s >= SECRET_MIN {
                    spans.push((s, end));
                }
                start = None;
            }
            (None, false) | (Some(_), true) => {}
        }
    }
    spans
}

fn phone_runs(value: &str) -> Vec<(usize, usize)> {
    let bytes = value.as_bytes();
    let digit = |k: usize| bytes.get(k).is_some_and(u8::is_ascii_digit);
    let separator = |k: usize| bytes.get(k).is_some_and(|c| b" .-/()".contains(c));
    let mut spans = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let opened = (bytes[i] == b'+' || bytes[i] == b'(') && digit(i + 1);
        if !digit(i) && !opened {
            i += 1;
            continue;
        }
        let start = i;
        let mut j = if opened { i + 1 } else { i };
        let end = loop {
            while digit(j) {
                j += 1;
            }
            let gap = (1..=2).find(|&k| (j..j + k).all(separator) && digit(j + k));
            match gap {
                Some(k) => j += k,
                None => break j,
            }
        };
        if digits_of(&value[start..end]).len() >= PHONE_DIGITS_MIN {
            spans.push((start, end));
        }
        i = end.max(start + 1);
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &[u8] = b"0123456789abcdef0123456789abcdef";

    fn scrubber(key: &[u8], names: &[&str], phones: &[&str], secrets: &[&str]) -> Scrubber {
        let mut originals = Originals::default();
        for (kind, values) in [(Kind::Name, names), (Kind::Phone, phones), (Kind::Secret, secrets)]
        {
            for value in values {
                originals.add(kind, value);
            }
        }
        Scrubber::new(key, &originals).unwrap()
    }

    #[test]
    fn fakes_are_keyed_and_keep_case_and_length() {
        let a = scrubber(KEY, &["Elena Torres", "TORRES Inc"], &[], &["tok-dana-0001"]);
        let b = scrubber(b"another key, also of 32 bytes ok", &["Elena Torres"], &[], &[]);
        let elena = a.scrub(Kind::Name, "Elena Torres").unwrap();
        assert_eq!(elena, a.scrub(Kind::Name, "Elena Torres").unwrap(), "the same every time");
        assert_ne!(elena, b.scrub(Kind::Name, "Elena Torres").unwrap(), "another key, another");
        assert_eq!(elena.len(), "Elena Torres".len());
        assert!(elena.chars().next().unwrap().is_uppercase() && elena.contains(' '), "{elena}");
        let shout = a.scrub(Kind::Name, "TORRES Inc").unwrap();
        assert_eq!(
            shout.split(' ').next().unwrap(),
            elena.split(' ').nth(1).unwrap().to_uppercase()
        );
        let token = a.scrub(Kind::Secret, "tok-dana-0001").unwrap();
        assert_eq!(token.len(), 13);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()), "{token}");
        assert_eq!(a.scrub(Kind::Blank, "x"), None);
        assert_eq!(a.scrub(Kind::Keep, "won").as_deref(), Some("won"));
    }

    #[test]
    fn a_name_word_reads_the_same_in_names_text_and_free_words() {
        let s = scrubber(KEY, &["Elena Torres", "Northwind Traders"], &[], &[]);
        let name = s.scrub(Kind::Name, "Elena Torres").unwrap();
        let (first, last) = name.split_once(' ').unwrap();
        let note = s.text("Spoke with Elena; Torres family business. TORRES confirmed.");
        assert_eq!(
            note,
            format!(
                "Spoke with {first}; {last} family business. {} confirmed.",
                last.to_uppercase()
            )
        );
        assert!(s.leaks(&note, true).is_empty(), "{note}");
        assert!(s.leaks("Elena called", true).contains(&Leak::NameWord));
        assert!(
            !s.leaks("Elena called", false).contains(&Leak::NameWord),
            "keep columns: no words"
        );
        assert!(s.leaks("by elena  torres", false).contains(&Leak::Name), "full names everywhere");
    }

    #[test]
    fn fakes_avoid_originals_and_each_other_and_short_words_shuffle() {
        let s = scrubber(KEY, &["Ada B.", "Ben C.", "Li Wu"], &["555 0100"], &[]);
        let originals: BTreeSet<&str> = ["ada", "ben", "b", "c", "li", "wu"].into();
        let fakes: Vec<&String> = s.words.values().collect();
        assert_eq!(fakes.len(), 6);
        assert_eq!(fakes.iter().collect::<BTreeSet<_>>().len(), 6, "no two share a fake");
        for (word, fake) in &s.words {
            assert_eq!(word.chars().count(), fake.chars().count(), "{word} -> {fake}");
            if word.len() >= NAME_WORD_MIN {
                assert!(!originals.contains(fake.as_str()), "{word} -> {fake}");
            }
        }
        let names: Vec<String> = ('A'..='Z').map(|c| format!("Ann {c}.")).collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let initials = scrubber(KEY, &names, &[], &[]);
        assert_eq!(initials.counts().words, 27, "ann, and every initial, dealt a letter");
        let dealt: BTreeSet<&String> =
            initials.words.iter().filter(|(w, _)| w.len() == 1).map(|(_, f)| f).collect();
        assert_eq!(dealt.len(), 26, "the 26 initials are shuffled among the 26 letters");
    }

    #[test]
    fn collisions_resolve_the_same_whatever_the_input_order() {
        let mut one = Originals::default();
        let mut two = Originals::default();
        for name in ["Mara Lind", "Omar Hale", "Ines Roth"] {
            one.add(Kind::Name, name);
        }
        for name in ["Ines Roth", "Mara Lind", "Omar Hale"] {
            two.add(Kind::Name, name);
        }
        let (one, two) = (Scrubber::new(KEY, &one).unwrap(), Scrubber::new(KEY, &two).unwrap());
        assert_eq!(one.words, two.words);
    }

    #[test]
    fn a_fake_equal_to_another_original_is_drawn_again() {
        let plain = scrubber(KEY, &["Elena"], &[], &[]);
        let taken = plain.words["elena"].clone();
        let crowded = scrubber(KEY, &["Elena", &cased(&taken, "Xx")], &[], &[]);
        assert_ne!(crowded.words["elena"], taken, "its first draw is now someone's name");
    }

    #[test]
    fn addresses_share_their_domains_fake_in_any_case() {
        let s = scrubber(KEY, &[], &[], &[]);
        let one = s.scrub(Kind::Email, "Elena@Northwind.test").unwrap();
        assert_eq!(one, s.scrub(Kind::Email, "elena@northwind.test").unwrap());
        let two = s.scrub(Kind::Email, "ap@northwind.test").unwrap();
        assert_ne!(one, two);
        assert_eq!(one.split_once('@').unwrap().1, two.split_once('@').unwrap().1);
        assert!(is_fake_address(&one) && one.ends_with(".example"), "{one}");
        assert!(s.leaks(&format!("mail {one} or {two}"), true).is_empty());
        assert!(s.leaks("mail ap@northwind.test", false).contains(&Leak::Email));
        let text = s.text("Write to Elena@Northwind.test, or ap@northwind.test.");
        assert_eq!(text, format!("Write to {one}, or {two}."));
    }

    #[test]
    fn phones_match_by_digits_whatever_the_format() {
        let s = scrubber(KEY, &[], &["+1 415-555-0134"], &[]);
        let fake = s.scrub(Kind::Phone, "+1 415-555-0134").unwrap();
        assert_eq!(fake.len(), 15);
        assert!(fake.starts_with('+') && &fake[2..3] == " " && &fake[6..7] == "-", "{fake}");
        let text = s.text("Call +1 (415) 555 0134 today");
        assert!(!text.contains("415") && text.starts_with("Call +") && text.ends_with(" today"));
        assert!(s.leaks("call 1.415.555.0134", false).contains(&Leak::Phone));
        assert!(s.leaks(&text, true).is_empty(), "{text}");
        assert!(s.text("Invoice INV-2026-0042 for 1250").contains("INV-2026-0042"), "not a phone");
    }

    #[test]
    fn a_pass_never_rewrites_what_an_earlier_one_wrote() {
        let s = scrubber(KEY, &["Ada Example"], &[], &[]);
        let fake = s.email("someone@company.test");
        let wrote = s.text("Ada Example wrote to someone@company.test");
        assert!(wrote.ends_with(&format!(" wrote to {fake}")), "{wrote}");
        assert!(!wrote.starts_with("Ada Example"), "the name itself is scrubbed: {wrote}");
        assert!(s.leaks(&wrote, true).is_empty(), "{wrote}");
    }

    #[test]
    fn secrets_are_found_as_whole_tokens() {
        let s = scrubber(KEY, &[], &[], &["seed-token-dana"]);
        let text = s.text("Authorization: Bearer seed-token-dana.");
        assert!(!text.contains("seed-token-dana") && text.ends_with('.'), "{text}");
        assert!(s.leaks("Bearer seed-token-dana", false).contains(&Leak::Secret));
        assert!(s.leaks(&text, false).is_empty());
    }

    fn trace(json: &str) -> Trace {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn a_trace_file_is_refused_line_by_line() {
        let ok =
            r#"{"id": "a", "consent": "opt-in", "request": "x", "calls": [{"tool": "api_docs"}]}"#;
        let cases = [
            ("{not json", "line 2"),
            (r#"{"id": "b", "request": "x", "calls": [], "consent": "y"}"#, "at least one call"),
            (r#"{"id": " ", "request": "x", "calls": [{"tool": "api_docs"}]}"#, "must hold text"),
            (r#"{"id": "b", "request": "x", "calls": [{"tool": "submit"}]}"#, "not api_request"),
            (
                r#"{"id": "b", "request": "x", "calls": [{"tool": "api_docs"}], "who": 1}"#,
                "unknown field",
            ),
            (
                r#"{"id": "b", "request": "x", "calls": [{"tool": "api_docs", "took": 1}]}"#,
                "unknown field",
            ),
        ];
        for (line, expected) in cases {
            let err = read_traces(&format!("{ok}\n{line}\n")).unwrap_err();
            assert!(err.contains(expected) && err.starts_with("line 2"), "{line}: {err}");
        }
        let blank = trace(
            r#"{"id": "a", "consent": "  ", "request": "x", "calls": [{"tool": "api_docs"}]}"#,
        );
        assert!(!consented(&blank));
        let missing = trace(r#"{"id": "a", "request": "x", "calls": [{"tool": "api_docs"}]}"#);
        assert!(!consented(&missing) && consented(&trace(ok)));
        assert_eq!(read_traces(&format!("\n{ok}\n\n")).unwrap()[0].0, 2, "lines count from 1");
    }

    #[test]
    fn a_trace_scrubs_like_the_snapshot_and_its_fields_alike() {
        let fields: BTreeMap<String, Kind> =
            [("name", Kind::Name), ("email", Kind::Email)].map(|(k, v)| (k.to_string(), v)).into();
        let raw = trace(
            r#"{"id": "inc-1", "consent": "opt-in by Mara Lindqvist", "request": "Mara Lindqvist asks to add Marco Bianchi, marco@harbor.test", "calls": [
                {"tool": "api_request", "arguments": {"app": "crm", "method": "GET", "path": "/contacts?q=Mara%20Lindqvist&limit=5"}, "status": 200},
                {"tool": "api_request", "arguments": {"method": "POST", "path": "/contacts", "headers": {"Authorization": "Bearer tok-1", "COOKIE": "s=1", "X-Note": "for Mara"}, "body": {"name": "Marco Bianchi", "email": "marco@harbor.test", "tags": ["Mara"]}}}]}"#,
        );
        let mut originals = Originals::default();
        originals.add(Kind::Name, "Mara Lindqvist");
        add_fields(&mut originals, &[(1, raw.clone())], &fields);
        let s = Scrubber::new(KEY, &originals).unwrap();
        let (clean, dropped) = s.trace(&raw, &fields);
        assert_eq!(dropped, 2, "Authorization and COOKIE, in any case");
        let marco = s.scrub(Kind::Name, "Marco Bianchi").unwrap();
        let mara = s.scrub(Kind::Name, "Mara Lindqvist").unwrap();
        let address = s.scrub(Kind::Email, "marco@harbor.test").unwrap();
        assert_eq!(clean.request, format!("{mara} asks to add {marco}, {address}"));
        assert_eq!(clean.consent.as_deref(), Some(format!("opt-in by {mara}").as_str()));
        let (get, post) = (&clean.calls[0].arguments, &clean.calls[1].arguments);
        assert_eq!((get["app"].as_str(), get["method"].as_str()), (Some("crm"), Some("GET")));
        let (first, last) = mara.split_once(' ').unwrap();
        assert_eq!(get["path"], format!("/contacts?q={first}%20{last}&limit=5"));
        assert_eq!(post["body"]["name"], marco.as_str());
        assert_eq!(post["body"]["email"], address.as_str());
        assert_eq!(post["body"]["tags"][0], first, "a string in a list, as text");
        assert_eq!(post["headers"], serde_json::json!({"X-Note": format!("for {first}")}));
        assert_eq!(clean.calls[0].status, Some(200));
        assert!(s.trace_leaks(&clean).is_empty(), "{:?}", s.trace_leaks(&clean));
        let mut keyed = clean.clone();
        keyed.calls[1].arguments.insert("marco@harbor.test".into(), Value::Bool(true));
        assert_eq!(
            s.trace_leaks(&keyed),
            [("calls[1]".to_string(), Leak::Email)],
            "keys are scanned too"
        );
    }

    #[test]
    fn a_short_key_is_refused() {
        let err = Scrubber::new(b"short", &Originals::default()).err().unwrap();
        assert!(err.contains("needs 32 or more"), "{err}");
    }

    fn table(name: &str, columns: &[(&str, &str, ColumnKind, bool)]) -> Table {
        let columns = columns
            .iter()
            .map(|(name, sql_type, kind, generated)| Column {
                name: (*name).into(),
                kind: *kind,
                generated: *generated,
                pk: false,
                sql_type: (*sql_type).into(),
            })
            .collect();
        Table {
            name: name.into(),
            schema: "public".into(),
            qualified: format!("\"public\".\"{name}\""),
            columns,
            partitioned: false,
            id: 0,
            captured: false,
        }
    }

    fn contacts() -> Vec<Table> {
        let other = ColumnKind::Other;
        vec![table(
            "contacts",
            &[
                ("id", "integer", other, false),
                ("name", "text", other, false),
                ("email", "character varying(255)", other, false),
                ("birthday", "date", ColumnKind::Date, false),
                ("created_at", "timestamp with time zone", ColumnKind::Timestamp, false),
                ("amount", "numeric(12,2)", other, false),
                ("photo", "bytea", other, false),
                ("search", "tsvector", other, true),
            ],
        )]
    }

    fn rules(yaml: &str) -> BTreeMap<String, BTreeMap<String, Kind>> {
        serde_saphyr::from_str(yaml).unwrap()
    }

    #[test]
    fn the_plan_lists_what_it_scrubs_and_scans_and_fails_closed() {
        let full = "contacts: {name: name, email: email, birthday: blank, photo: keep}";
        let planned = plan("crm", &contacts(), &rules(full)).unwrap();
        let scrubbed: Vec<String> = planned.scrub.iter().map(Planned::label).collect();
        assert_eq!(scrubbed, ["contacts.name", "contacts.email", "contacts.birthday"]);
        let scanned: Vec<String> = planned.scan.iter().map(Planned::label).collect();
        assert!(!scanned.contains(&"contacts.photo".to_string()), "binary isn't scanned");
        assert!(scanned.contains(&"contacts.search".to_string()), "generated is scanned");
        let missing = plan("crm", &contacts(), &rules("contacts: {name: name}")).unwrap_err();
        assert!(missing.starts_with("app 'crm': "), "{missing}");
        assert!(
            missing.contains("contacts.email (character varying(255)), contacts.birthday (date), contacts.photo (bytea)"),
            "{missing}"
        );
        assert!(
            !missing.contains("created_at")
                && !missing.contains("amount")
                && !missing.contains("search")
        );
    }

    #[test]
    fn the_plan_refuses_what_it_cant_scrub() {
        let full = "name: name, email: email, birthday: blank, photo: keep";
        let cases = [
            (format!("contacts: {{{full}}}\ndeals: {{name: text}}"), "table 'deals' is listed"),
            (format!("contacts: {{{full}, nickname: name}}"), "contacts.nickname is listed, but"),
            (
                "contacts: {name: name, email: email, birthday: name, photo: keep}".to_string(),
                "contacts.birthday (date) can't be scrubbed as name",
            ),
            (
                "contacts: {name: name, email: email, birthday: blank, photo: text}".to_string(),
                "contacts.photo (bytea) can't be scrubbed as text",
            ),
            (format!("contacts: {{{full}, search: blank}}"), "contacts.search is generated"),
        ];
        for (yaml, expected) in cases {
            let err = plan("crm", &contacts(), &rules(&yaml)).unwrap_err();
            assert!(err.contains(expected), "{yaml}: {err}");
        }
    }

    #[test]
    fn a_field_holds_a_value_kind() {
        let spec = Spec::parse("apps: {crm: {}}\nfields: {email: email, company: name}").unwrap();
        assert_eq!(spec.fields["company"], Kind::Name);
        let err = Spec::parse("apps: {}\nfields: {note: text}").unwrap_err();
        assert!(err.contains("'note' is text"), "{err}");
        assert!(Spec::parse("apps: {}\nsurprise: 1").unwrap_err().contains("unknown field"));
    }
}
