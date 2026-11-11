"""Checks of the worlds and task families that need no Gitea.

`miroir check` replays every task against Gitea itself; these catch a broken generator in a
second. Run them from this directory:

    uv run --with pyyaml==6.0.3 --with pytest==9.2.0 pytest -q
"""

import itertools
from pathlib import Path

import families
import pytest
import seed
import worlds
import yaml

MANIFEST = Path(__file__).parent.parent / "miroir.yaml"


def fake_ids(world: worlds.World) -> families.Ids:
    """Ids as Gitea hands them out: one sequence per table, in the order the seed creates rows."""
    users = {seed.ADMIN: 1} | {login: i for i, login in enumerate(world.people, start=2)}
    org = len(users) + 1
    count = itertools.count(1)
    issues = {(i.repo, i.number): next(count) for i in world.issues}
    count = itertools.count(1)
    for issue in world.issues:
        issue.comment_ids = [next(count) for _ in issue.comments]
    labels: dict[tuple[str, str], int] = {}
    milestones: dict[tuple[str, str], int] = {}
    for repo in world.repos.values():
        for name, *_ in world.scheme.labels(repo):
            labels[(repo.name, name)] = len(labels) + 1
        if repo.release:
            for title in (repo.release.milestone, repo.release.next_milestone):
                milestones[(repo.name, title)] = len(milestones) + 1
    return families.Ids(
        org=org,
        users=users | {world.org: org},
        repos={name: i for i, name in enumerate(world.repos, start=1)},
        issues=issues,
        labels=labels,
        milestones=milestones,
        teams={name: i for i, name in enumerate(world.teams, start=2)},
        next_hook=len(world.hooks) + 1,
    )


@pytest.fixture(scope="module", params=list(worlds.WORLDS))
def built(request: pytest.FixtureRequest) -> tuple[worlds.World, list[dict]]:
    world, plans = seed.build(request.param)
    return world, families.tasks(world, fake_ids(world), plans)


def test_every_family_asks_four_ways_in_every_world(built) -> None:
    world, tasks = built
    by_family = {
        f.name: [t for t in tasks if t["id"].startswith(f"{f.name}.")] for f in families.FAMILIES
    }
    assert all(len(found) == families.INSTANCES for found in by_family.values()), by_family.keys()
    assert len({t["id"] for t in tasks}) == len(tasks)
    assert all(t["world"] == world.name and t["persona"] == world.manager for t in tasks)


def test_a_test_world_holds_only_test_tasks_and_held_out_families_never_train(built) -> None:
    world, tasks = built
    splits = {t["split"] for t in tasks}
    if world.split == "test":
        assert splits == {"test"}
        return
    trained = {f.name for f in families.FAMILIES if f.trained}
    for task in tasks:
        family = task["id"].split(".")[0]
        assert task["split"] in (("train", "validation") if family in trained else ("validation",))
    assert sum(t["split"] == "train" for t in tasks) == len(trained) * (families.INSTANCES - 1)


def test_tasks_are_well_formed(built) -> None:
    _, tasks = built
    for task in tasks:
        weights = [c["weight"] for c in task["checks"] if "weight" in c]
        assert abs(sum(weights) - 1) < 1e-9, task["id"]
        assert len({c["name"] for c in task["checks"]}) == len(task["checks"]), task["id"]
        for steps in [task["golden"], *(p["steps"] for p in task["probes"])]:
            assert [s["tool"] for s in steps].count("submit") == 1, task["id"]
            assert steps[-1]["tool"] == "submit", task["id"]
            assert len(steps) <= task["max_steps"], task["id"]
        assert len({p["name"] for p in task["probes"]}) == len(task["probes"]), task["id"]
        assert set(task) - {"id", "note"} <= set(seed.FIELDS), task["id"]


def test_answers_are_unambiguous(built) -> None:
    world, tasks = built
    for task in tasks:
        if task.get("answer") is None:
            continue
        golden = task["golden"][-1]["arguments"]["answer"]
        assert golden, task["id"]
        for found in task["probes"]:
            assert found["steps"][-1]["arguments"]["answer"] != golden, task["id"]
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
            step["arguments"]["path"].split("/pulls/")[0]
            for step in task["golden"]
            if step["tool"] == "api_request" and step["arguments"]["path"].endswith("/merge")
        ]
        assert len(merged) == len(set(merged)), task["id"]


def test_a_probe_that_reaches_another_issue_changes_it(built) -> None:
    """Assigning someone where they already are changes nothing, and earns no harm."""
    world, tasks = built
    for task in tasks:
        for found in task["probes"]:
            if found["name"] != "triages-a-neighbor-too":
                continue
            step = found["steps"][-2]["arguments"]
            repo, number = step["path"].split("/")[3], int(step["path"].split("/")[-1])
            issue = next(i for i in world.in_repo(repo) if i.number == number)
            assert step["body"]["assignees"] != issue.assignees, task["id"]


def test_planted_work_stays_with_its_plan(built) -> None:
    """A leaver's open work is only what the offboarding plan gave them."""
    world, tasks = built
    for task in tasks:
        if task["id"].startswith("offboard."):
            leaver = task["instruction"].split("(")[1].split(")")[0]
            work = [i for i in world.issues if i.is_open and leaver in i.assignees]
            assert 4 <= len(work) <= 6, (leaver, [(i.repo, i.number, i.title) for i in work])


def test_worlds_build_the_same_every_time() -> None:
    for name in worlds.WORLDS:
        first, again = seed.build(name)[0], seed.build(name)[0]
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


def test_personas_hold_the_tokens_the_seed_stores() -> None:
    manifest = yaml.safe_load(MANIFEST.read_text(encoding="utf-8"))
    for name, make in worlds.WORLDS.items():
        world = make()
        persona = manifest["personas"][world.manager]
        expected = f"token {worlds.token(name, world.manager)}"
        assert persona["headers"]["gitea"]["Authorization"] == expected
    assert manifest["worlds"] == list(worlds.WORLDS)


def test_every_assignee_can_be_assigned(built) -> None:
    """Gitea refuses to assign someone without write access to the repository."""
    world, _ = built
    for issue in world.issues:
        for login in issue.assignees:
            assert world.access(login, issue.repo) in ("write", "admin"), (issue.repo, login)
    for repo in world.repos.values():
        for _, login in repo.owners:
            assert world.access(login, repo.name) in ("write", "admin"), (repo.name, login)
