import itertools
import json
from pathlib import Path

import pytest

from miroir.calibrate import calibrated, pass_at, pass_hat
from miroir.cli import main

HASH = "a" * 16


def rollout(task: str, reward: float | None, *, harm: bool = False, task_hash: str = HASH) -> dict:
    metadata = {"task_hash": task_hash, "success": reward == 1.0, "harm": harm}
    if reward is None:
        metadata = {"task_hash": task_hash, "success": False, "invalid": "grading failed"}
    return {
        "episode_id": f"{task}-{reward}",
        "task_id": task,
        "reward": reward,
        "signature": None if reward is None else "b" * 16,
        "metadata": metadata,
        "actions": [],
    }


def traces(tmp_path: Path, *records: dict) -> Path:
    folder = tmp_path / "traces"
    folder.mkdir(exist_ok=True)
    lines = "".join(json.dumps(each) + "\n" for each in records)
    (folder / "20261104T100000Z-host-1.jsonl").write_text(lines, encoding="utf-8")
    return folder


def calibrate(folder: Path, out: Path, *flags: str) -> dict:
    main(["calibrate", str(folder), "--model", "m", "--out", str(out), *flags])
    return json.loads(out.read_text(encoding="utf-8"))


@pytest.mark.parametrize("n", range(1, 7))
def test_the_estimators_match_counting_every_draw_of_k_rollouts(n):
    for c in range(n + 1):
        outcomes = [True] * c + [False] * (n - c)
        for k in range(1, n + 1):
            draws = list(itertools.combinations(outcomes, k))
            assert pass_at(n, c, k) == pytest.approx(sum(map(any, draws)) / len(draws))
            assert pass_hat(n, c, k) == pytest.approx(sum(map(all, draws)) / len(draws))
        assert pass_at(n, c, n + 1) is None and pass_hat(n, c, n + 1) is None


def test_each_task_gets_its_rates_and_class(tmp_path, capsys):
    folder = traces(
        tmp_path,
        *(rollout("learn", reward) for reward in (1.0, 0.0, 0.0, 0.0)),
        *(rollout("solved", 1.0) for _ in range(4)),
        *(rollout("stuck", 0.5) for _ in range(4)),
        *(rollout("zero", 0.0) for _ in range(4)),
        rollout("harm", 0.0, harm=True),
        *(rollout("harm", 1.0) for _ in range(3)),
        *(rollout("flaky", None) for _ in range(3)),
        rollout("flaky", 1.0),
    )
    found = calibrate(folder, tmp_path / "out" / "calibration.json")
    assert (found["model"], found["k"]) == ("m", 4), "k is the most graded rollouts of a task"
    tasks = found["tasks"]
    assert list(tasks) == ["flaky", "harm", "learn", "solved", "stuck", "zero"]
    assert tasks["learn"] == {
        "task_hash": HASH,
        "class": "learnable",
        "rollouts": 4,
        "void": 0,
        "pass@1": 0.25,
        "pass@k": 1.0,
        "pass^k": 0.0,
        "reward_mean": 0.25,
        "reward_variance": 0.1875,
        "harm_rate": 0.0,
    }
    classes = {task_id: entry["class"] for task_id, entry in tasks.items()}
    assert classes == {
        "flaky": None,
        "harm": "learnable",
        "learn": "learnable",
        "solved": "solved",
        "stuck": "out_of_reach",
        "zero": "out_of_reach",
    }, "partial credit that ties is no signal, and one graded rollout can't differ"
    assert (tasks["harm"]["harm_rate"], tasks["harm"]["pass@1"]) == (0.25, 0.75)
    assert (tasks["solved"]["pass^k"], tasks["stuck"]["reward_mean"]) == (1.0, 0.5)
    flaky = {key: tasks["flaky"][key] for key in ("rollouts", "void", "pass@k")}
    assert flaky == {"rollouts": 1, "void": 3, "pass@k": None}, "void rollouts count apart"
    shown = [" ".join(line.split()) for line in capsys.readouterr().out.splitlines()]
    assert shown[0] == "task n void pass@1 pass@4 pass^4 reward var harm class"
    assert shown[1] == "flaky 1 3 1.00 - - 1.00 0.00 0.00 -"
    assert shown[3] == "learn 4 0 0.25 1.00 0.00 0.25 0.19 0.00 learnable"
    assert shown[7:10] == [
        "6 tasks: 1 solved, 2 learnable, 2 out of reach",
        "no class, with fewer than 2 graded rollouts: flaky",
        "3 of 24 rollouts (12.5%) void: miroir couldn't grade",
    ]
    again = calibrate(folder, tmp_path / "k2.json", "-k", "2")
    assert (again["k"], again["tasks"]["learn"]["pass@k"]) == (2, 0.5), "1 - C(3,2)/C(4,2)"


def test_rollouts_of_two_versions_of_a_task_are_refused(tmp_path):
    folder = traces(tmp_path, rollout("t", 1.0), rollout("t", 0.0, task_hash="c" * 16))
    out = tmp_path / "calibration.json"
    with pytest.raises(SystemExit, match=f"two versions of the task \\(task_hash {HASH} and c"):
        calibrate(folder, out)
    assert not out.exists()


def test_without_two_graded_rollouts_of_a_task_nothing_is_written(tmp_path):
    folder = traces(tmp_path, rollout("t1", 1.0), rollout("t2", 0.0), rollout("t2", None))
    out = tmp_path / "calibration.json"
    with pytest.raises(SystemExit, match="no task has two graded rollouts"):
        calibrate(folder, out)
    assert not out.exists()
    with pytest.raises(SystemExit, match="k must be at least 1"):
        calibrate(folder, out, "-k", "0")


LISTED = [{"id": "t1", "task_hash": HASH}, {"id": "t2", "task_hash": HASH}]


def calibration(tmp_path: Path, classes: dict[str, str | None], task_hash: str = HASH) -> Path:
    path = tmp_path / "calibration.json"
    tasks = {task: {"task_hash": task_hash, "class": kind} for task, kind in classes.items()}
    path.write_text(json.dumps({"model": "m", "k": 4, "tasks": tasks}), encoding="utf-8")
    return path


def test_the_filter_keeps_the_tasks_of_the_classes_asked_for(tmp_path):
    path = calibration(tmp_path, {"t1": "out_of_reach", "t2": "learnable", "t3": "solved"})
    assert calibrated(LISTED, path) == [LISTED[1]], "learnable by default"
    assert calibrated(LISTED, str(path), ["solved", "out_of_reach"]) == [LISTED[0]]


@pytest.mark.parametrize(
    ("classes", "listed", "keep", "expected"),
    [
        (
            {"t1": "learnable"},
            LISTED,
            ["learnable"],
            "no entry for t2. Calibrate these tasks again",
        ),
        ({"t1": "solved", "t2": None}, LISTED, ["learnable"], "1 solved, 1 without a class"),
        ({"t1": "learnable"}, LISTED[:1], ["learnabel"], "keep must name classes"),
        ({"t1": "learnable"}, LISTED[:1], [], "keep must name classes"),
    ],
)
def test_the_filter_refuses_what_would_train_on_the_wrong_tasks(
    tmp_path, classes, listed, keep, expected
):
    path = calibration(tmp_path, classes)
    with pytest.raises(ValueError, match=expected):
        calibrated(listed, path, keep)


def test_the_filter_refuses_a_task_that_changed_since_it_was_calibrated(tmp_path):
    path = calibration(tmp_path, {"t1": "learnable", "t2": "learnable"}, task_hash="c" * 16)
    with pytest.raises(ValueError, match="t1, t2 changed since it was written"):
        calibrated(LISTED, path)


@pytest.mark.parametrize(
    ("content", "expected"),
    [
        (None, "cannot read the calibration"),
        ("not json", "is not a calibration that `miroir calibrate` wrote: JSONDecodeError"),
        ('{"tasks": {"t1": {"task_hash": "x"}}}', "each task needs a task_hash and a class"),
        ("[]", "is not a calibration"),
    ],
)
def test_the_filter_refuses_a_file_that_is_no_calibration(tmp_path, content, expected):
    path = tmp_path / "calibration.json"
    if content is not None:
        path.write_text(content, encoding="utf-8")
    with pytest.raises(ValueError, match=expected):
        calibrated(LISTED, path)
