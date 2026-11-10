"""`miroir calibrate`: class each task as solved, learnable or out of reach from rollouts."""

from __future__ import annotations

import json
import math
import os
import statistics
from collections import Counter
from collections.abc import Iterable, Mapping, Sequence
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from miroir.review import load

__all__ = ["CLASSES", "calibrated", "run_calibrate"]

CLASSES = ("solved", "learnable", "out_of_reach")
NAMED = 5

Entry = dict[str, Any]


@dataclass
class Tally:
    """A task's graded rollouts."""

    task_hash: str
    rewards: list[float] = field(default_factory=list)
    successes: int = 0
    harmed: int = 0
    void: int = 0


def run_calibrate(traces: list[Path], model: str, out: Path, k: int | None) -> None:
    """Write the calibration of `model` from trace files, and print its table."""
    if k is not None and k < 1:
        raise SystemExit(f"-k {k}: k must be at least 1")
    tallies = tally(load(traces))
    kinds = {task_id: classify(found) for task_id, found in tallies.items()}
    if all(kind is None for kind in kinds.values()):
        raise SystemExit(
            "no task has two graded rollouts, so none has a class: run more rollouts of each "
            "task, such as eight"
        )
    k = k or max(len(found.rewards) for found in tallies.values())
    entries = {task_id: entry(tallies[task_id], kinds[task_id], k) for task_id in sorted(tallies)}
    out.parent.mkdir(parents=True, exist_ok=True)
    calibration = {"model": model, "k": k, "tasks": entries}
    out.write_text(json.dumps(calibration, indent=2) + "\n", encoding="utf-8")
    print(table(entries, k))
    print(summary(entries, model, k, out))


def tally(records: Iterable[dict[str, Any]]) -> dict[str, Tally]:
    """Tally records by task; refuses rollouts of two versions of a task."""
    tallies: dict[str, Tally] = {}
    for record in records:
        task_id, task_hash = record["task_id"], record["metadata"]["task_hash"]
        found = tallies.setdefault(task_id, Tally(task_hash))
        if found.task_hash != task_hash:
            raise SystemExit(
                f"{task_id}: the traces hold rollouts of two versions of the task (task_hash "
                f"{found.task_hash} and {task_hash}); calibrate rollouts of one version"
            )
        if record["reward"] is None:
            found.void += 1
            continue
        found.rewards.append(record["reward"])
        found.successes += bool(record["metadata"].get("success"))
        found.harmed += bool(record["metadata"].get("harm"))
    return tallies


def classify(found: Tally) -> str | None:
    """Learnable when rewards vary, else solved or out of reach; None under 2 rollouts."""
    rewards = found.rewards
    if len(rewards) < 2:
        return None
    if max(rewards) > min(rewards):
        return "learnable"
    return "solved" if found.successes == len(rewards) else "out_of_reach"


def entry(found: Tally, kind: str | None, k: int) -> Entry:
    n, c = len(found.rewards), found.successes
    return {
        "task_hash": found.task_hash,
        "class": kind,
        "rollouts": n,
        "void": found.void,
        "pass@1": c / n if n else None,
        "pass@k": pass_at(n, c, k),
        "pass^k": pass_hat(n, c, k),
        "reward_mean": statistics.fmean(found.rewards) if n else None,
        "reward_variance": statistics.pvariance(found.rewards) if n else None,
        "harm_rate": found.harmed / n if n else None,
    }


def pass_at(n: int, c: int, k: int) -> float | None:
    """pass@k, the chance at least one of k rollouts succeeds; None when n < k."""
    if n < k:
        return None
    return 1 - math.comb(n - c, k) / math.comb(n, k)


def pass_hat(n: int, c: int, k: int) -> float | None:
    """pass^k, the chance all k rollouts succeed; None when n < k."""
    if n < k:
        return None
    return math.comb(c, k) / math.comb(n, k)


def table(entries: Mapping[str, Entry], k: int) -> str:
    stats = [
        ("pass@1", "pass@1"),
        (f"pass@{k}", "pass@k"),
        (f"pass^{k}", "pass^k"),
        ("reward", "reward_mean"),
        ("var", "reward_variance"),
        ("harm", "harm_rate"),
    ]
    width = max(len(task_id) for task_id in [*entries, "task"])
    heads = [f"{'task':<{width}}", f"{'n':>3}", f"{'void':>4}", *(f"{t:>7}" for t, _ in stats)]
    lines = ["  ".join([*heads, "class"])]
    for task_id, found in entries.items():
        rates = (f"{rate(found[key]):>7}" for _, key in stats)
        counts = [f"{task_id:<{width}}", f"{found['rollouts']:>3}", f"{found['void']:>4}"]
        lines.append("  ".join([*counts, *rates, found["class"] or "-"]))
    return "\n".join(lines)


def rate(value: float | None) -> str:
    return "-" if value is None else f"{value:.2f}"


def summary(entries: Mapping[str, Entry], model: str, k: int, out: Path) -> str:
    counts = Counter(found["class"] for found in entries.values())
    tasks = f"{len(entries)} task{'' if len(entries) == 1 else 's'}"
    classes = f"{counts['solved']} solved, {counts['learnable']} learnable"
    lines = [f"{tasks}: {classes}, {counts['out_of_reach']} out of reach"]
    unclassed = [task_id for task_id, found in entries.items() if found["class"] is None]
    if unclassed:
        lines.append(f"no class, with fewer than 2 graded rollouts: {named(unclassed)}")
    void = sum(found["void"] for found in entries.values())
    if void:
        total = void + sum(found["rollouts"] for found in entries.values())
        lines.append(f"{void} of {total} rollouts ({void / total:.1%}) void: miroir couldn't grade")
    lines.append(f"wrote the calibration of {model} at k={k} to {out}")
    return "\n".join(lines)


def calibrated(
    tasks: Sequence[dict[str, Any]],
    path: str | os.PathLike[str],
    keep: Sequence[str] = ("learnable",),
) -> list[dict[str, Any]]:
    """The tasks the calibration file classes as one of `keep`; refuses missing or stale
    entries.
    """
    if not keep or set(keep) - set(CLASSES):
        raise ValueError(f"keep must name classes among {list(CLASSES)}, not {list(keep)}")
    entries = read_calibration(path)
    missing = [task["id"] for task in tasks if task["id"] not in entries]
    changed = [
        task["id"]
        for task in tasks
        if task["id"] in entries and entries[task["id"]]["task_hash"] != task["task_hash"]
    ]
    problems = []
    if missing:
        problems.append(f"no entry for {named(missing)}")
    if changed:
        problems.append(f"{named(changed)} changed since it was written")
    if problems:
        raise ValueError(
            f"{path}: {'; '.join(problems)}. Calibrate these tasks again: run rollouts of the "
            "split, then `miroir calibrate` on their traces"
        )
    kept = [task for task in tasks if entries[task["id"]]["class"] in keep]
    if not kept:
        counts = Counter(entries[task["id"]]["class"] or "without a class" for task in tasks)
        shown = ", ".join(f"{count} {kind}" for kind, count in sorted(counts.items()))
        raise ValueError(
            f"none of the {len(tasks)} tasks is {' or '.join(keep)} in {path}: {shown}"
        )
    return kept


def read_calibration(path: str | os.PathLike[str]) -> dict[str, Entry]:
    try:
        text = Path(path).read_text(encoding="utf-8")
    except OSError as e:
        raise ValueError(f"cannot read the calibration {path}: {e}") from e
    not_one = f"{path} is not a calibration that `miroir calibrate` wrote"
    try:
        entries = json.loads(text)["tasks"]
    except (json.JSONDecodeError, KeyError, TypeError) as e:
        raise ValueError(f"{not_one}: {e!r}") from e
    if not isinstance(entries, dict) or not all(
        isinstance(found, dict) and {"task_hash", "class"} <= found.keys()
        for found in entries.values()
    ):
        raise ValueError(f"{not_one}: each task needs a task_hash and a class")
    return entries


def named(task_ids: Sequence[str]) -> str:
    shown = ", ".join(task_ids[:NAMED])
    return shown if len(task_ids) <= NAMED else f"{shown} and {len(task_ids) - NAMED} more"
