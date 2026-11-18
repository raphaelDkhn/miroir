//! `miroir draft` (Appendix B1), on the host: a draft `MIROIR_SPEC` from the evidence of one or
//! more reference runs of a task, so that a reviewer edits a spec instead of writing one.
//!
//! What every run did is drafted: each row change gets an `allow` entry and a check, and each
//! request, email and answer gets a check, over the values every run shares. Row changes match
//! across runs by table, op and primary key, and inserts by their order, since the keys a product
//! assigns differ between runs. A row change only some runs made is allowed but not checked. A
//! check leaves out the values that differ between runs, those that are the key of a row the run
//! inserted, and long text; comments after the spec name them, what only some runs did, and the
//! dates and times the draft compares as constants. A printed draft always parses, and every run
//! it was drafted from scores reward 1 and harm 0 with it, unless the run did what no spec admits
//! (a truncate, a schema change or a fault), which a comment names.
//!
//! A value counts as the key of a row the run inserted only in the answer, or in a column named
//! after that row's table (`label_id` for a new `labels` row, see `names`). A reference named by
//! role or abbreviation, such as Gitea's `poster_id` for a new `user` row or `repo_id` for a new
//! `repository` row, is not recognized: when every run got the same key, the draft compares it as
//! a constant, against authoring rule 4. Review each `*_id` value that equals a new row's key.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, anyhow, bail};
use serde_json::{Map, Value, json};
use sqlx::Connection;

use crate::evidence::{self, ChangeOp, NetChange};
use crate::grade::{self, Answer, allow_line, key_condition, sql_literal};
use crate::paths::{self, Paths};
use crate::spec::{self, Op};

/// The longest text a check compares, in bytes: longer text is what a legitimate run words its own
/// way, and enough of it would pass the spec's size limit.
const CHECKED_BYTES: usize = 256;

/// The errors Harbor records when its verifier failed after the run, such as on the void of a
/// placeholder spec: the run and its evidence are whole.
const VERIFIER_ERRORS: [&str; 4] = [
    "RewardFileNotFoundError",
    "RewardFileEmptyError",
    "VerifierOutputParseError",
    "VerifierTimeoutError",
];

/// Unix times from 2000 to 2100, in seconds.
const UNIX_SECONDS: Range<i64> = 946_684_800..4_102_444_800;

/// What to draft from.
#[derive(Debug, Clone)]
pub struct DraftArgs {
    /// Harbor trial directories of one task, or `evidence.db` files.
    pub inputs: Vec<PathBuf>,
}

/// Print a draft spec to `out`. `Ok(true)` when a draft was printed, `Ok(false)` when there is
/// nothing to draft (then `out` gets a comment saying why); an error is a usage or I/O error.
pub fn run(args: &DraftArgs, out: &mut impl std::io::Write) -> anyhow::Result<bool> {
    if args.inputs.is_empty() {
        bail!(
            "nothing to draft from: pass the Harbor trial directories of one task, or \
             evidence.db files"
        );
    }
    let runs = read_all(&args.inputs)?;
    distinct(&runs)?;
    let task = task(&runs)?;
    let draft = Draft::of(&runs);
    if draft.checks.is_empty() {
        out.write_all(draft.nothing(&runs).as_bytes())?;
        return Ok(false);
    }
    let text = draft.text(&runs, task);
    spec::parse(&text)
        .map_err(|e| anyhow!("the draft is not a valid spec, so it is not printed: {e}"))?;
    out.write_all(text.as_bytes())?;
    Ok(true)
}

/// What one reference run did.
struct Run {
    /// The input as given.
    input: PathBuf,
    /// The evidence file it was read from, as a canonical path.
    evidence: PathBuf,
    /// The task its `result.json` names; `None` for an `evidence.db` input.
    task: Option<String>,
    changes: Vec<NetChange>,
    /// The method and path of each request the product sent, in journal order.
    requests: Vec<(String, String)>,
    /// The recipients (a JSON array) and the subject of each email, in journal order.
    emails: Vec<(String, Option<String>)>,
    /// As the grader reads it; none for an `evidence.db` input.
    answer: Answer,
    /// The kind and detail of each fault.
    faults: Vec<(String, String)>,
}

/// Read every input on a runtime of its own, in a thread of its own: the command line calls `run`
/// from inside its runtime, where blocking on another runtime would panic.
fn read_all(inputs: &[PathBuf]) -> anyhow::Result<Vec<Run>> {
    std::thread::scope(|scope| {
        let reader = scope.spawn(|| -> anyhow::Result<Vec<Run>> {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
            runtime.block_on(async {
                let mut runs = Vec::new();
                for input in inputs {
                    runs.push(read(input).await?);
                }
                Ok(runs)
            })
        });
        reader.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    })
}

/// Read one input: a Harbor trial directory, or an `evidence.db` file.
async fn read(input: &Path) -> anyhow::Result<Run> {
    if !input.is_dir() {
        return recorded(input).await;
    }
    let task = Some(reference_task(input)?);
    let answer = read_answer(&artifact(input, paths::ANSWER))?;
    let run = recorded(&artifact(input, Paths::default().evidence())).await?;
    Ok(Run { input: input.to_path_buf(), task, answer, ..run })
}

/// Where a trial directory holds what Harbor collected from `path` in a container: under
/// `artifacts/`, at the same path.
fn artifact(trial: &Path, path: impl AsRef<Path>) -> PathBuf {
    let path = path.as_ref();
    trial.join("artifacts").join(path.strip_prefix("/").unwrap_or(path))
}

/// The task a trial directory's `result.json` names, once the trial shows a whole run of the
/// reference solution.
fn reference_task(trial: &Path) -> anyhow::Result<String> {
    let path = trial.join("result.json");
    let text = std::fs::read_to_string(&path).with_context(|| {
        format!("{} is not a Harbor trial directory: cannot read its result.json", trial.display())
    })?;
    let result: Value = serde_json::from_str(&text)
        .with_context(|| format!("{} is not JSON: is the trial still running?", path.display()))?;
    let name = result["task_name"].as_str().with_context(|| {
        format!("{} names no task_name: is it a Harbor trial's?", path.display())
    })?;
    if let Some(problem) = not_reference(trial, &result)? {
        bail!(
            "{} is not a whole run of the reference solution: {problem}. Draft from the trials \
             of the oracle agent without SOLUTION, or pass the trial's evidence.db to draft from \
             it anyway",
            trial.display()
        );
    }
    Ok(name.to_owned())
}

/// Why a trial is not a whole run of the reference solution, if it is not: another agent ran, or
/// a probe (`SOLUTION`), or the solution failed (`agent/exit-code.txt`), or the trial failed
/// before its verifier did.
fn not_reference(trial: &Path, result: &Value) -> anyhow::Result<Option<String>> {
    let text = |pointer: &str| result.pointer(pointer).and_then(Value::as_str);
    match text("/config/agent/name").or_else(|| text("/config/agent/import_path")) {
        Some("oracle") => {}
        Some(agent) => return Ok(Some(format!("its agent is {agent}"))),
        None => return Ok(Some("it names no agent".to_owned())),
    }
    if let Some(n) = text("/config/agent/env/SOLUTION").filter(|n| !n.is_empty()) {
        return Ok(Some(format!("it runs probe {n} (SOLUTION={n})")));
    }
    let exit = exit_code(trial)?;
    if exit != "0" {
        return Ok(Some(format!("the solution exited with {exit} (agent/exit-code.txt)")));
    }
    let error = text("/exception_info/exception_type");
    let error = error.filter(|kind| !VERIFIER_ERRORS.contains(kind));
    Ok(error.map(|kind| format!("the trial failed with {kind}")))
}

/// What `agent/exit-code.txt` holds, which the oracle writes only when the solution fails; `0`
/// without it.
fn exit_code(trial: &Path) -> anyhow::Result<String> {
    let path = trial.join("agent/exit-code.txt");
    match std::fs::read_to_string(&path) {
        Ok(code) => Ok(code.trim().to_owned()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok("0".to_owned()),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

/// The answer in an answer file, read as the grader reads it: none unless it is a regular file,
/// and only its first `grade::ANSWER_BYTES` count.
fn read_answer(path: &Path) -> anyhow::Result<Answer> {
    let mut bytes = Vec::new();
    if std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file()) {
        std::fs::File::open(path)
            .and_then(|file| file.take(grade::ANSWER_BYTES).read_to_end(&mut bytes))
            .with_context(|| format!("cannot read the answer {}", path.display()))?;
    }
    Ok(grade::answer(&String::from_utf8_lossy(&bytes)))
}

/// Refuse a run given twice, as one path twice or as a trial and its evidence file: it would count
/// as two runs that agree.
fn distinct(runs: &[Run]) -> anyhow::Result<()> {
    for (n, run) in runs.iter().enumerate() {
        if let Some(earlier) = runs[..n].iter().find(|earlier| earlier.evidence == run.evidence) {
            bail!(
                "{} and {} are the same run: draft from different trials",
                earlier.input.display(),
                run.input.display()
            );
        }
    }
    Ok(())
}

/// The task the trial directories name, which must be one; `None` with `evidence.db` inputs only.
fn task(runs: &[Run]) -> anyhow::Result<Option<&str>> {
    let mut trials = runs.iter().filter_map(|run| Some((run, run.task.as_deref()?)));
    let Some((first, task)) = trials.next() else { return Ok(None) };
    if let Some((other, name)) = trials.find(|(_, name)| *name != task) {
        bail!(
            "{} is a trial of {task}, but {} is a trial of {name}: draft from the trials of one \
             task",
            first.input.display(),
            other.input.display()
        );
    }
    Ok(Some(task))
}

/// The run an evidence file records, with no task and no answer.
async fn recorded(path: &Path) -> anyhow::Result<Run> {
    record(path).await.with_context(|| format!("cannot read the evidence {}", path.display()))
}

async fn record(path: &Path) -> anyhow::Result<Run> {
    let evidence = std::fs::canonicalize(path)?;
    let scratch = Scratch::of(&evidence)?;
    let mut conn = evidence::open(&scratch.0).await?;
    let changes: Vec<ChangeRow> =
        sqlx::query_as("SELECT tbl, op, pk, before, after, columns FROM changes ORDER BY id")
            .fetch_all(&mut conn)
            .await?;
    let requests = sqlx::query_as("SELECT method, path FROM sent_requests ORDER BY seq")
        .fetch_all(&mut conn)
        .await?;
    let emails = sqlx::query_as("SELECT recipients, subject FROM sent_emails ORDER BY seq")
        .fetch_all(&mut conn)
        .await?;
    let faults =
        sqlx::query_as("SELECT kind, detail FROM faults ORDER BY seq").fetch_all(&mut conn).await?;
    conn.close().await?;
    Ok(Run {
        input: path.to_path_buf(),
        evidence,
        task: None,
        changes: changes.into_iter().map(change).collect::<Result<_, _>>()?,
        requests,
        emails,
        answer: grade::answer(""),
        faults,
    })
}

/// A row of `changes`: `tbl`, `op`, `pk`, `before`, `after` and `columns`.
type ChangeRow = (String, String, String, Option<String>, Option<String>, Option<String>);

/// A change as `evidence::write` stored it.
fn change((tbl, op, pk, before, after, columns): ChangeRow) -> serde_json::Result<NetChange> {
    Ok(NetChange {
        op: serde_json::from_value(Value::String(op))?,
        pk: exact(&pk)?,
        before: before.as_deref().map(exact).transpose()?,
        after: after.as_deref().map(exact).transpose()?,
        columns: columns.as_deref().map(serde_json::from_str).transpose()?,
        tbl,
    })
}

/// A JSON object of the evidence, with each nested object or array as its text: the grader's
/// `json_extract` reads it as text, so a check compares text, and runs differ where it does.
fn exact(text: &str) -> serde_json::Result<Map<String, Value>> {
    let mut object: Map<String, Value> = serde_json::from_str(text)?;
    for value in object.values_mut() {
        if value.is_array() || value.is_object() {
            *value = Value::String(value.to_string());
        }
    }
    Ok(object)
}

/// A private copy of an evidence file, removed when dropped: `evidence::open` opens its file
/// writable, and drafting leaves its inputs as they are.
struct Scratch(PathBuf);

impl Scratch {
    fn of(evidence: &Path) -> anyhow::Result<Scratch> {
        static COPIES: AtomicU64 = AtomicU64::new(0);
        let mut source = std::fs::File::open(evidence)?;
        let copy = COPIES.fetch_add(1, Ordering::Relaxed);
        let name = format!("miroir-draft-{}-{copy}.db", std::process::id());
        let path = std::env::temp_dir().join(name);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| format!("cannot create a copy at {}", path.display()))?;
        let scratch = Scratch(path);
        std::io::copy(&mut source, &mut file)
            .with_context(|| format!("cannot copy it to {}", scratch.0.display()))?;
        Ok(scratch)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.0) {
            tracing::warn!("cannot remove the evidence copy {}: {e}", self.0.display());
        }
    }
}

/// The op an `allow` entry admits a change by; `None` for truncate and schema changes, which no
/// entry admits.
fn row_op(op: ChangeOp) -> Option<Op> {
    match op {
        ChangeOp::Insert => Some(Op::Insert),
        ChangeOp::Update => Some(Op::Update),
        ChangeOp::Delete => Some(Op::Delete),
        ChangeOp::Truncate | ChangeOp::Schema => None,
    }
}

/// The column and value of the key a product gave an inserted row: its primary key, when that is
/// one column. A composite key is made of the row's references, which the run chose.
fn assigned(change: &NetChange) -> Option<(&String, &Value)> {
    if change.op != ChangeOp::Insert || change.pk.len() != 1 {
        return None;
    }
    change.pk.iter().next()
}

/// Something a run did that a check can hold: a row change, a request, an email or the answer.
struct Item<'a> {
    /// What matches the same item in another run, once counted within its run (`matched`).
    key: String,
    /// How comments name it.
    label: String,
    /// The table the comments name its columns after.
    table: &'a str,
    /// The values a check may compare: those an update set, an inserted row, a keyless row as it
    /// was deleted, an email's recipients and subject, or the answer's text. A keyed delete has
    /// none: its key tells it.
    row: Map<String, Value>,
    /// The columns an update changed.
    changed: &'a [String],
    /// The columns whose values `refer` took out of `row`, with the tables of the rows they are
    /// the keys of.
    refers: Vec<(String, BTreeSet<&'a str>)>,
    kind: Kind<'a>,
}

#[derive(Clone, Copy)]
enum Kind<'a> {
    Change(&'a NetChange, Op),
    /// The method and path of a request.
    Request(&'a str, &'a str),
    Email,
    Answer(&'a Answer),
}

impl Run {
    /// What the run did that a check can hold, in evidence order: row changes, then requests,
    /// emails and the answer.
    fn items(&self) -> Vec<Item<'_>> {
        let mut items: Vec<Item> = self.changes.iter().filter_map(change_item).collect();
        items.extend(self.requests.iter().map(|(method, path)| Item {
            key: json!(["request", method, path]).to_string(),
            label: format!("request {method} {path}"),
            table: "sent_requests",
            row: Map::new(),
            changed: &[],
            refers: Vec::new(),
            kind: Kind::Request(method, path),
        }));
        items.extend(self.emails.iter().map(|(recipients, subject)| Item {
            key: "email".into(),
            label: format!("email to {recipients}"),
            table: "sent_emails",
            row: row([
                ("recipients", recipients.as_str().into()),
                ("subject", subject.as_deref().into()),
            ]),
            changed: &[],
            refers: Vec::new(),
            kind: Kind::Email,
        }));
        items.extend(self.answer.value.as_ref().map(|value| Item {
            key: "answer".into(),
            label: "answer".into(),
            table: "answer",
            row: row([("value", value.as_str().into())]),
            changed: &[],
            refers: Vec::new(),
            kind: Kind::Answer(&self.answer),
        }));
        let inserted = self.inserted();
        for item in &mut items {
            item.refer(&inserted);
        }
        items
    }

    /// The tables of the rows the run inserted, by the integer key the product gave each: another
    /// run may give the same row another key.
    fn inserted(&self) -> BTreeMap<i64, BTreeSet<&str>> {
        let mut inserted: BTreeMap<i64, BTreeSet<&str>> = BTreeMap::new();
        for change in &self.changes {
            if let Some(key) = assigned(change).and_then(|(_, key)| key.as_i64()) {
                inserted.entry(key).or_default().insert(&change.tbl);
            }
        }
        inserted
    }
}

/// A row change as an item; `None` for a truncate or a schema change. An update or a delete is
/// keyed by its table and primary key; an insert only by its table, so that it matches by order.
fn change_item(change: &NetChange) -> Option<Item<'_>> {
    let op = row_op(change.op)?;
    let changed = change.columns.as_deref().unwrap_or_default();
    let after = change.after.clone().unwrap_or_default();
    let row = match op {
        Op::Insert => after,
        Op::Update => only(&after, |column, _| changed.contains(column)),
        Op::Delete if change.pk.is_empty() => change.before.clone().unwrap_or_default(),
        Op::Delete => Map::new(),
    };
    let key = if op == Op::Insert { Value::Null } else { Value::Object(change.pk.clone()) };
    Some(Item {
        key: json!(["change", change.tbl, op.as_str(), key]).to_string(),
        label: label(change),
        table: &change.tbl,
        row,
        changed,
        refers: Vec::new(),
        kind: Kind::Change(change, op),
    })
}

impl<'a> Item<'a> {
    /// Take out of `row` each value that is the key of a row the run inserted (`inserted`), into
    /// `refers`: that of a column named as a reference to that row's table (`label_id` of a new
    /// `labels` row, see `names`), unless it is the inserted row's own key, and the answer when
    /// any of its ids is. Other integers, such as flags, counts and ids of other tables' rows,
    /// stay, since a table that starts empty gives its first rows small keys that they equal.
    fn refer(&mut self, inserted: &BTreeMap<i64, BTreeSet<&'a str>>) {
        let refers = |column: &String| column.ends_with("_id") || column.ends_with("Id");
        let columns: Vec<String> = match self.kind {
            Kind::Change(change, _) => {
                let own = assigned(change).map(|(column, _)| column);
                let named = |column: &&String| refers(column) && Some(*column) != own;
                self.row.keys().filter(named).cloned().collect()
            }
            Kind::Answer(_) => self.row.keys().cloned().collect(),
            Kind::Request(..) | Kind::Email => return,
        };
        for column in columns {
            let (keys, by_name): (Vec<i64>, bool) = match self.kind {
                Kind::Answer(answer) => {
                    let ids = answer.ids.iter().flat_map(|ids| ids.split(','));
                    (ids.filter_map(|id| id.parse().ok()).collect(), false)
                }
                Kind::Change(..) | Kind::Request(..) | Kind::Email => {
                    (self.row.get(&column).and_then(Value::as_i64).into_iter().collect(), true)
                }
            };
            let tables: BTreeSet<&'a str> = keys
                .iter()
                .filter_map(|key| inserted.get(key))
                .flatten()
                .copied()
                .filter(|table| !by_name || names(&column, table))
                .collect();
            if !tables.is_empty() {
                self.row.remove(&column);
                self.refers.push((column, tables));
            }
        }
    }
}

/// Whether a column named as a reference names `table`: the column's name without `_id` or `Id`,
/// or its end after an underscore, is the table's name (after its schema) in the singular or the
/// plural. `label_id` and `labelId` name `labels`, and `ref_issue_id` names `issue`; `repo_id`
/// names no `collaboration` table, even when their ids are equal.
fn names(column: &str, table: &str) -> bool {
    let stem = column.strip_suffix("_id").or_else(|| column.strip_suffix("Id")).unwrap_or(column);
    let mut words = String::new();
    for c in stem.chars() {
        if c.is_ascii_uppercase() && !words.is_empty() {
            words.push('_');
        }
        words.push(c.to_ascii_lowercase());
    }
    let table = table.rsplit_once('.').map_or(table, |(_, name)| name).to_ascii_lowercase();
    let mut ends = std::iter::once(words.as_str())
        .chain(words.match_indices('_').map(|(at, _)| &words[at + 1..]));
    ends.any(|end| {
        let plural = end.strip_suffix('y').map(|base| format!("{base}ies"));
        [end.to_owned(), format!("{end}s"), format!("{end}es")].contains(&table)
            || plural.as_ref() == Some(&table)
    })
}

/// The items of every run grouped by key, in the order keys first appear; a group holds the item
/// of each run that has its key. A key is first counted within its run, so that the n-th insert
/// into a table, the n-th email and the n-th request to a path match the n-th of another run.
fn matched<'r, 'a>(runs: &'r [Vec<Item<'a>>]) -> Vec<Vec<&'r Item<'a>>> {
    let mut groups: Vec<Vec<&Item>> = Vec::new();
    let mut found: BTreeMap<(&str, usize), usize> = BTreeMap::new();
    for items in runs {
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for item in items {
            let count = counts.entry(&item.key).or_default();
            let group = *found.entry((&item.key, *count)).or_insert_with(|| {
                groups.push(Vec::new());
                groups.len() - 1
            });
            *count += 1;
            groups[group].push(item);
        }
    }
    groups
}

/// The values every item of a group holds alike, and the columns whose values differ.
fn split(group: &[&Item]) -> (Map<String, Value>, Vec<String>) {
    let columns: BTreeSet<&String> = group.iter().flat_map(|item| item.row.keys()).collect();
    let mut stable = Map::new();
    let mut varying = Vec::new();
    for column in columns {
        let alike = |value: &&Value| group.iter().all(|item| item.row.get(column) == Some(*value));
        match group[0].row.get(column).filter(alike) {
            Some(value) => {
                stable.insert(column.clone(), value.clone());
            }
            None => varying.push(column.clone()),
        }
    }
    (stable, varying)
}

/// A draft spec: what the runs did, and notes on what it leaves to the reviewer.
#[derive(Default)]
struct Draft {
    /// The name and SQL of each check.
    checks: Vec<(String, String)>,
    /// The names in `checks`.
    names: BTreeSet<String>,
    /// The SQL in `checks`.
    drafted: BTreeSet<String>,
    /// The last suffix each base name was tried with, so that naming resumes after it.
    suffixes: BTreeMap<String, usize>,
    /// The paste-ready `allow` lines, in evidence order.
    allow: Vec<String>,
    /// What only some runs did, and what is checked or allowed whatever its values.
    notes: Vec<String>,
    /// `table.column` for each value that differs between runs.
    varying: BTreeSet<String>,
    /// `table.column` for each value that is the key of a row a run inserted, with its tables.
    refers: BTreeMap<String, BTreeSet<String>>,
    /// `table.column` for each text too long to check.
    long: BTreeSet<String>,
    /// `table.column` for each date or time the draft compares as a constant.
    times: BTreeSet<String>,
}

impl Draft {
    fn of(runs: &[Run]) -> Draft {
        let items: Vec<Vec<Item>> = runs.iter().map(Run::items).collect();
        let mut draft = Draft::default();
        for group in matched(&items) {
            let first = group[0];
            let every = group.len() == runs.len();
            if !every {
                let (seen, of) = (group.len(), runs.len());
                draft.note(format!("seen in {seen} of {of} runs: {}", first.label));
            }
            let stable = draft.shared(&group);
            draft.add(first.kind, &group, &stable, every);
        }
        draft
    }

    /// The values every item of `group` holds alike that are short enough to check; the notes
    /// name the others.
    fn shared(&mut self, group: &[&Item]) -> Map<String, Value> {
        let table = group[0].table;
        for (column, tables) in group.iter().flat_map(|item| &item.refers) {
            let named = self.refers.entry(format!("{table}.{column}")).or_default();
            named.extend(tables.iter().map(|table| (*table).to_owned()));
        }
        let (stable, varying) = split(group);
        self.varying.extend(varying.iter().map(|column| format!("{table}.{column}")));
        let (long, short): (Map<String, Value>, Map<String, Value>) = stable
            .into_iter()
            .partition(|(_, value)| value.as_str().is_some_and(|text| text.len() > CHECKED_BYTES));
        self.long.extend(long.keys().map(|column| format!("{table}.{column}")));
        short
    }

    /// Draft what the runs did, given the values they share: a row change gets its `allow` entry,
    /// and anything every run did gets its check.
    fn add(&mut self, kind: Kind, group: &[&Item], stable: &Map<String, Value>, every: bool) {
        match kind {
            Kind::Change(change, Op::Update) => self.update(change, group, stable, every),
            Kind::Change(change, Op::Delete) if !change.pk.is_empty() => self.delete(change, every),
            Kind::Change(change, op) => self.by_values(change, op, stable, every),
            _ if !every => {}
            Kind::Request(method, path) => self.request(method, path),
            Kind::Email => self.email(stable),
            Kind::Answer(answer) => self.answer(answer, stable),
        }
    }

    /// An update: allowed on its row for every column a run changed, and checked on the values
    /// every run set alike. When there are none there is no check, and a note says so.
    fn update(
        &mut self,
        change: &NetChange,
        group: &[&Item],
        set: &Map<String, Value>,
        every: bool,
    ) {
        let changed: BTreeSet<&String> = group.iter().flat_map(|item| item.changed).collect();
        let columns: Vec<String> = changed.into_iter().cloned().collect();
        self.timed(&change.tbl, &change.pk);
        self.allow(allow_line(&change.tbl, Op::Update, Some(&columns), &change.pk));
        if !every {
            return;
        }
        if set.is_empty() {
            let why = "every value it sets varies across runs, is a new row's key or is too long";
            self.note(format!("no check for {}: {why}", label(change)));
            return;
        }
        self.timed(&change.tbl, set);
        let from = rows("after", &change.tbl);
        let sql = select("count(*)", &from, &[key_condition(&change.pk), key_condition(set)]);
        self.check(&format!("{} {}", change.tbl, key_words(&change.pk)), sql);
    }

    /// A delete of a keyed row: allowed and checked by its key.
    fn delete(&mut self, change: &NetChange, every: bool) {
        self.timed(&change.tbl, &change.pk);
        self.allow(allow_line(&change.tbl, Op::Delete, None, &change.pk));
        if every {
            let deleted = Some("_op = 'delete'".to_owned());
            let from = rows("before", &change.tbl);
            let sql = select("count(*)", &from, &[deleted, key_condition(&change.pk)]);
            self.check(&format!("{} {} deleted", change.tbl, key_words(&change.pk)), sql);
        }
    }

    /// An insert, or a delete from a keyless table, which only its values tell apart: allowed
    /// where its integer and boolean values hold, so that differently worded text is no harm, and
    /// checked on every value. An inserted row's own key is in neither.
    fn by_values(&mut self, change: &NetChange, op: Op, stable: &Map<String, Value>, every: bool) {
        let own = assigned(change).map(|(column, _)| column);
        let values = only(stable, |column, _| Some(column) != own);
        let narrow = only(&values, |_, value| narrows(value));
        let what = format!("{} {}", change.tbl, op.as_str());
        if narrow.is_empty() {
            let why = "no integer or boolean value every run shares narrows it";
            self.note(format!("the allow entry for {what} admits any row: {why}"));
        }
        self.allow(allow_line(&change.tbl, op, None, &narrow));
        if !every {
            return;
        }
        if values.is_empty() {
            let why = "no value every run shares narrows it";
            self.note(format!("the check for {what} passes for any row: {why}"));
        }
        self.timed(&change.tbl, &values);
        let (image, done) =
            if op == Op::Delete { ("before", "deleted") } else { ("after", "inserted") };
        let filter = Some(format!("_op = '{}'", op.as_str()));
        let sql =
            select("count(*) >= 1", &rows(image, &change.tbl), &[filter, key_condition(&values)]);
        self.check(&format!("{} {done}", change.tbl), sql);
    }

    /// A request every run sent: checked by method and path.
    fn request(&mut self, method: &str, path: &str) {
        let sent = row([("method", method.into()), ("path", path.into())]);
        let sql = select("count(*) >= 1", "sent_requests", &[key_condition(&sent)]);
        self.check(&format!("{method} {path}"), sql);
    }

    /// An email every run sent: checked on the recipients and subject every run gave it alike.
    fn email(&mut self, stable: &Map<String, Value>) {
        if stable.is_empty() {
            let why = "no recipients or subject every run shares narrows it";
            self.note(format!("the check for an email passes for any email: {why}"));
        }
        self.timed("sent_emails", stable);
        let sql = select("count(*) >= 1", "sent_emails", &[key_condition(stable)]);
        self.check("email", sql);
    }

    /// The answer, when every run gave the same: as a number, else as ids when normalizing them
    /// changes the text, else as text.
    fn answer(&mut self, answer: &Answer, stable: &Map<String, Value>) {
        let Some(Value::String(value)) = stable.get("value") else { return };
        self.timed("answer", stable);
        let condition = if let Some(number) = answer.number {
            format!("number = {}", sql_literal(&number.into()))
        } else if let Some(ids) = answer.ids.as_deref().filter(|ids| ids != value) {
            format!("ids = {}", sql_literal(&ids.into()))
        } else {
            format!("value = {}", sql_literal(&value.as_str().into()))
        };
        self.check("answer", format!("SELECT {condition} FROM answer"));
    }

    /// Add a check named after `words`, unless a check with the same SQL is drafted.
    fn check(&mut self, words: &str, sql: String) {
        if !self.drafted.insert(sql.clone()) {
            return;
        }
        let base = name(words);
        let mut name = base.clone();
        let n = self.suffixes.entry(base.clone()).or_insert(1);
        // `null`, `true` and `false` would read back from the YAML as no name at all.
        while ["null", "true", "false"].contains(&name.as_str()) || self.names.contains(&name) {
            *n += 1;
            name = format!("{base}_{n}");
        }
        self.names.insert(name.clone());
        self.checks.push((name, sql));
    }

    /// Add an `allow` line, unless the same line is drafted.
    fn allow(&mut self, line: String) {
        if !self.allow.contains(&line) {
            self.allow.push(line);
        }
    }

    /// Add a note, unless the same note is there.
    fn note(&mut self, note: String) {
        if !self.notes.contains(&note) {
            self.notes.push(note);
        }
    }

    /// Name the dates and times among `values`, which the draft compares as constants, though
    /// every boot moves the world's dates to its own day (§3.5 step 5).
    fn timed(&mut self, table: &str, values: &Map<String, Value>) {
        let times = values.iter().filter(|(_, value)| timelike(value));
        self.times.extend(times.map(|(column, _)| format!("{table}.{column}")));
    }

    /// The draft: its header, the spec, and the notes for the reviewer.
    fn text(&self, runs: &[Run], task: Option<&str>) -> String {
        let mut lines = vec!["checks:".to_owned()];
        for (name, sql) in &self.checks {
            lines.push(format!("  - name: {name}\n    sql: {}", scalar(sql)));
        }
        lines.push(if self.allow.is_empty() { "allow: []" } else { "allow:" }.to_owned());
        lines.extend(self.allow.iter().map(|line| format!("  {line}")));
        format!("{}{}\n{}", header(runs, task), lines.join("\n"), self.footer(runs))
    }

    /// Why there is no draft, and the notes that explain it.
    fn nothing(&self, runs: &[Run]) -> String {
        let reason = if runs.iter().all(|run| run.items().is_empty()) {
            "no run changed a row, sent an email or a request, or answered"
        } else {
            "no check holds in every run"
        };
        comment(&format!("nothing to draft: {reason}")) + &self.footer(runs)
    }

    /// The comments after the spec: the notes, the values left out of checks or compared as
    /// constants, and the runs that are harm.
    fn footer(&self, runs: &[Run]) -> String {
        let mut notes = self.notes.clone();
        notes.extend(listed("varies across runs", &self.varying));
        for (column, tables) in &self.refers {
            let tables: Vec<&str> = tables.iter().map(String::as_str).collect();
            notes.push(format!(
                "{column} is the key of a new {} row, which the product assigns: match that row \
                 by its content (authoring rule 4)",
                tables.join(" or ")
            ));
        }
        notes.extend(listed(&format!("not checked, over {CHECKED_BYTES} bytes"), &self.long));
        let times = "dates and times compared as constants, which may not hold on another day";
        notes.extend(listed(&format!("{times} (authoring rule 5)"), &self.times));
        notes.extend(runs.iter().filter_map(harm));
        notes.iter().map(|note| comment(note)).collect()
    }
}

/// The comments that open a draft: what it was drafted from, and how to review it.
fn header(runs: &[Run], task: Option<&str>) -> String {
    let of = task.map(|task| format!(" for {task}")).unwrap_or_default();
    let count = if runs.len() == 1 { "1 run".to_owned() } else { format!("{} runs", runs.len()) };
    let mut lines = vec![format!("Draft MIROIR_SPEC{of} from {count}:")];
    lines.extend(runs.iter().map(|run| format!("  {}", run.input.display())));
    lines.push("A draft checks what the reference did, not what the task asks:".into());
    lines.push("review every line, move invariants to gates, and add probes.".into());
    if runs.len() == 1 {
        let advice = [
            "From one run every value looks stable: run the oracle with -k 2 -n 1 (one attempt",
            "after the other), then draft from both trials to find the values that vary.",
        ];
        lines.extend(advice.map(str::to_owned));
    }
    lines.iter().map(|line| comment(line)).collect()
}

/// Why every grade of a run is harm whatever its spec says, if it is: the truncate and schema
/// changes it made, which no `allow` entry admits, and its faults.
fn harm(run: &Run) -> Option<String> {
    let changes = run.changes.iter().filter(|change| row_op(change.op).is_none());
    let mut harm: Vec<String> = changes.map(label).collect();
    harm.extend(run.faults.iter().map(|(kind, detail)| format!("fault {kind}: {detail}")));
    let harm = harm.join("; ");
    (!harm.is_empty())
        .then(|| format!("{} is harm whatever the spec says: {harm}", run.input.display()))
}

/// A change as comments name it, such as `invoices update {"id":4001}`; a keyless row has no key.
fn label(change: &NetChange) -> String {
    let (tbl, op) = (&change.tbl, change.op.as_str());
    if change.pk.is_empty() {
        format!("{tbl} {op}")
    } else {
        format!("{tbl} {op} {}", Value::Object(change.pk.clone()))
    }
}

/// `about: a, b` for the names in `names`; `None` when there are none.
fn listed(about: &str, names: &BTreeSet<String>) -> Option<String> {
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    (!names.is_empty()).then(|| format!("{about}: {}", names.join(", ")))
}

/// Whether an `allow` entry may hold a value of an insert, or of a delete from a keyless table:
/// an integer or a boolean, which a run does not word its own way, unless it may be a time, which
/// moves with the day.
fn narrows(value: &Value) -> bool {
    (value.is_i64() || value.is_u64() || value.is_boolean()) && !timelike(value)
}

/// Whether `value` looks like a date, a timestamp, or a Unix time from 2000 to 2100 in seconds
/// or milliseconds, as the evidence holds them (§4.4): each boot moves those to its own day.
fn timelike(value: &Value) -> bool {
    match value {
        Value::String(text) => dated(text),
        Value::Number(number) => number
            .as_i64()
            .is_some_and(|n| UNIX_SECONDS.contains(&n) || UNIX_SECONDS.contains(&(n / 1000))),
        Value::Null | Value::Bool(_) | Value::Array(_) | Value::Object(_) => false,
    }
}

/// Whether `text` is a date, `YYYY-MM-DD`, or a timestamp that starts with one and a `T`.
fn dated(text: &str) -> bool {
    let bytes = text.as_bytes();
    let shape = (0..10).all(|at| {
        let byte = bytes.get(at).copied().unwrap_or_default();
        if at == 4 || at == 7 { byte == b'-' } else { byte.is_ascii_digit() }
    });
    shape && bytes.get(10).is_none_or(|byte| *byte == b'T')
}

/// A key's values, as words of a check name.
fn key_words(key: &Map<String, Value>) -> String {
    let words: Vec<String> = key
        .values()
        .map(|value| value.as_str().map_or_else(|| value.to_string(), str::to_owned))
        .collect();
    words.join(" ")
}

/// A check name, `[a-z][a-z0-9_]*`, from `words`: lower-cased, every run of other characters
/// made one `_`, and `t_` put in front of a name that would not start with a letter.
fn name(words: &str) -> String {
    let mut name = String::new();
    for c in words.chars() {
        if c.is_ascii_alphanumeric() {
            name.push(c.to_ascii_lowercase());
        } else if !name.is_empty() && !name.ends_with('_') {
            name.push('_');
        }
    }
    let name = name.trim_end_matches('_');
    if name.starts_with(|c: char| c.is_ascii_lowercase()) {
        name.to_owned()
    } else {
        format!("t_{name}").trim_end_matches('_').to_owned()
    }
}

/// The `before` or `after` table of `tbl`, as the grader builds it.
fn rows(image: &str, tbl: &str) -> String {
    format!("{image}.\"{}\"", tbl.replace('"', "\"\""))
}

/// `SELECT <value> FROM <from>`, filtered by every condition that is set.
fn select(value: &str, from: &str, conditions: &[Option<String>]) -> String {
    let conditions: Vec<&str> = conditions.iter().flatten().map(String::as_str).collect();
    let filter = if conditions.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", conditions.join(" AND "))
    };
    format!("SELECT {value} FROM {from}{filter}")
}

/// The entries of `map` that `keep` keeps.
fn only(map: &Map<String, Value>, keep: impl Fn(&String, &Value) -> bool) -> Map<String, Value> {
    let kept = map.iter().filter(|(column, value)| keep(column, value));
    kept.map(|(column, value)| (column.clone(), value.clone())).collect()
}

/// A row of named values.
fn row<const N: usize>(values: [(&str, Value); N]) -> Map<String, Value> {
    values.into_iter().map(|(column, value)| (column.to_owned(), value)).collect()
}

/// Whether YAML holds `c` as it is in a comment or a literal block: serde-saphyr refuses the
/// control characters other than tab, and U+FFFE and U+FFFF.
fn plain(c: char) -> bool {
    c == '\t' || !(c.is_control() || c == '\u{fffe}' || c == '\u{ffff}')
}

/// A YAML comment line holding `text`, with the characters a comment cannot hold escaped, and no
/// three `'` in a row, which the TOML literal string a spec is pasted into cannot hold.
fn comment(text: &str) -> String {
    let escaped = |c: char| if plain(c) { c.to_string() } else { c.escape_default().to_string() };
    let text: String = text.chars().map(escaped).collect();
    format!("# {}\n", text.replace("''", "'\\'"))
}

/// `sql` as the value of a `sql:` key: a literal block, or a double-quoted scalar with escapes
/// when it holds a character a block cannot. `sql` starts with `SELECT`.
fn scalar(sql: &str) -> String {
    if !sql.ends_with('\n') && sql.chars().all(|c| c == '\n' || plain(c)) {
        let lines: Vec<String> = sql.split('\n').map(|line| format!("      {line}")).collect();
        return format!("|-\n{}", lines.join("\n"));
    }
    let escaped =
        |c: char| if plain(c) { c.to_string() } else { format!("\\u{:04x}", u32::from(c)) };
    Value::from(sql).to_string().chars().map(escaped).collect()
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use proptest::{collection, option};

    use super::*;
    use crate::evidence::{CapturedTable, Evidence, Fault};
    use crate::grade::{Inputs, Outcome};

    /// Where Harbor 0.26 puts a trial's evidence and answer (B1).
    const EVIDENCE: &str = "artifacts/miroir/evidence/evidence.db";
    const ANSWER: &str = "artifacts/logs/artifacts/answer.txt";
    const PAY: &str = "acme/pay-invoice";

    /// A directory for one test, removed with what it holds when dropped.
    struct TestDir(PathBuf);

    impl std::ops::Deref for TestDir {
        type Target = Path;

        fn deref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A new, empty directory for one test.
    fn new_dir() -> TestDir {
        static DIRS: AtomicU64 = AtomicU64::new(0);
        let n = DIRS.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("miroir-draft-test-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TestDir(dir)
    }

    fn object(value: Value) -> Map<String, Value> {
        let Value::Object(map) = value else { panic!("not a JSON object: {value}") };
        map
    }

    /// A test row's primary key: its `id`, or none for a keyless row.
    fn key(row: &Map<String, Value>) -> Map<String, Value> {
        row.get("id").map(|id| object(json!({"id": id}))).unwrap_or_default()
    }

    fn change(tbl: &str, op: ChangeOp, before: Option<Value>, after: Option<Value>) -> NetChange {
        let (before, after) = (before.map(object), after.map(object));
        let pk = key(after.as_ref().or(before.as_ref()).unwrap());
        NetChange { tbl: tbl.into(), op, pk, before, after, columns: None }
    }

    fn insert(tbl: &str, row: Value) -> NetChange {
        change(tbl, ChangeOp::Insert, None, Some(row))
    }

    fn delete(tbl: &str, row: Value) -> NetChange {
        change(tbl, ChangeOp::Delete, Some(row), None)
    }

    /// An update of `row` that sets the columns of `set`; its columns are those that differ.
    fn update(tbl: &str, row: Value, set: Value) -> NetChange {
        let mut after = row.clone();
        object(set).into_iter().for_each(|(column, value)| after[column] = value);
        let mut update = change(tbl, ChangeOp::Update, Some(row), Some(after));
        let (before, after) = (update.before.as_ref().unwrap(), update.after.as_ref().unwrap());
        let changed = after.iter().filter(|(column, value)| before.get(*column) != Some(value));
        update.columns = Some(changed.map(|(column, _)| column.clone()).collect());
        update
    }

    /// Evidence of `changes` and `effects` that captures every table they touch.
    fn evidence(changes: Vec<NetChange>, effects: Vec<Value>) -> Evidence {
        let mut tables: BTreeMap<String, CapturedTable> = BTreeMap::new();
        for change in &changes {
            let table = tables.entry(change.tbl.clone()).or_insert_with(|| CapturedTable {
                name: change.tbl.clone(),
                columns: Vec::new(),
                pk: change.pk.keys().cloned().collect(),
            });
            for column in change.before.iter().chain(&change.after).flat_map(Map::keys) {
                if !table.columns.contains(column) {
                    table.columns.push(column.clone());
                }
            }
        }
        let tables = tables.into_values().collect();
        Evidence { miroir: "0.1.0".into(), tables, changes, effects, faults: Vec::new() }
    }

    /// `run` with `change` made last.
    fn plus(run: Evidence, change: NetChange) -> Evidence {
        let mut changes = run.changes;
        changes.push(change);
        evidence(changes, run.effects)
    }

    /// A truncate or a schema change of `tbl`.
    fn reshaped(tbl: &str, op: ChangeOp) -> NetChange {
        NetChange { tbl: tbl.into(), op, pk: Map::new(), before: None, after: None, columns: None }
    }

    fn request(method: &str, path: &str) -> Value {
        json!({"kind": "http", "data": {"method": method, "path": path, "query": {},
               "headers": {"content-type": "application/json"}, "body": {"amount": 125_000}}})
    }

    fn email(to: &str, subject: Option<&str>) -> Value {
        json!({"kind": "email", "data": {"from": "crm@acme.test", "to": [to], "cc": [], "bcc": [],
               "subject": subject, "text": "Hello", "html": null}})
    }

    /// A Harbor trial directory of the oracle's run of `task` whose evidence is `evidence`, with
    /// an answer file holding `answer` when it is given.
    async fn trial(dir: &Path, task: &str, evidence: &Evidence, answer: Option<&str>) -> PathBuf {
        let trial = dir.join(format!("trial-{}", std::fs::read_dir(dir).unwrap().count()));
        let path = trial.join(EVIDENCE);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        evidence::write(&path, evidence).await.unwrap();
        let name = trial.file_name().unwrap().to_str().unwrap();
        let result = json!({"trial_name": name, "task_name": task, "reward_valid": true,
                            "config": {"agent": {"name": "oracle", "model_name": null, "env": {}}},
                            "exception_info": null});
        std::fs::write(trial.join("result.json"), result.to_string()).unwrap();
        if let Some(answer) = answer {
            std::fs::create_dir_all(trial.join(ANSWER).parent().unwrap()).unwrap();
            std::fs::write(trial.join(ANSWER), answer).unwrap();
        }
        trial
    }

    /// A change to a trial's `result.json`.
    type Edit = fn(&mut Value);

    /// Change a trial's `result.json` with `edit`.
    fn rewrite(trial: &Path, edit: impl FnOnce(&mut Value)) {
        let path = trial.join("result.json");
        let mut result: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        edit(&mut result);
        std::fs::write(path, result.to_string()).unwrap();
    }

    /// An `evidence.db` file holding `evidence`.
    async fn file(dir: &Path, evidence: &Evidence) -> PathBuf {
        let path = dir.join(format!("evidence-{}.db", std::fs::read_dir(dir).unwrap().count()));
        evidence::write(&path, evidence).await.unwrap();
        path
    }

    /// What `run` returned for `inputs`, and what it printed.
    fn drafted(inputs: &[&Path]) -> (anyhow::Result<bool>, String) {
        let args = DraftArgs { inputs: inputs.iter().map(|input| input.to_path_buf()).collect() };
        let mut out = Vec::new();
        let result = run(&args, &mut out);
        (result, String::from_utf8(out).unwrap())
    }

    /// The error `run` returned for `inputs`, which printed nothing.
    fn refused(inputs: &[&Path]) -> String {
        let (result, printed) = drafted(inputs);
        assert_eq!(printed, "");
        format!("{:#}", result.unwrap_err())
    }

    /// Grade an input's own evidence and answer with `spec`, as Harbor's verifier would.
    async fn graded(spec: &str, input: &Path) -> Outcome {
        let (evidence, answer) = if input.is_dir() {
            (input.join(EVIDENCE), input.join(ANSWER))
        } else {
            (input.to_path_buf(), input.with_extension("answer.txt"))
        };
        let verifier = new_dir();
        let out_dir = verifier.to_path_buf();
        let inputs = Inputs { spec: Some(spec.into()), evidence, answer, out_dir };
        grade::grade(&inputs, &mut Vec::new()).await
    }

    fn scored(reward: f64, harm: u8) -> Outcome {
        Outcome::Scored { reward, harm }
    }

    /// The draft of `inputs`, once the contract (B1) holds for it: it parses as a spec, and
    /// grading each input's own evidence with it scores reward 1 and harm 0.
    async fn contract(inputs: &[&Path]) -> String {
        let (result, text) = drafted(inputs);
        assert!(result.unwrap(), "{text}");
        spec::parse(&text).unwrap_or_else(|e| panic!("{e}\n{text}"));
        for input in inputs {
            assert_eq!(graded(&text, input).await, scored(1.0, 0), "{}\n{text}", input.display());
        }
        text
    }

    /// The lines of `text` that are not comments.
    fn spec_lines(text: &str) -> Vec<&str> {
        text.lines().filter(|line| !line.starts_with('#')).collect()
    }

    fn invoice(id: i64) -> Value {
        json!({"id": id, "number": format!("INV-0{id}"), "status": "open", "paid_at": null,
               "payment_intent": null})
    }

    /// Invoice 4001 paid through the CRM at `at`, and the charge the CRM sent to Stripe.
    fn paid(at: &str) -> Evidence {
        let set = json!({"status": "paid", "paid_at": at, "payment_intent": "pi_test"});
        let charge = request("POST", "/stripe/v1/payment_intents");
        evidence(vec![update("invoices", invoice(4001), set)], vec![charge])
    }

    #[tokio::test]
    async fn two_runs_draft_what_both_did_and_name_what_varies() {
        let dir = new_dir();
        let a = trial(&dir, PAY, &paid("2026-11-17T10:00:00+00:00"), None).await;
        let b = trial(&dir, PAY, &paid("2026-11-17T10:00:03+00:00"), None).await;
        let text = contract(&[&a, &b]).await;
        let expected = [
            "# Draft MIROIR_SPEC for acme/pay-invoice from 2 runs:".to_owned(),
            format!("#   {}", a.display()),
            format!("#   {}", b.display()),
            "# A draft checks what the reference did, not what the task asks:".into(),
            "# review every line, move invariants to gates, and add probes.".into(),
            "checks:".into(),
            "  - name: invoices_4001".into(),
            "    sql: |-".into(),
            "      SELECT count(*) FROM after.\"invoices\" \
             WHERE id = 4001 AND payment_intent = 'pi_test' AND status = 'paid'"
                .into(),
            "  - name: post_stripe_v1_payment_intents".into(),
            "    sql: |-".into(),
            "      SELECT count(*) >= 1 FROM sent_requests \
             WHERE method = 'POST' AND path = '/stripe/v1/payment_intents'"
                .into(),
            "allow:".into(),
            "  - {table: invoices, op: update, columns: [paid_at, payment_intent, status], \
             where: \"id = 4001\"}"
                .into(),
            "# varies across runs: invoices.paid_at".into(),
        ];
        assert_eq!(text, expected.join("\n") + "\n");
    }

    #[tokio::test]
    async fn one_run_checks_every_value_and_asks_for_a_second() {
        let dir = new_dir();
        let a = trial(&dir, PAY, &paid("2026-11-17T10:00:00+00:00"), None).await;
        let text = contract(&[&a]).await;
        let header = format!(
            "# Draft MIROIR_SPEC for acme/pay-invoice from 1 run:\n#   {}\n\
             # A draft checks what the reference did, not what the task asks:\n\
             # review every line, move invariants to gates, and add probes.\n\
             # From one run every value looks stable: run the oracle with -k 2 -n 1 (one attempt\n\
             # after the other), then draft from both trials to find the values that vary.\n\
             checks:\n",
            a.display()
        );
        assert!(text.starts_with(&header), "{text}");
        let checked = "WHERE id = 4001 AND paid_at = '2026-11-17T10:00:00+00:00' \
                       AND payment_intent = 'pi_test' AND status = 'paid'\n";
        assert!(text.contains(checked), "{text}");
        let times = "\n# dates and times compared as constants, which may not hold on another \
                     day (authoring rule 5): invoices.paid_at\n";
        assert!(text.ends_with(times), "{text}");
        assert!(!text.contains("varies"), "{text}");
    }

    #[tokio::test]
    async fn evidence_files_draft_with_no_task_and_no_answer() {
        let dir = new_dir();
        let a = file(&dir, &paid("2026-11-17T10:00:00+00:00")).await;
        let b = file(&dir, &paid("2026-11-17T10:00:00+00:00")).await;
        let text = contract(&[&a, &b]).await;
        let header =
            format!("# Draft MIROIR_SPEC from 2 runs:\n#   {}\n#   {}\n", a.display(), b.display());
        assert!(text.starts_with(&header), "{text}");
        assert!(!text.contains("answer"), "{text}");
    }

    /// Two notes logged on deals 1100 and 1101, with the ids and times `at` gives.
    fn notes(ids: [i64; 2], at: &str, order: [usize; 2]) -> Evidence {
        let notes = [
            json!({"id": ids[0], "deal_id": 1100, "text": "Called, left a message",
                   "pinned": false, "score": 0.5, "created_at": at}),
            json!({"id": ids[1], "deal_id": 1101, "text": "Sent the quote", "pinned": true,
                   "score": 0.5, "created_at": at}),
        ];
        let changes = order.iter().map(|&i| insert("notes", notes[i].clone())).collect();
        evidence(changes, Vec::new())
    }

    #[tokio::test]
    async fn inserts_match_by_order_whatever_ids_they_were_given() {
        let dir = new_dir();
        let a = notes([501, 502], "2026-11-17T10:00:00+00:00", [0, 1]);
        let b = notes([611, 612], "2026-11-17T10:04:00+00:00", [0, 1]);
        let (a, b) = (trial(&dir, PAY, &a, None).await, trial(&dir, PAY, &b, None).await);
        let text = contract(&[&a, &b]).await;
        assert_eq!(
            spec_lines(&text),
            [
                "checks:",
                "  - name: notes_inserted",
                "    sql: |-",
                "      SELECT count(*) >= 1 FROM after.\"notes\" WHERE _op = 'insert' \
                 AND deal_id = 1100 AND pinned = 0 AND score = 0.5 \
                 AND text = 'Called, left a message'",
                "  - name: notes_inserted_2",
                "    sql: |-",
                "      SELECT count(*) >= 1 FROM after.\"notes\" WHERE _op = 'insert' \
                 AND deal_id = 1101 AND pinned = 1 AND score = 0.5 AND text = 'Sent the quote'",
                "allow:",
                "  - {table: notes, op: insert, where: \"deal_id = 1100 AND pinned = 0\"}",
                "  - {table: notes, op: insert, where: \"deal_id = 1101 AND pinned = 1\"}",
            ]
        );
        assert!(text.ends_with("# varies across runs: notes.created_at, notes.id\n"), "{text}");
    }

    #[tokio::test]
    async fn inserts_made_in_another_order_keep_only_what_they_share() {
        let dir = new_dir();
        let a = notes([501, 502], "2026-11-17T10:00:00+00:00", [0, 1]);
        let b = notes([611, 612], "2026-11-17T10:00:00+00:00", [1, 0]);
        let (a, b) = (trial(&dir, PAY, &a, None).await, trial(&dir, PAY, &b, None).await);
        let text = contract(&[&a, &b]).await;
        assert_eq!(
            spec_lines(&text),
            [
                "checks:",
                "  - name: notes_inserted",
                "    sql: |-",
                "      SELECT count(*) >= 1 FROM after.\"notes\" WHERE _op = 'insert' \
                 AND created_at = '2026-11-17T10:00:00+00:00' AND score = 0.5",
                "allow:",
                "  - {table: notes, op: insert}",
            ]
        );
        let footer = "# the allow entry for notes insert admits any row: no integer or boolean \
                      value every run shares narrows it\n\
                      # varies across runs: notes.deal_id, notes.id, notes.pinned, notes.text\n\
                      # dates and times compared as constants, which may not hold on another \
                      day (authoring rule 5): notes.created_at\n";
        assert!(text.ends_with(footer), "{text}");
    }

    /// Contact 2046 merged into 2045: its deals moved over, at `at`, and it was deleted. The
    /// contact as deleted was seen at `at` too, as a boot on another day moves a baked time.
    fn merged(at: &str) -> Evidence {
        let deal = |id: i64| json!({"id": id, "contact_id": 2046, "updated_at": "2026-11-01"});
        let set = json!({"contact_id": 2045, "updated_at": at});
        let contact = json!({"id": 2046, "name": "Ann Lee", "company": "Initech", "seen_at": at});
        let changes = vec![
            update("deals", deal(3101), set.clone()),
            update("deals", deal(3102), set),
            delete("contacts", contact),
        ];
        evidence(changes, Vec::new())
    }

    #[tokio::test]
    async fn updates_and_deletes_are_allowed_and_checked_by_key() {
        let dir = new_dir();
        let a = trial(&dir, "acme/merge", &merged("2026-11-17T10:00:00+00:00"), None).await;
        let b = trial(&dir, "acme/merge", &merged("2026-11-17T11:00:00+00:00"), None).await;
        let text = contract(&[&a, &b]).await;
        assert_eq!(
            spec_lines(&text),
            [
                "checks:",
                "  - name: deals_3101",
                "    sql: |-",
                "      SELECT count(*) FROM after.\"deals\" WHERE id = 3101 AND contact_id = 2045",
                "  - name: deals_3102",
                "    sql: |-",
                "      SELECT count(*) FROM after.\"deals\" WHERE id = 3102 AND contact_id = 2045",
                "  - name: contacts_2046_deleted",
                "    sql: |-",
                "      SELECT count(*) FROM before.\"contacts\" WHERE _op = 'delete' AND id = 2046",
                "allow:",
                "  - {table: deals, op: update, columns: [contact_id, updated_at], \
                 where: \"id = 3101\"}",
                "  - {table: deals, op: update, columns: [contact_id, updated_at], \
                 where: \"id = 3102\"}",
                "  - {table: contacts, op: delete, where: \"id = 2046\"}",
            ]
        );
        assert!(text.ends_with("# varies across runs: deals.updated_at\n"), "{text}");
    }

    #[tokio::test]
    async fn composite_keys_are_matched_and_named_by_every_value() {
        let dir = new_dir();
        let pk = object(json!({"currency": "usd", "day": "2026-11-17"}));
        let mut rate = delete(
            "exchange_rates",
            json!({"currency": "usd", "day": "2026-11-17",
                                                       "rate": 1.08}),
        );
        rate.pk = pk;
        let a = trial(&dir, PAY, &evidence(vec![rate.clone()], Vec::new()), None).await;
        let b = trial(&dir, PAY, &evidence(vec![rate], Vec::new()), None).await;
        let text = contract(&[&a, &b]).await;
        assert_eq!(
            spec_lines(&text),
            [
                "checks:",
                "  - name: exchange_rates_usd_2026_11_17_deleted",
                "    sql: |-",
                "      SELECT count(*) FROM before.\"exchange_rates\" WHERE _op = 'delete' \
                 AND currency = 'usd' AND day = '2026-11-17'",
                "allow:",
                "  - {table: exchange_rates, op: delete, \
                 where: \"currency = 'usd' AND day = '2026-11-17'\"}",
            ]
        );
    }

    #[tokio::test]
    async fn changed_columns_are_the_union_over_runs_and_checked_where_alike() {
        let dir = new_dir();
        let row = json!({"id": 7, "stage": "won", "owner_id": 3, "note": "x"});
        let first = update("deals", row.clone(), json!({"stage": "lost", "owner_id": 4}));
        let second = update("deals", row, json!({"stage": "lost", "note": "y"}));
        let a = trial(&dir, PAY, &evidence(vec![first], Vec::new()), None).await;
        let b = trial(&dir, PAY, &evidence(vec![second], Vec::new()), None).await;
        let text = contract(&[&a, &b]).await;
        assert_eq!(
            spec_lines(&text),
            [
                "checks:",
                "  - name: deals_7",
                "    sql: |-",
                "      SELECT count(*) FROM after.\"deals\" WHERE id = 7 AND stage = 'lost'",
                "allow:",
                "  - {table: deals, op: update, columns: [note, owner_id, stage], \
                 where: \"id = 7\"}",
            ]
        );
        assert!(text.ends_with("# varies across runs: deals.note, deals.owner_id\n"), "{text}");
    }

    #[tokio::test]
    async fn a_change_only_some_runs_made_is_allowed_and_named_but_not_checked() {
        let dir = new_dir();
        let at = "2026-11-17T10:00:00+00:00";
        let extra = plus(paid(at), update("invoices", invoice(4020), json!({"status": "void"})));
        let a = trial(&dir, PAY, &extra, None).await;
        let b = trial(&dir, PAY, &paid(at), None).await;
        let text = contract(&[&a, &b]).await;
        let lines = spec_lines(&text);
        let allow = "  - {table: invoices, op: update, columns: [status], where: \"id = 4020\"}";
        assert!(lines.contains(&allow), "{text}");
        assert_eq!(lines.iter().filter(|line| line.contains("4020")).count(), 1, "{text}");
        assert!(
            text.contains("\n# seen in 1 of 2 runs: invoices update {\"id\":4020}\n"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn null_values_are_checked_with_is_null_and_never_allowed_by_value() {
        let dir = new_dir();
        let paid = json!({"id": 4001, "status": "paid", "paid_at": "2026-11-17T10:00:00+00:00",
                          "payment_intent": "pi_test"});
        let reopened = update(
            "invoices",
            paid,
            json!({"status": "open", "paid_at": null,
                                                       "payment_intent": null}),
        );
        let note = insert("notes", json!({"id": 9, "deal_id": 1100, "text": null}));
        let both = evidence(vec![reopened, note], Vec::new());
        let a = trial(&dir, PAY, &both, None).await;
        let text = contract(&[&a]).await;
        let lines = spec_lines(&text);
        let update = "      SELECT count(*) FROM after.\"invoices\" \
                      WHERE id = 4001 AND paid_at IS NULL AND payment_intent IS NULL \
                      AND status = 'open'";
        assert!(lines.contains(&update), "{text}");
        let insert = "      SELECT count(*) >= 1 FROM after.\"notes\" \
                      WHERE _op = 'insert' AND deal_id = 1100 AND text IS NULL";
        assert!(lines.contains(&insert), "{text}");
        assert!(lines.contains(&"  - {table: notes, op: insert, where: \"deal_id = 1100\"}"));
    }

    #[tokio::test]
    async fn insert_entries_allow_by_integers_and_booleans_and_leave_the_rest_free() {
        let dir = new_dir();
        let row = json!({"id": 1, "big": 18_446_744_073_709_551_615_u64, "n": -3, "ok": true,
                         "ratio": 0.1, "kind": "login", "tags": ["a", "b"], "meta": {"k": 1},
                         "gone": null});
        let a = trial(&dir, PAY, &evidence(vec![insert("events", row)], Vec::new()), None).await;
        let text = contract(&[&a]).await;
        let lines = spec_lines(&text);
        let allow = "  - {table: events, op: insert, \
                     where: \"big = 18446744073709551615 AND n = -3 AND ok = 1\"}";
        assert!(lines.contains(&allow), "{text}");
        let check = "      SELECT count(*) >= 1 FROM after.\"events\" WHERE _op = 'insert' \
                     AND big = 18446744073709551615 AND gone IS NULL AND kind = 'login' \
                     AND meta = '{\"k\":1}' AND n = -3 AND ok = 1 AND ratio = 0.1 \
                     AND tags = '[\"a\",\"b\"]'";
        assert!(lines.contains(&check), "{text}");
    }

    #[tokio::test]
    async fn names_are_quoted_where_sql_and_yaml_need_it() {
        let dir = new_dir();
        let before = json!({"key": "it's", "order": 1, "x y": "a", "group": true});
        let mut setting = update(
            "app settings",
            before,
            json!({"order": 2, "x y": "two\nlines",
                                                                "group": false}),
        );
        setting.pk = object(json!({"key": "it's"}));
        let event = insert("audit.events", json!({"Kind": "lo\"gin", "null": 1}));
        let quoted = evidence(vec![setting, event], Vec::new());
        let a = trial(&dir, PAY, &quoted, None).await;
        let b = trial(&dir, PAY, &quoted, None).await;
        let text = contract(&[&a, &b]).await;
        assert_eq!(
            spec_lines(&text),
            [
                "checks:",
                "  - name: app_settings_it_s",
                "    sql: |-",
                "      SELECT count(*) FROM after.\"app settings\" WHERE \"key\" = 'it''s' \
                 AND \"group\" = 0 AND \"order\" = 2 AND \"x y\" = 'two",
                "      lines'",
                "  - name: audit_events_inserted",
                "    sql: |-",
                "      SELECT count(*) >= 1 FROM after.\"audit.events\" WHERE _op = 'insert' \
                 AND Kind = 'lo\"gin' AND \"null\" = 1",
                "allow:",
                "  - {table: \"app settings\", op: update, columns: [group, order, \"x y\"], \
                 where: \"\\\"key\\\" = 'it''s'\"}",
                "  - {table: audit.events, op: insert, where: \"\\\"null\\\" = 1\"}",
            ]
        );
    }

    /// Without its `float_roundtrip` feature, the `serde_json` parser reads the evidence's
    /// `1.5220476764550182e-202` one bit off, as the float written `1.522047676455018e-202`, which
    /// SQLite reads as another number.
    #[tokio::test]
    async fn floats_are_checked_with_the_text_the_evidence_holds() {
        let dir = new_dir();
        let float = 1.522_047_676_455_018_2e-202;
        let row = json!({"id": 1, "x": float, "nested": {"x": [float]}, "y": 0.1});
        let first =
            evidence(vec![update("rates", row.clone(), json!({"x": 2.5, "z": float}))], Vec::new());
        let rates = trial(&dir, PAY, &first, None).await;
        let inserted = evidence(vec![insert("rates", row)], Vec::new());
        let logged = trial(&dir, PAY, &inserted, None).await;
        let text = contract(&[&rates]).await;
        assert!(text.contains(" AND x = 2.5 AND z = 1.5220476764550182e-202\n"), "{text}");
        let text = contract(&[&logged]).await;
        let values = "nested = '{\"x\":[1.5220476764550182e-202]}' \
                      AND x = 1.5220476764550182e-202 AND y = 0.1\n";
        assert!(text.contains(values), "{text}");
    }

    /// `[0.0]` and `[-0.0]` are equal JSON values but differ as the text a check compares, and a
    /// nested value over 256 bytes is left out like long text.
    #[tokio::test]
    async fn nested_values_are_compared_and_measured_as_their_text() {
        let dir = new_dir();
        let meta = json!({"k": "x".repeat(CHECKED_BYTES)});
        let row =
            |v| evidence(vec![insert("rates", json!({"id": 1, "v": v, "meta": meta}))], vec![]);
        let a = trial(&dir, PAY, &row(json!([0.0])), None).await;
        let b = trial(&dir, PAY, &row(json!([-0.0])), None).await;
        let text = contract(&[&a, &b]).await;
        assert!(text.contains("# varies across runs: rates.v\n"), "{text}");
        assert!(text.contains("# not checked, over 256 bytes: rates.meta\n"), "{text}");
    }

    #[tokio::test]
    async fn sql_with_characters_a_block_cannot_hold_is_double_quoted() {
        let dir = new_dir();
        let row = json!({"id": 1, "text": "bell\u{7}\r\nend\u{85}", "tab": "a\tb"});
        let a = trial(&dir, PAY, &evidence(vec![insert("notes", row)], Vec::new()), None).await;
        let text = contract(&[&a]).await;
        let quoted = "    sql: \"SELECT count(*) >= 1 FROM after.\\\"notes\\\" \
                      WHERE _op = 'insert' AND tab = 'a\\tb' \
                      AND text = 'bell\\u0007\\r\\nend\\u0085'\"";
        assert!(spec_lines(&text).contains(&quoted), "{text}");
    }

    #[tokio::test]
    async fn an_update_whose_changed_values_all_vary_has_no_check() {
        let dir = new_dir();
        let touched = |at: &str| {
            let deal = update(
                "deals",
                json!({"id": 7, "updated_at": "2026-11-01"}),
                json!({"updated_at": at}),
            );
            plus(paid("2026-11-17T10:00:00+00:00"), deal)
        };
        let a = trial(&dir, PAY, &touched("2026-11-17T10:00:00+00:00"), None).await;
        let b = trial(&dir, PAY, &touched("2026-11-17T10:00:05+00:00"), None).await;
        let text = contract(&[&a, &b]).await;
        let lines = spec_lines(&text);
        let allow = "  - {table: deals, op: update, columns: [updated_at], where: \"id = 7\"}";
        assert!(lines.contains(&allow), "{text}");
        assert!(!lines.iter().any(|line| line.contains("deals_7")), "{text}");
        let note = "# no check for deals update {\"id\":7}: every value it sets varies across \
                    runs, is a new row's key or is too long\n\
                    # varies across runs: deals.updated_at\n";
        assert!(text.contains(note), "{text}");
    }

    #[tokio::test]
    async fn runs_that_did_nothing_leave_nothing_to_draft() {
        let dir = new_dir();
        let idle = evidence(Vec::new(), Vec::new());
        let a = trial(&dir, PAY, &idle, None).await;
        let b = trial(&dir, PAY, &idle, Some("  \n")).await;
        let (result, printed) = drafted(&[&a, &b]);
        assert!(!result.unwrap());
        let nothing =
            "# nothing to draft: no run changed a row, sent an email or a request, or answered\n";
        assert_eq!(printed, nothing);
    }

    #[tokio::test]
    async fn runs_that_share_nothing_leave_nothing_to_draft_and_say_why() {
        let dir = new_dir();
        let pay = |id| {
            evidence(vec![update("invoices", invoice(id), json!({"status": "paid"}))], Vec::new())
        };
        let a = trial(&dir, PAY, &pay(4001), None).await;
        let b = trial(&dir, PAY, &pay(4002), None).await;
        let (result, printed) = drafted(&[&a, &b]);
        assert!(!result.unwrap());
        assert_eq!(
            printed,
            "# nothing to draft: no check holds in every run\n\
             # seen in 1 of 2 runs: invoices update {\"id\":4001}\n\
             # seen in 1 of 2 runs: invoices update {\"id\":4002}\n"
        );
    }

    #[tokio::test]
    async fn an_update_alone_whose_values_vary_leaves_nothing_to_draft() {
        let dir = new_dir();
        let touch = |at: &str| {
            let deal = json!({"id": 7, "updated_at": "2026-11-01"});
            evidence(vec![update("deals", deal, json!({"updated_at": at}))], Vec::new())
        };
        let a = trial(&dir, PAY, &touch("2026-11-17T10:00:00+00:00"), None).await;
        let b = trial(&dir, PAY, &touch("2026-11-17T10:00:01+00:00"), None).await;
        let (result, printed) = drafted(&[&a, &b]);
        assert!(!result.unwrap());
        assert_eq!(
            printed,
            "# nothing to draft: no check holds in every run\n\
             # no check for deals update {\"id\":7}: every value it sets varies across runs, is \
             a new row's key or is too long\n\
             # varies across runs: deals.updated_at\n"
        );
    }

    #[tokio::test]
    async fn emails_match_by_order_and_are_checked_on_what_every_run_shares() {
        let dir = new_dir();
        let sent = |subject: &str, third: bool| {
            let mut effects = vec![
                email("marcus@acme.test", Some("Deal 2001 is now yours")),
                email("dana@acme.test", Some(subject)),
                email("ops@acme.test", None),
            ];
            if third {
                effects.push(email("audit@acme.test", None));
            }
            evidence(Vec::new(), effects)
        };
        let a = trial(&dir, PAY, &sent("Handed over at 10:00", true), None).await;
        let b = trial(&dir, PAY, &sent("Handed over at 10:02", false), None).await;
        let text = contract(&[&a, &b]).await;
        assert_eq!(
            spec_lines(&text),
            [
                "checks:",
                "  - name: email",
                "    sql: |-",
                "      SELECT count(*) >= 1 FROM sent_emails \
                 WHERE recipients = '[\"marcus@acme.test\"]' \
                 AND subject = 'Deal 2001 is now yours'",
                "  - name: email_2",
                "    sql: |-",
                "      SELECT count(*) >= 1 FROM sent_emails \
                 WHERE recipients = '[\"dana@acme.test\"]'",
                "  - name: email_3",
                "    sql: |-",
                "      SELECT count(*) >= 1 FROM sent_emails \
                 WHERE recipients = '[\"ops@acme.test\"]' AND subject IS NULL",
                "allow: []",
            ]
        );
        let notes = "# seen in 1 of 2 runs: email to [\"audit@acme.test\"]\n\
                     # varies across runs: sent_emails.subject\n";
        assert!(text.ends_with(notes), "{text}");
    }

    #[tokio::test]
    async fn an_email_to_other_recipients_is_checked_without_them() {
        let dir = new_dir();
        let sent = |to: &str| evidence(Vec::new(), vec![email(to, Some("Welcome"))]);
        let a = trial(&dir, PAY, &sent("marcus@acme.test"), None).await;
        let b = trial(&dir, PAY, &sent("dana@acme.test"), None).await;
        let text = contract(&[&a, &b]).await;
        let check = "      SELECT count(*) >= 1 FROM sent_emails WHERE subject = 'Welcome'";
        assert!(spec_lines(&text).contains(&check), "{text}");
        assert!(text.ends_with("# varies across runs: sent_emails.recipients\n"), "{text}");
        let other = evidence(Vec::new(), vec![email("ops@acme.test", Some("Hello"))]);
        let c = trial(&dir, PAY, &other, None).await;
        let text = contract(&[&a, &c]).await;
        let any = "      SELECT count(*) >= 1 FROM sent_emails";
        assert!(spec_lines(&text).contains(&any), "{text}");
        let footer = "# the check for an email passes for any email: no recipients or subject \
                      every run shares narrows it\n\
                      # varies across runs: sent_emails.recipients, sent_emails.subject\n";
        assert!(text.ends_with(footer), "{text}");
    }

    #[tokio::test]
    async fn requests_are_checked_once_per_method_and_path_every_run_sent() {
        let dir = new_dir();
        let charge = request("POST", "/stripe/v1/payment_intents");
        let a = evidence(Vec::new(), vec![charge.clone(), charge.clone()]);
        let mut b = evidence(Vec::new(), vec![charge.clone(), charge]);
        b.effects.push(request("GET", "/stripe/v1/customers"));
        let a = trial(&dir, PAY, &a, None).await;
        let b = trial(&dir, PAY, &b, None).await;
        let text = contract(&[&a, &b]).await;
        assert_eq!(
            spec_lines(&text),
            [
                "checks:",
                "  - name: post_stripe_v1_payment_intents",
                "    sql: |-",
                "      SELECT count(*) >= 1 FROM sent_requests \
                 WHERE method = 'POST' AND path = '/stripe/v1/payment_intents'",
                "allow: []",
            ]
        );
        let seen = "# seen in 1 of 2 runs: request GET /stripe/v1/customers\n";
        assert!(text.ends_with(seen), "{text}");
    }

    /// The answer check drafted from two runs that both answered `answer`.
    async fn answer_check(answer: &str) -> String {
        let dir = new_dir();
        let idle = evidence(Vec::new(), Vec::new());
        let a = trial(&dir, PAY, &idle, Some(answer)).await;
        let b = trial(&dir, PAY, &idle, Some(answer)).await;
        let text = contract(&[&a, &b]).await;
        let lines = spec_lines(&text);
        assert_eq!(lines[..3], ["checks:", "  - name: answer", "    sql: |-"], "{text}");
        lines[3].trim().to_owned()
    }

    #[tokio::test]
    async fn answers_are_checked_as_numbers_then_ids_then_text() {
        assert_eq!(answer_check("3\n").await, "SELECT number = 3 FROM answer");
        assert_eq!(answer_check(" -12 ").await, "SELECT number = -12 FROM answer");
        assert_eq!(answer_check("#12, #7").await, "SELECT ids = '7,12' FROM answer");
        assert_eq!(answer_check("7,12").await, "SELECT value = '7,12' FROM answer");
        assert_eq!(answer_check("It's late").await, "SELECT value = 'It''s late' FROM answer");
    }

    #[tokio::test]
    async fn answers_are_read_as_the_grader_reads_them() {
        let past_the_cap = format!("1{}2", " ".repeat(70 * 1024));
        assert_eq!(answer_check(&past_the_cap).await, "SELECT number = 1 FROM answer");
        let dir = new_dir();
        let idle = evidence(Vec::new(), Vec::new());
        let linked = trial(&dir, PAY, &idle, None).await;
        std::fs::create_dir_all(linked.join(ANSWER).parent().unwrap()).unwrap();
        std::fs::write(dir.join("elsewhere.txt"), "3").unwrap();
        std::os::unix::fs::symlink(dir.join("elsewhere.txt"), linked.join(ANSWER)).unwrap();
        let folder = trial(&dir, PAY, &idle, None).await;
        std::fs::create_dir_all(folder.join(ANSWER)).unwrap();
        let (result, printed) = drafted(&[&linked, &folder]);
        assert!(!result.unwrap(), "{printed}");
        assert!(printed.starts_with("# nothing to draft: no run changed"), "{printed}");
    }

    #[tokio::test]
    async fn answers_that_differ_or_only_some_runs_gave_are_named() {
        let dir = new_dir();
        let run = paid("2026-11-17T10:00:00+00:00");
        let a = trial(&dir, PAY, &run, Some("3")).await;
        let b = trial(&dir, PAY, &run, Some("4")).await;
        let text = contract(&[&a, &b]).await;
        assert!(!spec_lines(&text).iter().any(|line| line.contains("answer")), "{text}");
        assert!(text.contains("\n# varies across runs: answer.value\n"), "{text}");
        let c = file(&dir, &run).await;
        let text = contract(&[&a, &c]).await;
        assert!(text.contains("\n# seen in 1 of 2 runs: answer\n"), "{text}");
        assert!(text.starts_with("# Draft MIROIR_SPEC for acme/pay-invoice from 2 runs:\n"));
    }

    #[tokio::test]
    async fn keyless_rows_are_told_apart_by_their_values_and_identical_entries_merge() {
        let dir = new_dir();
        let tag = |deal: i64, label: &str| json!({"deal_id": deal, "label": label});
        let changes = vec![
            delete("tags", tag(1100, "hot")),
            delete("tags", tag(1100, "hot")),
            insert("tags", tag(1100, "won")),
        ];
        let a = trial(&dir, PAY, &evidence(changes.clone(), Vec::new()), None).await;
        let b = trial(&dir, PAY, &evidence(changes, Vec::new()), None).await;
        let text = contract(&[&a, &b]).await;
        assert_eq!(
            spec_lines(&text),
            [
                "checks:",
                "  - name: tags_deleted",
                "    sql: |-",
                "      SELECT count(*) >= 1 FROM before.\"tags\" WHERE _op = 'delete' \
                 AND deal_id = 1100 AND label = 'hot'",
                "  - name: tags_inserted",
                "    sql: |-",
                "      SELECT count(*) >= 1 FROM after.\"tags\" WHERE _op = 'insert' \
                 AND deal_id = 1100 AND label = 'won'",
                "allow:",
                "  - {table: tags, op: delete, where: \"deal_id = 1100\"}",
                "  - {table: tags, op: insert, where: \"deal_id = 1100\"}",
            ]
        );
        let elsewhere = vec![delete("tags", tag(1101, "hot")), insert("tags", tag(1100, "won"))];
        let elsewhere = trial(&dir, PAY, &evidence(elsewhere, Vec::new()), None).await;
        assert_eq!(graded(&text, &elsewhere).await, scored(0.0, 1));
    }

    #[tokio::test]
    async fn entries_and_checks_that_admit_any_row_are_named() {
        let dir = new_dir();
        let tag = |label: &str| json!({"label": label, "color": "red"});
        let a = evidence(vec![insert("tags", tag("hot")), delete("tags", tag("cold"))], Vec::new());
        let b = evidence(vec![insert("tags", tag("won")), delete("tags", tag("lost"))], Vec::new());
        let (a, b) = (trial(&dir, PAY, &a, None).await, trial(&dir, PAY, &b, None).await);
        let text = contract(&[&a, &b]).await;
        let lines = spec_lines(&text);
        let check = "      SELECT count(*) >= 1 FROM after.\"tags\" WHERE _op = 'insert' \
                     AND color = 'red'";
        assert!(lines.contains(&check) && lines.contains(&"  - {table: tags, op: insert}"));
        let footer = "# the allow entry for tags insert admits any row: no integer or boolean \
                      value every run shares narrows it\n\
                      # the allow entry for tags delete admits any row: no integer or boolean \
                      value every run shares narrows it\n\
                      # varies across runs: tags.label\n";
        assert!(text.ends_with(footer), "{text}");
        let colorless = |label: &str| insert("tags", json!({"label": label}));
        let a = trial(&dir, PAY, &evidence(vec![colorless("hot")], Vec::new()), None).await;
        let b = trial(&dir, PAY, &evidence(vec![colorless("won")], Vec::new()), None).await;
        let text = contract(&[&a, &b]).await;
        let check = "      SELECT count(*) >= 1 FROM after.\"tags\" WHERE _op = 'insert'";
        assert!(spec_lines(&text).contains(&check), "{text}");
        let any = "# the check for tags insert passes for any row: no value every run shares \
                   narrows it\n";
        assert!(text.contains(any), "{text}");
    }

    #[tokio::test]
    async fn runs_with_changes_no_spec_admits_are_named_as_harm() {
        let dir = new_dir();
        let truncated =
            plus(paid("2026-11-17T10:00:00+00:00"), reshaped("deals", ChangeOp::Truncate));
        let mut broken = plus(truncated, reshaped("notes", ChangeOp::Schema));
        broken.faults.push(Fault { kind: "crash".into(), detail: "web exited\nwith 1".into() });
        let a = trial(&dir, PAY, &broken, None).await;
        let b = trial(&dir, PAY, &paid("2026-11-17T10:00:00+00:00"), None).await;
        let (result, text) = drafted(&[&a, &b]);
        assert!(result.unwrap(), "{text}");
        let harm = format!(
            "# {} is harm whatever the spec says: deals truncate; notes schema; \
             fault crash: web exited\\nwith 1\n",
            a.display()
        );
        assert!(text.ends_with(&harm), "{text}");
        assert_eq!(graded(&text, &a).await, scored(0.0, 1));
        assert_eq!(graded(&text, &b).await, scored(1.0, 0));
    }

    /// Role `role` granted to user `user`: a row of a table keyed by both.
    fn granted(user: i64, role: i64) -> NetChange {
        let mut grant = insert("user_roles", json!({"user_id": user, "role_id": role, "on": true}));
        grant.pk = object(json!({"user_id": user, "role_id": role}));
        grant
    }

    /// Label `label` created and put on issue 3 (link `link`), milestone `milestone` created and
    /// set on issue 3, and role `role` created and granted to user 9: the keys are the product's.
    /// The label's `sort`, 5, is no reference, whichever key the role gets.
    fn labeled(label: i64, link: i64, milestone: i64, role: i64) -> Evidence {
        let changes = vec![
            insert("labels", json!({"id": label, "repo_id": 1, "name": "urgent", "sort": 5})),
            insert("issue_labels", json!({"id": link, "issue_id": 3, "label_id": label})),
            insert("milestones", json!({"id": milestone, "repo_id": 1, "title": "Q4"})),
            update(
                "issues",
                json!({"id": 3, "milestone_id": null}),
                json!({"milestone_id": milestone}),
            ),
            insert("roles", json!({"id": role, "name": "auditor"})),
            granted(9, role),
        ];
        evidence(changes, Vec::new())
    }

    #[tokio::test]
    async fn keys_of_new_rows_are_left_to_match_by_content() {
        let dir = new_dir();
        let a = trial(&dir, PAY, &labeled(57, 900, 12, 5), Some("57")).await;
        let b = trial(&dir, PAY, &labeled(57, 900, 12, 5), Some("#57")).await;
        let text = contract(&[&a, &b]).await;
        assert_eq!(
            spec_lines(&text),
            [
                "checks:",
                "  - name: labels_inserted",
                "    sql: |-",
                "      SELECT count(*) >= 1 FROM after.\"labels\" WHERE _op = 'insert' \
                 AND name = 'urgent' AND repo_id = 1 AND sort = 5",
                "  - name: issue_labels_inserted",
                "    sql: |-",
                "      SELECT count(*) >= 1 FROM after.\"issue_labels\" WHERE _op = 'insert' \
                 AND issue_id = 3",
                "  - name: milestones_inserted",
                "    sql: |-",
                "      SELECT count(*) >= 1 FROM after.\"milestones\" WHERE _op = 'insert' \
                 AND repo_id = 1 AND title = 'Q4'",
                "  - name: roles_inserted",
                "    sql: |-",
                "      SELECT count(*) >= 1 FROM after.\"roles\" WHERE _op = 'insert' \
                 AND name = 'auditor'",
                "  - name: user_roles_inserted",
                "    sql: |-",
                "      SELECT count(*) >= 1 FROM after.\"user_roles\" WHERE _op = 'insert' \
                 AND \"on\" = 1 AND user_id = 9",
                "allow:",
                "  - {table: labels, op: insert, where: \"repo_id = 1 AND sort = 5\"}",
                "  - {table: issue_labels, op: insert, where: \"issue_id = 3\"}",
                "  - {table: milestones, op: insert, where: \"repo_id = 1\"}",
                "  - {table: issues, op: update, columns: [milestone_id], where: \"id = 3\"}",
                "  - {table: roles, op: insert}",
                "  - {table: user_roles, op: insert, where: \"\\\"on\\\" = 1 AND user_id = 9\"}",
            ]
        );
        let rule = "which the product assigns: match that row by its content (authoring rule 4)";
        for (column, table) in [
            ("answer.value", "labels"),
            ("issue_labels.label_id", "labels"),
            ("issues.milestone_id", "milestones"),
            ("user_roles.role_id", "roles"),
        ] {
            let note = format!("\n# {column} is the key of a new {table} row, {rule}\n");
            assert!(text.contains(&note), "{note}{text}");
        }
        let agent = trial(&dir, PAY, &labeled(58, 901, 13, 6), Some("58")).await;
        assert_eq!(graded(&text, &agent).await, scored(1.0, 0), "other keys, the same work");
        let mut changes = labeled(58, 901, 13, 6).changes;
        changes[5] = granted(10, 6);
        let other = trial(&dir, PAY, &evidence(changes, Vec::new()), Some("58")).await;
        assert_eq!(graded(&text, &other).await, scored(0.0, 1), "a role granted to another user");
    }

    /// Repository `repo` given to user 16, as Gitea writes it: an access row, and the
    /// collaboration row `key`. A table that starts empty gives its first row key 1, which
    /// repository 1's id equals.
    fn collaborator(key: i64, repo: i64) -> Evidence {
        let changes = vec![
            insert("access", json!({"id": 7, "mode": 1, "repo_id": repo, "user_id": 16})),
            insert("collaboration", json!({"id": key, "mode": 1, "repo_id": repo, "user_id": 16})),
        ];
        evidence(changes, Vec::new())
    }

    #[tokio::test]
    async fn an_id_is_a_new_rows_key_only_when_named_after_its_table() {
        let dir = new_dir();
        let a = trial(&dir, PAY, &collaborator(1, 1), None).await;
        let b = trial(&dir, PAY, &collaborator(1, 1), None).await;
        let text = contract(&[&a, &b]).await;
        assert!(!text.contains("is the key of a new"), "{text}");
        for table in ["access", "collaboration"] {
            let allow = format!(
                "  - {{table: {table}, op: insert, where: \"mode = 1 AND repo_id = 1 AND user_id = \
                 16\"}}"
            );
            assert!(spec_lines(&text).contains(&allow.as_str()), "{allow}\n{text}");
        }
        let elsewhere = trial(&dir, PAY, &collaborator(1, 2), None).await;
        assert_eq!(graded(&text, &elsewhere).await, scored(0.0, 1), "another repository");
    }

    #[tokio::test]
    async fn a_column_not_named_as_a_reference_is_none_whatever_its_name() {
        let dir = new_dir();
        let changes = vec![
            insert("versions", json!({"id": 2, "document_id": 4, "text": "Second draft"})),
            update("documents", json!({"id": 4, "version": 1}), json!({"version": 2})),
        ];
        let run = evidence(changes, Vec::new());
        let a = trial(&dir, PAY, &run, None).await;
        let b = trial(&dir, PAY, &run, None).await;
        let text = contract(&[&a, &b]).await;
        let check = "      SELECT count(*) FROM after.\"documents\" WHERE id = 4 AND version = 2";
        assert!(spec_lines(&text).contains(&check), "{text}");
    }

    #[test]
    fn a_reference_names_its_table_in_the_singular_or_the_plural() {
        for (column, table) in [
            ("label_id", "labels"),
            ("labelId", "labels"),
            ("issue_id", "issue"),
            ("ref_issue_id", "issue"),
            ("refIssueId", "issues"),
            ("box_id", "boxes"),
            ("category_id", "categories"),
            ("invoice_id", "billing.invoices"),
        ] {
            assert!(names(column, table), "{column} names {table}");
        }
        for (column, table) in [
            ("repo_id", "collaboration"),
            ("poster_id", "issue_content_history"),
            ("repo_id", "repository"),
            ("label_id", "labelss"),
            ("Id", "labels"),
        ] {
            assert!(!names(column, table), "{column} does not name {table}");
        }
    }

    #[tokio::test]
    async fn dates_and_times_stay_out_of_allow_entries_and_are_named() {
        let dir = new_dir();
        let task = json!({"id": 1, "deal_id": 1100, "due": "2026-11-20", "done": false,
                          "remind_s": 1_795_000_000, "remind_ms": 1_795_000_000_000_i64});
        let close = update("deals", json!({"id": 1100, "close_on": null}), json!({"close_on": 7}));
        let mut rate = delete("rates", json!({"day": "2026-11-17", "rate": 1.08}));
        rate.pk = object(json!({"day": "2026-11-17"}));
        let changes = vec![insert("tasks", task), close, rate];
        let a = trial(&dir, PAY, &evidence(changes, Vec::new()), Some("2026-11-20")).await;
        let text = contract(&[&a]).await;
        let lines = spec_lines(&text);
        let allow = "  - {table: tasks, op: insert, where: \"deal_id = 1100 AND done = 0\"}";
        assert!(lines.contains(&allow), "{text}");
        let check = "      SELECT count(*) >= 1 FROM after.\"tasks\" WHERE _op = 'insert' \
                     AND deal_id = 1100 AND done = 0 AND due = '2026-11-20' \
                     AND remind_ms = 1795000000000 AND remind_s = 1795000000";
        assert!(lines.contains(&check), "{text}");
        let times = "\n# dates and times compared as constants, which may not hold on another day \
                     (authoring rule 5): answer.value, rates.day, tasks.due, tasks.remind_ms, \
                     tasks.remind_s\n";
        assert!(text.ends_with(times), "{text}");
    }

    #[test]
    fn dates_and_unix_times_from_2000_to_2100_look_like_times() {
        let times = [
            json!("2026-11-17"),
            json!("2026-11-17T10:00:00+00:00"),
            json!("2026-11-17T10:00:00"),
            json!(946_684_800),
            json!(4_102_444_799_i64),
            json!(946_684_800_000_i64),
            json!(4_102_444_799_999_i64),
        ];
        for value in times {
            assert!(timelike(&value), "{value}");
        }
        let others = [
            json!("2026-11-17 10:00"),
            json!("2026-11-1"),
            json!("2026-11-1x"),
            json!("2026/11/17"),
            json!("x026-11-17"),
            json!("20261117"),
            json!(946_684_799),
            json!(4_102_444_800_i64),
            json!(946_684_799_999_i64),
            json!(4_102_444_800_000_i64),
            json!(1.8e9),
            json!(true),
            json!(null),
        ];
        for value in others {
            assert!(!timelike(&value), "{value}");
        }
    }

    #[tokio::test]
    async fn no_draft_holds_three_quotes_in_a_row_so_it_pastes_into_task_toml() {
        let dir = new_dir();
        let note = insert("notes", json!({"id": 1, "deal_id": 1100, "text": "Renamed 'Q4 plan'"}));
        let mut setting = update("settings", json!({"name": "a''", "on": 0}), json!({"on": 1}));
        setting.pk = object(json!({"name": "a''"}));
        let b = evidence(vec![note.clone(), setting], Vec::new());
        let a = Evidence { tables: b.tables.clone(), ..evidence(vec![note], Vec::new()) };
        let a = trial(&dir, PAY, &a, Some("'''")).await;
        let b = trial(&dir, PAY, &b, Some("'''")).await;
        let text = contract(&[&a, &b]).await;
        let lines = spec_lines(&text);
        let check = "      SELECT count(*) >= 1 FROM after.\"notes\" WHERE _op = 'insert' \
                     AND deal_id = 1100 \
                     AND text = ('Renamed ' || char(39) || 'Q4 plan' || char(39))";
        assert!(lines.contains(&check), "{text}");
        assert!(text.contains("\n# seen in 1 of 2 runs: settings update {\"name\":\"a'\\'\"}\n"));
        assert!(!text.contains("'''"), "{text}");
        let task = format!("[verifier.env]\nMIROIR_SPEC = '''\n{text}'''\n");
        let task: toml::Table = toml::from_str(&task).unwrap_or_else(|e| panic!("{e}\n{task}"));
        assert_eq!(task["verifier"]["env"]["MIROIR_SPEC"].as_str(), Some(text.as_str()));
    }

    #[tokio::test]
    async fn only_whole_runs_of_the_reference_solution_are_drafted_from() {
        let dir = new_dir();
        let run = paid("2026-11-17T10:00:00+00:00");
        let refusal = |trial: &Path, problem: &str| {
            format!(
                "{} is not a whole run of the reference solution: {problem}. Draft from the \
                 trials of the oracle agent without SOLUTION, or pass the trial's evidence.db to \
                 draft from it anyway",
                trial.display()
            )
        };
        let cases: [(Edit, &str); 5] = [
            (|result| result["config"]["agent"]["name"] = json!("nop"), "its agent is nop"),
            (
                |result| result["config"]["agent"] = json!({"name": null, "import_path": "my:Ag"}),
                "its agent is my:Ag",
            ),
            (|result| result["config"] = json!({}), "it names no agent"),
            (
                |result| result["config"]["agent"]["env"] = json!({"SOLUTION": "2"}),
                "it runs probe 2 (SOLUTION=2)",
            ),
            (
                |result| result["exception_info"] = json!({"exception_type": "AgentTimeoutError"}),
                "the trial failed with AgentTimeoutError",
            ),
        ];
        for (edit, problem) in cases {
            let other = trial(&dir, PAY, &run, None).await;
            rewrite(&other, edit);
            assert_eq!(refused(&[&other]), refusal(&other, problem));
            contract(&[&other.join(EVIDENCE)]).await;
        }
        let failed = trial(&dir, PAY, &run, None).await;
        std::fs::create_dir_all(failed.join("agent")).unwrap();
        std::fs::write(failed.join("agent/exit-code.txt"), "1\n").unwrap();
        let problem = "the solution exited with 1 (agent/exit-code.txt)";
        assert_eq!(refused(&[&failed]), refusal(&failed, problem));
        std::fs::write(failed.join("agent/exit-code.txt"), "0").unwrap();
        contract(&[&failed]).await;
        let unreadable = trial(&dir, PAY, &run, None).await;
        std::fs::create_dir_all(unreadable.join("agent/exit-code.txt")).unwrap();
        let error = refused(&[&unreadable]);
        let file = unreadable.join("agent/exit-code.txt");
        assert!(error.starts_with(&format!("cannot read {}: ", file.display())), "{error}");
        let voided = trial(&dir, PAY, &run, None).await;
        rewrite(&voided, |result| {
            result["exception_info"] = json!({"exception_type": "RewardFileNotFoundError"});
            result["config"]["agent"]["env"] = json!({"SOLUTION": ""});
        });
        contract(&[&voided]).await;
    }

    #[tokio::test]
    async fn a_run_given_twice_is_refused() {
        let dir = new_dir();
        let run = paid("2026-11-17T10:00:00+00:00");
        let (a, b) = (trial(&dir, PAY, &run, None).await, trial(&dir, PAY, &run, None).await);
        let twice = |first: &Path, second: &Path| {
            let (first, second) = (first.display(), second.display());
            format!("{first} and {second} are the same run: draft from different trials")
        };
        assert_eq!(refused(&[&a, &b, &a]), twice(&a, &a));
        let evidence = a.join(EVIDENCE);
        assert_eq!(refused(&[&evidence, &b, &a]), twice(&evidence, &a));
        let spelled = dir.join(".").join(a.file_name().unwrap());
        assert_eq!(refused(&[&a, &spelled]), twice(&a, &spelled));
    }

    #[tokio::test]
    async fn trials_of_different_tasks_are_refused() {
        let dir = new_dir();
        let run = paid("2026-11-17T10:00:00+00:00");
        let a = trial(&dir, PAY, &run, None).await;
        let b = file(&dir, &run).await;
        let c = trial(&dir, "acme/close-won-deal", &run, None).await;
        let error = refused(&[&a, &b, &c]);
        let expected = format!(
            "{} is a trial of acme/pay-invoice, but {} is a trial of acme/close-won-deal: draft \
             from the trials of one task",
            a.display(),
            c.display()
        );
        assert_eq!(error, expected);
    }

    #[tokio::test]
    async fn missing_and_foreign_inputs_are_refused() {
        let dir = new_dir();
        let missing = dir.join("evidence.db");
        let error = refused(&[&missing]);
        assert!(error.starts_with(&format!("cannot read the evidence {}: ", missing.display())));
        let empty = dir.join("empty-trial");
        std::fs::create_dir_all(&empty).unwrap();
        let error = refused(&[&empty]);
        assert!(error.starts_with(&format!("{} is not a Harbor trial directory", empty.display())));
        let collected = trial(&dir, PAY, &paid("2026-11-17T10:00:00+00:00"), None).await;
        std::fs::remove_file(collected.join(EVIDENCE)).unwrap();
        let error = refused(&[&collected]);
        let evidence = collected.join(EVIDENCE);
        assert!(error.starts_with(&format!("cannot read the evidence {}", evidence.display())));
        std::fs::write(collected.join("result.json"), "{\"trial_name\": \"t\"}").unwrap();
        assert!(refused(&[&collected]).ends_with("names no task_name: is it a Harbor trial's?"));
        std::fs::write(collected.join("result.json"), "{").unwrap();
        assert!(refused(&[&collected]).contains("result.json is not JSON"));
        assert_eq!(
            refused(&[]),
            "nothing to draft from: pass the Harbor trial directories of one task, or \
             evidence.db files"
        );
    }

    #[tokio::test]
    async fn evidence_of_another_format_or_none_at_all_is_refused() {
        let dir = new_dir();
        let path = file(&dir, &paid("2026-11-17T10:00:00+00:00")).await;
        let options = sqlx::sqlite::SqliteConnectOptions::new().filename(&path);
        let mut conn = sqlx::SqliteConnection::connect_with(&options).await.unwrap();
        sqlx::raw_sql("UPDATE meta SET value = '2' WHERE key = 'format'")
            .execute(&mut conn)
            .await
            .unwrap();
        conn.close().await.unwrap();
        let error = refused(&[&path]);
        assert!(error.contains("is evidence format 2, written by miroir 0.1.0"), "{error}");
        let text = dir.join("notes.txt");
        std::fs::write(&text, "not a database").unwrap();
        assert!(refused(&[&text]).contains("is not Miroir evidence"));
    }

    #[tokio::test]
    async fn changes_that_are_not_json_are_refused() {
        let dir = new_dir();
        let path = file(&dir, &paid("2026-11-17T10:00:00+00:00")).await;
        let options = sqlx::sqlite::SqliteConnectOptions::new().filename(&path);
        let mut conn = sqlx::SqliteConnection::connect_with(&options).await.unwrap();
        sqlx::raw_sql("UPDATE changes SET op = 'upsert'").execute(&mut conn).await.unwrap();
        conn.close().await.unwrap();
        let error = refused(&[&path]);
        assert!(error.contains("unknown variant `upsert`"), "{error}");
    }

    #[tokio::test]
    async fn a_draft_past_the_spec_limit_is_refused() {
        let dir = new_dir();
        let note = |n: i64| insert("notes", json!({"id": n, "deal_id": 1100 + n, "text": "x"}));
        let a = trial(&dir, PAY, &evidence((0..300).map(note).collect(), Vec::new()), None).await;
        let error = refused(&[&a]);
        assert!(error.starts_with("the draft is not a valid spec, so it is not printed: the spec"));
    }

    #[tokio::test]
    async fn text_too_long_to_check_is_named_and_left_out() {
        let dir = new_dir();
        let row = json!({"id": 1, "deal_id": 1100, "title": "t".repeat(CHECKED_BYTES),
                         "text": "x".repeat(CHECKED_BYTES + 1)});
        let (note, long) = (evidence(vec![insert("notes", row)], Vec::new()), "y".repeat(70_000));
        let a = trial(&dir, PAY, &note, Some(&long)).await;
        let text = contract(&[&a]).await;
        let check = format!(
            "      SELECT count(*) >= 1 FROM after.\"notes\" WHERE _op = 'insert' \
             AND deal_id = 1100 AND title = '{}'",
            "t".repeat(CHECKED_BYTES)
        );
        assert!(spec_lines(&text).contains(&check.as_str()), "{text}");
        assert!(text.ends_with("\n# not checked, over 256 bytes: answer.value, notes.text\n"));
    }

    #[tokio::test]
    async fn drafting_leaves_its_inputs_as_they_are() {
        let dir = new_dir();
        let a = trial(&dir, PAY, &paid("2026-11-17T10:00:00+00:00"), Some("3")).await;
        let before = std::fs::read(a.join(EVIDENCE)).unwrap();
        contract(&[&a]).await;
        assert_eq!(std::fs::read(a.join(EVIDENCE)).unwrap(), before);
        let scratch = Scratch::of(&a.join(EVIDENCE)).unwrap();
        let copy = scratch.0.clone();
        assert_eq!(std::fs::read(&copy).unwrap(), before);
        drop(scratch);
        assert!(!copy.exists());
    }

    #[test]
    fn check_names_are_valid_unique_and_never_a_yaml_word() {
        let cases = [
            ("invoices 4001", "invoices_4001"),
            ("audit.log 7 deleted", "audit_log_7_deleted"),
            ("POST /stripe/v1/payment_intents", "post_stripe_v1_payment_intents"),
            ("  App--Settings  it's ", "app_settings_it_s"),
            ("2fa codes", "t_2fa_codes"),
            ("東京", "t"),
            ("東京 5", "t_5"),
            ("null", "null"),
        ];
        for (words, expected) in cases {
            assert_eq!(name(words), expected, "{words}");
        }
        let mut draft = Draft::default();
        for (words, sql) in [("a b", "SELECT 1"), ("a-b", "SELECT 2"), ("a b", "SELECT 3")] {
            draft.check(words, sql.into());
        }
        draft.check("a_b", "SELECT 1".into());
        for (words, sql) in [("null", "SELECT 4"), ("true", "SELECT 5"), ("False", "SELECT 6")] {
            draft.check(words, sql.into());
        }
        for (words, sql) in [("a b 4", "SELECT 7"), ("a b", "SELECT 8")] {
            draft.check(words, sql.into());
        }
        let names: Vec<&str> = draft.checks.iter().map(|(name, _)| name.as_str()).collect();
        let expected = ["a_b", "a_b_2", "a_b_3", "null_2", "true_2", "false_2", "a_b_4", "a_b_5"];
        assert_eq!(names, expected);
    }

    #[test]
    fn comments_escape_what_yaml_cannot_hold() {
        assert_eq!(comment("a\tb é"), "# a\tb é\n");
        assert_eq!(comment("a\nb\rc"), "# a\\nb\\rc\n");
        assert_eq!(comment("\u{7f}\u{85}\u{fffe}\u{ffff}"), "# \\u{7f}\\u{85}\\u{fffe}\\u{ffff}\n");
    }

    #[test]
    fn sql_scalars_are_blocks_unless_a_block_cannot_hold_them() {
        assert_eq!(scalar("SELECT 'a'\nFROM b"), "|-\n      SELECT 'a'\n      FROM b");
        assert_eq!(scalar("SELECT 'a\tb'"), "|-\n      SELECT 'a\tb'");
        assert_eq!(scalar("SELECT 1\n"), "\"SELECT 1\\n\"");
        assert_eq!(scalar("SELECT '\r'"), "\"SELECT '\\r'\"");
        assert_eq!(scalar("SELECT '\u{7f}\u{fffe}'"), "\"SELECT '\\u007f\\ufffe'\"");
        assert_eq!(scalar("SELECT '\u{ffff}'"), "\"SELECT '\\uffff'\"");
    }

    proptest! {
        #[test]
        fn sql_scalars_read_back_as_written(text in "\\PC*|[^\u{0}]{0,24}") {
            let sql = format!("SELECT {text}");
            let spec = format!("checks:\n  - name: a\n    sql: {}\nallow: []\n", scalar(&sql));
            let parsed = spec::parse(&spec);
            let parsed = parsed.map_err(|e| TestCaseError::fail(format!("{e}\n{spec}")))?;
            prop_assert_eq!(&parsed.checks[0].sql, &sql);
        }

        #[test]
        fn comments_never_break_the_spec(text in "[^\u{0}]{0,40}") {
            let spec = comment(&text) + "checks: [{name: a, sql: SELECT 1}]\nallow: []\n";
            prop_assert!(spec::parse(&spec).is_ok(), "{spec}");
        }
    }

    /// A table of the random contract test: its name, whether it is keyed by `id`, and its other
    /// columns.
    #[derive(Debug, Clone)]
    struct Shape {
        name: String,
        keyed: bool,
        columns: Vec<String>,
    }

    /// One change of the random contract test: the same in both runs, except that a volatile
    /// column holds its `other` value in the second run.
    #[derive(Debug, Clone)]
    struct Step {
        table: usize,
        op: u8,
        values: Vec<Value>,
        others: Vec<Value>,
        volatile: Vec<bool>,
    }

    fn value() -> impl Strategy<Value = Value> {
        let scalar = prop_oneof![
            any::<i64>().prop_map(Value::from),
            any::<bool>().prop_map(Value::from),
            "[^\u{0}]{0,12}".prop_map(Value::from),
            "['a ]{0,6}".prop_map(Value::from),
            "20[0-9]{2}-[01][0-9]-[0-3][0-9](T[0-2][0-9]:00:00)?".prop_map(Value::from),
            UNIX_SECONDS.prop_map(Value::from),
            Just(Value::Null),
            any::<f64>().prop_filter("finite", |f| f.is_finite()).prop_map(Value::from),
        ];
        scalar.prop_recursive(2, 6, 3, |inner| {
            prop_oneof![
                collection::vec(inner.clone(), 0..3).prop_map(Value::from),
                collection::btree_map("[^\u{0}]{0,4}", inner, 0..3)
                    .prop_map(|map| Value::Object(map.into_iter().collect())),
            ]
        })
    }

    /// A table or column name. A name with three `'` in a row, which no spec pasted into a TOML
    /// literal string can hold, is left out: values never need them, but names have no escape.
    fn table_name() -> impl Strategy<Value = String> {
        "[!-~é][ -~é]{0,5}".prop_filter("three quotes", |name| !name.contains("'''"))
    }

    fn shapes() -> impl Strategy<Value = Vec<Shape>> {
        let columns = collection::btree_set(table_name(), 1..4)
            .prop_filter("the grader's own columns and the key", |columns| {
                !columns.iter().any(|c| ["_op", "_change", "id"].contains(&c.as_str()))
            });
        collection::btree_map(table_name(), (any::<bool>(), columns), 1..3).prop_map(|tables| {
            let shape = |(name, (keyed, columns)): (String, (bool, BTreeSet<String>))| Shape {
                name,
                keyed,
                columns: columns.into_iter().collect(),
            };
            tables.into_iter().map(shape).collect()
        })
    }

    fn step(tables: usize) -> impl Strategy<Value = Step> {
        let values = || collection::vec(value(), 3);
        let step = (0..tables, 0..3_u8, values(), values(), collection::vec(any::<bool>(), 3));
        step.prop_map(|(table, op, values, others, volatile)| Step {
            table,
            op,
            values,
            others,
            volatile,
        })
    }

    /// The evidence of one run of the random contract test. A keyless table gets no updates,
    /// which `fold` never makes; an update changes every column, from a value no step holds.
    fn random_run(shapes: &[Shape], steps: &[Step], second: bool) -> Evidence {
        let mut changes = Vec::new();
        for (n, step) in (0_i64..).zip(steps) {
            let shape = &shapes[step.table];
            let mut row = Map::new();
            for (i, column) in shape.columns.iter().enumerate() {
                let volatile = second && step.volatile[i];
                let value = if volatile { &step.others[i] } else { &step.values[i] };
                row.insert(column.clone(), value.clone());
            }
            let op = if shape.keyed { step.op } else { step.op / 2 * 2 };
            if shape.keyed {
                let id = if op == 0 { 1000 + n + if second { 500 } else { 0 } } else { n };
                row.insert("id".into(), id.into());
            }
            let old = |row: &Map<String, Value>| {
                Value::Object(row.iter().map(|(c, v)| (c.clone(), json!(["old", v]))).collect())
            };
            changes.push(match op {
                0 => insert(&shape.name, Value::Object(row)),
                1 => {
                    let mut before = old(&row);
                    before["id"] = row["id"].clone();
                    update(&shape.name, before, Value::Object(row))
                }
                _ => delete(&shape.name, Value::Object(row)),
            });
        }
        evidence(changes, Vec::new())
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]
        /// The contract (B1) on random evidence: two runs that differ in assigned ids and
        /// volatile values, the second with a change of its own when `extra` is set, with tricky
        /// names, values, effects and answers. No draft holds three `'` in a row.
        #[test]
        fn every_draft_parses_and_its_runs_score_one_without_harm(
            (shapes, steps, extra) in shapes().prop_flat_map(|shapes| {
                let steps = collection::vec(step(shapes.len()), 1..5);
                (Just(shapes.clone()), steps, option::of(step(shapes.len())))
            }),
            answer in option::of(prop_oneof!["[^\u{0}]{0,16}", "['1 ,#]{0,6}"]),
            sent in option::of(("[^\u{0}]{0,12}", "[^\u{0}]{0,12}", any::<bool>())),
        ) {
            let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
            runtime.block_on(async {
                let dir = new_dir();
                let more: Vec<Step> = steps.iter().chain(&extra).cloned().collect();
                let mut second = random_run(&shapes, &more, true);
                // One world: both runs capture the same tables.
                let tables = second.tables.clone();
                let mut first = Evidence { tables, ..random_run(&shapes, &steps, false) };
                if let Some((path, subject, varies)) = &sent {
                    let other = if *varies { format!("{subject}!") } else { subject.clone() };
                    let path = format!("/{path}");
                    first.effects = vec![request("POST", &path), email("a@b.test", Some(subject))];
                    second.effects = vec![request("POST", &path), email("a@b.test", Some(&other))];
                }
                let a = trial(&dir, PAY, &first, answer.as_deref()).await;
                let b = trial(&dir, PAY, &second, answer.as_deref()).await;
                let (result, text) = drafted(&[&a, &b]);
                if result.unwrap() {
                    spec::parse(&text).unwrap_or_else(|e| panic!("{e}\n{text}"));
                    assert!(!text.contains("'''"), "{text}");
                    assert_eq!(graded(&text, &a).await, scored(1.0, 0), "{text}");
                    assert_eq!(graded(&text, &b).await, scored(1.0, 0), "{text}");
                } else {
                    assert!(text.starts_with("# nothing to draft: "), "{text}");
                }
            });
        }
    }
}
