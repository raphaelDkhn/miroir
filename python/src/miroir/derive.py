"""`miroir draft` and `miroir derive`: turn scrubbed traces into tasks.

`draft` writes a task per trace; `derive` replays each on the env and writes the checks
its outcome holds to.
"""

from __future__ import annotations

import json
import re
import sys
from collections import Counter
from collections.abc import Mapping, Sequence
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from miroir import MiroirError, Session, tasks
from miroir.review import flow

__all__ = ["Derived", "Refused", "content_hash", "derive", "run_derive", "run_draft"]

DRAFT = "# miroir draft: serve it to the env, then run miroir derive, which replaces this file."
DERIVED = (
    "# miroir derive: checks derived from a replay of the trace below; review them against the "
    "request before you keep them."
)
SPLITS = ("train", "validation", "test")
TRACE_KEYS = {"id", "consent", "request", "calls", "answer", "persona"}
CALL_KEYS = {"tool", "arguments", "status"}
CALL_TOOLS = ("api_request", "api_docs")
PLAIN_NAME = re.compile(r"[a-z0-9][a-z0-9_-]{0,63}")
ISO_DATE = re.compile(r"\d{4}-\d{2}-\d{2}")
EPOCH_SECONDS = range(10**9, 10**10)
EPOCH_MILLISECONDS = range(10**12, 10**13)
STEPS_PER_CALL = 5
MIN_STEPS = 20

Record = dict[str, Any]


class Refused(Exception):
    """A trace that can't become a task; the message says why."""


@dataclass
class Derived:
    """Checks and allowed changes derived from a replay, with notes for the reviewer."""

    checks: list[dict[str, Any]]
    allowed_changes: list[dict[str, Any]]
    notes: list[str] = field(default_factory=list)


def content_hash(data: bytes) -> str:
    """64-bit FNV-1a as 16 hex digits, matching the env's task hash."""
    value = 0xCBF29CE484222325
    for byte in data:
        value = ((value ^ byte) * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return f"{value:016x}"


def run_draft(traces: Path, world: str, tasks_dir: Path, split: str) -> None:
    """Write a draft task for each trace; exits 1 if any was refused."""
    if not PLAIN_NAME.fullmatch(world):
        raise SystemExit(f"--world {world!r}: a world is lowercase letters, digits, '_' and '-'")
    if split not in SPLITS:
        raise SystemExit(f"--split {split!r}: one of {', '.join(SPLITS)}")
    tasks_dir.mkdir(parents=True, exist_ok=True)
    refused = 0
    seen: set[str] = set()
    for line, trace in read_traces(traces):
        task_id = f"{world}-{trace['id']}"
        try:
            if task_id in seen:
                raise Refused("its id is used by an earlier trace")
            seen.add(task_id)
            dated = ISO_DATE.search(
                json.dumps([trace["request"], trace.get("answer"), trace["calls"]])
            )
            if dated:
                raise Refused(
                    f"it holds the date {dated.group()}, and a reset moves the data's dates but "
                    "not a task's text; rephrase it relative to today, or leave it out"
                )
            path = tasks_dir / f"{task_id}.yaml"
            if path.exists() and not is_draft(path):
                raise Refused(f"{path} is a finished task; delete it to draft the trace again")
            path.write_text(render(draft_task(trace, world, split)), encoding="utf-8")
            print(f"drafted {path}", file=sys.stderr)
        except Refused as e:
            refused += 1
            print(f"{traces}:{line}: trace {trace['id']!r} refused: {e}", file=sys.stderr)
    if refused:
        raise SystemExit(1)


def read_traces(path: Path) -> list[tuple[int, dict[str, Any]]]:
    """Read a scrubbed trace file, with each trace's line number."""
    found = []
    for line, text in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        if not text.strip():
            continue
        try:
            trace = json.loads(text)
            check_trace(trace)
        except (ValueError, TypeError) as e:
            raise SystemExit(f"{path}:{line}: not a trace `miroir import` writes: {e}") from e
        found.append((line, trace))
    return found


def check_trace(trace: Any) -> None:
    if not isinstance(trace, dict) or not TRACE_KEYS.issuperset(trace):
        raise ValueError(f"keys must be among {sorted(TRACE_KEYS)}")
    if not isinstance(trace.get("id"), str) or not PLAIN_NAME.fullmatch(trace["id"]):
        raise ValueError("`id` must be lowercase letters, digits, '_' and '-', up to 64")
    for key in ("consent", "request"):
        if not isinstance(trace.get(key), str) or not trace[key].strip():
            raise ValueError(f"`{key}` must be a string with text in it")
    calls = trace.get("calls")
    if not isinstance(calls, list) or not calls:
        raise ValueError("`calls` must list at least one call")
    for call in calls:
        if not isinstance(call, dict) or not CALL_KEYS.issuperset(call):
            raise ValueError(f"each call's keys must be among {sorted(CALL_KEYS)}")
        if call.get("tool") not in CALL_TOOLS or not isinstance(call.get("arguments", {}), dict):
            raise ValueError(f"each call's tool must be {' or '.join(CALL_TOOLS)}, with arguments")


def draft_task(trace: Mapping[str, Any], world: str, split: str) -> dict[str, Any]:
    """A draft that replays the trace's calls, with a placeholder check."""
    golden = [step(call) for call in trace["calls"]]
    answer = trace.get("answer")
    golden.append({"tool": "submit", "arguments": {"answer": answer} if answer else {}})
    return {
        "header": [DRAFT, provenance(trace)],
        "split": split,
        "world": world,
        "persona": trace.get("persona"),
        "instruction": trace["request"],
        "max_steps": max(MIN_STEPS, STEPS_PER_CALL * len(golden)),
        "checks": [{"name": "draft", "weight": 1.0, "db": "episode", "sql": "SELECT 0"}],
        "any_change": True,
        "golden": golden,
    }


def step(call: Mapping[str, Any]) -> dict[str, Any]:
    return {"tool": call["tool"], "arguments": call.get("arguments", {})}


def provenance(trace: Mapping[str, Any]) -> str:
    return f"# Trace {json.dumps(trace['id'])}, consent: {json.dumps(trace['consent'])}"


def is_draft(path: Path) -> bool:
    with path.open(encoding="utf-8") as file:
        return file.readline().rstrip("\n") == DRAFT


def render(task: Mapping[str, Any]) -> str:
    lines = [*task["header"], f"split: {task['split']}", f"world: {task['world']}"]
    if task.get("persona"):
        lines.append(f"persona: {flow(task['persona'])}")
    lines += [f"instruction: {flow(task['instruction'])}", f"max_steps: {task['max_steps']}"]
    lines.append("checks:")
    for check in task["checks"]:
        items = list(check.items())
        lines.append(f"  - {items[0][0]}: {flow(items[0][1])}")
        lines += [f"    {key}: {flow(value)}" for key, value in items[1:]]
    if task.get("any_change"):
        lines.append("any_change: true")
    else:
        rules = task["allowed_changes"]
        lines.append("allowed_changes: []" if not rules else "allowed_changes:")
        lines += [f"  - {flow(rule)}" for rule in rules]
    lines.append("golden:")
    lines += [f"  - {flow(golden_step)}" for golden_step in task["golden"]]
    return "\n".join(lines) + "\n"


def run_derive(url: str, traces: Path, world: str, tasks_dir: Path, token: str | None) -> None:
    """Replace each draft the env serves with the task its replays derive; exits 1 on
    refusals.
    """
    listed = {task["id"]: task for split in SPLITS for task in tasks(url, split, token)}
    session = Session.lease([url], token, wait_busy=False)
    refused = 0
    try:
        for line, trace in read_traces(traces):
            task_id = f"{world}-{trace['id']}"
            path = tasks_dir / f"{task_id}.yaml"
            try:
                served = served_draft(path, listed.get(task_id))
                golden, derived = replay(session, task_id, trace)
                path.write_text(render(final_task(trace, served, golden, derived)), "utf-8")
                count = len(derived.checks) - 1
                plural = "" if count == 1 else "s"
                print(f"derived {path}: {count} check{plural} and a gate", file=sys.stderr)
                for note in derived.notes:
                    print(f"  note: {note}", file=sys.stderr)
            except Refused as e:
                refused += 1
                print(f"{traces}:{line}: {task_id} refused: {e}", file=sys.stderr)
    finally:
        session.close()
    if refused:
        raise SystemExit(1)


def served_draft(path: Path, listed: Mapping[str, Any] | None) -> Mapping[str, Any]:
    if not path.exists() or not is_draft(path):
        raise Refused(f"{path} isn't a draft; run miroir draft first")
    if listed is None:
        raise Refused(
            "the env doesn't serve its draft: rebuild the image with it, or start the container "
            'with -v "$PWD/tasks:/miroir/tasks:ro"'
        )
    if listed["task_hash"] != content_hash(path.read_bytes()):
        raise Refused("the env serves another version of its draft: restart it on this one")
    return listed


def replay(
    session: Session, task_id: str, trace: Mapping[str, Any]
) -> tuple[list[dict[str, Any]], Derived]:
    """Replay a trace's calls, keep those that work, replay those again, and derive checks."""
    calls = [step(call) for call in trace["calls"]]
    first, statuses = run(session, task_id, calls)
    golden = []
    notes = []
    for i, (call, ok) in enumerate(zip(calls, statuses, strict=True), 1):
        produced = trace["calls"][i - 1].get("status")
        if produced is not None and (produced < 400) != ok:
            raise Refused(
                f"call {i} answered {produced} in production but {'works' if ok else 'fails'} on "
                "the snapshot, which must then predate or postdate what the request needed"
            )
        if ok:
            golden.append(call)
        else:
            notes.append(f"call {i} ({describe(call)}) failed on the snapshot; left out")
    second, again = run(session, task_id, golden)
    if not all(again):
        raise Refused("a call that worked once failed when replayed again")
    derived = derive(trace["request"], first, second)
    notes += [
        f"the calls send {value!r}, which the request never shows: an agent can't know it; was "
        "it scrubbed apart from the request, or never asked for?"
        for value in unseen(trace["request"], golden)
    ]
    derived.notes[:0] = notes
    answer = trace.get("answer")
    golden.append({"tool": "submit", "arguments": {"answer": answer} if answer else {}})
    return golden, derived


def run(
    session: Session, task_id: str, calls: Sequence[Mapping[str, Any]]
) -> tuple[Record, list[bool]]:
    session.reset(task_id)
    worked = []
    for call in calls:
        try:
            observation = session.step(call["tool"], call["arguments"])["observation"]
        except MiroirError as e:
            raise Refused(f"the env refused a call: {e}") from e
        worked.append(call_worked(call["tool"], observation))
    return session.finish()["record"], worked


def unseen(instruction: str, calls: Sequence[Mapping[str, Any]]) -> list[str]:
    found: list[str] = []

    def walk(value: Any) -> None:
        if isinstance(value, str) and normal(value) not in normal(instruction):
            found.append(value)
        elif isinstance(value, dict):
            for item in value.values():
                walk(item)
        elif isinstance(value, list):
            for item in value:
                walk(item)

    for call in calls:
        walk(call["arguments"].get("body"))
    return sorted(set(found))


def call_worked(tool: str, observation: Mapping[str, Any]) -> bool:
    if tool == "api_request":
        status = (observation.get("result") or {}).get("status")
        return isinstance(status, int) and status < 400
    return observation.get("error") is None


def describe(call: Mapping[str, Any]) -> str:
    arguments = call["arguments"]
    if call["tool"] == "api_request":
        return f"{arguments.get('method', '?')} {arguments.get('path', '?')}"
    return call["tool"]


def final_task(
    trace: Mapping[str, Any],
    served: Mapping[str, Any],
    golden: list[dict[str, Any]],
    derived: Derived,
) -> dict[str, Any]:
    persona = (served.get("persona") or {}).get("name")
    return {
        "header": [DERIVED, provenance(trace)],
        "split": served["split"],
        "world": served["world"],
        "persona": persona if persona and trace.get("persona") else None,
        "instruction": trace["request"],
        "max_steps": served["max_steps"],
        "checks": derived.checks,
        "allowed_changes": derived.allowed_changes,
        "golden": golden,
    }


def derive(instruction: str, first: Record, second: Record) -> Derived:
    """Checks on what two replays agree on, and a gate against any other change or effect."""
    refuse_unusable(second)
    changes, effects = second["changes"], second["effects"]
    if shape(first["changes"]) != shape(changes) or kinds(first["effects"]) != kinds(effects):
        raise Refused("the two replays changed different rows or sent different effects")
    if not changes and not effects:
        raise Refused(
            "nothing changed and nothing was sent: a question, whose answer check you write by hand"
        )
    inserted = inserted_keys(changes)
    predicates: list[tuple[str, str, str]] = []
    for before, after in zip(first["changes"], changes, strict=True):
        found = change_predicate(after, before, instruction, inserted)
        if found:
            predicates.append(found)
    for before, after in zip(first["effects"], effects, strict=True):
        predicates.append(effect_predicate(after, before, instruction))
    if not predicates:
        raise Refused("every change is a time or a value another path would draw: nothing to check")
    checks = weighted(predicates)
    checks.append(
        {"name": "only_these_changes", "gate": True, "db": "episode", "sql": gate(changes, effects)}
    )
    return Derived(checks, allowed(changes))


def refuse_unusable(record: Record) -> None:
    metadata = record.get("metadata") or {}
    if record.get("reward") is None or metadata.get("fault"):
        reason = metadata.get("fault") or metadata.get("invalid") or "no reward"
        raise Refused(f"the replay broke the env or couldn't be graded: {reason}")
    truncated = record.get("truncated") or {}
    if truncated.get("changes") or truncated.get("effects"):
        raise Refused("the replay's record was cut before every change and effect fit")
    for change in record["changes"]:
        if change["op"] in ("schema", "truncate"):
            raise Refused(f"the replay made a {change['op']} change to {change['tbl']}")
        if change["op"] in ("update", "delete") and not change["pk"]:
            raise Refused(f"{change['tbl']} has no primary key, so its rows can't be told apart")
        if change["op"] in ("update", "delete") and any(map(timelike, change["pk"].values())):
            raise Refused(f"{change['tbl']} is keyed by a time, which every reset moves")


def shape(changes: Sequence[Mapping[str, Any]]) -> list[tuple[Any, ...]]:
    return [
        (
            c.get("app"),
            c["tbl"],
            c["op"],
            json.dumps(c["pk"], sort_keys=True) if c["op"] != "insert" else None,
        )
        for c in changes
    ]


def kinds(effects: Sequence[Mapping[str, Any]]) -> list[str]:
    return [effect["kind"] for effect in effects]


def inserted_keys(changes: Sequence[Mapping[str, Any]]) -> set[int]:
    return {
        value
        for c in changes
        if c["op"] == "insert" and len(c["pk"]) == 1
        for value in c["pk"].values()
        if isinstance(value, int) and not isinstance(value, bool)
    }


def timelike(value: Any) -> bool:
    if isinstance(value, bool):
        return False
    if isinstance(value, int):
        return value in EPOCH_SECONDS or value in EPOCH_MILLISECONDS
    return isinstance(value, str) and bool(ISO_DATE.search(value))


def checkable(value: Any, other: Any, instruction: str, inserted: set[int]) -> bool:
    if value != other or isinstance(value, dict | list) or timelike(value):
        return False
    if isinstance(value, int) and not isinstance(value, bool) and value in inserted:
        return False
    if isinstance(value, str) and " " in value.strip():
        return normal(value) in normal(instruction)
    return True


def normal(text: str) -> str:
    return " ".join(text.lower().split())


def change_predicate(
    change: Mapping[str, Any], other: Mapping[str, Any], instruction: str, inserted: set[int]
) -> tuple[str, str, str] | None:
    table, op = change["tbl"], change["op"]
    where = [f"tbl = {literal(table)}", f"op = {literal(op)}"]
    if change.get("app"):
        where.insert(0, f"app = {literal(change['app'])}")
    if op in ("update", "delete"):
        where += [equals("pk", key, value) for key, value in change["pk"].items()]
    if op == "delete":
        return name("delete", table, change["pk"]), "changes", " AND ".join(where)
    image, prior = change["after"], other["after"] or {}
    if op == "update":
        columns = change["columns"] or []
        if all(timelike(image.get(column)) for column in columns):
            return None
    else:
        single = len(change["pk"]) == 1 and all(isinstance(v, int) for v in change["pk"].values())
        columns = [c for c in image if not (single and c in change["pk"])]
    for column in columns:
        if column in image and checkable(image[column], prior.get(column), instruction, inserted):
            where.append(equals("after", column, image[column]))
    label = name(op, table, change["pk"] if op == "update" else {})
    return label, "changes", " AND ".join(where)


def effect_predicate(
    effect: Mapping[str, Any], other: Mapping[str, Any], instruction: str
) -> tuple[str, str, str]:
    data, prior = effect["data"], other["data"]
    where = [f"kind = {literal(effect['kind'])}"]
    if effect["kind"] == "email":
        for key in ("to", "cc", "bcc"):
            recipients = data.get(key)
            if isinstance(recipients, list) and recipients == prior.get(key):
                where.append(f"json_array_length(data, {literal(path(key))}) = {len(recipients)}")
                where += [
                    equals("data", f"{key}[{i}]", r, raw=True) for i, r in enumerate(recipients)
                ]
        if checkable(data.get("subject"), prior.get("subject"), instruction, set()):
            where.append(equals("data", "subject", data["subject"]))
    else:
        for key in ("method", "path"):
            if data.get(key) == prior.get(key) and isinstance(data.get(key), str):
                where.append(equals("data", key, data[key]))
        body, prior_body = data.get("body"), prior.get("body")
        if isinstance(body, dict) and isinstance(prior_body, dict):
            for key, value in body.items():
                if checkable(value, prior_body.get(key), instruction, set()):
                    where.append(equals("data", f"body.{key}", value, raw=True))
    return effect["kind"], "effects", " AND ".join(where)


def equals(column: str, key: str, value: Any, raw: bool = False) -> str:
    at = literal(f"$.{key}" if raw else path(key))
    if value is None:
        return f"json_type({column}, {at}) = 'null'"
    return f"json_extract({column}, {at}) = {literal(value)}"


def path(key: str) -> str:
    return f"$.{key}" if re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", key) else f'$."{key}"'


def literal(value: Any) -> str:
    if isinstance(value, bool):
        return "1" if value else "0"
    if isinstance(value, int | float):
        return repr(value)
    return "'" + str(value).replace("'", "''") + "'"


def name(op: str, table: str, pk: Mapping[str, Any]) -> str:
    parts = [op, table, *(str(value) for value in pk.values())]
    return re.sub(r"[^a-z0-9_]+", "_", "_".join(parts).lower()).strip("_")


def weighted(predicates: Sequence[tuple[str, str, str]]) -> list[dict[str, Any]]:
    counts = Counter(predicates)
    names: Counter[str] = Counter()
    checks = []
    for (label, table, where), count in counts.items():
        names[label] += 1
        unique = label if names[label] == 1 else f"{label}_{names[label]}"
        sql = f"SELECT count(*) = {count} FROM {table} WHERE {where}"
        checks.append({"name": unique, "weight": 0.0, "db": "episode", "sql": sql})
    for check in checks:
        check["weight"] = 1 / len(checks)
    return checks


def gate(changes: Sequence[Mapping[str, Any]], effects: Sequence[Mapping[str, Any]]) -> str:
    """SQL that holds when no row or effect changed beyond the replay's."""
    rows = []
    for c in changes:
        if c["op"] in ("update", "delete"):
            keys = [f"tbl = {literal(c['tbl'])}"] + [equals("pk", k, v) for k, v in c["pk"].items()]
            if c.get("app"):
                keys.insert(0, f"app = {literal(c['app'])}")
            rows.append(f"({' AND '.join(keys)})")
    outside = f"NOT ({' OR '.join(rows)})" if rows else "1"
    parts = [f"(SELECT count(*) FROM changes WHERE op IN ('update', 'delete') AND {outside}) = 0"]
    inserts = Counter((c.get("app"), c["tbl"]) for c in changes if c["op"] == "insert")
    for (app, table), count in inserts.items():
        scope = f"app = {literal(app)} AND " if app else ""
        parts.append(
            f"(SELECT count(*) FROM changes WHERE {scope}tbl = {literal(table)} AND op = 'insert') "
            f"<= {count}"
        )
    sent = Counter(e["kind"] for e in effects)
    for kind in ("email", "http"):
        parts.append(f"(SELECT count(*) FROM effects WHERE kind = '{kind}') <= {sent[kind]}")
    return "SELECT " + "\n   AND ".join(parts)


def allowed(changes: Sequence[Mapping[str, Any]]) -> list[dict[str, Any]]:
    """The `allowed_changes` that cover the replay's changes."""
    rules: dict[tuple[Any, str, str], set[str]] = {}
    for c in changes:
        rules.setdefault((c.get("app"), c["tbl"], c["op"]), set()).update(c.get("columns") or [])
    found = []
    for (app, table, op), columns in rules.items():
        rule: dict[str, Any] = {"app": app} if app else {}
        rule |= {"table": table, "op": op}
        if op == "update":
            rule["columns"] = sorted(columns)
        found.append(rule)
    return found
