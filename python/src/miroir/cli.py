"""The host-side `miroir` CLI: review, calibrate, pool, draft and derive."""

from __future__ import annotations

import argparse
import os
from collections.abc import Sequence
from pathlib import Path

from miroir import calibrate, derive, pool, review

__all__ = ["main"]


def main(argv: Sequence[str] | None = None) -> None:
    """Run the `miroir` CLI."""
    parser = argparse.ArgumentParser(prog="miroir")
    commands = parser.add_subparsers(dest="command", required=True)
    traces = "trace files, or directories of them"
    reviewing = commands.add_parser(
        "review",
        help="label end states, and turn wrong grades into probes",
    )
    reviewing.add_argument("traces", nargs="+", type=Path, help=traces)
    reviewing.add_argument(
        "--tasks",
        type=Path,
        required=True,
        help="the env's tasks directory: labels and probes go in its probes/ folder",
    )
    reviewing.add_argument(
        "--json", action="store_true", help="print the queue as JSON; ask nothing"
    )
    reviewing.add_argument(
        "--label",
        action="append",
        default=[],
        metavar="TASK@SIGNATURE=LABEL",
        help=f"label an end state without asking; LABEL is one of {', '.join(review.LABELS)}",
    )
    calibrating = commands.add_parser(
        "calibrate",
        help="sort tasks into solved, learnable and out of reach",
    )
    calibrating.add_argument("traces", nargs="+", type=Path, help=traces)
    calibrating.add_argument(
        "--model", required=True, help="the model the rollouts came from, which the file records"
    )
    calibrating.add_argument(
        "--out", type=Path, required=True, help="the calibration file to write"
    )
    calibrating.add_argument(
        "-k", type=int, help="k for pass@k and pass^k (default: the most graded rollouts of a task)"
    )
    pooling = commands.add_parser(
        "pool",
        help="keep env containers healthy, and list their URLs in a file for leases",
    )
    pooling.add_argument(
        "--size",
        type=int,
        required=True,
        help="how many containers to keep: the trainer's rollouts in flight, and a few spare",
    )
    pooling.add_argument(
        "--urls", type=Path, required=True, help="the URL file to write, which leases read"
    )
    pooling.add_argument(
        "program",
        nargs="+",
        metavar="COMMAND",
        help="after --, the command that starts (COMMAND start) and stops (COMMAND stop ID) one",
    )
    drafting = commands.add_parser(
        "draft",
        help="write a draft task for each trace `miroir import` scrubbed",
    )
    drafting.add_argument("traces", type=Path, help="the scrubbed traces, traces.jsonl")
    drafting.add_argument("--world", required=True, help="the world the traces' snapshot is")
    drafting.add_argument("--tasks", type=Path, required=True, help="the env's tasks directory")
    drafting.add_argument(
        "--split", default="train", choices=derive.SPLITS, help="the drafts' split (train)"
    )
    deriving = commands.add_parser(
        "derive",
        help="replay each draft the env serves, and write the checks its outcome holds to",
    )
    deriving.add_argument("url", help="the env serving the drafts, such as http://127.0.0.1:8000")
    deriving.add_argument("traces", type=Path, help="the scrubbed traces the drafts came from")
    deriving.add_argument("--world", required=True, help="the world the drafts run in")
    deriving.add_argument("--tasks", type=Path, required=True, help="the env's tasks directory")
    deriving.add_argument(
        "--token-var", help="the variable that holds the env's MIROIR_TOKEN, if it has one"
    )
    args = parser.parse_args(argv)
    if args.command == "draft":
        derive.run_draft(args.traces, args.world, args.tasks, args.split)
    elif args.command == "derive":
        token = None
        if args.token_var:
            token = os.environ.get(args.token_var)
            if token is None:
                raise SystemExit(f"--token-var {args.token_var}: the variable isn't set")
        derive.run_derive(args.url, args.traces, args.world, args.tasks, token)
    elif args.command == "review":
        review.run_review(args.traces, args.tasks / "probes", args.label, as_json=args.json)
    elif args.command == "calibrate":
        calibrate.run_calibrate(args.traces, args.model, args.out, args.k)
    else:
        try:
            pool.run_pool(args.program, args.size, args.urls)
        except pool.PoolError as e:
            raise SystemExit(f"miroir pool: {e}") from e
