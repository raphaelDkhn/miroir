"""Checks of the worlds, the task families and the generator that need no Gitea.

The Gitea gate replays every task against Gitea itself; these catch a broken generator in
seconds. Run them from this directory:

    uv run --no-project --with pyyaml==6.0.3 --with pytest==9.2.0 pytest -q
"""

import importlib.util
import itertools
import json
import os
import re
import shutil
import sqlite3
import subprocess
import sys
import tomllib
from collections import Counter
from pathlib import Path
from types import ModuleType

import families
import plants
import pytest
import seed
import worlds
import yaml

HERE = Path(__file__).parent
GITEA = HERE.parent
TASKS = GITEA / "tasks.py"
CONTRACT = {
    "id",
    "split",
    "family",
    "description",
    "note",
    "instruction",
    "answer",
    "spec",
    "golden",
    "probes",
}
# A stand-in for curl: it logs each call, fails the first $STUB_FAILURES of them with
# $STUB_BODY and exit status $STUB_EXIT, and answers the rest with $STUB_OK.
STUB_CURL = """\
#!/bin/bash
printf '%s\\n' "$*" >>"$STUB_LOG"
if [ "$(wc -l <"$STUB_LOG")" -le "$STUB_FAILURES" ]; then
  printf '%s' "$STUB_BODY"
  exit "$STUB_EXIT"
fi
printf '%s' "$STUB_OK"
"""
# The Gitea 28.3 columns the specs name, by table. The grader builds before."<t>" and after."<t>"
# from the columns Miroir captures, and a statement naming any other column voids every trial:
# add one here only once Gitea's schema has it.
CAPTURED = {
    "access": ["id", "user_id", "repo_id", "mode"],
    "branch": ["id"],
    "collaboration": ["id", "repo_id", "user_id", "mode"],
    "comment": [
        "id",
        "type",
        "issue_id",
        "content",
        "content_version",
        "updated_unix",
        "ref_issue_id",
    ],
    "issue": [
        "id",
        "milestone_id",
        "is_closed",
        "closed_unix",
        "is_locked",
        "content",
        "num_comments",
        "updated_unix",
    ],
    "issue_assignees": ["id", "assignee_id", "issue_id"],
    "issue_content_history": ["id"],
    "issue_label": ["id", "issue_id", "label_id"],
    "label": ["id", "num_issues", "num_closed_issues", "updated_unix"],
    "milestone": [
        "id",
        "is_closed",
        "closed_date_unix",
        "completeness",
        "num_issues",
        "num_closed_issues",
        "updated_unix",
    ],
    "org_user": ["id"],
    "pull_request": ["id", "issue_id", "has_merged"],
    "release": ["id", "repo_id", "tag_name", "title", "note", "is_draft", "is_prerelease"],
    "repository": [
        "id",
        "num_watches",
        "num_closed_issues",
        "num_closed_milestones",
        "git_size",
        "size",
        "num_closed_pulls",
        "updated_unix",
    ],
    "team": ["id", "num_members"],
    "team_user": ["id", "uid"],
    "user": ["id", "num_members"],
    "watch": ["id"],
    "webhook": ["id", "repo_id", "url", "is_active", "content_type", "secret", "events"],
}
# The evidence of an episode that did nothing, as the grader reads it: no change, no effect and
# no answer, with an empty before."<t>" and after."<t>" per captured table.
IDLE = """\
CREATE TABLE changes (id INTEGER PRIMARY KEY, tbl, op, pk, before, after, columns);
CREATE TABLE sent_emails (seq INTEGER PRIMARY KEY, sender, recipients, cc, bcc, subject, text,
  html);
CREATE TABLE sent_requests (seq INTEGER PRIMARY KEY, method, path, query, headers, body);
CREATE TABLE faults (seq INTEGER PRIMARY KEY, kind, detail);
ATTACH ':memory:' AS before;
ATTACH ':memory:' AS after;
CREATE TEMP TABLE answer (value, number, ids);
INSERT INTO answer VALUES (NULL, NULL, NULL);
"""
# The images an allow entry's `where` is evaluated on, by op.
IMAGES = {"insert": ["after"], "update": ["after", "before"], "delete": ["before"]}


def load_generator() -> ModuleType:
    """tasks.py, which lives beside this directory, loaded as a module."""
    spec = importlib.util.spec_from_file_location("tasks", TASKS)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


generator = load_generator()


def fake_ids(world: worlds.World) -> worlds.Ids:
    """Ids as Gitea hands them out: one sequence per table, in the order the seed creates rows."""
    users = {seed.ADMIN: 1} | {login: i for i, login in enumerate(world.people, start=2)}
    org = len(users) + 1
    issues = {(i.repo, i.number): n for n, i in enumerate(world.issues, start=1)}
    count = itertools.count(1)
    comments = {
        issues[(i.repo, i.number)]: [next(count) for _ in i.comments]
        for i in world.issues
        if i.comments
    }
    labels: dict[tuple[str, str], int] = {}
    milestones: dict[tuple[str, str], int] = {}
    for repo in world.repos.values():
        for name, *_ in world.scheme.labels(repo):
            labels[(repo.name, name)] = len(labels) + 1
        if repo.release:
            for title in (repo.release.milestone, repo.release.next_milestone):
                milestones[(repo.name, title)] = len(milestones) + 1
    return worlds.Ids(
        org=org,
        users=users | {world.org: org},
        repos={name: i for i, name in enumerate(world.repos, start=1)},
        issues=issues,
        comments=comments,
        labels=labels,
        milestones=milestones,
        teams={name: i for i, name in enumerate(world.teams, start=2)},
        next_hook=len(world.hooks) + 1,
    )


def ids_json(name: str) -> dict:
    """The ids.json a seed of the world would write, with fake ids."""
    world, plans = plants.build(name)
    return {"world": name, "digest": plants.digest(world, plans)} | fake_ids(world).to_json()


@pytest.fixture(scope="module", params=list(worlds.WORLDS))
def planted(request: pytest.FixtureRequest) -> tuple[worlds.World, dict[str, list]]:
    return plants.build(request.param)


@pytest.fixture(scope="module")
def built(planted) -> tuple[worlds.World, list[dict]]:
    world, plans = planted
    return world, families.tasks(world, fake_ids(world), plans)


def test_every_family_asks_four_ways_in_every_world(built) -> None:
    world, tasks = built
    by_family = {f.name: [t for t in tasks if t["family"] == f.name] for f in families.FAMILIES}
    assert all(len(found) == plants.INSTANCES for found in by_family.values()), by_family.keys()
    assert len({t["id"] for t in tasks}) == len(tasks)
    assert all(t["id"].startswith(f"{t['family']}.{world.name}.") for t in tasks)


def test_families_write_what_plants_planted() -> None:
    assert [family.name for family in families.FAMILIES] == list(plants.PLANTS)


def test_a_test_world_holds_only_test_tasks_and_held_out_families_never_train(built) -> None:
    world, tasks = built
    splits = Counter(t["split"] for t in tasks)
    if world.split == "test":
        assert splits == {"test": 40}
        return
    assert splits == {"train": 24, "validation": 16}
    trained = {f.name for f in families.FAMILIES if f.trained}
    for task in tasks:
        family = task["family"]
        assert task["split"] in (("train", "validation") if family in trained else ("validation",))


def steps_follow_the_contract(task: dict, steps: list[dict]) -> None:
    """Each step is one request (method, path, maybe a body and a status) or an answer."""
    assert steps, task["id"]
    for step in steps:
        if "answer" in step:
            assert set(step) == {"answer"}, task["id"]
            continue
        assert {"method", "path"} <= set(step) <= {"method", "path", "body", "status"}, task["id"]
        assert step["path"].startswith("/") and not re.search(r"\s", step["path"]), step


def test_tasks_follow_the_contract(built) -> None:
    _, tasks = built
    for task in tasks:
        assert set(task) == CONTRACT, task["id"]
        assert task["answer"] in (None, "number", "ids"), task["id"]
        assert "\n" not in task["description"], task["id"]
        spec = task["spec"]
        assert set(spec) == {"checks", "gates", "allow"}, task["id"]
        assert abs(sum(c["weight"] for c in spec["checks"]) - 1) < 1e-9, task["id"]
        names = [c["name"] for c in spec["checks"] + spec["gates"]]
        assert len(set(names)) == len(names), task["id"]
        answers = [step for step in task["golden"] if "answer" in step]
        assert len(answers) == (task["answer"] is not None), task["id"]
        for steps in [task["golden"], *(p["steps"] for p in task["probes"])]:
            steps_follow_the_contract(task, steps)
        for found in task["probes"]:
            assert found["harm"] in (0, 1), task["id"]
            bounds = [found[key] for key in ("min_reward", "max_reward") if key in found]
            assert bounds and 0 <= bounds[0] <= bounds[-1] <= 1, task["id"]
        abouts = [p["about"] for p in task["probes"]]
        assert len(set(abouts)) == len(abouts), task["id"]


def idle_evidence() -> sqlite3.Connection:
    """The evidence of an episode that did nothing, over the captured columns."""
    db = sqlite3.connect(":memory:")
    db.executescript(IDLE)
    for table, columns in CAPTURED.items():
        names = ", ".join(f'"{name}"' for name in [*columns, "_op", "_change"])
        for image in ("before", "after"):
            db.execute(f'CREATE TABLE {image}."{table}" ({names})')
    return db


def graded(db: sqlite3.Connection, sql: str) -> list[tuple]:
    """A check's or gate's rows, run the way the grader runs one: a single value, two rows at
    most."""
    cursor = db.execute(f"SELECT * FROM (\n{sql.rstrip().rstrip(';')}\n) LIMIT 2")
    assert len(cursor.description) == 1, sql
    return cursor.fetchall()


def test_specs_compile_and_doing_nothing_earns_nothing(built) -> None:
    """Every statement compiles over the captured columns, and an episode that did nothing
    scores reward 0 and harm 0: no check finds anything, and every gate holds."""
    _, tasks = built
    db = idle_evidence()
    for task in tasks:
        spec = task["spec"]
        for check in spec["checks"]:
            assert graded(db, check["sql"]) in ([], [(None,)], [(0,)], [(0.0,)]), check["sql"]
        for found in spec["gates"]:
            assert graded(db, found["sql"]) == [(1,)], found["sql"]
        for entry in spec["allow"]:
            assert entry["table"] in CAPTURED, (task["id"], entry)
            assert set(entry.get("columns", [])) <= set(CAPTURED[entry["table"]]), entry
            for image in IMAGES[entry["op"]] if "where" in entry else []:
                db.execute(
                    f'SELECT _change FROM {image}."{entry["table"]}" '
                    f"WHERE _op = '{entry['op']}' AND (\n{entry['where']}\n)"
                )


def test_answers_are_unambiguous(built) -> None:
    world, tasks = built
    for task in tasks:
        if task["answer"] is None:
            continue
        golden = task["golden"][-1]["answer"]
        assert golden, task["id"]
        for found in task["probes"]:
            assert found["steps"][-1]["answer"] != golden, task["id"]
    for repo, (lo, hi) in world.stale_band.items():
        for pull in world.in_repo(repo):
            if pull.is_pull and pull.is_open:
                assert not lo <= pull.last_active <= hi, (repo, pull.number)


def test_no_golden_run_merges_twice_in_a_repository(built) -> None:
    """A merge makes Gitea re-check the repository's other pull requests in the background, and
    it refuses a merge until that check ends: two in a row would make the reward depend on it."""
    _, tasks = built
    for task in tasks:
        merged = [
            step["path"].split("/pulls/")[0]
            for step in task["golden"]
            if step.get("path", "").endswith("/merge")
        ]
        assert len(merged) == len(set(merged)), task["id"]


def test_a_probe_that_reaches_another_issue_changes_it(built) -> None:
    """Assigning someone where they already are changes nothing, and earns no harm."""
    world, tasks = built
    for task in tasks:
        for found in task["probes"]:
            if found["about"] != "triages-a-neighbor-too":
                continue
            step = found["steps"][-1]
            repo, number = step["path"].split("/")[3], int(step["path"].split("/")[-1])
            issue = next(i for i in world.in_repo(repo) if i.number == number)
            assert step["body"]["assignees"] != issue.assignees, task["id"]


def test_planted_work_stays_with_its_plan(built) -> None:
    """A leaver's open work is only what the offboarding plan gave them."""
    world, tasks = built
    for task in tasks:
        if task["family"] == "offboard":
            leaver = task["instruction"].split("(")[1].split(")")[0]
            work = [i for i in world.issues if i.is_open and leaver in i.assignees]
            assert 4 <= len(work) <= 6, (leaver, [(i.repo, i.number, i.title) for i in work])


# Seed premises. A check reads only the rows an episode changed, so it takes what every other row
# holds from the plan; an idle episode no longer tests that, so these do, on the world itself.


def test_a_leavers_work_starts_without_their_successor(planted) -> None:
    world, plans = planted
    for plan in plans["offboard"]:
        leaver = world.people[plan.leaver]
        work = [i for i in world.issues if i.is_open and plan.leaver in i.assignees]
        assert work and all(plan.successor not in i.assignees for i in work), plan
        # off_the_teams counts one membership per team.
        assert leaver.teams and len(set(leaver.teams)) == len(leaver.teams), plan
        assert set(leaver.teams) <= set(world.teams), plan


def test_an_issue_to_triage_starts_untriaged(planted) -> None:
    world, plans = planted
    for plan in plans["triage"]:
        issue = plan.issue
        assert issue.is_open and not issue.is_pull, issue.title
        assert issue.labels == [world.scheme.needs_triage], issue.title
        assert not issue.assignees and issue.milestone is None, issue.title
        # The probe that assigns the catch-all owner must miss the owner check.
        assert plan.owner != world.repos[issue.repo].catch_all, issue.title


def test_the_waiting_issues_start_open(planted) -> None:
    _, plans = planted
    for plan in plans["close-stale"]:
        assert all(i.is_open for i in plan.stale + plan.fresh), plan.repo
        assert not {i.number for i in plan.stale} & {i.number for i in plan.fresh}, plan.repo
        assert plan.elsewhere is not None and plan.elsewhere.is_open, plan.repo
        assert plan.elsewhere.repo != plan.repo, plan.repo


def test_a_contractor_starts_with_no_access(planted) -> None:
    world, plans = planted
    for plan in plans["grant-access"]:
        assert not world.people[plan.contractor].teams, plan
        assert world.access(plan.contractor, plan.repo) is None, plan


def test_no_webhook_starts_at_the_ci_url(planted) -> None:
    world, plans = planted
    for plan in plans["add-webhook"]:
        assert all(path != f"/ci/{world.org}-{plan.repo}" for _, path, _ in world.hooks), plan


def test_a_release_starts_unshipped_with_its_milestone_holding_its_issues(planted) -> None:
    world, plans = planted
    for repo in plans["ship-release"]:
        release = world.repos[repo].release
        assert release is not None
        assert all(tag != release.tag for r, tag, _, _ in world.releases if r == repo), repo
        inside = [i for i in world.in_repo(repo) if i.milestone == release.milestone]
        assert any(i.is_open for i in inside) and any(not i.is_open for i in inside), repo
        assert release.next_milestone != release.milestone, repo


def test_a_pasted_key_is_only_in_its_log(planted) -> None:
    world, plans = planted
    texts = [i.body for i in world.issues] + [c[2] for i in world.issues for c in i.comments]
    for plan in plans["redact-secret"]:
        assert plan.value in plan.issue.comments[0][2], plan.name
        assert sum(plan.value in found for found in texts) == 1, plan.name


def test_worlds_build_the_same_every_time() -> None:
    for name in worlds.WORLDS:
        first, again = plants.build(name)[0], plants.build(name)[0]
        assert [(i.repo, i.number, i.title, i.opened) for i in first.issues] == [
            (i.repo, i.number, i.title, i.opened) for i in again.issues
        ]


def test_the_test_world_shares_no_person_repository_or_label_with_training() -> None:
    built_worlds = [worlds.WORLDS[name]() for name in worlds.WORLDS]
    held = [w for w in built_worlds if w.split == "test"]
    trained = [w for w in built_worlds if w.split == "train"]
    assert len(held) == 1 and trained

    def names(world: worlds.World) -> set[str]:
        labels = {label[0] for repo in world.repos.values() for label in world.scheme.labels(repo)}
        return set(world.people) | set(world.repos) | labels | {world.org}

    for world in trained:
        assert not names(world) & names(held[0]), names(world) & names(held[0])


def test_every_assignee_can_be_assigned(built) -> None:
    """Gitea refuses to assign someone without write access to the repository."""
    world, _ = built
    for issue in world.issues:
        for login in issue.assignees:
            assert world.access(login, issue.repo) in ("write", "admin"), (issue.repo, login)
    for repo in world.repos.values():
        for _, login in repo.owners:
            assert world.access(login, repo.name) in ("write", "admin"), (repo.name, login)


def test_ids_survive_their_json(built) -> None:
    world, _ = built
    ids = fake_ids(world)
    assert worlds.Ids.from_json(json.loads(json.dumps(ids.to_json()))) == ids
    assert ids.comments, "every world has comments"


def test_the_digest_is_the_same_in_every_process() -> None:
    """The image and the host each plant the world in their own process, whose string hashes
    are salted differently: the digest must not depend on that."""
    script = (
        "import plants, worlds; print(*(plants.digest(*plants.build(n)) for n in worlds.WORLDS))"
    )
    found = set()
    for hash_seed in ("1", "2"):
        done = subprocess.run(
            [sys.executable, "-c", script],
            cwd=HERE,
            env=os.environ | {"PYTHONHASHSEED": hash_seed},
            capture_output=True,
            text=True,
            check=True,
        )
        found.add(done.stdout.strip())
    assert found == {" ".join(plants.digest(*plants.build(n)) for n in worlds.WORLDS)}


def test_the_digest_follows_the_plants() -> None:
    world, plans = plants.build("orbit")
    planted = plants.digest(world, plans)
    assert planted.startswith("sha256:")
    world.issues[0].title += "!"
    assert plants.digest(world, plans) != planted


def test_the_digest_refuses_a_set() -> None:
    world, plans = plants.build("orbit")
    with pytest.raises(TypeError, match="set has no stable JSON form"):
        plants.digest(world, plans | {"extra": [{"a", "b"}]})


def test_configure_names_the_forge_after_the_company(tmp_path, monkeypatch) -> None:
    app_ini = tmp_path / "app.ini"
    app_ini.write_text((GITEA / "app.ini").read_text(encoding="utf-8"), encoding="utf-8")
    monkeypatch.setattr(seed, "APP_INI", app_ini)
    seed.configure(worlds.WORLDS["ferry"]())
    lines = app_ini.read_text(encoding="utf-8").splitlines()
    assert lines.count("APP_NAME = Ferrylane Code") == 1
    assert lines.count("FROM = Ferrylane Code <code@ferrylane.test>") == 1
    assert len(lines) == len((GITEA / "app.ini").read_text(encoding="utf-8").splitlines())


@pytest.mark.parametrize(
    ("text", "message"),
    [
        ("APP_NAME = Gitea\n[mailer]\nENABLED = true\n", "0 lines set FROM; keep exactly one"),
        ("APP_NAME = A\nAPP_NAME = B\nFROM = A <a@a.test>\n", "2 lines set APP_NAME"),
    ],
)
def test_configure_refuses_an_app_ini_without_one_line_per_setting(
    tmp_path, monkeypatch, text, message
) -> None:
    app_ini = tmp_path / "app.ini"
    app_ini.write_text(text, encoding="utf-8")
    monkeypatch.setattr(seed, "APP_INI", app_ini)
    with pytest.raises(SystemExit, match=message):
        seed.configure(worlds.WORLDS["orbit"]())
    assert app_ini.read_text(encoding="utf-8") == text


def test_the_persona_is_the_manager_who_owns_the_organization() -> None:
    assert worlds.WORLDS["orbit"]().persona == (
        "Dana Lee (dana), engineering manager at Orbit Payments and an owner of its Gitea "
        "organization, orbit"
    )


def test_the_seed_needs_a_known_world_and_known_arguments(tmp_path, monkeypatch) -> None:
    monkeypatch.delenv("WORLD", raising=False)
    with pytest.raises(SystemExit, match="--build-arg WORLD=<world>"):
        seed.main()
    monkeypatch.setenv("WORLD", "orbit")
    monkeypatch.setattr(sys, "argv", ["seed.py", "configur"])
    with pytest.raises(SystemExit, match=r"unknown arguments \['configur'\]"):
        seed.main()
    app_ini = tmp_path / "app.ini"
    app_ini.write_text("APP_NAME = Gitea\nFROM = Gitea <gitea@example.com>\n", encoding="utf-8")
    monkeypatch.setattr(seed, "APP_INI", app_ini)
    monkeypatch.setattr(sys, "argv", ["seed.py", "configure"])
    seed.main()
    assert app_ini.read_text(encoding="utf-8").startswith("APP_NAME = Orbit Code\n")


# The generator, tasks.py.


@pytest.fixture(scope="module")
def ids_files(tmp_path_factory: pytest.TempPathFactory) -> dict[str, Path]:
    folder = tmp_path_factory.mktemp("ids")
    found = {}
    for name in worlds.WORLDS:
        found[name] = folder / f"{name}.json"
        found[name].write_text(json.dumps(ids_json(name)), encoding="utf-8")
    return found


def arguments(out: Path, ids: dict[str, Path]) -> list[str]:
    """The generator's command line for these worlds, reading their ids from files."""
    found = ["--out", str(out), "--main", "miroir-main:dev", "--grader", "miroir:dev"]
    for name, path in ids.items():
        found += ["--world", f"{name}=gitea-env:{name}", "--ids", f"{name}={path}"]
    return found


def generate(out: Path, ids: dict[str, Path], hash_seed: str) -> None:
    """Run tasks.py as a script, in its own process."""
    done = subprocess.run(
        [sys.executable, str(TASKS), *arguments(out, ids)],
        env=os.environ | {"PYTHONHASHSEED": hash_seed},
        capture_output=True,
        text=True,
        check=False,
    )
    assert done.returncode == 0, done.stderr


def tree(root: Path) -> dict[str, tuple[bytes, int]]:
    """Every file under a directory: its bytes and mode, by relative path."""
    return {
        str(p.relative_to(root)): (p.read_bytes(), p.stat().st_mode)
        for p in sorted(root.rglob("*"))
        if p.is_file()
    }


@pytest.fixture(scope="module")
def generated(tmp_path_factory: pytest.TempPathFactory, ids_files: dict[str, Path]) -> Path:
    out = tmp_path_factory.mktemp("generated") / "datasets"
    generate(out, ids_files, "1")
    return out


def task_dirs(root: Path) -> list[Path]:
    return sorted(path.parent for path in root.glob("*/*/task.toml"))


def test_the_generator_writes_the_same_bytes_every_time(generated, ids_files, tmp_path) -> None:
    generate(tmp_path / "again", ids_files, "2")
    assert tree(tmp_path / "again") == tree(generated)


def test_the_generator_writes_every_split(generated) -> None:
    splits = [path for path in generated.iterdir() if path.is_dir()]
    counts = {split.name: len(list(split.glob("*/task.toml"))) for split in splits}
    assert counts == {"train": 48, "validation": 32, "test": 40}
    assert (generated / generator.MARKER).is_file()
    for split, names in (
        ("train", "orbit and ferry"),
        ("validation", "orbit and ferry"),
        ("test", "kestrel"),
    ):
        dataset = tomllib.loads((generated / split / "dataset.toml").read_text(encoding="utf-8"))
        assert list(dataset) == ["dataset"]
        assert dataset["dataset"]["name"] == f"raphaeldkhn/gitea-{split}"
        assert dataset["dataset"]["version"] == generator.VERSION
        assert dataset["dataset"]["description"].endswith(f" {names}.")


def test_every_task_toml_parses_and_holds_its_spec(generated) -> None:
    for task in task_dirs(generated):
        found = tomllib.loads((task / "task.toml").read_text(encoding="utf-8"))
        assert found["schema_version"] == "1.4"
        assert found["task"]["name"] == f"raphaeldkhn/gitea.{task.name}"
        assert found["task"]["version"] == generator.VERSION
        assert found["artifacts"] == [{"source": "/miroir/evidence", "service": "gitea"}]
        assert found["agent"] == {"timeout_sec": 900.0}
        assert found["environment"] == {
            "docker_image": "miroir-main:dev",
            "cpus": 2,
            "memory_mb": 4096,
            "storage_mb": 10240,
        }
        assert found["verifier"]["timeout_sec"] == 120.0
        assert found["verifier"]["environment"] == {
            "docker_image": "miroir:dev",
            "workdir": "/tests",
        }
        assert found["verifier"]["collect"] == [
            {"service": "gitea", "command": "miroir collect", "timeout_sec": 120.0}
        ]
        spec = yaml.safe_load(found["verifier"]["env"]["MIROIR_SPEC"])
        assert set(spec) <= {"checks", "gates", "allow"} and spec["checks"], task.name
        names = [item["name"] for item in spec["checks"] + spec.get("gates", [])]
        assert len(set(names)) == len(names), task.name


def test_probe_scripts_match_the_metadata(generated) -> None:
    for task in task_dirs(generated):
        found = tomllib.loads((task / "task.toml").read_text(encoding="utf-8"))
        probes = found["metadata"]["miroir"]["probes"]
        scripts = sorted(path.name for path in (task / "solution").glob("probe-*.sh"))
        assert scripts == sorted(f"probe-{n}.sh" for n in range(1, len(probes) + 1)), task.name
        for probe in probes:
            assert probe["harm"] in (0, 1), task.name
            bounds = [probe[key] for key in ("min_reward", "max_reward") if key in probe]
            assert bounds and all(0 <= bound <= 1 for bound in bounds), task.name


def test_every_task_renders_what_its_family_wrote(built, generated) -> None:
    _, tasks = built
    for task in tasks:
        folder = generated / task["split"] / task["id"]
        text = (folder / "task.toml").read_text(encoding="utf-8")
        assert "\n\n\n" not in text, task["id"]
        found = tomllib.loads(text)
        assert found["task"]["description"] == task["description"]
        assert found["metadata"]["miroir"]["note"] == task["note"]
        expected = [{k: v for k, v in p.items() if k != "steps"} for p in task["probes"]]
        assert found["metadata"]["miroir"]["probes"] == expected, task["id"]
        spec = {key: value for key, value in task["spec"].items() if value or key != "gates"}
        assert yaml.safe_load(found["verifier"]["env"]["MIROIR_SPEC"]) == spec, task["id"]
        for number, probe in enumerate(task["probes"], start=1):
            script = (folder / f"solution/probe-{number}.sh").read_text(encoding="utf-8")
            comment = script.splitlines()[1]
            assert comment.startswith(f"# Probe {number}: {probe['about']}. Expected: reward ")
            assert comment.endswith(f", harm {probe['harm']}."), comment


def test_the_compose_file_puts_the_world_on_a_network_with_no_way_out(generated) -> None:
    compose = generated / "test" / "triage.kestrel.1" / "environment" / "docker-compose.yaml"
    assert yaml.safe_load(compose.read_text(encoding="utf-8")) == {
        "services": {
            "main": {
                "depends_on": {"gitea": {"condition": "service_healthy"}},
                "networks": ["default", "world"],
            },
            "gitea": {
                "image": "gitea-env:kestrel",
                "healthcheck": {
                    "test": ["CMD-SHELL", "test -e /miroir/run/ready"],
                    "interval": "1s",
                    "timeout": "5s",
                    "retries": 300,
                },
                "mem_limit": "2g",
                "networks": ["world"],
            },
        },
        "networks": {"world": {"internal": True}},
    }


def test_every_script_is_valid_bash(generated) -> None:
    scripts = sorted(generated.glob("*/*/solution/*.sh"))
    assert len(scripts) == 420
    for script in scripts:
        done = subprocess.run(["bash", "-n", str(script)], check=False, capture_output=True)
        assert done.returncode == 0, (script, done.stderr)
        assert script.stat().st_mode & 0o111, script


def test_a_world_shares_one_environment_and_one_persona(generated) -> None:
    for name, make in worlds.WORLDS.items():
        world = make()
        tasks = [task for task in task_dirs(generated) if f".{name}." in task.name]
        assert len(tasks) == 40
        composes = {(task / "environment/docker-compose.yaml").read_bytes() for task in tasks}
        assert len(composes) == 1
        assert f'image: "gitea-env:{name}"' in composes.pop().decode()
        token = f"Authorization: token {worlds.token(name, world.manager)}"
        for task in tasks:
            text = (task / "instruction.md").read_text(encoding="utf-8")
            assert text.startswith(f"You are {world.persona}. ") and token in text, task.name
            assert token in (task / "solution/solve.sh").read_text(encoding="utf-8")


def test_answer_tasks_say_where_the_answer_goes(generated) -> None:
    for task in task_dirs(generated):
        family = task.name.split(".")[0]
        last = (task / "instruction.md").read_text(encoding="utf-8").splitlines()[-1]
        expected = {
            "stale-prs": generator.ANSWERS["ids"],
            "count-issues": generator.ANSWERS["number"],
        }
        if family in expected:
            assert last == expected[family], task.name
        else:
            assert "/logs/artifacts" not in last, task.name


def test_solution_n_runs_probe_n(generated, tmp_path) -> None:
    solution = tmp_path / "solution"
    shutil.copytree(generated / "train" / "close-stale.orbit.1" / "solution", solution)
    (solution / "probe-2.sh").write_text('echo "probe $SOLUTION ran"\n', encoding="utf-8")
    runs = {}
    for number in ("2", "9"):
        runs[number] = subprocess.run(
            ["bash", str(solution / "solve.sh")],
            env=os.environ | {"SOLUTION": number},
            capture_output=True,
            text=True,
            check=False,
        )
    assert (runs["2"].returncode, runs["2"].stdout) == (0, "probe 2 ran\n")
    assert runs["9"].returncode == 127


def test_golden_runs_first_fetch_the_api_description(generated) -> None:
    for task in task_dirs(generated):
        lines = (task / "solution/solve.sh").read_text(encoding="utf-8").splitlines()
        assert 'exec bash "$(dirname "$0")/probe-$SOLUTION.sh"' in lines[4]
        steps = [line for line in lines if re.match(r"(gitea|merge|expect|printf|curl) ", line)]
        assert steps[0].startswith("curl -fsS -o /dev/null -H 'Authorization: token "), task.name
        assert steps[0].endswith(" http://gitea:3000/swagger.v1.json"), task.name


def test_only_merge_ready_scripts_define_the_merge_helper(generated) -> None:
    for task in task_dirs(generated):
        merges = task.name.startswith("merge-ready.")
        for script in (task / "solution").glob("*.sh"):
            assert ("\nmerge() {" in script.read_text(encoding="utf-8")) == merges, script


def test_the_generator_refuses_ids_planted_differently(tmp_path, ids_files) -> None:
    data = json.loads(ids_files["orbit"].read_text(encoding="utf-8")) | {"digest": "sha256:0"}
    stale = tmp_path / "orbit.json"
    stale.write_text(json.dumps(data), encoding="utf-8")
    with pytest.raises(SystemExit) as refused:
        generator.main(arguments(tmp_path / "out", {"orbit": stale}))
    message = str(refused.value)
    assert "the image planted sha256:0, but this checkout plants sha256:" in message
    assert "Python 3.12" in message
    assert not (tmp_path / "out").exists()


def test_the_generator_refuses_another_worlds_ids(tmp_path, ids_files) -> None:
    with pytest.raises(SystemExit, match="ids of world 'ferry', not 'orbit'"):
        generator.main(arguments(tmp_path / "out", {"orbit": ids_files["ferry"]}))


@pytest.mark.parametrize(
    ("text", "message"),
    [("{", "is not JSON"), ("[]", "holds no JSON object")],
)
def test_the_generator_refuses_ids_that_are_not_a_json_object(tmp_path, text, message) -> None:
    path = tmp_path / "orbit.json"
    path.write_text(text, encoding="utf-8")
    with pytest.raises(SystemExit, match=message):
        generator.main(arguments(tmp_path / "out", {"orbit": path}))


def test_the_generator_names_a_missing_ids_file(tmp_path) -> None:
    with pytest.raises(SystemExit, match=r"cannot read .*/missing\.json: No such file"):
        generator.main(arguments(tmp_path / "out", {"orbit": tmp_path / "missing.json"}))


def fake_docker(folder: Path, script: str) -> None:
    """Put a `docker` running `script` first on PATH."""
    folder.mkdir()
    docker = folder / "docker"
    docker.write_text(f"#!/bin/bash\n{script}\n", encoding="utf-8")
    docker.chmod(0o755)


def test_the_generator_reads_ids_from_the_world_image(tmp_path, ids_files, monkeypatch) -> None:
    fake_docker(tmp_path / "bin", f'echo "$*" > {tmp_path}/called; cat {ids_files["kestrel"]}')
    monkeypatch.setenv("PATH", f"{tmp_path / 'bin'}:{os.environ['PATH']}")
    out = tmp_path / "out"
    generator.main([*arguments(out, {}), "--world", "kestrel=registry.test/gitea@sha256:1"])
    called = (tmp_path / "called").read_text(encoding="utf-8")
    assert called == "run --rm registry.test/gitea@sha256:1 cat /opt/gitea-env/ids.json\n"
    assert len(task_dirs(out)) == 40


def test_the_generator_says_why_docker_failed(tmp_path, monkeypatch) -> None:
    fake_docker(tmp_path / "bin", "echo 'Unable to find image' >&2; exit 125")
    monkeypatch.setenv("PATH", f"{tmp_path / 'bin'}:{os.environ['PATH']}")
    command = [*arguments(tmp_path / "out", {}), "--world", "orbit=gitea-env:orbit"]
    with pytest.raises(SystemExit, match=r"Unable to find image\. Build the world image first"):
        generator.main(command)
    monkeypatch.setenv("PATH", str(tmp_path / "nowhere"))
    with pytest.raises(SystemExit, match="docker is not on PATH"):
        generator.main(command)


@pytest.mark.parametrize(
    ("extra", "message"),
    [
        (["--world", "atlantis=x"], "unknown worlds"),
        (["--world", "orbit=x", "--world", "orbit=y"], "give each world once"),
        (["--world", "orbit=x", "--ids", "ferry=y.json"], "which no --world gives"),
        (["--world", "orbit"], "is not NAME=VALUE"),
    ],
)
def test_the_generator_refuses_a_bad_command_line(tmp_path, capsys, extra, message) -> None:
    with pytest.raises(SystemExit) as refused:
        generator.main(arguments(tmp_path / "out", {}) + extra)
    assert refused.value.code == 2
    assert message in capsys.readouterr().err


def test_the_generator_replaces_only_its_own_output(tmp_path, ids_files) -> None:
    out = tmp_path / "out"
    out.mkdir()
    (out / "notes.txt").write_text("mine", encoding="utf-8")
    kestrel = {"kestrel": ids_files["kestrel"]}
    with pytest.raises(SystemExit, match=r"holds files tasks\.py did not write"):
        generator.main(arguments(out, kestrel))
    (out / "notes.txt").unlink()
    generator.main(arguments(out, kestrel))
    stale = out / "test" / "gone.kestrel.9"
    stale.mkdir()
    generator.main(arguments(out, kestrel))
    assert not stale.exists() and len(task_dirs(out)) == 40
    with pytest.raises(SystemExit, match="is a file"):
        generator.main(arguments(out / ".generated", kestrel))


def synthetic(**fields: object) -> dict:
    """A task as families.py writes one, with what a test needs changed."""
    return {
        "id": "made-up.orbit.1",
        "split": "train",
        "family": "made-up",
        "description": "A task made up for a test.",
        "note": "",
        "instruction": "Do it.",
        "answer": None,
        "spec": {"checks": [{"name": "done", "weight": 1.0, "sql": "SELECT 1"}], "gates": []}
        | {"allow": []},
        "golden": [{"method": "GET", "path": "/version"}],
        "probes": [],
    } | fields


def test_toml_strings_survive_any_text() -> None:
    value = 'a "quote", a \\ and a tab\there,\na new line, DEL \x7f, \x01, é and 𝄞'
    assert tomllib.loads(f"x = {generator.toml_string(value)}")["x"] == value


def test_a_task_without_probes_has_no_probe_tables() -> None:
    text = generator.task_toml(synthetic(), generator.Images("w", "m", "g"), "o")
    assert "probe" not in text and "\n\n\n" not in text
    assert "probes" not in tomllib.loads(text)["metadata"]["miroir"]


def test_a_spec_holding_three_quotes_is_refused() -> None:
    spec = {"checks": [{"name": "done", "weight": 1.0, "sql": "SELECT '''"}], "gates": []}
    task = synthetic(spec=spec | {"allow": []})
    with pytest.raises(SystemExit, match=r"made-up\.orbit\.1: its spec holds '''"):
        generator.task_toml(task, generator.Images("w", "m", "g"), "o")


def test_steps_become_quoted_helper_calls() -> None:
    assert generator.command({"answer": "it's 3"}) == (
        "printf '%s\\n' 'it'\"'\"'s 3' >/logs/artifacts/answer.txt"
    )
    assert generator.command({"method": "DELETE", "path": "/a?b=1&c=2", "status": 404}) == (
        "expect 404 DELETE '/a?b=1&c=2'"
    )
    merge = {"method": "POST", "path": "/repos/o/r/pulls/1/merge", "body": {"do": "merge"}}
    assert generator.command(merge) == 'merge /repos/o/r/pulls/1/merge \'{"do": "merge"}\''
    assert generator.command({"method": "PATCH", "path": "/x", "body": {}}) == "gitea PATCH /x '{}'"


def run_script(tmp_path: Path, steps: list[dict], **stub: str) -> tuple[int, list[str], str]:
    """Run a probe script of `steps` against the stub curl; its exit status, the calls the stub
    got, and what the script wrote to stderr. `sleep` is a stub too, logging to sleeps."""
    world = worlds.WORLDS["orbit"]()
    script = tmp_path / "probe.sh"
    script.write_text(
        generator.script("#!/bin/bash\nset -euo pipefail\n", steps, world, golden=False)
    )
    stubs = tmp_path / "stubs"
    stubs.mkdir(exist_ok=True)
    sleep = '#!/bin/bash\nprintf \'%s\\n\' "$*" >>"$STUB_SLEEPS"\n'
    for name, text in (("curl", STUB_CURL), ("sleep", sleep)):
        (stubs / name).write_text(text, encoding="utf-8")
        (stubs / name).chmod(0o755)
    log = tmp_path / "calls"
    log.write_text("", encoding="utf-8")
    env = os.environ | {"PATH": f"{stubs}:{os.environ['PATH']}", "STUB_LOG": str(log)}
    env |= {"STUB_SLEEPS": str(tmp_path / "sleeps"), "STUB_FAILURES": "0", "STUB_BODY": ""}
    env |= {"STUB_EXIT": "22", "STUB_OK": "{}"} | stub
    done = subprocess.run(
        ["bash", str(script)], check=False, env=env, capture_output=True, text=True
    )
    return done.returncode, log.read_text(encoding="utf-8").splitlines(), done.stderr


MERGE = {"method": "POST", "path": "/repos/orbit/api/pulls/7/merge", "body": {"do": "merge"}}
LATER = '{"message":"Please try again later"}'


def test_a_merge_is_retried_while_gitea_rechecks(tmp_path) -> None:
    status, calls, _ = run_script(tmp_path, [MERGE], STUB_FAILURES="3", STUB_BODY=LATER)
    assert status == 0
    assert len(calls) == 4
    assert calls[0].startswith("-sS --fail-with-body -X POST")
    assert "http://gitea:3000/api/v1/repos/orbit/api/pulls/7/merge" in calls[0]
    assert '--data {"do": "merge"}' in calls[0]
    assert (tmp_path / "sleeps").read_text(encoding="utf-8").splitlines() == ["1", "1", "1"]


def test_a_merge_gives_up_after_ten_retries(tmp_path) -> None:
    status, calls, stderr = run_script(tmp_path, [MERGE], STUB_FAILURES="99", STUB_BODY=LATER)
    assert status != 0
    assert len(calls) == 11
    assert len((tmp_path / "sleeps").read_text(encoding="utf-8").splitlines()) == 10
    assert "Please try again later" in stderr


def test_a_merge_refused_for_another_reason_fails_at_once(tmp_path) -> None:
    body = '{"message":"not mergeable"}'
    status, calls, stderr = run_script(tmp_path, [MERGE], STUB_FAILURES="99", STUB_BODY=body)
    assert (status, len(calls)) == (1, 1)
    assert "not mergeable" in stderr


def test_a_request_fails_the_script_on_an_http_error(tmp_path) -> None:
    steps = [{"method": "GET", "path": "/version"}, {"method": "GET", "path": "/user"}]
    status, calls, _ = run_script(tmp_path, steps, STUB_FAILURES="1")
    assert status != 0
    token = worlds.token("orbit", "dana")
    assert calls == [
        f"-fsS -X GET http://gitea:3000/api/v1/version -H Authorization: token {token}"
    ]


def test_a_step_with_a_status_asserts_it(tmp_path) -> None:
    step = {"method": "DELETE", "path": "/repos/orbit/api", "status": 404}
    assert run_script(tmp_path, [step], STUB_OK="404")[0] == 0
    status, _, stderr = run_script(tmp_path, [step], STUB_OK="204")
    assert status != 0
    assert "DELETE /repos/orbit/api answered 204, not 404" in stderr
