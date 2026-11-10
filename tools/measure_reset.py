#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.13"
# dependencies = ["websockets==17.3"]
# ///


"""Time an image's resets: start containers, reset them, print the timings as JSON."""

import argparse
import asyncio
import json
import statistics
import subprocess
import tempfile
import time
import urllib.request
from datetime import datetime, timedelta
from pathlib import Path

import websockets

READY_LIMIT_S = 600
POSTGRES = "command: [postgres, -D, /var/lib/postgresql/data"
POSTGRES_NO_DURABILITY = (
    f"{POSTGRES}, -c, fsync=off, -c, synchronous_commit=off, -c, full_page_writes=off"
)


def arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("image")
    parser.add_argument(
        "task",
        nargs="+",
        help="the tasks the resets pick in turn; tasks of different worlds time world switches",
    )
    parser.add_argument("--containers", type=int, default=1)
    parser.add_argument("--resets", type=int, default=11, help="resets per container")
    parser.add_argument(
        "--age-days", type=int, default=0, help="pretend bake was this long ago"
    )
    parser.add_argument(
        "--fast-postgres", action="store_true", help="run Postgres without durability"
    )
    parser.add_argument(
        "--tmpfs",
        action="append",
        default=[],
        help="a state directory to keep in memory",
    )
    return parser.parse_args()


def docker(*args: str) -> str:
    done = subprocess.run(["docker", *args], check=True, capture_output=True, text=True)
    return done.stdout.strip()


def replaced(text: str, old: str, new: str) -> str:
    if old not in text:
        raise SystemExit(f"the manifest has no {old!r} to replace")
    return text.replace(old, new)


def manifest_for(args: argparse.Namespace) -> str | None:
    if not args.age_days and not args.fast_postgres:
        return None
    read = ("run", "--rm", "--entrypoint", "cat", args.image)
    text = docker(*read, "/miroir/miroir.yaml")
    if args.age_days:
        baked = json.loads(docker(*read, "/miroir/bake.json"))["baked_at"]
        anchor = datetime.fromisoformat(baked) - timedelta(days=args.age_days)
        text = replaced(text, "anchor: bake", f'anchor: "{anchor.isoformat()}"')
    if args.fast_postgres:
        text = replaced(text, POSTGRES, POSTGRES_NO_DURABILITY)
    return text


def create_container(args: argparse.Namespace, manifest: str | None) -> str:
    mounts = [flag for path in args.tmpfs for flag in ("--tmpfs", path)]
    container = docker("create", "-p", "127.0.0.1::8000", *mounts, args.image)
    if manifest is not None:
        with tempfile.TemporaryDirectory() as scratch:
            path = Path(scratch) / "miroir.yaml"
            path.write_text(manifest)
            docker("cp", str(path), f"{container}:/miroir/miroir.yaml")
    return container


def wait_ready(container: str, started: float) -> float:
    port = docker("port", container, "8000/tcp").rsplit(":", 1)[1]
    while time.monotonic() - started < READY_LIMIT_S:
        try:
            with urllib.request.urlopen(f"http://127.0.0.1:{port}/health", timeout=2):
                return time.monotonic() - started
        except OSError:
            time.sleep(0.2)
    raise SystemExit(f"{container} was not ready:\n{docker('logs', container)[-2000:]}")


async def play_resets(container: str, tasks: list[str], count: int) -> list[dict]:
    port = docker("port", container, "8000/tcp").rsplit(":", 1)[1]
    url = f"ws://127.0.0.1:{port}/ws"
    results = []
    async with websockets.connect(url, ping_interval=None, max_size=None) as socket:
        for i in range(count):
            reset = {"type": "reset", "data": {"task_id": tasks[i % len(tasks)]}}
            started = time.monotonic()
            await socket.send(json.dumps(reset))
            reply = json.loads(await socket.recv())
            if reply["type"] != "observation":
                raise SystemExit(f"{container}: the reset failed: {reply}")
            seconds = reply["data"]["observation"]["metadata"]["reset_s"]
            results.append({**seconds, "round_trip": time.monotonic() - started})
    return results


def memory_mib(containers: list[str]) -> float:
    factors = {"GiB": 1024.0, "MiB": 1.0, "KiB": 1 / 1024}
    usages = docker(
        "stats", "--no-stream", "--format", "{{.MemUsage}}", *containers
    ).splitlines()
    sizes = []
    for usage in usages:
        used = usage.split("/")[0].strip()
        sizes.append(float(used[:-3]) * factors[used[-3:]])
    return round(statistics.median(sizes), 1)


def percentile(samples: list[float], pct: int) -> float:
    ordered = sorted(samples)
    return round(ordered[min(len(ordered) - 1, len(ordered) * pct // 100)], 3)


def summary(resets: list[dict], pct: int) -> dict[str, float]:
    steps = {step for reset in resets for step in reset}
    return {
        step: percentile([reset[step] for reset in resets if step in reset], pct)
        for step in sorted(steps)
    }


async def measure(args: argparse.Namespace) -> dict:
    manifest = manifest_for(args)
    containers: list[str] = []
    try:
        for _ in range(args.containers):
            containers.append(create_container(args, manifest))
        started = time.monotonic()
        docker("start", *containers)
        ready = [wait_ready(container, started) for container in containers]
        plays = [
            play_resets(container, args.task, args.resets) for container in containers
        ]
        played = await asyncio.gather(*plays)
        first = [resets[0] for resets in played]
        later = [reset for resets in played for reset in resets[1:]]
        pristine = docker(
            "exec", containers[0], "du", "-sm", "/miroir/pristine"
        ).split()[0]
        return {
            "image": args.image,
            "tasks": args.task,
            "containers": args.containers,
            "resets_per_container": args.resets,
            "age_days": args.age_days,
            "fast_postgres": args.fast_postgres,
            "tmpfs": args.tmpfs,
            "pristine_mb": int(pristine),
            "memory_mib": memory_mib(containers),
            "ready_s": percentile(ready, 50),
            "first_reset_p50_s": summary(first, 50),
            "p50_s": summary(later, 50),
            "p95_s": summary(later, 95),
        }
    finally:
        if containers:
            docker("rm", "-f", *containers)


if __name__ == "__main__":
    print(json.dumps(asyncio.run(measure(arguments())), indent=1))
