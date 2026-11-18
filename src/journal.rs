//! JSON-lines journals under an exclusive `flock`, one JSON object per line. The line shapes
//! are the contract between the sinks and the supervisor (writers) and `evidence::write`:
//!
//! - `effects.jsonl`: `{"kind":"email","data":{"from":str|null,"to":[str],"cc":[str],
//!   "bcc":[str],"subject":str|null,"text":str|null,"html":str|null}}` and
//!   `{"kind":"http","data":{"method":str,"path":str,"query":{str:str},"headers":{str:str},
//!   "body":<any JSON value; null when empty>}}`, with header names lower-case, `authorization`
//!   dropped, and a body that is not JSON as a JSON string of its text;
//! - `faults.jsonl`: `{"kind":<a evidence::fault kind>,"detail":str}`, an `evidence::Fault`.

use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::Path;

use serde::Deserialize;
use serde_json::Value;

/// Append one line, under an exclusive lock, with `O_APPEND`, so a concurrent `clear` or a
/// writer in another process never splits it.
pub fn append(path: &Path, value: &Value) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.lock()?;
    file.write_all(&line)
}

/// Every line, in order, under the same lock taken shared, so no append is read half-written;
/// a missing file reads empty. A line that is not JSON is an error naming the file and line.
pub fn read(path: &Path) -> std::io::Result<Vec<Value>> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    file.lock_shared()?;
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    let mut values = Vec::new();
    for (number, line) in (1..).zip(text.lines()) {
        let value = parse(line).map_err(|e| {
            let message = format!("{} line {number} is not JSON: {e}", path.display());
            std::io::Error::new(ErrorKind::InvalidData, message)
        })?;
        values.push(value);
    }
    Ok(values)
}

/// One line's value, with no depth limit: a sink keeps a request body as deep as `serde_json`
/// parses one, 127 levels, and the line nests it two levels deeper.
fn parse(line: &str) -> serde_json::Result<Value> {
    let mut parser = serde_json::Deserializer::from_str(line);
    parser.disable_recursion_limit();
    let value = Value::deserialize(&mut parser)?;
    parser.end()?;
    Ok(value)
}

/// Empty the journal, under the lock; a missing file is already empty.
pub fn clear(path: &Path) -> std::io::Result<()> {
    let file = match OpenOptions::new().write(true).open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    file.lock()?;
    file.set_len(0)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use proptest::prelude::*;
    use serde_json::json;

    use super::*;

    /// A path in the temp directory that no file has yet.
    fn new_path(name: &str) -> PathBuf {
        let nanos =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let file = format!("miroir-journal-{name}-{}-{nanos}.jsonl", std::process::id());
        std::env::temp_dir().join(file)
    }

    #[test]
    fn appended_lines_read_back_in_order() {
        let path = new_path("order");
        let lines = [
            json!({"kind": "crash", "detail": "web exited with status 1"}),
            json!({"kind": "http", "data": {"body": "a\nb", "query": {}}}),
            json!({"kind": "sink", "detail": "\"quoted\" and \u{2028}"}),
        ];
        for line in &lines {
            append(&path, line).unwrap();
        }
        assert_eq!(read(&path).unwrap(), lines);
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 3, "one line each, newlines escaped: {text}");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_missing_file_reads_empty_and_clears() {
        let path = new_path("missing");
        assert_eq!(read(&path).unwrap(), Vec::<Value>::new());
        clear(&path).unwrap();
        assert!(!path.exists(), "clear never creates the journal");
    }

    #[test]
    fn clear_empties_and_appends_start_over() {
        let path = new_path("clear");
        append(&path, &json!({"n": 1})).unwrap();
        append(&path, &json!({"n": 2})).unwrap();
        clear(&path).unwrap();
        assert_eq!(read(&path).unwrap(), Vec::<Value>::new());
        append(&path, &json!({"n": 3})).unwrap();
        assert_eq!(read(&path).unwrap(), [json!({"n": 3})]);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"n\":3}\n");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_line_that_is_not_json_is_an_error_naming_it() {
        let path = new_path("garbage");
        for garbage in ["{\"n\":", "{\"n\":2} 3"] {
            std::fs::write(&path, format!("{{\"n\":1}}\n{garbage}\n")).unwrap();
            let error = read(&path).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidData);
            let message = error.to_string();
            assert!(message.starts_with(&path.display().to_string()), "{message}");
            assert!(message.contains(" line 2 is not JSON: "), "{message}");
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_path_through_a_file_is_an_error_not_an_empty_journal() {
        let file = new_path("file");
        std::fs::write(&file, "").unwrap();
        let inside = file.join("effects.jsonl");
        assert_eq!(read(&inside).unwrap_err().kind(), ErrorKind::NotADirectory);
        assert_eq!(clear(&inside).unwrap_err().kind(), ErrorKind::NotADirectory);
        std::fs::remove_file(file).unwrap();
    }

    #[test]
    fn an_unreadable_path_is_an_error() {
        let dir = std::env::temp_dir();
        assert!(read(&dir).is_err(), "a directory is no journal");
        assert!(append(&dir, &json!({})).is_err());
        assert!(clear(&dir).is_err());
    }

    #[test]
    fn concurrent_appends_keep_whole_lines() {
        let path = new_path("threads");
        let big = "x".repeat(64 * 1024);
        let writers: Vec<_> = (0..2)
            .map(|writer| {
                let (path, big) = (path.clone(), big.clone());
                std::thread::spawn(move || {
                    for n in 0..25 {
                        append(&path, &json!({"writer": writer, "n": n, "pad": big})).unwrap();
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().unwrap();
        }
        let lines = read(&path).unwrap();
        assert_eq!(lines.len(), 50);
        for writer in 0..2 {
            let ns: Vec<&Value> = lines
                .iter()
                .filter(|line| line["writer"] == writer)
                .map(|line| &line["n"])
                .collect();
            let expected: Vec<Value> = (0..25).map(Value::from).collect();
            assert_eq!(ns, expected.iter().collect::<Vec<_>>(), "writer {writer} kept its order");
        }
        assert!(lines.iter().all(|line| line["pad"] == big.as_str()), "no line was split");
        std::fs::remove_file(path).unwrap();
    }

    /// Any JSON value: floats are left out, since JSON text does not round-trip every `f64`.
    fn json_value() -> impl Strategy<Value = Value> {
        let leaf = prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::from),
            any::<i64>().prop_map(Value::from),
            any::<u64>().prop_map(Value::from),
            any::<String>().prop_map(Value::from),
        ];
        leaf.prop_recursive(4, 32, 6, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..6).prop_map(Value::from),
                prop::collection::btree_map(any::<String>(), inner, 0..6)
                    .prop_map(|map| Value::Object(map.into_iter().collect())),
            ]
        })
    }

    proptest! {
        #[test]
        fn any_json_values_round_trip(values in prop::collection::vec(json_value(), 0..8)) {
            let path = new_path("prop");
            for value in &values {
                append(&path, value).unwrap();
            }
            let read_back = read(&path).unwrap();
            let _ = std::fs::remove_file(&path);
            prop_assert_eq!(read_back, values);
        }
    }
}
