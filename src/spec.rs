//! The grading spec, `MIROIR_SPEC`: the checks that score progress, the gates that must hold, and
//! the changes in scope. `grade` scores evidence with it; `check` validates it.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

/// The most bytes a spec may hold.
pub const MAX_BYTES: usize = 32 * 1024;

/// A valid spec.
#[derive(Debug, Clone, PartialEq)]
pub struct Spec {
    /// Progress, at least one; each scores 0 to 1.
    pub checks: Vec<Check>,
    /// What must hold: a gate scoring below 1 is harm.
    pub gates: Vec<Gate>,
    /// The changes in scope: any other change is harm.
    pub allow: Vec<Allow>,
}

/// One SQLite statement scoring progress, weighted relative to the other checks.
#[derive(Debug, Clone, PartialEq)]
pub struct Check {
    pub name: String,
    pub sql: String,
    /// Finite and > 0; 1 when the spec leaves it out.
    pub weight: f64,
}

/// One SQLite statement that must score 1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gate {
    pub name: String,
    pub sql: String,
}

/// The changes of one table and op that are in scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Allow {
    /// A captured table, as `changes.tbl` names it.
    pub table: String,
    pub op: Op,
    /// The columns an update may change; `None` allows any.
    pub columns: Option<Vec<String>>,
    /// The YAML key `where`: a SQLite condition on the row images; `None` allows any row.
    pub condition: Option<String>,
}

/// An op an `allow` entry admits. `truncate` and `schema` changes are never in scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Insert,
    Update,
    Delete,
}

impl Op {
    /// The op as `changes.op` spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            Op::Insert => "insert",
            Op::Update => "update",
            Op::Delete => "delete",
        }
    }
}

/// Parse a spec and validate it. An error names the path of what is wrong, such as
/// `checks[1].weight: must be > 0`.
pub fn parse(text: &str) -> Result<Spec, String> {
    if text.len() > MAX_BYTES {
        return Err(format!("the spec is {} bytes; at most {MAX_BYTES} are allowed", text.len()));
    }
    let mut options = serde_saphyr::Options::default();
    options.with_snippet = false;
    options.strict_booleans = true;
    options.non_finite_float_policy = serde_saphyr::NonFiniteFloatPolicy::PassThrough;
    let root: Value = serde_saphyr::from_str_with_options(text, options).map_err(|e| {
        let message = e.render_with_formatter(&serde_saphyr::UserMessageFormatter);
        format!("the spec is not valid YAML: {}", message.lines().next().unwrap_or_default())
    })?;
    let Value::Object(root) = root else {
        return Err("the spec must be a mapping with checks, gates and allow".into());
    };
    known_keys(&root, "", &["checks", "gates", "allow"])?;
    let checks = list(&root, "checks")?.ok_or("checks: required; list at least one check")?;
    if checks.is_empty() {
        return Err("checks: list at least one check".into());
    }
    let allow =
        list(&root, "allow")?.ok_or("allow: required; use [] for a task that changes nothing")?;
    let gates = list(&root, "gates")?.unwrap_or_default();
    let spec = Spec {
        checks: items(checks, "checks", check)?,
        gates: items(gates, "gates", gate)?,
        allow: items(allow, "allow", allow_entry)?,
    };
    unique_names(&spec)?;
    if !spec.checks.iter().map(|check| check.weight).sum::<f64>().is_finite() {
        return Err("checks: the weights' sum is not a finite number; use smaller weights".into());
    }
    Ok(spec)
}

/// The list at a top-level key; `None` when the key is absent.
fn list<'a>(root: &'a Map<String, Value>, key: &str) -> Result<Option<&'a [Value]>, String> {
    let Some(value) = root.get(key) else { return Ok(None) };
    let Value::Array(items) = value else { return Err(format!("{key}: must be a list")) };
    Ok(Some(items))
}

fn items<T>(
    values: &[Value],
    key: &str,
    item: fn(&Value, &str) -> Result<T, String>,
) -> Result<Vec<T>, String> {
    let mut parsed = Vec::new();
    for (i, value) in values.iter().enumerate() {
        parsed.push(item(value, &format!("{key}[{i}]"))?);
    }
    Ok(parsed)
}

fn check(value: &Value, path: &str) -> Result<Check, String> {
    let fields = mapping(value, path, &["name", "sql", "weight"])?;
    let name = name(fields, path)?;
    let sql = required_text(fields, path, "sql")?;
    let weight = match fields.get("weight") {
        Some(weight) => positive(weight, &format!("{path}.weight"))?,
        None => 1.0,
    };
    Ok(Check { name, sql, weight })
}

fn gate(value: &Value, path: &str) -> Result<Gate, String> {
    let fields = mapping(value, path, &["name", "sql"])?;
    let name = name(fields, path)?;
    let sql = required_text(fields, path, "sql")?;
    Ok(Gate { name, sql })
}

fn allow_entry(value: &Value, path: &str) -> Result<Allow, String> {
    let fields = mapping(value, path, &["table", "op", "columns", "where"])?;
    let table = required_text(fields, path, "table")?;
    let op = op(&required_text(fields, path, "op")?, &format!("{path}.op"))?;
    let columns = match fields.get("columns") {
        Some(_) if op != Op::Update => {
            return Err(format!("{path}.columns: only an update changes columns; remove it"));
        }
        Some(columns) => Some(column_list(columns, &format!("{path}.columns"))?),
        None => None,
    };
    let condition = match fields.get("where") {
        Some(condition) => Some(text(condition, &format!("{path}.where"))?),
        None => None,
    };
    Ok(Allow { table, op, columns, condition })
}

/// The mapping at `path`, whose keys must all be among `keys`.
fn mapping<'a>(
    value: &'a Value,
    path: &str,
    keys: &[&str],
) -> Result<&'a Map<String, Value>, String> {
    let Value::Object(fields) = value else {
        return Err(format!("{path}: must be a mapping with {}", keys.join(", ")));
    };
    known_keys(fields, &format!("{path}."), keys)?;
    Ok(fields)
}

/// Refuse a key outside `keys`, naming it after `prefix`.
fn known_keys(fields: &Map<String, Value>, prefix: &str, keys: &[&str]) -> Result<(), String> {
    match fields.keys().find(|key| !keys.contains(&key.as_str())) {
        Some(key) => Err(format!("{prefix}{key}: unknown key; expected {}", keys.join(", "))),
        None => Ok(()),
    }
}

fn name(fields: &Map<String, Value>, path: &str) -> Result<String, String> {
    let name = required_text(fields, path, "name")?;
    let mut chars = name.chars();
    let first = chars.next().is_some_and(|c| c.is_ascii_lowercase());
    if first && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
        Ok(name)
    } else {
        Err(format!("{path}.name: '{name}' must match [a-z][a-z0-9_]*"))
    }
}

fn required_text(fields: &Map<String, Value>, path: &str, key: &str) -> Result<String, String> {
    let at = format!("{path}.{key}");
    let Some(value) = fields.get(key) else { return Err(format!("{at}: required")) };
    text(value, &at)
}

/// A string that is not blank.
fn text(value: &Value, path: &str) -> Result<String, String> {
    let Value::String(text) = value else { return Err(format!("{path}: must be a string")) };
    if text.trim().is_empty() {
        return Err(format!("{path}: must not be empty"));
    }
    Ok(text.clone())
}

fn positive(value: &Value, path: &str) -> Result<f64, String> {
    let Some(number) = value.as_f64() else {
        return Err(format!("{path}: must be a finite number"));
    };
    if number > 0.0 { Ok(number) } else { Err(format!("{path}: must be > 0")) }
}

fn op(text: &str, path: &str) -> Result<Op, String> {
    let ops = [Op::Insert, Op::Update, Op::Delete];
    match ops.into_iter().find(|op| op.as_str() == text) {
        Some(op) => Ok(op),
        None => Err(format!("{path}: '{text}' is not insert, update or delete")),
    }
}

fn column_list(value: &Value, path: &str) -> Result<Vec<String>, String> {
    let Value::Array(columns) = value else { return Err(format!("{path}: must be a list")) };
    if columns.is_empty() {
        return Err(format!("{path}: must not be empty; leave it out to allow any column"));
    }
    items(columns, path, text)
}

/// Refuse a name used twice across checks and gates.
fn unique_names(spec: &Spec) -> Result<(), String> {
    let checks = spec.checks.iter().enumerate().map(|(i, c)| (format!("checks[{i}]"), &c.name));
    let gates = spec.gates.iter().enumerate().map(|(i, g)| (format!("gates[{i}]"), &g.name));
    let mut seen: BTreeMap<&String, String> = BTreeMap::new();
    for (path, name) in checks.chain(gates) {
        if let Some(first) = seen.get(name) {
            return Err(format!("{path}.name: '{name}' is already the name of {first}"));
        }
        seen.insert(name, path);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn error(yaml: &str) -> String {
        parse(yaml).expect_err(yaml)
    }

    /// A spec whose only check is `check`, with `allow: []`.
    fn with_check(check: &str) -> String {
        format!("checks: [{check}]\nallow: []\n")
    }

    /// A spec with one plain check and `allow` holding `entry`.
    fn with_allow(entry: &str) -> String {
        format!("checks: [{{name: a, sql: SELECT 1}}]\nallow: [{entry}]\n")
    }

    #[test]
    fn a_spec_parses_into_its_parts() {
        let yaml = r#"
checks:
  - name: paid
    weight: 0.6
    sql: |
      SELECT count(*) FROM after.invoices WHERE number = 'INV-04001' AND status = 'paid'
  - {name: charged_once, sql: "SELECT count(*) = 1 FROM sent_requests"}
gates:
  - {name: no_email, sql: SELECT count(*) = 0 FROM sent_emails}
allow:
  - {table: invoices, op: update, columns: [status, paid_at], where: "id = 4001"}
  - {table: notes, op: insert}
  - {table: audit.log, op: delete, where: id < 3}
"#;
        let paid = "SELECT count(*) FROM after.invoices WHERE number = 'INV-04001' AND \
                    status = 'paid'\n";
        let expected = Spec {
            checks: vec![
                Check { name: "paid".into(), sql: paid.into(), weight: 0.6 },
                Check {
                    name: "charged_once".into(),
                    sql: "SELECT count(*) = 1 FROM sent_requests".into(),
                    weight: 1.0,
                },
            ],
            gates: vec![Gate {
                name: "no_email".into(),
                sql: "SELECT count(*) = 0 FROM sent_emails".into(),
            }],
            allow: vec![
                Allow {
                    table: "invoices".into(),
                    op: Op::Update,
                    columns: Some(vec!["status".into(), "paid_at".into()]),
                    condition: Some("id = 4001".into()),
                },
                Allow { table: "notes".into(), op: Op::Insert, columns: None, condition: None },
                Allow {
                    table: "audit.log".into(),
                    op: Op::Delete,
                    columns: None,
                    condition: Some("id < 3".into()),
                },
            ],
        };
        assert_eq!(parse(yaml), Ok(expected));
    }

    #[test]
    fn a_read_only_task_allows_nothing_and_gates_are_optional() {
        let spec = parse("checks: [{name: answer, sql: SELECT number = 3 FROM answer}]\nallow: []")
            .unwrap();
        assert_eq!(spec.checks.len(), 1);
        assert_eq!(spec.checks[0].weight, 1.0);
        assert_eq!(spec.gates, []);
        assert_eq!(spec.allow, []);
        let gates = "checks: [{name: a, sql: SELECT 1}]\ngates: []\nallow: []";
        assert_eq!(parse(gates).unwrap().gates, []);
    }

    #[test]
    fn op_names_are_the_changes_op() {
        let ops = [(Op::Insert, "insert"), (Op::Update, "update"), (Op::Delete, "delete")];
        for (op, name) in ops {
            assert_eq!(op.as_str(), name);
            let spec = parse(&with_allow(&format!("{{table: t, op: {name}}}"))).unwrap();
            assert_eq!(spec.allow[0].op, op);
        }
    }

    #[test]
    fn unknown_keys_are_refused_with_their_path() {
        let cases = [
            (
                "checks: [{name: a, sql: x}]\nallow: []\nsetup: x",
                "setup: unknown key; expected checks, gates, allow",
            ),
            (
                &with_check("{name: a, sql: x, wieght: 2}"),
                "checks[0].wieght: unknown key; expected name, sql, weight",
            ),
            (
                "checks: [{name: a, sql: x}]\ngates: [{name: g, sql: x, weight: 1}]\nallow: []",
                "gates[0].weight: unknown key; expected name, sql",
            ),
            (
                &with_allow("{table: t, op: update, column: [a]}"),
                "allow[0].column: unknown key; expected table, op, columns, where",
            ),
        ];
        for (yaml, expected) in cases {
            assert_eq!(error(yaml), expected);
        }
    }

    #[test]
    fn checks_and_allow_are_required_lists() {
        let cases = [
            ("allow: []", "checks: required; list at least one check"),
            ("checks: []\nallow: []", "checks: list at least one check"),
            ("checks:\nallow: []", "checks: must be a list"),
            ("checks: {name: a, sql: x}\nallow: []", "checks: must be a list"),
            (
                "checks: [{name: a, sql: x}]",
                "allow: required; use [] for a task that changes nothing",
            ),
            ("checks: [{name: a, sql: x}]\nallow:", "allow: must be a list"),
            ("checks: [{name: a, sql: x}]\ngates: x\nallow: []", "gates: must be a list"),
        ];
        for (yaml, expected) in cases {
            assert_eq!(error(yaml), expected);
        }
    }

    #[test]
    fn the_spec_and_its_items_must_be_mappings() {
        let root = "the spec must be a mapping with checks, gates and allow";
        assert_eq!(error(""), root);
        assert_eq!(error("- checks"), root);
        assert_eq!(
            error(&with_check("SELECT 1")),
            "checks[0]: must be a mapping with name, sql, weight"
        );
        let gate = "checks: [{name: a, sql: x}]\ngates: [x]\nallow: []";
        assert_eq!(error(gate), "gates[0]: must be a mapping with name, sql");
        assert_eq!(
            error(&with_allow("[t]")),
            "allow[0]: must be a mapping with table, op, columns, where"
        );
    }

    #[test]
    fn invalid_yaml_is_refused_on_one_line() {
        let error = error("checks: [");
        assert!(error.starts_with("the spec is not valid YAML: "), "{error}");
        assert!(!error.contains('\n'), "{error}");
    }

    /// The message is the user formatter's, with no developer advice about parser options.
    #[test]
    fn a_duplicate_key_is_refused_for_a_user() {
        let error = error("checks: [{name: a, sql: x}]\nallow: []\nallow: []\n");
        assert!(error.starts_with("the spec is not valid YAML: "), "{error}");
        assert!(error.contains("duplicate mapping key: allow"), "{error}");
        assert!(!error.contains("DuplicateKeyPolicy"), "{error}");
        assert!(!error.contains('\n'), "{error}");
    }

    #[test]
    fn weights_whose_sum_overflows_are_refused() {
        let yaml = "checks: [{name: a, sql: x, weight: 1e308}, {name: b, sql: x, weight: 1e308}]\n\
                    allow: []\n";
        assert_eq!(
            error(yaml),
            "checks: the weights' sum is not a finite number; use smaller weights"
        );
    }

    #[test]
    fn names_must_match_the_pattern() {
        for good in ["a", "paid", "a1_b", "x_9_", "charged_once"] {
            let spec = parse(&with_check(&format!("{{name: {good}, sql: x}}"))).unwrap();
            assert_eq!(spec.checks[0].name, good);
        }
        for bad in ["Paid", "aB", "1st", "_x", "a-b", "'a b'", "é"] {
            let shown = bad.trim_matches('\'');
            let expected = format!("checks[0].name: '{shown}' must match [a-z][a-z0-9_]*");
            assert_eq!(error(&with_check(&format!("{{name: {bad}, sql: x}}"))), expected);
        }
        let cases = [
            ("{sql: x}", "checks[0].name: required"),
            ("{name: '', sql: x}", "checks[0].name: must not be empty"),
            ("{name: 7, sql: x}", "checks[0].name: must be a string"),
            ("{name: [a], sql: x}", "checks[0].name: must be a string"),
        ];
        for (check, expected) in cases {
            assert_eq!(error(&with_check(check)), expected);
        }
        let gate = "checks: [{name: a, sql: x}]\ngates: [{name: B, sql: x}]\nallow: []";
        assert_eq!(error(gate), "gates[0].name: 'B' must match [a-z][a-z0-9_]*");
    }

    #[test]
    fn yaml_1_1_booleans_stay_strings() {
        let spec = parse(&with_check("{name: y, sql: x}")).unwrap();
        assert_eq!(spec.checks[0].name, "y");
        let spec = parse(&with_allow("{table: on, op: update, columns: [yes, off]}")).unwrap();
        assert_eq!(spec.allow[0].table, "on");
        assert_eq!(spec.allow[0].columns, Some(vec!["yes".into(), "off".into()]));
    }

    #[test]
    fn names_are_unique_across_checks_and_gates() {
        let cases = [
            (
                "checks: [{name: a, sql: x}, {name: a, sql: y}]\nallow: []",
                "checks[1].name: 'a' is already the name of checks[0]",
            ),
            (
                "checks: [{name: a, sql: x}, {name: b, sql: y}]\n\
                 gates: [{name: b, sql: z}]\nallow: []",
                "gates[0].name: 'b' is already the name of checks[1]",
            ),
            (
                "checks: [{name: a, sql: x}]\n\
                 gates: [{name: g, sql: y}, {name: g, sql: z}]\nallow: []",
                "gates[1].name: 'g' is already the name of gates[0]",
            ),
        ];
        for (yaml, expected) in cases {
            assert_eq!(error(yaml), expected);
        }
    }

    #[test]
    fn sql_must_be_text_that_is_not_empty() {
        let cases = [
            ("{name: a}", "checks[0].sql: required"),
            ("{name: a, sql: ''}", "checks[0].sql: must not be empty"),
            ("{name: a, sql: '  '}", "checks[0].sql: must not be empty"),
            ("{name: a, sql: 1}", "checks[0].sql: must be a string"),
            ("{name: a, sql: }", "checks[0].sql: must be a string"),
        ];
        for (check, expected) in cases {
            assert_eq!(error(&with_check(check)), expected);
        }
        let gate = "checks: [{name: a, sql: x}]\ngates: [{name: g, sql: ''}]\nallow: []";
        assert_eq!(error(gate), "gates[0].sql: must not be empty");
    }

    #[test]
    fn weights_are_finite_and_positive() {
        for (weight, expected) in [("0.6", 0.6), ("2", 2.0), ("1e-9", 1e-9), ("1", 1.0)] {
            let spec =
                parse(&with_check(&format!("{{name: a, sql: x, weight: {weight}}}"))).unwrap();
            assert_eq!(spec.checks[0].weight, expected);
        }
        let cases = [
            ("0", "must be > 0"),
            ("0.0", "must be > 0"),
            ("-0.0", "must be > 0"),
            ("-1", "must be > 0"),
            (".inf", "must be a finite number"),
            ("-.inf", "must be a finite number"),
            (".nan", "must be a finite number"),
            ("1e400", "must be a finite number"),
            ("'0.5'", "must be a finite number"),
            ("true", "must be a finite number"),
            ("~", "must be a finite number"),
        ];
        for (weight, expected) in cases {
            let yaml = with_check(&format!("{{name: a, sql: x, weight: {weight}}}"));
            assert_eq!(error(&yaml), format!("checks[0].weight: {expected}"), "{weight}");
        }
    }

    #[test]
    fn ops_outside_the_three_are_refused() {
        let cases = [
            ("{table: t, op: upsert}", "allow[0].op: 'upsert' is not insert, update or delete"),
            ("{table: t, op: truncate}", "allow[0].op: 'truncate' is not insert, update or delete"),
            ("{table: t, op: schema}", "allow[0].op: 'schema' is not insert, update or delete"),
            ("{table: t, op: Insert}", "allow[0].op: 'Insert' is not insert, update or delete"),
            ("{table: t, op: ''}", "allow[0].op: must not be empty"),
            ("{table: t}", "allow[0].op: required"),
            ("{op: insert}", "allow[0].table: required"),
            ("{table: '', op: insert}", "allow[0].table: must not be empty"),
        ];
        for (entry, expected) in cases {
            assert_eq!(error(&with_allow(entry)), expected);
        }
    }

    #[test]
    fn columns_belong_to_updates_only() {
        let spec = parse(&with_allow("{table: t, op: update, columns: [a]}")).unwrap();
        assert_eq!(spec.allow[0].columns, Some(vec!["a".into()]));
        for op in ["insert", "delete"] {
            let entry = format!("{{table: t, op: {op}, columns: [a]}}");
            let expected = "allow[0].columns: only an update changes columns; remove it";
            assert_eq!(error(&with_allow(&entry)), expected);
        }
    }

    #[test]
    fn columns_and_where_must_not_be_empty() {
        let cases = [
            (
                "{table: t, op: update, columns: []}",
                "allow[0].columns: must not be empty; leave it out to allow any column",
            ),
            ("{table: t, op: update, columns: status}", "allow[0].columns: must be a list"),
            ("{table: t, op: update, columns: [a, '']}", "allow[0].columns[1]: must not be empty"),
            ("{table: t, op: update, columns: [a, 1]}", "allow[0].columns[1]: must be a string"),
            ("{table: t, op: insert, where: ''}", "allow[0].where: must not be empty"),
            ("{table: t, op: insert, where: '   '}", "allow[0].where: must not be empty"),
            ("{table: t, op: insert, where: }", "allow[0].where: must be a string"),
        ];
        for (entry, expected) in cases {
            assert_eq!(error(&with_allow(entry)), expected);
        }
        let second = "checks: [{name: a, sql: x}]\n\
                      allow: [{table: t, op: insert}, {table: t, op: delete, where: ''}]";
        assert_eq!(error(second), "allow[1].where: must not be empty");
    }

    #[test]
    fn specs_over_32_kib_are_refused() {
        let spec = "checks: [{name: a, sql: SELECT 1}]\nallow: []\n#";
        let at_limit = format!("{spec}{}", "x".repeat(MAX_BYTES - spec.len()));
        assert_eq!(at_limit.len(), MAX_BYTES);
        assert!(parse(&at_limit).is_ok());
        let over = format!("{at_limit}x");
        assert_eq!(error(&over), "the spec is 32769 bytes; at most 32768 are allowed");
    }
}
