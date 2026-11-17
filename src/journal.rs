//! JSON-lines journals under an exclusive `flock`, one JSON object per line. The line shapes
//! are the contract between the sinks and the supervisor (writers) and `evidence::write`:
//!
//! - `effects.jsonl`: `{"kind":"email","data":{"from":str|null,"to":[str],"cc":[str],
//!   "bcc":[str],"subject":str|null,"text":str|null,"html":str|null}}` and
//!   `{"kind":"http","data":{"method":str,"path":str,"query":{str:str},"headers":{str:str},
//!   "body":<any JSON value; null when empty>}}`, with header names lower-case, `authorization`
//!   dropped, and a body that is not JSON as a JSON string of its text;
//! - `faults.jsonl`: `{"kind":<a evidence::fault kind>,"detail":str}`, an `evidence::Fault`.
//!
//! Scaffold: WP1 implements it.

use std::path::Path;

use serde_json::Value;

/// Append one line, under an exclusive lock, with `O_APPEND`.
pub fn append(path: &Path, value: &Value) -> std::io::Result<()> {
    let _ = (path, value);
    Err(std::io::Error::other("WP1: not implemented"))
}

/// Every line, in order, under the same lock; a missing file reads empty.
pub fn read(path: &Path) -> std::io::Result<Vec<Value>> {
    let _ = path;
    Err(std::io::Error::other("WP1: not implemented"))
}

/// Empty the journal.
pub fn clear(path: &Path) -> std::io::Result<()> {
    let _ = path;
    Err(std::io::Error::other("WP1: not implemented"))
}
