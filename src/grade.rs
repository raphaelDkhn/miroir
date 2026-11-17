//! `miroir grade` (§3.7), the verifier's `/tests/test.sh`: score the evidence with the spec and
//! write `reward.json`, or void. Scaffold: WP5 implements it.

use std::path::PathBuf;

use serde_json::{Map, Value};

use crate::spec::Op;

/// The SQLite VM steps one statement may take before it is stopped.
pub const CHECK_STEPS: u64 = 100_000_000;
/// The most bytes of the answer file the grader reads.
pub const ANSWER_BYTES: u64 = 64 * 1024;

/// What the grader reads, and where it writes.
#[derive(Debug, Clone)]
pub struct Inputs {
    /// The spec; `None` when `MIROIR_SPEC` is not set.
    pub spec: Option<String>,
    pub evidence: PathBuf,
    pub answer: PathBuf,
    /// Where `reward.json` and `reward-details.json` go; created when missing.
    pub out_dir: PathBuf,
}

/// How a trial grades.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// The reward file is written.
    Scored { reward: f64, harm: u8 },
    /// Miroir's own failure: no reward file, and `void: <reason>` on stdout.
    Void(String),
}

/// Grade a trial, printing its summary, or `void: <reason>`, to `out`.
#[expect(clippy::unused_async, reason = "scaffold: WP5 awaits once it implements this")]
pub async fn grade(inputs: &Inputs, out: &mut impl std::io::Write) -> Outcome {
    let _ = (inputs, out);
    Outcome::Void("WP5: not implemented".into())
}

/// The one row of the `answer` table (§4.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    /// The trimmed text; `None` when there is no answer.
    pub value: Option<String>,
    /// `value` as a signed 64-bit integer.
    pub number: Option<i64>,
    /// `value` as normalized ids: sorted, unique, joined with `,`.
    pub ids: Option<String>,
}

/// The answer the text of `/logs/artifacts/answer.txt` gives.
pub fn answer(text: &str) -> Answer {
    let _ = text;
    Answer { value: None, number: None, ids: None }
}

/// The paste-ready `allow` line for a change (§4.5), such as `- {table: invoices, op: update,
/// columns: [paid_at, status], where: "id = 4020"}`: its `where` is the conjunction of
/// `column = value` over `key`, and an empty `key` gets none. Grade passes a change's primary
/// key; `miroir draft` passes an insert's stable integer and boolean columns. The one renderer
/// of these lines, shared by grade's out-of-scope report and `miroir draft`.
pub fn allow_line(
    table: &str,
    op: Op,
    columns: Option<&[String]>,
    key: &Map<String, Value>,
) -> String {
    let _ = (table, op, columns, key);
    String::new()
}

/// A SQLite literal for a JSON value, as allow lines and `miroir draft` write it in SQL. The one
/// renderer of SQL literals, shared by grade and draft.
pub fn sql_literal(value: &Value) -> String {
    let _ = value;
    String::new()
}
