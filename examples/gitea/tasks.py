# /// script
# requires-python = ">=3.12"
# dependencies = ["pyyaml==6.0.3"]
# ///
"""Write the Gitea worlds' tasks as Harbor task directories, one directory per split.

    uv run examples/gitea/tasks.py --out examples/gitea/datasets \\
      --world orbit=gitea-env:orbit --world ferry=gitea-env:ferry \\
      --world kestrel=gitea-env:kestrel --main miroir-main:dev --grader miroir:dev

For each world it reads the ids Gitea gave the world's rows from the world image
(/opt/gitea-env/ids.json), plants the world again here, and stops unless its digest matches the
image's. seed/families.py then writes the world's tasks, and this renders each one as a Harbor
task directory: instruction.md, task.toml with the grading spec, the compose file and the
scripts, plus a dataset.toml per split. The same inputs give the same bytes. The output
directory is replaced whole on every run, so edit families.py and regenerate instead.
"""

import argparse
import json
import platform
import shlex
import shutil
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import yaml

sys.path.insert(0, str(Path(__file__).resolve().parent / "seed"))

import families
import plants
import worlds

# One version for every task and dataset. Bump the major when instructions or worlds change
# (Harbor reruns), the minor when a spec or the grader image does (it regrades), and the patch
# when only solutions, probes or notes do (it reuses the trials).
VERSION = "1.1.0"
MARKER = ".generated"
IDS = "/opt/gitea-env/ids.json"
SERVICE = "gitea"
BASE = f"http://{SERVICE}:3000"
API = f"{BASE}/api/v1"
ANSWERS = {
    "number": "Write just the number to /logs/artifacts/answer.txt.",
    "ids": "Write their numbers to /logs/artifacts/answer.txt, separated by commas.",
}
COMPOSE = """\
# The {world} world: the agent runs in `main`; Gitea, its database and Miroir run in `gitea`,
# on a network with no way out. Only port 3000 is reachable from `main`.
services:
  main:
    depends_on:
      gitea:
        condition: service_healthy
    networks: [default, world]
  gitea:
    image: {image}
    healthcheck:
      test: ["CMD-SHELL", "test -e /miroir/run/ready"]
      interval: 1s
      timeout: 5s
      retries: 300
    mem_limit: 2g
    networks: [world]
networks:
  world:
    internal: true
"""
SOLVE = """\
#!/bin/bash
# The reference solution, which Harbor's oracle agent runs. SOLUTION=N runs probe N instead
# (probe-N.sh): a wrong way to do the task, whose bounds and harm task.toml declares.
set -euo pipefail
if [ -n "${SOLUTION:-}" ]; then exec bash "$(dirname "$0")/probe-$SOLUTION.sh"; fi
"""
# The helpers a script defines, each only where a step uses it. @LOGIN@, @API@ and @AUTH@ stand
# for the persona's login, the API's base URL and the quoted Authorization header.
HELPERS = {
    "gitea": """\
gitea() { # METHOD PATH [JSON]: one request to Gitea's API as @LOGIN@; fails on an HTTP error
  curl -fsS -X "$1" "@API@$2" \\
    -H @AUTH@ \\
    ${3:+-H 'Content-Type: application/json' --data "$3"}
  echo
}
""",
    # Gitea re-checks a repository's open pull requests in the background after each merge,
    # and refuses a merge sent before that ends with 405 "Please try again later".
    "merge": """\
merge() { # PATH JSON: one merge as @LOGIN@, retried while Gitea re-checks mergeability
  local out tries=0
  until out=$(curl -sS --fail-with-body -X POST "@API@$1" \\
    -H @AUTH@ \\
    -H 'Content-Type: application/json' --data "$2"); do
    if [[ $out != *'Please try again later'* ]] || ((++tries > 10)); then
      echo "$out" >&2
      return 1
    fi
    sleep 1
  done
  echo "$out"
}
""",
}


@dataclass(frozen=True)
class Images:
    """The images a task names: its world's, the agent's (main) and the grader's."""

    world: str
    main: str
    grader: str


class SpecDumper(yaml.SafeDumper):
    """Block YAML with indented lists, and no anchors or aliases."""

    def ignore_aliases(self, data: Any) -> bool:
        """Write a repeated value in full each time."""
        return True

    def increase_indent(self, flow: bool = False, indentless: bool = False) -> None:
        """Indent a list under its key, as people write YAML."""
        super().increase_indent(flow, False)


def literal(dumper: yaml.SafeDumper, value: str) -> yaml.ScalarNode:
    """A string, as a literal block when it spans lines: SQL reads as written."""
    style = "|" if "\n" in value else None
    return dumper.represent_scalar("tag:yaml.org,2002:str", value, style=style)


SpecDumper.add_representer(str, literal)


def toml_string(value: str) -> str:
    """A TOML basic string. JSON's escapes are TOML's too, and JSON escapes every character
    TOML forbids raw but DEL."""
    return json.dumps(value, ensure_ascii=False).replace("\x7f", "\\u007f")


def spec_yaml(task: families.Task) -> str:
    """The task's MIROIR_SPEC. It goes in a TOML literal string, which ''' would end."""
    spec = task["spec"]
    gates = {"gates": spec["gates"]} if spec["gates"] else {}
    shown = {"checks": spec["checks"]} | gates | {"allow": spec["allow"]}
    text = yaml.dump(shown, Dumper=SpecDumper, sort_keys=False, allow_unicode=True, width=100)
    if "'''" in text:
        raise SystemExit(
            f"{task['id']}: its spec holds ''', which would end the TOML string around it: "
            "write that SQL without three quotes in a row"
        )
    return text


def instruction(task: families.Task, world: worlds.World) -> str:
    """Who the agent is and how it reaches Gitea, the request, and where an answer goes."""
    token = worlds.token(world.name, world.manager)
    preamble = (
        f"You are {world.persona}. The company's Git forge is Gitea at {BASE}. Its REST API is "
        f"at {API}, described by {BASE}/swagger.v1.json. Send the header "
        f"`Authorization: token {token}` with every request; it acts as you. Repositories are "
        "named `<organization>/<name>`, issues and pull requests share one number sequence in "
        "each repository, and lists are paginated with `page` and `limit` (at most 50). Do what "
        "you are asked and nothing more."
    )
    parts = [preamble, task["instruction"]]
    if task["answer"] is not None:
        parts.append(ANSWERS[task["answer"]])
    return "\n\n".join(parts) + "\n"


def probe_lines(probes: list[dict[str, Any]]) -> list[str]:
    """The [[metadata.miroir.probes]] tables, each followed by a blank line: probe N is entry N."""
    if not probes:
        return []
    lines = [
        "# solution/probe-N.sh, run with SOLUTION=N; `miroir check` holds each to its bounds",
        "# and harm.",
    ]
    for probe in probes:
        lines += ["[[metadata.miroir.probes]]", f"about = {toml_string(probe['about'])}"]
        bounds = [bound for bound in ("min_reward", "max_reward") if bound in probe]
        lines += [f"{bound} = {float(probe[bound])!r}" for bound in bounds]
        lines += [f"harm = {probe['harm']}", ""]
    return lines


def task_toml(task: families.Task, images: Images, org: str) -> str:
    """The task's task.toml, with every field set the way every Miroir task sets it."""
    name = f"{org}/gitea.{task['id']}"
    lines = [
        "# Written by examples/gitea/tasks.py from seed/families.py: edit that, then regenerate.",
        'schema_version = "1.4"',
        "# What the grader reads: what Gitea changed in its database and what it sent out, written",
        "# by `miroir collect` after Harbor stops the agent's container.",
        f'artifacts = [{{ source = "/miroir/evidence", service = "{SERVICE}" }}]',
        "",
        "[task]",
        f"name = {toml_string(name)}",
        f'version = "{VERSION}"',
        f"description = {toml_string(task['description'])}",
        "",
        "[metadata.miroir]",
        f"note = {toml_string(task['note'])}",
        "",
        *probe_lines(task["probes"]),
        "[agent]",
        "timeout_sec = 900.0",
        "",
        "[environment]",
        f"docker_image = {toml_string(images.main)}",
        "cpus = 2",
        "memory_mb = 4096",
        "storage_mb = 10240",
        "",
        "[verifier]",
        "timeout_sec = 120.0",
        "",
        "[verifier.environment]",
        f"docker_image = {toml_string(images.grader)}",
        'workdir = "/tests"',
        "",
        "[verifier.env]",
        f"MIROIR_SPEC = '''\n{spec_yaml(task)}'''",
        "",
        "[[verifier.collect]]",
        f'service = "{SERVICE}"',
        'command = "miroir collect"',
        "timeout_sec = 120.0",
    ]
    return "\n".join(lines) + "\n"


def helper(step: families.Step) -> str:
    """What a step runs: the answer file, or the merge or gitea helper."""
    if "answer" in step:
        return "answer"
    if step["method"] == "POST" and step["path"].endswith("/merge"):
        return "merge"
    return "gitea"


def command(step: families.Step) -> str:
    """A step as one line of bash, every argument quoted."""
    used = helper(step)
    if used == "answer":
        return f"printf '%s\\n' {shlex.quote(step['answer'])} >/logs/artifacts/answer.txt"
    words = [step["method"], step["path"]]
    if "body" in step:
        words.append(json.dumps(step["body"], ensure_ascii=False))
    if used == "merge":
        words = words[1:]
    return " ".join([used, *(shlex.quote(word) for word in words)])


def script(head: str, steps: list[families.Step], world: worlds.World, *, golden: bool) -> str:
    """A golden or probe script: its head, the helpers its steps use, then the steps."""
    auth = shlex.quote(f"Authorization: token {worlds.token(world.name, world.manager)}")
    used = {helper(step) for step in steps}
    parts = [head]
    for name, template in HELPERS.items():
        if name in used:
            defined = template.replace("@LOGIN@", world.manager).replace("@API@", API)
            parts.append(defined.replace("@AUTH@", auth))
    lines = [command(step) for step in steps]
    if golden:
        lines.insert(0, f"curl -fsS -o /dev/null -H {auth} {BASE}/swagger.v1.json")
    return "".join(parts) + "\n".join(lines) + "\n"


def probe_head(number: int, probe: dict[str, Any]) -> str:
    """A probe script's first lines: what it does wrong and what it should score."""
    bounds = [f"at least {probe['min_reward']:g}"] if "min_reward" in probe else []
    bounds += [f"at most {probe['max_reward']:g}"] if "max_reward" in probe else []
    expected = f"reward {' and '.join(bounds)}, harm {probe['harm']}"
    comment = f"# Probe {number}: {probe['about']}. Expected: {expected}."
    return f"#!/bin/bash\n{comment}\nset -euo pipefail\n"


def task_files(
    task: families.Task, world: worlds.World, images: Images, org: str
) -> dict[str, str]:
    """A task's files, by their path in its directory."""
    files = {
        "instruction.md": instruction(task, world),
        "task.toml": task_toml(task, images, org),
        "environment/docker-compose.yaml": COMPOSE.format(
            world=world.name, image=json.dumps(images.world)
        ),
        "solution/solve.sh": script(SOLVE, task["golden"], world, golden=True),
    }
    for number, probe in enumerate(task["probes"], start=1):
        head = probe_head(number, probe)
        files[f"solution/probe-{number}.sh"] = script(head, probe["steps"], world, golden=False)
    return files


def dataset_toml(split: str, names: list[str], org: str) -> str:
    """A split's dataset.toml: the [dataset] table only, as `harbor dataset add` adds tasks."""
    where = f"the {'world' if len(names) == 1 else 'worlds'} {' and '.join(names)}"
    description = f"Engineering work in a company's Gitea, the {split} split: tasks from {where}."
    lines = [
        "[dataset]",
        f"name = {toml_string(f'{org}/gitea-{split}')}",
        f'version = "{VERSION}"',
        f"description = {toml_string(description)}",
    ]
    return "\n".join(lines) + "\n"


def image_ids(name: str, ref: str) -> str:
    """The ids.json the seed wrote in a world image."""
    command = ["docker", "run", "--rm", ref, "cat", IDS]
    try:
        done = subprocess.run(command, capture_output=True, text=True, check=False)
    except FileNotFoundError as e:
        raise SystemExit("docker is not on PATH: install Docker, or pass --ids NAME=PATH") from e
    if done.returncode != 0:
        raise SystemExit(
            f"{name}: `{shlex.join(command)}` failed: {done.stderr.strip()}. Build the world "
            "image first (docker buildx bake gitea), or pass --ids NAME=PATH"
        )
    return done.stdout


def read_ids(name: str, ref: str, path: Path | None) -> tuple[dict[str, Any], str]:
    """A world's ids.json, from `path` when given, else from its image; and where it came from."""
    if path is None:
        source, text = f"{ref}:{IDS}", image_ids(name, ref)
    else:
        source = str(path)
        try:
            text = path.read_text(encoding="utf-8")
        except OSError as e:
            raise SystemExit(f"{name}: cannot read {path}: {e.strerror}") from e
    try:
        data = json.loads(text)
    except json.JSONDecodeError as e:
        raise SystemExit(f"{name}: {source} is not JSON ({e})") from e
    if not isinstance(data, dict):
        raise SystemExit(f"{name}: {source} holds no JSON object")
    return data, source


def world_tasks(
    name: str, data: dict[str, Any], source: str
) -> tuple[worlds.World, list[families.Task]]:
    """Plant the world here, check it is the one the image planted, and write its tasks."""
    if data.get("world") != name:
        raise SystemExit(
            f"{source} holds the ids of world {data.get('world')!r}, not {name!r}: "
            "pair each world with its own image"
        )
    world, plans = plants.build(name)
    here = plants.digest(world, plans)
    if data.get("digest") != here:
        raise SystemExit(
            f"{name}: the image planted {data.get('digest')}, but this checkout plants {here}. "
            "Either the image was built from other plant code (seed/plants.py, seed/worlds.py) "
            "than this checkout's: rebuild it; or the image's Python 3.12 plants differently "
            f"from this Python {platform.python_version()}: run this with "
            "`uv run --python 3.12`."
        )
    return world, families.tasks(world, worlds.Ids.from_json(data), plans)


def pair(text: str) -> tuple[str, str]:
    """A NAME=VALUE argument, both parts non-empty."""
    name, _, value = text.partition("=")
    if not name or not value:
        raise argparse.ArgumentTypeError(f"{text!r} is not NAME=VALUE, such as orbit=REF")
    return name, value


def arguments(argv: list[str] | None) -> argparse.Namespace:
    """The command line, with every world known and given once."""
    parser = argparse.ArgumentParser(
        description="Write the Gitea worlds' tasks as Harbor task directories."
    )
    parser.add_argument("--out", type=Path, required=True, help="replaced whole on every run")
    parser.add_argument(
        "--world",
        type=pair,
        action="append",
        required=True,
        metavar="NAME=REF",
        help="a world and its image, such as orbit=gitea-env:orbit; once per world",
    )
    parser.add_argument("--main", required=True, metavar="REF", help="the agent's image")
    parser.add_argument("--grader", required=True, metavar="REF", help="the grader's image")
    parser.add_argument("--org", default="raphaeldkhn", help="names tasks and datasets")
    parser.add_argument(
        "--ids",
        type=pair,
        action="append",
        default=[],
        metavar="NAME=PATH",
        help="read a world's ids.json from PATH instead of its image",
    )
    args = parser.parse_args(argv)
    names = [name for name, _ in args.world]
    unknown = sorted(set(names) - set(worlds.WORLDS))
    if unknown:
        parser.error(f"unknown worlds {unknown}; the worlds are {', '.join(worlds.WORLDS)}")
    if len(set(names)) < len(names):
        parser.error("give each world once")
    stray = sorted({name for name, _ in args.ids} - set(names))
    if stray:
        parser.error(f"--ids names {stray}, which no --world gives")
    return args


def write(out: Path, files: dict[str, str]) -> None:
    """Replace `out` with `files`, refusing a directory this did not write."""
    if out.exists() and not out.is_dir():
        raise SystemExit(f"--out {out} is a file: pass a directory")
    if out.exists() and any(out.iterdir()):
        if not (out / MARKER).is_file():
            raise SystemExit(
                f"--out {out} holds files tasks.py did not write (no {MARKER} file in it): "
                "pass an empty or new directory"
            )
        shutil.rmtree(out)
    out.mkdir(parents=True, exist_ok=True)
    # First, so that a run that fails halfway leaves a directory the next run may replace.
    (out / MARKER).write_text(
        "Written by examples/gitea/tasks.py, which replaces this directory on every run.\n",
        encoding="utf-8",
    )
    for path, text in sorted(files.items()):
        target = out / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(text, encoding="utf-8", newline="\n")
        if target.suffix == ".sh":
            target.chmod(0o755)


def main(argv: list[str] | None = None) -> None:
    """Read each world's ids, write its tasks, and replace the output directory with them."""
    args = arguments(argv)
    paths = {name: Path(path) for name, path in args.ids}
    given = dict(args.world)
    files: dict[str, str] = {}
    # Tasks by split, then by world, in the order worlds.WORLDS lists them.
    splits: dict[str, dict[str, int]] = {}
    for name in [known for known in worlds.WORLDS if known in given]:
        world, tasks = world_tasks(name, *read_ids(name, given[name], paths.get(name)))
        images = Images(given[name], args.main, args.grader)
        for task in tasks:
            for path, text in task_files(task, world, images, args.org).items():
                files[f"{task['split']}/{task['id']}/{path}"] = text
            counts = splits.setdefault(task["split"], {})
            counts[name] = counts.get(name, 0) + 1
    for split, counts in splits.items():
        files[f"{split}/dataset.toml"] = dataset_toml(split, list(counts), args.org)
    write(args.out, files)
    shown = ", ".join(f"{sum(counts.values())} {split}" for split, counts in splits.items())
    print(f"wrote {shown} tasks to {args.out}")


if __name__ == "__main__":
    main()
