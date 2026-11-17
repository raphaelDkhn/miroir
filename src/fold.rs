//! Folding the change log into net changes against the boot catalog, and diffing the boot
//! catalog against the live one (§3.6 step 4).

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value};

use crate::evidence::{ChangeOp, NetChange};
use crate::pg::{Ignore, LogRow, Table};

type Row = Map<String, Value>;

/// The net changes of `log` against `tables`, the boot catalog (`catalog.json`); each
/// `NetChange::tbl` is `Table::evidence_name`. Truncates come first, then keyed rows by table
/// and key, then the rows of keyless tables, which count as a multiset: one change per net
/// instance. An error is Miroir's own failure: a log row naming a table the boot catalog lacks,
/// an unknown op, or an image that is not a JSON object.
pub fn fold(log: &[LogRow], tables: &[Table], ignore: &Ignore) -> Result<Vec<NetChange>, String> {
    let keys = primary_keys(tables);
    let mut folding = Folding::default();
    for (n, row) in (1..).zip(log) {
        if ignore.tables.contains(&row.tbl) {
            continue;
        }
        let Some(pk) = keys.get(&row.tbl) else {
            return Err(format!(
                "change log row {n} names table '{}', which the boot catalog does not have: a \
                 table renamed after bake keeps its old name in miroir.log; give it its final \
                 name before capture is installed",
                row.tbl
            ));
        };
        folding
            .apply(row, pk, ignore.columns.get(&row.tbl))
            .map_err(|e| format!("change log row {n} ({} {}): {e}", row.tbl, row.op))?;
    }
    Ok(folding.changes())
}

/// A `schema` change for every table created, dropped or reshaped since boot, or whose capture
/// trigger is gone, and a `truncate` change for every table re-created. Ignored tables and
/// columns never count. A schema change's images map each column to its type.
pub fn schema_changes(boot: &[Table], now: &[Table], ignore: &Ignore) -> Vec<NetChange> {
    let boot = by_name(boot);
    let now = by_name(now);
    let names: BTreeSet<&String> = boot.keys().chain(now.keys()).collect();
    let mut changes = Vec::new();
    for name in names {
        if ignore.tables.contains(name) {
            continue;
        }
        let skip = ignore.columns.get(name);
        let tables = (boot.get(name).copied(), now.get(name).copied());
        changes.extend(schema_change(name, tables, skip));
    }
    changes
}

/// A keyed row as the episode found it and as it left it.
struct Entry {
    pk: Row,
    before: Option<Row>,
    after: Option<Row>,
}

/// The change log folded so far.
#[derive(Default)]
struct Folding {
    /// Keyed rows by table and key.
    entries: BTreeMap<(String, String), Entry>,
    /// The net count of each distinct keyless row, by table and row text.
    counts: BTreeMap<(String, String), (i64, Row)>,
    /// Tables in the order of their first truncate.
    truncated: Vec<String>,
}

impl Folding {
    fn apply(
        &mut self,
        row: &LogRow,
        pk: &[&str],
        skip: Option<&BTreeSet<String>>,
    ) -> Result<(), String> {
        let old = image(row.old.as_deref(), skip)?;
        let new = image(row.new.as_deref(), skip)?;
        match row.op.as_str() {
            "truncate" => self.truncate(&row.tbl),
            "insert" | "update" | "delete" if pk.is_empty() => self.count(&row.tbl, old, new),
            "insert" | "update" | "delete" => self.record(&row.tbl, pk, old, new),
            other => return Err(format!("unknown op '{other}'")),
        }
        Ok(())
    }

    /// Forget the table's changes so far: they are gone with its rows.
    fn truncate(&mut self, tbl: &str) {
        self.entries.retain(|(table, _), _| table != tbl);
        self.counts.retain(|(table, _), _| table != tbl);
        if !self.truncated.iter().any(|table| table == tbl) {
            self.truncated.push(tbl.to_string());
        }
    }

    fn count(&mut self, tbl: &str, old: Option<Row>, new: Option<Row>) {
        for (row, by) in [(old, -1), (new, 1)] {
            let Some(row) = row else { continue };
            let text = serde_json::to_string(&row).unwrap_or_default();
            self.counts.entry((tbl.to_string(), text)).or_insert((0, row)).0 += by;
        }
    }

    /// Fold one change of a keyed row. An update that changes the key deletes one row and
    /// inserts another.
    fn record(&mut self, tbl: &str, pk: &[&str], old: Option<Row>, new: Option<Row>) {
        let rekeyed =
            old.as_ref().zip(new.as_ref()).is_some_and(|(old, new)| key(pk, old) != key(pk, new));
        if rekeyed {
            self.step(tbl, pk, old, None);
            self.step(tbl, pk, None, new);
        } else {
            self.step(tbl, pk, old, new);
        }
    }

    /// Move the row from `old` to `new`, where at least one is set and both share a key.
    fn step(&mut self, tbl: &str, pk: &[&str], old: Option<Row>, new: Option<Row>) {
        let Some(row) = old.as_ref().or(new.as_ref()) else { return };
        let pk = key(pk, row);
        let id = (tbl.to_string(), serde_json::to_string(&pk).unwrap_or_default());
        let entry =
            self.entries.entry(id).or_insert_with(|| Entry { pk, before: old.clone(), after: old });
        entry.after = new;
    }

    fn changes(self) -> Vec<NetChange> {
        let mut changes = Vec::new();
        for tbl in self.truncated {
            changes.push(NetChange {
                tbl,
                op: ChangeOp::Truncate,
                pk: Map::new(),
                before: None,
                after: None,
                columns: None,
            });
        }
        for ((tbl, _), entry) in self.entries {
            changes.extend(net_change(tbl, entry));
        }
        for ((tbl, _), (net, row)) in self.counts {
            let (op, before, after) = match net.cmp(&0) {
                Ordering::Greater => (ChangeOp::Insert, None, Some(row)),
                Ordering::Less => (ChangeOp::Delete, Some(row), None),
                Ordering::Equal => continue,
            };
            let change = NetChange { tbl, op, pk: Map::new(), before, after, columns: None };
            for _ in 0..net.unsigned_abs() {
                changes.push(change.clone());
            }
        }
        changes
    }
}

/// The primary-key columns of each table, by evidence name; empty for a keyless table.
fn primary_keys(tables: &[Table]) -> BTreeMap<String, Vec<&str>> {
    let mut keys = BTreeMap::new();
    for table in tables {
        let pk = table.columns.iter().filter(|c| c.pk).map(|c| c.name.as_str()).collect();
        keys.insert(table.evidence_name(), pk);
    }
    keys
}

/// A row image, without its ignored columns.
fn image(text: Option<&str>, skip: Option<&BTreeSet<String>>) -> Result<Option<Row>, String> {
    let Some(text) = text else { return Ok(None) };
    let mut row: Row =
        serde_json::from_str(text).map_err(|e| format!("a row image is not a JSON object: {e}"))?;
    if let Some(skip) = skip {
        row.retain(|column, _| !skip.contains(column));
    }
    Ok(Some(row))
}

/// The primary key of `row`.
fn key(pk: &[&str], row: &Row) -> Row {
    let mut key = Row::new();
    for column in pk {
        key.insert((*column).to_string(), row.get(*column).cloned().unwrap_or(Value::Null));
    }
    key
}

fn net_change(tbl: String, entry: Entry) -> Option<NetChange> {
    let (op, columns) = match (&entry.before, &entry.after) {
        (None, None) => return None,
        (None, Some(_)) => (ChangeOp::Insert, None),
        (Some(_), None) => (ChangeOp::Delete, None),
        (Some(before), Some(after)) => {
            let changed = changed(before, after);
            if changed.is_empty() {
                return None;
            }
            (ChangeOp::Update, Some(changed))
        }
    };
    Some(NetChange { tbl, op, pk: entry.pk, before: entry.before, after: entry.after, columns })
}

/// The keys whose values differ between two objects, sorted.
fn changed(before: &Row, after: &Row) -> Vec<String> {
    let mut changed = BTreeSet::new();
    for column in before.keys().chain(after.keys()) {
        if before.get(column) != after.get(column) {
            changed.insert(column.clone());
        }
    }
    changed.into_iter().collect()
}

fn by_name(tables: &[Table]) -> BTreeMap<String, &Table> {
    tables.iter().map(|table| (table.evidence_name(), table)).collect()
}

/// The schema change of one table, given as it was at boot and as it is now.
fn schema_change(
    tbl: &str,
    (boot, now): (Option<&Table>, Option<&Table>),
    skip: Option<&BTreeSet<String>>,
) -> Option<NetChange> {
    let change = |op, before, after, columns| NetChange {
        tbl: tbl.to_string(),
        op,
        pk: Map::new(),
        before,
        after,
        columns,
    };
    let (Some(boot), Some(now)) = (boot, now) else {
        let before = boot.map(|table| shape(table, skip));
        let after = now.map(|table| shape(table, skip));
        return Some(change(ChangeOp::Schema, before, after, None));
    };
    let (before, after) = (shape(boot, skip), shape(now, skip));
    let reshaped = changed(&before, &after);
    if !reshaped.is_empty() || (boot.captured && !now.captured) {
        let columns = (!reshaped.is_empty()).then_some(reshaped);
        return Some(change(ChangeOp::Schema, Some(before), Some(after), columns));
    }
    (boot.id != now.id).then(|| change(ChangeOp::Truncate, None, None, None))
}

/// Each column's type, ignored columns left out.
fn shape(table: &Table, skip: Option<&BTreeSet<String>>) -> Row {
    let mut shape = Row::new();
    for column in &table.columns {
        if !skip.is_some_and(|names| names.contains(&column.name)) {
            shape.insert(column.name.clone(), Value::String(column.sql_type.clone()));
        }
    }
    shape
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::pg::{Column, ColumnKind};

    fn table_in(schema: &str, name: &str, pk: &[&str], columns: &[&str]) -> Table {
        let columns = columns
            .iter()
            .map(|c| Column {
                name: (*c).into(),
                kind: ColumnKind::Other,
                generated: false,
                pk: pk.contains(c),
                sql_type: "text".into(),
            })
            .collect();
        Table {
            name: name.into(),
            schema: schema.into(),
            qualified: format!("\"{schema}\".\"{name}\""),
            columns,
            partitioned: false,
            id: 1,
            captured: true,
        }
    }

    fn table(name: &str, pk: &[&str], columns: &[&str]) -> Table {
        table_in("public", name, pk, columns)
    }

    fn named<'a>(tables: &'a mut [Table], name: &str) -> &'a mut Table {
        tables.iter_mut().find(|t| t.name == name).unwrap()
    }

    fn log(tbl: &str, op: &str, old: Option<Value>, new: Option<Value>) -> LogRow {
        LogRow {
            tbl: tbl.into(),
            op: op.into(),
            old: old.map(|v| v.to_string()),
            new: new.map(|v| v.to_string()),
        }
    }

    fn object(value: Value) -> Row {
        let Value::Object(map) = value else { panic!("not a JSON object: {value}") };
        map
    }

    fn ignoring(tables: &[&str], columns: &[(&str, &str)]) -> Ignore {
        let tables = tables.iter().map(|t| (*t).to_string()).collect();
        let mut ignore = Ignore { tables, columns: BTreeMap::new() };
        for (table, column) in columns {
            ignore.columns.entry((*table).to_string()).or_default().insert((*column).to_string());
        }
        ignore
    }

    #[test]
    fn schema_changes_are_the_changes_no_trigger_logs() {
        let t = |name: &str, columns: &[&str]| table(name, &["id"], columns);
        let before = vec![
            t("activities", &["id"]),
            t("api_tokens", &["id", "last_used_at"]),
            t("cache", &["id"]),
            t("deals", &["id", "owner_id"]),
            t("invoices", &["id"]),
            t("meetings", &["id"]),
            t("notes", &["id"]),
            t("users", &["id"]),
        ];
        let mut after = before.clone();
        after.retain(|t| !["activities", "cache"].contains(&t.name.as_str()));
        after.push(t("custom", &["id"]));
        named(&mut after, "api_tokens").columns.pop();
        named(&mut after, "deals").columns[1].sql_type = "bigint".into();
        let invoices = named(&mut after, "invoices");
        (invoices.id, invoices.captured) = (2, false);
        named(&mut after, "meetings").captured = false;
        named(&mut after, "notes").id = 2;
        let ignore = ignoring(&["cache"], &[("api_tokens", "last_used_at")]);
        let changes = schema_changes(&before, &after, &ignore);
        let found: Vec<(&str, ChangeOp, Option<Vec<String>>)> =
            changes.iter().map(|c| (c.tbl.as_str(), c.op, c.columns.clone())).collect();
        assert_eq!(
            found,
            [
                ("activities", ChangeOp::Schema, None),
                ("custom", ChangeOp::Schema, None),
                ("deals", ChangeOp::Schema, Some(vec!["owner_id".to_string()])),
                ("invoices", ChangeOp::Schema, None),
                ("meetings", ChangeOp::Schema, None),
                ("notes", ChangeOp::Truncate, None),
            ],
            "re-created without capture is schema, with it truncate; ignored ones never count"
        );
        let dropped = &changes[0];
        assert_eq!(dropped.before, Some(object(json!({"id": "text"}))));
        assert_eq!(dropped.after, None);
        let created = &changes[1];
        assert_eq!((created.before.is_none(), created.after.is_some()), (true, true));
        assert_eq!(changes[2].before.as_ref().unwrap()["owner_id"], "text");
        assert_eq!(changes[2].after.as_ref().unwrap()["owner_id"], "bigint");
        assert_eq!((changes[5].before.is_none(), changes[5].after.is_none()), (true, true));
        assert!(changes.iter().all(|c| c.pk.is_empty()), "{changes:?}");
        assert_eq!(schema_changes(&before, &before, &ignore), [], "nothing changed");
    }

    #[test]
    fn a_lost_capture_trigger_is_a_schema_change() {
        let boot = vec![table("deals", &["id"], &["id", "owner_id"])];
        let mut now = boot.clone();
        now[0].captured = false;
        let changes = schema_changes(&boot, &now, &Ignore::default());
        assert_eq!(changes.len(), 1, "{changes:?}");
        let change = &changes[0];
        let found = (change.tbl.as_str(), change.op, change.columns.as_ref());
        assert_eq!(found, ("deals", ChangeOp::Schema, None));
        let shape = object(json!({"id": "text", "owner_id": "text"}));
        assert_eq!((change.before.as_ref(), change.after.as_ref()), (Some(&shape), Some(&shape)));
        let none = Ignore::default();
        assert_eq!(schema_changes(&now, &now, &none), [], "never captured, never lost");
        assert_eq!(schema_changes(&now, &boot, &none), [], "a trigger gained is no harm");
        let ignored = ignoring(&["deals"], &[]);
        assert_eq!(schema_changes(&boot, &now, &ignored), [], "an ignored table has no trigger");
    }

    #[test]
    fn a_reshaped_ignored_column_is_no_schema_change() {
        let boot = vec![table("api_tokens", &["id"], &["id", "last_used_at"])];
        let mut now = boot.clone();
        now[0].columns[1].sql_type = "timestamp with time zone".into();
        let ignore = ignoring(&[], &[("api_tokens", "last_used_at")]);
        assert_eq!(schema_changes(&boot, &now, &ignore), []);
        let changes = schema_changes(&boot, &now, &Ignore::default());
        assert_eq!(changes[0].columns, Some(vec!["last_used_at".to_string()]));
    }

    #[test]
    fn tables_outside_public_carry_their_schema() {
        let boot = vec![
            table_in("audit", "events", &["id"], &["id", "kind"]),
            table("events", &[], &["kind"]),
        ];
        let rows = [
            log("audit.events", "insert", None, Some(json!({"id": 7, "kind": "login"}))),
            log("events", "insert", None, Some(json!({"kind": "logout"}))),
        ];
        let changes = fold(&rows, &boot, &Ignore::default()).unwrap();
        let found: Vec<(&str, ChangeOp, &Row)> =
            changes.iter().map(|c| (c.tbl.as_str(), c.op, &c.pk)).collect();
        let (seven, keyless) = (object(json!({"id": 7})), Row::new());
        assert_eq!(
            found,
            [("audit.events", ChangeOp::Insert, &seven), ("events", ChangeOp::Insert, &keyless)],
            "the key comes from the schema's own table"
        );
        let mut now = boot.clone();
        now[0].captured = false;
        let schema = schema_changes(&boot, &now, &Ignore::default());
        assert_eq!(schema.iter().map(|c| c.tbl.as_str()).collect::<Vec<_>>(), ["audit.events"]);
        let ignore = ignoring(&["audit.events"], &[]);
        assert_eq!(schema_changes(&boot, &now, &ignore), []);
        assert_eq!(fold(&rows[..1], &boot, &ignore).unwrap(), []);
    }

    #[test]
    fn changes_fold_to_net() {
        let tables =
            [table("deals", &["id"], &["id", "owner_id", "name"]), table("tags", &[], &["label"])];
        let deal = |owner: i64, name: &str| json!({"id": 1, "owner_id": owner, "name": name});
        let rows = [
            log("deals", "insert", None, Some(json!({"id": 9, "owner_id": 1, "name": "tmp"}))),
            log("deals", "delete", Some(json!({"id": 9, "owner_id": 1, "name": "tmp"})), None),
            log(
                "deals",
                "update",
                Some(json!({"id": 2, "owner_id": 1, "name": "a"})),
                Some(json!({"id": 2, "owner_id": 5, "name": "a"})),
            ),
            log(
                "deals",
                "update",
                Some(json!({"id": 2, "owner_id": 5, "name": "a"})),
                Some(json!({"id": 2, "owner_id": 1, "name": "a"})),
            ),
            log("deals", "update", Some(deal(12, "x")), Some(deal(7, "x"))),
            log("deals", "update", Some(deal(7, "x")), Some(deal(7, "y"))),
            log("deals", "update", Some(deal(7, "y")), Some(deal(7, "x"))),
            log("tags", "insert", None, Some(json!({"label": "hot"}))),
        ];
        let changes = fold(&rows, &tables, &Ignore::default()).unwrap();
        assert_eq!(changes.len(), 2, "{changes:?}");
        assert_eq!((changes[0].tbl.as_str(), changes[0].op), ("deals", ChangeOp::Update));
        assert_eq!(changes[0].pk, object(json!({"id": 1})));
        assert_eq!(changes[0].columns, Some(vec!["owner_id".to_string()]));
        assert_eq!(changes[0].before, Some(object(deal(12, "x"))));
        assert_eq!(changes[0].after, Some(object(deal(7, "x"))));
        assert_eq!((changes[1].tbl.as_str(), changes[1].op), ("tags", ChangeOp::Insert));
        assert!(changes[1].pk.is_empty(), "a table without a key has an empty pk");
        assert_eq!((changes[1].before.as_ref(), changes[1].columns.as_ref()), (None, None));
    }

    #[test]
    fn inserts_and_deletes_carry_one_image_and_their_key() {
        let tables = [table("notes", &["id", "rev"], &["id", "rev", "text"])];
        let note = |id: i64, text: &str| json!({"id": id, "rev": 1, "text": text});
        let rows = [
            log("notes", "insert", None, Some(note(5, "new"))),
            log("notes", "delete", Some(note(3, "old")), None),
        ];
        let changes = fold(&rows, &tables, &Ignore::default()).unwrap();
        let deleted = NetChange {
            tbl: "notes".into(),
            op: ChangeOp::Delete,
            pk: object(json!({"id": 3, "rev": 1})),
            before: Some(object(note(3, "old"))),
            after: None,
            columns: None,
        };
        let inserted = NetChange {
            op: ChangeOp::Insert,
            pk: object(json!({"id": 5, "rev": 1})),
            before: None,
            after: Some(object(note(5, "new"))),
            ..deleted.clone()
        };
        assert_eq!(changes, [deleted, inserted], "ordered by table, then key");
    }

    #[test]
    fn an_update_of_the_key_moves_the_row() {
        let tables = [table("deals", &["id"], &["id", "v"])];
        let (one, two) = (json!({"id": 1, "v": 0}), json!({"id": 2, "v": 0}));
        let rows = [log("deals", "update", Some(one), Some(two))];
        let changes = fold(&rows, &tables, &Ignore::default()).unwrap();
        let found: Vec<(ChangeOp, &Row)> = changes.iter().map(|c| (c.op, &c.pk)).collect();
        let (one, two) = (object(json!({"id": 1})), object(json!({"id": 2})));
        assert_eq!(found, [(ChangeOp::Delete, &one), (ChangeOp::Insert, &two)]);
    }

    #[test]
    fn identical_rows_without_a_key_each_count() {
        let tables = [table("tags", &[], &["label"])];
        let hot = || json!({"label": "hot"});
        let rows = [
            log("tags", "insert", None, Some(hot())),
            log("tags", "insert", None, Some(hot())),
            log("tags", "insert", None, Some(hot())),
            log("tags", "delete", Some(hot()), None),
            log("tags", "update", Some(json!({"label": "old"})), Some(json!({"label": "new"}))),
        ];
        let changes = fold(&rows, &tables, &Ignore::default()).unwrap();
        let ops: Vec<(ChangeOp, Option<&str>)> = changes
            .iter()
            .map(|c| {
                let row = c.after.as_ref().or(c.before.as_ref()).unwrap();
                (c.op, row["label"].as_str())
            })
            .collect();
        assert_eq!(
            ops,
            [
                (ChangeOp::Insert, Some("hot")),
                (ChangeOp::Insert, Some("hot")),
                (ChangeOp::Insert, Some("new")),
                (ChangeOp::Delete, Some("old")),
            ]
        );
        let old = object(json!({"label": "old"}));
        assert_eq!((changes[3].before.as_ref(), changes[3].after.as_ref()), (Some(&old), None));
    }

    #[test]
    fn an_update_of_ignored_columns_leaves_a_table_without_a_key_unchanged() {
        let tables = [table("tags", &[], &["label", "seen"])];
        let tag = |seen: i64| json!({"label": "hot", "seen": seen});
        let ignore = ignoring(&[], &[("tags", "seen")]);
        let touched = [log("tags", "update", Some(tag(1)), Some(tag(2)))];
        assert_eq!(fold(&touched, &tables, &ignore).unwrap(), []);
        let deleted = [
            log("tags", "delete", Some(tag(1)), None),
            log("tags", "update", Some(tag(2)), Some(tag(3))),
        ];
        let changes = fold(&deleted, &tables, &ignore).unwrap();
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert_eq!(changes[0].op, ChangeOp::Delete, "the update does not hide the delete");
        assert_eq!(changes[0].before, Some(object(json!({"label": "hot"}))), "seen is stripped");
    }

    #[derive(Clone, Default)]
    struct Model {
        keyed: BTreeMap<i64, i64>,
        unkeyed: Vec<i64>,
    }

    fn apply(model: &mut Model, log_rows: &mut Vec<LogRow>, (kind, a, b): (u8, i64, i64)) {
        let keyed = |id: i64, v: i64| json!({"id": id, "v": v});
        let tag = |label: i64| json!({"label": label});
        match (kind, model.keyed.get(&a).copied()) {
            (0, None) => {
                model.keyed.insert(a, b);
                log_rows.push(log("deals", "insert", None, Some(keyed(a, b))));
            }
            (1, Some(old)) => {
                model.keyed.insert(a, b);
                log_rows.push(log("deals", "update", Some(keyed(a, old)), Some(keyed(a, b))));
            }
            (2, Some(old)) => {
                model.keyed.remove(&a);
                log_rows.push(log("deals", "delete", Some(keyed(a, old)), None));
            }
            (3, Some(old)) if !model.keyed.contains_key(&b) => {
                model.keyed.remove(&a);
                model.keyed.insert(b, old);
                log_rows.push(log("deals", "update", Some(keyed(a, old)), Some(keyed(b, old))));
            }
            (4, _) => {
                model.unkeyed.push(a);
                log_rows.push(log("tags", "insert", None, Some(tag(a))));
            }
            (5 | 6, _) => {
                if let Some(at) = model.unkeyed.iter().position(|label| *label == a) {
                    model.unkeyed.remove(at);
                    let new = (kind == 6).then(|| tag(b));
                    if kind == 6 {
                        model.unkeyed.push(b);
                    }
                    let op = if kind == 6 { "update" } else { "delete" };
                    log_rows.push(log("tags", op, Some(tag(a)), new));
                }
            }
            _ => {}
        }
    }

    type Expected = Vec<(String, ChangeOp, i64, Option<i64>)>;

    fn net_difference(start: &Model, end: &Model) -> Expected {
        let mut expected: Expected = Vec::new();
        for (id, value) in &start.keyed {
            match end.keyed.get(id) {
                None => expected.push(("deals".into(), ChangeOp::Delete, *id, Some(*value))),
                Some(now) if now != value => {
                    expected.push(("deals".into(), ChangeOp::Update, *id, Some(*now)));
                }
                Some(_) => {}
            }
        }
        for (id, value) in &end.keyed {
            if !start.keyed.contains_key(id) {
                expected.push(("deals".into(), ChangeOp::Insert, *id, Some(*value)));
            }
        }
        let tally = |labels: &[i64]| {
            let mut counts: BTreeMap<i64, i64> = BTreeMap::new();
            for label in labels {
                *counts.entry(*label).or_default() += 1;
            }
            counts
        };
        let (before, after) = (tally(&start.unkeyed), tally(&end.unkeyed));
        let labels: BTreeSet<&i64> = before.keys().chain(after.keys()).collect();
        for label in labels {
            let net = after.get(label).unwrap_or(&0) - before.get(label).unwrap_or(&0);
            let op = if net > 0 { ChangeOp::Insert } else { ChangeOp::Delete };
            expected.extend((0..net.abs()).map(|_| ("tags".into(), op, *label, None)));
        }
        expected.sort_by_key(|row| (row.0.clone(), row.1.as_str(), row.2, row.3));
        expected
    }

    proptest::proptest! {
        #[test]
        fn a_fold_is_the_net_difference(
            operations in proptest::collection::vec((0..7_u8, 0..5_i64, 0..5_i64), 0..80),
        ) {
            let tables = [table("deals", &["id"], &["id", "v"]), table("tags", &[], &["label"])];
            let start = Model { keyed: [(0, 0), (1, 1)].into(), unkeyed: vec![0, 0, 1] };
            let mut end = start.clone();
            let mut rows = Vec::new();
            for operation in operations {
                apply(&mut end, &mut rows, operation);
            }
            let changes = fold(&rows, &tables, &Ignore::default()).unwrap();
            let mut folded: Expected = changes
                .iter()
                .map(|c| {
                    let row = c.after.as_ref().or(c.before.as_ref()).unwrap();
                    match row.get("id") {
                        Some(id) => (c.tbl.clone(), c.op, id.as_i64().unwrap(), row["v"].as_i64()),
                        None => (c.tbl.clone(), c.op, row["label"].as_i64().unwrap(), None),
                    }
                })
                .collect();
            folded.sort_by_key(|row| (row.0.clone(), row.1.as_str(), row.2, row.3));
            proptest::prop_assert_eq!(folded, net_difference(&start, &end));
        }
    }

    #[test]
    fn truncate_and_ignore_fold() {
        let tables = [
            table("notes", &["id"], &["id", "text"]),
            table("api_tokens", &["id"], &["id", "last_used_at"]),
            table("tags", &[], &["label"]),
        ];
        let rows = [
            log("notes", "insert", None, Some(json!({"id": 1, "text": "a"}))),
            log("tags", "insert", None, Some(json!({"label": "hot"}))),
            log("tags", "truncate", None, None),
            log("notes", "truncate", None, None),
            log("notes", "insert", None, Some(json!({"id": 2, "text": "b"}))),
            log("notes", "truncate", None, None),
            log("notes", "insert", None, Some(json!({"id": 3, "text": "c"}))),
            log(
                "api_tokens",
                "update",
                Some(json!({"id": 1, "last_used_at": null})),
                Some(json!({"id": 1, "last_used_at": "now"})),
            ),
        ];
        let ignore = ignoring(&[], &[("api_tokens", "last_used_at")]);
        let changes = fold(&rows, &tables, &ignore).unwrap();
        let ops: Vec<(&str, ChangeOp)> = changes.iter().map(|c| (c.tbl.as_str(), c.op)).collect();
        assert_eq!(
            ops,
            [
                ("tags", ChangeOp::Truncate),
                ("notes", ChangeOp::Truncate),
                ("notes", ChangeOp::Insert)
            ],
            "one truncate per table, in order; only what follows the last one counts"
        );
        assert_eq!(changes[2].after.as_ref().unwrap()["id"], 3);
        let truncate = &changes[0];
        assert!(truncate.pk.is_empty() && truncate.before.is_none() && truncate.after.is_none());
        assert_eq!(truncate.columns, None);
    }

    #[test]
    fn a_truncate_forgets_only_its_own_tables_rows() {
        let tables = [
            table("labels", &[], &["name"]),
            table("notes", &["id"], &["id"]),
            table("tags", &[], &["label"]),
        ];
        let rows = [
            log("labels", "insert", None, Some(json!({"name": "kept"}))),
            log("notes", "insert", None, Some(json!({"id": 1}))),
            log("tags", "insert", None, Some(json!({"label": "gone"}))),
            log("tags", "truncate", None, None),
        ];
        let changes = fold(&rows, &tables, &Ignore::default()).unwrap();
        let ops: Vec<(&str, ChangeOp)> = changes.iter().map(|c| (c.tbl.as_str(), c.op)).collect();
        let expected = [
            ("tags", ChangeOp::Truncate),
            ("notes", ChangeOp::Insert),
            ("labels", ChangeOp::Insert),
        ];
        assert_eq!(ops, expected);
    }

    #[test]
    fn rows_of_ignored_tables_never_count() {
        let rows = [log("cache", "insert", None, Some(json!({"key": "a"})))];
        let ignore = ignoring(&["cache"], &[]);
        assert_eq!(fold(&rows, &[], &ignore).unwrap(), [], "not even an unknown table");
    }

    #[test]
    fn a_log_the_catalog_cannot_explain_is_an_error() {
        let tables = [table("deals", &["id"], &["id"])];
        let unknown = [
            log("deals", "insert", None, Some(json!({"id": 1}))),
            log("old_deals", "insert", None, Some(json!({"id": 1}))),
        ];
        let error = fold(&unknown, &tables, &Ignore::default()).unwrap_err();
        assert!(error.starts_with("change log row 2 names table 'old_deals'"), "{error}");
        let op = [log("deals", "merge", None, Some(json!({"id": 1})))];
        let error = fold(&op, &tables, &Ignore::default()).unwrap_err();
        assert_eq!(error, "change log row 1 (deals merge): unknown op 'merge'");
        let image = [log("deals", "insert", None, Some(json!([1])))];
        let error = fold(&image, &tables, &Ignore::default()).unwrap_err();
        let expected = "change log row 1 (deals insert): a row image is not a JSON object";
        assert!(error.starts_with(expected), "{error}");
        let old = [log("deals", "delete", Some(json!("x")), None)];
        assert!(fold(&old, &tables, &Ignore::default()).is_err(), "the old image is checked too");
    }
}
