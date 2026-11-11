"""Summarize `miroir check --json` reports into the figures README.md states.

for w in orbit ferry kestrel; do
  docker run --rm --network none gitea-env check --world $w --json > check-$w.json
done
python examples/gitea/evidence.py check-orbit.json check-ferry.json check-kestrel.json
"""

import json
import sys
from pathlib import Path
from typing import Any

STAGES = ("golden", "null", "repeat", "reset", "writes", "probes", "hedge", "drift")
# Fingerprinted resets per task: the golden run, the empty submit and two repeats.
FINGERPRINTED = 4
# Episodes every task replays: those four, and the golden run again for the drift stage.
REPLAYED = 5


def summary(reports: list[dict[str, Any]]) -> list[str]:
    """The figures, one per line."""
    tasks = [task for report in reports for task in report["tasks"]]
    lines = [f"tasks: {sum(t['passed'] is True for t in tasks)} of {len(tasks)} pass"]
    for stage in STAGES:
        ran = [t for t in tasks if stage in t]
        lines.append(f"  {stage}: {sum(t[stage]['passed'] is True for t in ran)} of {len(ran)}")
    probes = [run for t in tasks for run in t.get("probes", {}).get("runs", [])]
    zero = sum(run.get("max_reward") == 0 for run in probes)
    partial = sum(run.get("max_reward") not in (None, 0) for run in probes)
    right = sum(run.get("min_reward") is not None for run in probes)
    harm = sum(bool(run["metadata"].get("harm")) for run in probes)
    lines.append(
        f"probes: {sum(run['passed'] is True for run in probes)} of {len(probes)} within bounds; "
        f"{zero} wrong ways held to 0, {partial} partial ones held below full credit, {right} "
        f"other right ways held to 1; {harm} of them harm"
    )
    episodes = sum(REPLAYED + len(t.get("probes", {}).get("runs", [])) for t in tasks)
    episodes += sum("hedge" in t for t in tasks)
    lines.append(f"episodes replayed: {episodes}")
    for report in reports:
        world = report["tasks"][0]["id"].split(".")[1] if report["tasks"] else "?"
        timings = report["timings"]
        lines.append(
            f"{world}: {FINGERPRINTED * len(report['tasks'])} fingerprinted resets; reset p50 "
            f"{timings['reset_p50_s']} s, p95 {timings['reset_p95_s']} s; grading p50 "
            f"{timings['grading_p50_s']} s, max {timings['grading_max_s']} s; idle "
            f"{'pass' if report['stages']['idle']['passed'] else 'FAIL'}"
        )
    return lines


def main() -> None:
    """Print the figures of the reports named on the command line."""
    if len(sys.argv) < 2:
        raise SystemExit("usage: evidence.py CHECK_JSON...")
    reports = [json.loads(Path(path).read_text(encoding="utf-8")) for path in sys.argv[1:]]
    print("\n".join(summary(reports)))


if __name__ == "__main__":
    main()
