#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11,<3.14"
# dependencies = ["miroir", "pyyaml==6.0.3"]
#
# [tool.uv.sources]
# miroir = { path = "../python" }
# ///

"""Soak a pool with golden runs; fail on voids, low rewards or a throughput dip."""

from __future__ import annotations

import argparse
import collections
import concurrent.futures
import multiprocessing
import os
import random
import statistics
import threading
import time
from pathlib import Path
from typing import Any

import yaml
from miroir import MiroirError, Session

VOID_LIMIT = 0.001
WARMUP_MINUTES = 2
DIP = 0.9

Golden = list[tuple[str, dict[str, Any]]]


def goldens(tasks: Path) -> tuple[dict[str, Golden], list[str]]:
    runs, skipped = {}, []
    for path in sorted(tasks.glob("*.yaml")):
        task = yaml.safe_load(path.read_text(encoding="utf-8"))
        coded = any(step["tool"] == "run_code" for step in task["golden"])
        if task.get("answer") == "date" or coded or task.get("coworkers"):
            skipped.append(path.stem)
            continue
        runs[path.stem] = [
            (step["tool"], step.get("arguments", {})) for step in task["golden"]
        ]
    return runs, skipped


def episode(
    urls: Path, token: str | None, runs: dict[str, Golden], think: float
) -> dict[str, Any]:
    task = random.choice(list(runs))
    asked = time.monotonic()
    try:
        session = Session.lease(urls, token, wait_busy=True)
    except MiroirError as e:
        time.sleep(1)
        return {
            "ended": time.time(),
            "task": task,
            "code": e.code,
            "message": e.message,
            "lease_s": time.monotonic() - asked,
        }
    lease_s = time.monotonic() - asked
    try:
        session.reset(task)
        for tool, arguments in runs[task]:
            time.sleep(think)
            session.step(tool, arguments)
        outcome = session.finish()
    except MiroirError as e:
        return {
            "ended": time.time(),
            "task": task,
            "code": e.code,
            "message": e.message,
            "lease_s": lease_s,
        }
    finally:
        session.close()
    reward = outcome["reward"]
    return {
        "ended": time.time(),
        "task": task,
        "code": None if reward is not None else "NO_REWARD",
        "reward": reward,
        "lease_s": lease_s,
        "reset_s": outcome["record"]["timings"]["reset_s"].get("total"),
    }


def drive(
    args: argparse.Namespace, token: str | None, rollouts: int, until: float
) -> list[dict[str, Any]]:
    runs, _ = goldens(Path(args.tasks))
    episodes: list[dict[str, Any]] = []
    lock = threading.Lock()

    def loop() -> None:
        while time.time() < until:
            ended = episode(Path(args.urls), token, runs, args.think)
            with lock:
                episodes.append(ended)

    threads = [threading.Thread(target=loop) for _ in range(rollouts)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
    return episodes


def percentile(values: list[float], share: float) -> float:
    return (
        sorted(values)[min(len(values) - 1, int(len(values) * share))]
        if values
        else float("nan")
    )


def report(episodes: list[dict[str, Any]], began: float, minutes: int) -> bool:
    voids = collections.Counter(
        found["code"] for found in episodes if found["code"] is not None
    )
    graded = [found for found in episodes if found["code"] is None]
    below = sorted({found["task"] for found in graded if found["reward"] < 1.0})
    by_minute = collections.Counter(
        int((found["ended"] - began) // 60) for found in episodes
    )
    counted = [by_minute[minute] for minute in range(WARMUP_MINUTES, minutes)]
    median = statistics.median(counted) if counted else 0
    lowest = min(counted) if counted else 0
    void_share = sum(voids.values()) / len(episodes) if episodes else 1.0
    leases = [found["lease_s"] for found in episodes]
    resets = [found["reset_s"] for found in graded if found.get("reset_s") is not None]
    print(f"episodes: {len(episodes)}, {void_share:.3%} void")
    for code, count in voids.most_common():
        example = next(found for found in episodes if found["code"] == code)
        print(f"  {count} {code}, such as: {example.get('message', 'no reward')}")
    print(f"per minute: {[by_minute[minute] for minute in range(minutes)]}")
    print(f"after {WARMUP_MINUTES} minutes: median {median}, lowest {lowest}")
    print(
        f"lease wait p50 {percentile(leases, 0.5):.2f} s, p95 {percentile(leases, 0.95):.2f} s, "
        f"max {max(leases, default=float('nan')):.2f} s"
    )
    print(
        f"reset p50 {percentile(resets, 0.5):.2f} s, p95 {percentile(resets, 0.95):.2f} s"
    )
    if below:
        print(f"golden runs below reward 1: {', '.join(below)}")
    held = not counted or lowest >= DIP * median
    passed = bool(episodes) and void_share < VOID_LIMIT and not below and held
    print("passed" if passed else "failed")
    return passed


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--urls", required=True, help="the URL file, as `miroir pool` writes it"
    )
    parser.add_argument("--tasks", required=True, help="the env's tasks directory")
    parser.add_argument(
        "--concurrency", type=int, required=True, help="rollouts at once"
    )
    parser.add_argument(
        "--processes", type=int, default=1, help="processes to spread them over"
    )
    parser.add_argument("--minutes", type=int, required=True, help="how long to run")
    parser.add_argument(
        "--think", type=float, default=0.0, help="seconds before each call"
    )
    parser.add_argument(
        "--token-var", help="the variable holding the containers' MIROIR_TOKEN"
    )
    args = parser.parse_args()
    runs, skipped = goldens(Path(args.tasks))
    if not runs:
        raise SystemExit(f"{args.tasks} has no task with a golden run to replay")
    if skipped:
        print(
            f"left out {len(skipped)} tasks whose answer is a date, that run code, or that have "
            f"coworkers: {', '.join(skipped)}"
        )
    token = os.environ[args.token_var] if args.token_var else None
    began = time.time()
    until = began + args.minutes * 60
    each, extra = divmod(args.concurrency, args.processes)
    shares = [each + (i < extra) for i in range(args.processes)]
    context = multiprocessing.get_context("spawn")
    with concurrent.futures.ProcessPoolExecutor(
        args.processes, mp_context=context
    ) as workers:
        driven = [
            workers.submit(drive, args, token, share, until)
            for share in shares
            if share
        ]
        episodes = [found for run in driven for found in run.result()]
    raise SystemExit(0 if report(episodes, began, args.minutes) else 1)


if __name__ == "__main__":
    main()
