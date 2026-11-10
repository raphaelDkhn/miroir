"""`miroir review`: group rollouts by end state, label them, and turn wrong grades into probes."""

from __future__ import annotations

import datetime
import json
import re
import sys
from collections import Counter
from collections.abc import Iterable, Iterator, Sequence
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

__all__ = ["LABELS", "load", "run_review"]

LABELS = ("correct", "should_fail", "should_pass")
CHOICES = {"c": "correct", "f": "should_fail", "p": "should_pass", "s": "skip", "q": "quit"}
LABELS_FILE = "labels.jsonl"
SHOWN = 120
RECORD_KEYS = {"episode_id", "task_id", "reward", "signature", "metadata", "actions"}

Record = dict[str, Any]


@dataclass
class Group:
    """Rollouts of one task that ended in the same state."""

    task_id: str
    signature: str
    example: Record
    rewards: Counter[float] = field(default_factory=Counter)

    @property
    def key(self) -> str:
        return f"{self.task_id}@{self.signature}"

    @property
    def count(self) -> int:
        return sum(self.rewards.values())


def run_review(traces: list[Path], probes: Path, given: list[str], *, as_json: bool) -> None:
    """Label the end states in trace files, by flag or by asking, then print the agreement."""
    groups, faults, void = group(load(traces))
    labels = read_labels(probes / LABELS_FILE)
    by_key = {found.key: found for found in groups}
    for text in given:
        key, label = parse_label(text)
        if key not in by_key:
            raise SystemExit(f"--label {text}: no end state {key} in these traces")
        print(apply(by_key[key], label, probes), file=sys.stderr)
        labels[key] = label
    queue = [found for found in groups if found.key not in labels]
    if as_json:
        print(json.dumps(queue_json(queue, faults, void), indent=2))
        return
    if not given:
        if not sys.stdin.isatty():
            raise SystemExit(
                "stdin is not a terminal: pass --json to print the queue, or --label to label"
            )
        print_faults(faults, void)
        ask(queue, probes, labels)
    print(summary(groups, labels))


def load(paths: Sequence[Path]) -> Iterator[Record]:
    """Yield the episode records in trace files, or directories of them."""
    files: list[Path] = []
    for path in paths:
        if path.is_dir():
            files.extend(sorted(path.glob("*.jsonl")))
        elif path.is_file():
            files.append(path)
        else:
            raise SystemExit(f"{path}: no such file or directory")
    found = False
    for file in files:
        with file.open(encoding="utf-8") as lines:
            for number, line in enumerate(lines, 1):
                if not line.strip():
                    continue
                try:
                    record = json.loads(line)
                except json.JSONDecodeError as e:
                    raise SystemExit(f"{file}:{number}: not JSON: {e}") from e
                if not isinstance(record, dict) or not record.keys() >= RECORD_KEYS:
                    raise SystemExit(f"{file}:{number}: not a Miroir episode record")
                found = True
                yield record
    if not found:
        raise SystemExit(f"no episode records in {', '.join(str(path) for path in paths)}")


Faults = dict[str, dict[str, Any]]


def group(records: Iterable[Record]) -> tuple[list[Group], Faults, int]:
    """Group graded records by end state; count faulted rollouts by task, and void ones."""
    states: dict[tuple[str, str], Group] = {}
    faults: Faults = {}
    void = 0
    for record in records:
        task_id, signature = record["task_id"], record["signature"]
        if record["reward"] is None:
            void += 1
        elif signature is None:
            first = {"rollouts": 0, "fault": record["metadata"].get("fault")}
            faults.setdefault(task_id, first)["rollouts"] += 1
        else:
            state = states.setdefault((task_id, signature), Group(task_id, signature, record))
            state.rewards[record["reward"]] += 1
    groups = sorted(
        states.values(), key=lambda found: (found.task_id, -max(found.rewards), -found.count)
    )
    return groups, dict(sorted(faults.items())), void


def read_labels(path: Path) -> dict[str, str]:
    if not path.exists():
        return {}
    labels = {}
    for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        if not line.strip():
            continue
        try:
            entry = json.loads(line)
            labels[f"{entry['task_id']}@{entry['signature']}"] = entry["label"]
        except (json.JSONDecodeError, KeyError, TypeError) as e:
            raise SystemExit(f"{path}:{number}: not a label: {e!r}") from e
    return labels


def parse_label(text: str) -> tuple[str, str]:
    key, _, label = text.rpartition("=")
    if "@" not in key or label not in LABELS:
        raise SystemExit(
            f"--label {text}: expected TASK@SIGNATURE=LABEL, with LABEL one of {', '.join(LABELS)}"
        )
    return key, label


def ask(queue: list[Group], probes: Path, labels: dict[str, str]) -> None:
    prompt = "label: [c]orrect, should [f]ail, should [p]ass, [s]kip, [q]uit? "
    for number, found in enumerate(queue, 1):
        print(f"\n[{number}/{len(queue)}] {render(found)}")
        try:
            choice = input(prompt).strip().lower()
            while choice not in CHOICES:
                choice = input(f"type one of {', '.join(CHOICES)}: ").strip().lower()
        except EOFError:
            return
        label = CHOICES[choice]
        if label == "quit":
            return
        if label != "skip":
            print(apply(found, label, probes))
            labels[found.key] = label


def apply(found: Group, label: str, probes: Path) -> str:
    """Append a label, and unless it is correct, a probe that pins the right grade."""
    probes.mkdir(parents=True, exist_ok=True)
    done = f"{found.key}: {label}"
    if label != "correct":
        try:
            done += f", probe {write_probe(found, label, probes)}"
        except ValueError as refused:
            done += f", but no probe: {refused}; write one by hand"
    example = found.example
    entry = {
        "task_id": found.task_id,
        "signature": found.signature,
        "label": label,
        "reward": example["reward"],
        "rollouts": found.count,
        "episode_id": example["episode_id"],
        "task_hash": example["metadata"].get("task_hash"),
        "labeled_at": datetime.datetime.now(datetime.UTC).isoformat(timespec="seconds"),
    }
    with (probes / LABELS_FILE).open("a", encoding="utf-8") as file:
        file.write(json.dumps(entry) + "\n")
    return done


def write_probe(found: Group, label: str, probes: Path) -> str:
    """Append a probe that replays the example rollout, bounded as the label says."""
    record = found.example
    if Path(found.task_id).name != found.task_id or found.task_id in {"", ".."}:
        raise ValueError(f"the task id {found.task_id!r} is not a plain file name")
    if record.get("truncated", {}).get("arguments"):
        raise ValueError("its record was cut before every call's and answer's arguments fit")
    name = f"review-{found.signature}"
    path = probes / f"{found.task_id}.yaml"
    existing = path.read_text(encoding="utf-8") if path.exists() else ""
    if f"- name: {name}\n" in existing:
        return name
    bound = f"max_reward: {record['null_max']}" if label == "should_fail" else "min_reward: 1"
    day = datetime.datetime.now(datetime.UTC).date().isoformat()
    why = f"{label.replace('_', ' ')}: {rollouts(found)} earned {rewards(found)}"
    lines = [
        f"# {why}; episode {record['episode_id']}, {day}",
        f"- name: {name}",
        "  steps:",
        *(f"    - {flow(step)}" for step in probe_steps(record)),
        f"  {bound}",
    ]
    gap = "\n" if existing and not existing.endswith("\n") else ""
    with path.open("a", encoding="utf-8") as file:
        file.write(gap + "\n".join(lines) + "\n")
    return name


def probe_steps(record: Record) -> list[dict[str, Any]]:
    """The rollout's own tool calls as probe steps, ending with its submit."""
    actions = record["actions"]
    steps = [
        {"tool": action["tool"], "arguments": action["arguments"]}
        for action in actions
        if action["tool"] != "submit"
        and not _no_tool(action)
        and action.get("via") not in ("run_code", "event")
    ]
    submitted = record["metadata"].get("termination") == "submit"
    ended = submitted and bool(actions) and actions[-1]["tool"] == "submit"
    arguments = actions[-1]["arguments"] if ended else {}
    stored = record["metadata"].get("answer")
    if ended and stored:
        arguments = {"answer": stored}
    steps.append({"tool": "submit", "arguments": arguments})
    return steps


def _no_tool(action: dict[str, Any]) -> bool:
    result = action.get("result")
    return isinstance(result, dict) and result.get("error_type") == "tool_not_found"


def flow(value: Any) -> str:
    """A value as YAML flow text, escaping the characters YAML refuses."""
    text = json.dumps(value, ensure_ascii=False)
    return re.sub(r"[\x7f-\x9f\ufffe\uffff]", lambda match: f"\\u{ord(match.group()):04x}", text)


def rewards(found: Group) -> str:
    counts = sorted(found.rewards.items(), reverse=True)
    if len(counts) == 1:
        return f"{counts[0][0]:g}"
    return ", ".join(f"{reward:g} for {count}" for reward, count in counts)


def rollouts(found: Group) -> str:
    count = found.count
    return f"{count} rollout{'' if count == 1 else 's'}"


def render(found: Group) -> str:
    example = found.example
    lines = [f"{found.key}: {rollouts(found)}, reward {rewards(found)}"]
    if len(found.rewards) > 1:
        lines.append("  the rewards differ: a check reads more than the end state, such as calls")
    state = end_state(example)
    lines += [f"  {line}" for line in state] if state else ["  no changes, no effects"]
    lines.append(f"  checks: {checks(example)}")
    answer = example["metadata"].get("answer")
    lines.append(f"  answer: {cut(json.dumps(answer, ensure_ascii=False)) if answer else 'none'}")
    lines.append(f"  calls in episode {example['episode_id']}:")
    lines += [f"    {n}. {call(action)}" for n, action in enumerate(example["actions"], 1)]
    if example.get("truncated"):
        lines.append(f"  cut at {example['limit_bytes']} bytes: {json.dumps(example['truncated'])}")
    return "\n".join(lines)


def end_state(record: Record) -> list[str]:
    lines = [change(each) for each in record.get("changes", [])]
    for effect in record.get("effects", []):
        data = effect["data"] if isinstance(effect["data"], dict) else {}
        if effect["kind"] == "email":
            to = ", ".join([*data.get("to", []), *data.get("cc", []), *data.get("bcc", [])])
            lines.append(cut(f"email to {to}: {data.get('subject')}"))
        else:
            lines.append(cut(f"{effect['kind']} {data.get('method')} {data.get('path')}"))
    return lines


def change(each: dict[str, Any]) -> str:
    op, key = each["op"], compact(each["pk"])
    table = f"{each['app']}.{each['tbl']}" if each.get("app") else each["tbl"]
    if op == "update":
        before, after = each["before"] or {}, each["after"] or {}
        moved = (
            f"{column} {compact(before.get(column))} → {compact(after.get(column))}"
            for column in each["columns"] or []
        )
        return cut(f"update {table} {key}: {', '.join(moved)}")
    if op == "insert":
        return cut(f"insert {table} {compact(each['after'])}")
    if op == "delete":
        return cut(f"delete {table} {key if each['pk'] else compact(each['before'])}")
    return cut(f"{op} {table} {compact(each['columns'])}")


def checks(record: Record) -> str:
    shown = [
        f"{check['name']} {check['score']:g}{' (gate)' if check.get('gate') else ''}"
        for check in record["metadata"].get("checks") or []
    ]
    return ", ".join(shown) or "none"


def call(action: dict[str, Any]) -> str:
    arguments = action["arguments"] if isinstance(action["arguments"], dict) else {}
    if action.get("via") == "event":
        return cut(f"event {arguments.get('event', '?')} → {action['status']}")
    if action.get("via") == "coworker":
        said = arguments.get("text", "")
        return cut(f"{arguments.get('coworker', '?')} answered: {said} → {action['status']}")
    if action["tool"] != "api_request":
        return cut(f"{action['tool']} {compact(arguments)}")
    body = arguments.get("body")
    sent = "" if body is None else f" {compact(body)}"
    result = action["result"] if isinstance(action["result"], dict) else {}
    outcome = action["status"] or result.get("error_type", "no status")
    app = f"{action['app']}: " if action.get("app") else ""
    made = "(code) " if action.get("via") else ""
    return cut(
        f"{made}{app}{arguments.get('method', '?')} {arguments.get('path', '?')}{sent} → {outcome}"
    )


def compact(value: Any) -> str:
    return json.dumps(value, ensure_ascii=False)


def cut(text: str) -> str:
    return text if len(text) <= SHOWN else text[: SHOWN - 1] + "…"


def print_faults(faults: Faults, void: int) -> None:
    for task_id, broke in faults.items():
        print(
            f"{task_id}: {broke['rollouts']} rollouts broke the env, so no end state; "
            f"first: {broke['fault']}"
        )
    if void:
        print(f"{void} rollouts have no grade, since miroir could not grade them")


def queue_json(queue: list[Group], faults: Faults, void: int) -> dict[str, Any]:
    return {
        "queue": [
            {
                "group": found.key,
                "task_id": found.task_id,
                "signature": found.signature,
                "rollouts": found.count,
                "rewards": {f"{reward:g}": count for reward, count in found.rewards.items()},
                "example": found.example,
            }
            for found in queue
        ],
        "faults": faults,
        "void": void,
    }


def summary(groups: list[Group], labels: dict[str, str]) -> str:
    labeled = Counter(labels[found.key] for found in groups if found.key in labels)
    total = sum(labeled.values())
    if not total:
        return f"{len(groups)} end states, none labeled"
    return (
        f"{total} of {len(groups)} end states labeled: {labeled['correct']} correct, "
        f"{labeled['should_fail']} should fail, {labeled['should_pass']} should pass; "
        f"agreement {labeled['correct'] / total:.0%}"
    )
