import builtins
import io
import json
import sys
from pathlib import Path
from typing import Any

import pytest
import yaml

from miroir.cli import main

PATCH = {"method": "PATCH", "path": "/repos/acme/infra/issues/3", "body": {"assignees": ["dan"]}}


def action(tool: str, arguments: Any, status: int | None = None, result: Any = None) -> dict:
    return {
        "tool": tool,
        "method": None,
        "path": None,
        "status": status,
        "latency_ms": 4,
        "arguments": arguments,
        "result": result,
    }


def record(task: str, signature: str | None, reward: float | None, **extra: Any) -> dict:
    checks = [
        {"name": "assigned", "score": reward or 0.0},
        {"name": "scope", "gate": True, "score": 1},
    ]
    return {
        "episode_id": f"{task}-{signature}-{reward}",
        "task_id": task,
        "reward": reward,
        "null_max": 0.0,
        "signature": signature,
        "diagnostics": {"calls": 3},
        "metadata": {"termination": "submit", "task_hash": "f" * 16, "checks": checks},
        "actions": [
            action("api_docs", {}, result=[]),
            action("api_request", PATCH, 201, {"status": 201}),
            action("submit", {"answer": "done 🎉\x7f\uffff"}, result={"submitted": True}),
        ],
        "changes": [
            {
                "tbl": "issue",
                "op": "update",
                "pk": {"id": 36},
                "before": {"assignee": 0},
                "after": {"assignee": 6},
                "columns": ["assignee"],
            }
        ],
        "effects": [{"kind": "email", "data": {"to": ["dan@acme.test"], "subject": "Assigned"}}],
        "limit_bytes": 4194304,
        **extra,
    }


def traces(tmp_path: Path, *records: dict) -> Path:
    folder = tmp_path / "traces"
    folder.mkdir(exist_ok=True)
    lines = "".join(json.dumps(each) + "\n" for each in records)
    (folder / "20261104T100000Z-host-1.jsonl").write_text(lines, encoding="utf-8")
    return folder


def review(tmp_path: Path, folder: Path, *flags: str) -> None:
    main(["review", str(folder), "--tasks", str(tmp_path / "tasks"), *flags])


def queue(tmp_path: Path, folder: Path, capsys: pytest.CaptureFixture[str]) -> dict:
    capsys.readouterr()
    review(tmp_path, folder, "--json")
    return json.loads(capsys.readouterr().out)


def probes(tmp_path: Path, task: str) -> list[dict]:
    return yaml.safe_load((tmp_path / "tasks" / "probes" / f"{task}.yaml").read_text("utf-8"))


def test_rollouts_group_into_end_states_most_rewarded_first(tmp_path, capsys):
    folder = traces(
        tmp_path,
        record("t2", "b" * 16, 0.0),
        record("t2", "a" * 16, 1.0),
        record("t2", "c" * 16, 0.5),
        record("t2", "a" * 16, 1.0),
        record("t1", "d" * 16, 0.0),
        record("t2", None, 0.0, metadata={"fault": "process 'gitea' exited with code 2"}),
        record("t2", None, None, metadata={"invalid": "grading failed"}),
    )
    found = queue(tmp_path, folder, capsys)
    assert [(g["group"], g["rollouts"]) for g in found["queue"]] == [
        ("t1@" + "d" * 16, 1),
        ("t2@" + "a" * 16, 2),
        ("t2@" + "c" * 16, 1),
        ("t2@" + "b" * 16, 1),
    ]
    assert found["queue"][1]["rewards"] == {"1": 2}
    assert found["faults"] == {"t2": {"rollouts": 1, "fault": "process 'gitea' exited with code 2"}}
    assert found["void"] == 1, "rollouts miroir could not grade have no end state either"


def test_a_label_holds_and_a_wrong_grade_becomes_a_probe_check_loads(tmp_path, capsys):
    hack, valid = "a" * 16, "b" * 16
    folder = traces(
        tmp_path, record("t", hack, 1.0), record("t", hack, 1.0), record("t", valid, 0.0)
    )
    review(
        tmp_path, folder, "--label", f"t@{hack}=should_fail", "--label", f"t@{valid}=should_pass"
    )
    said = capsys.readouterr().err
    assert f"t@{hack}: should_fail, probe review-aaaaaaaaaaaaaaaa" in said, said
    steps = [
        {"tool": "api_docs", "arguments": {}},
        {"tool": "api_request", "arguments": PATCH},
        {"tool": "submit", "arguments": {"answer": "done 🎉\x7f\uffff"}},
    ]
    assert probes(tmp_path, "t") == [
        {"name": "review-aaaaaaaaaaaaaaaa", "steps": steps, "max_reward": 0.0},
        {"name": "review-bbbbbbbbbbbbbbbb", "steps": steps, "min_reward": 1},
    ]
    assert queue(tmp_path, folder, capsys)["queue"] == [], "each end state is labeled once"
    review(tmp_path, folder, "--label", f"t@{hack}=should_fail")
    assert len(probes(tmp_path, "t")) == 2, "labeling again adds no second probe"
    labels = (tmp_path / "tasks" / "probes" / "labels.jsonl").read_text("utf-8").splitlines()
    first = json.loads(labels[0])
    assert (first["label"], first["rollouts"], first["reward"]) == ("should_fail", 2, 1.0)
    assert first["task_hash"] == "f" * 16 and len(labels) == 3


def test_an_episode_that_ran_out_of_steps_replays_with_a_closing_submit(tmp_path, capsys):
    actions = [
        action("submit", {"answer": 3}, result={"error_type": "invalid_args"}),
        action("teleport", {}, result={"error_type": "tool_not_found"}),
        action("api_request", PATCH, 201, {"status": 201}),
    ]
    ran_out = record("t", "a" * 16, 0.0, actions=actions, metadata={"termination": "max_steps"})
    review(tmp_path, traces(tmp_path, ran_out), "--label", f"t@{'a' * 16}=should_pass")
    [probe] = probes(tmp_path, "t")
    assert probe["steps"] == [
        {"tool": "api_request", "arguments": PATCH},
        {"tool": "submit", "arguments": {}},
    ], "failed submits and unknown tools changed nothing"


def test_an_apps_mcp_calls_replay_in_its_probe(tmp_path, capsys):
    moved = {"deal_id": 1100, "status": "won"}
    actions = [
        action("update_deal", moved, result={"content": [{"type": "text", "text": "{}"}]}),
        action("submit", {}, result={"submitted": True}),
    ]
    through_mcp = record("t", "a" * 16, 0.0, actions=actions)
    review(tmp_path, traces(tmp_path, through_mcp), "--label", f"t@{'a' * 16}=should_pass")
    [probe] = probes(tmp_path, "t")
    assert probe["steps"] == [
        {"tool": "update_deal", "arguments": moved},
        {"tool": "submit", "arguments": {}},
    ]


def test_code_replays_as_its_run_not_as_the_requests_it_made(tmp_path, capsys):
    from miroir.review import call

    command = {"command": "curl -X PATCH $API_CRM/deals/1100"}
    made = {**action("api_request", PATCH, 200, {"status": 200}), "via": "run_code"}
    actions = [
        made,
        action("run_code", command, result={"exit_code": 0, "output": ""}),
        action("submit", {}, result={"submitted": True}),
    ]
    coded = record("t", "c" * 16, 0.0, actions=actions)
    review(tmp_path, traces(tmp_path, coded), "--label", f"t@{'c' * 16}=should_pass")
    [probe] = probes(tmp_path, "t")
    assert probe["steps"] == [
        {"tool": "run_code", "arguments": command},
        {"tool": "submit", "arguments": {}},
    ], "the run makes its requests again, so they replay once"
    assert call(made).startswith("(code) PATCH /repos/acme/infra/issues/3 ")


def test_a_wait_replays_and_the_events_it_reached_fire_again_on_their_own(tmp_path, capsys):
    from miroir.review import call

    waited = {"minutes": 600}
    event = {
        **action("event", {"event": "signed"}, 201, {"status": 201}),
        "path": "/deals/1100/notes",
        "via": "event",
    }
    actions = [
        event,
        action("wait", waited, result={"waited_minutes": 90, "elapsed_minutes": 90}),
        action("api_request", PATCH, 200, {"status": 200}),
        action("submit", {}, result={"submitted": True}),
    ]
    clocked = record("t", "e" * 16, 0.0, actions=actions)
    review(tmp_path, traces(tmp_path, clocked), "--label", f"t@{'e' * 16}=should_pass")
    [probe] = probes(tmp_path, "t")
    assert probe["steps"] == [
        {"tool": "wait", "arguments": waited},
        {"tool": "api_request", "arguments": PATCH},
        {"tool": "submit", "arguments": {}},
    ], "the replay's wait fires the event itself"
    assert call(event) == "event signed → 201"


def test_a_probe_submits_the_answer_as_grading_stored_it(tmp_path, capsys):
    sent = action("submit", {"answer": "2026-11-12"}, result={"submitted": True})
    metadata = {"termination": "submit", "task_hash": "f" * 16, "answer": "2026-03-31"}
    dated = record("t", "a" * 16, 1.0, actions=[sent], metadata=metadata)
    review(tmp_path, traces(tmp_path, dated), "--label", f"t@{'a' * 16}=should_fail")
    [probe] = probes(tmp_path, "t")
    assert probe["steps"] == [{"tool": "submit", "arguments": {"answer": "2026-03-31"}}], (
        "a date as of the anchor day, which check moves to its own"
    )


def test_a_record_cut_before_its_arguments_fit_gets_its_label_but_no_probe(tmp_path, capsys):
    cut = record("t", "a" * 16, 1.0, truncated={"arguments": 1})
    review(tmp_path, traces(tmp_path, cut), "--label", f"t@{'a' * 16}=should_fail")
    assert "but no probe: its record was cut" in capsys.readouterr().err
    assert not (tmp_path / "tasks" / "probes" / "t.yaml").exists()
    assert (tmp_path / "tasks" / "probes" / "labels.jsonl").read_text("utf-8").count("\n") == 1


class Terminal:
    def isatty(self) -> bool:
        return True


def test_the_reviewer_labels_end_states_in_turn(tmp_path, capsys, monkeypatch):
    folder = traces(
        tmp_path, record("t", "a" * 16, 1.0), record("t", "b" * 16, 0.5), record("t", "c" * 16, 0.0)
    )
    answers = iter(["maybe", "c", "s", "q"])
    monkeypatch.setattr(sys, "stdin", Terminal())
    monkeypatch.setattr(builtins, "input", lambda _: next(answers))
    review(tmp_path, folder)
    shown = capsys.readouterr().out
    assert "[1/3] t@aaaaaaaaaaaaaaaa: 1 rollout, reward 1" in shown
    assert 'update issue {"id": 36}: assignee 0 → 6' in shown
    assert "email to dan@acme.test: Assigned" in shown
    assert "checks: assigned 1, scope 1 (gate)" in shown
    assert f'2. PATCH {PATCH["path"]} {{"assignees": ["dan"]}} → 201' in shown
    assert "[3/3]" in shown and shown.rstrip().endswith(
        "1 of 3 end states labeled: 1 correct, 0 should fail, 0 should pass; agreement 100%"
    ), shown


def test_without_a_terminal_or_a_flag_it_says_what_to_pass(tmp_path, monkeypatch):
    monkeypatch.setattr(sys, "stdin", io.StringIO("c\n"))
    with pytest.raises(SystemExit, match="pass --json to print the queue, or --label"):
        review(tmp_path, traces(tmp_path, record("t", "a" * 16, 1.0)))


@pytest.mark.parametrize(
    ("line", "expected"),
    [("not json", "not JSON"), ('{"episode_id": "e"}', "not a Miroir episode record")],
)
def test_a_line_that_is_no_record_names_its_file_and_line(tmp_path, line, expected):
    folder = traces(tmp_path, record("t", "a" * 16, 1.0))
    (folder / "bad.jsonl").write_text(f"\n{line}\n", encoding="utf-8")
    with pytest.raises(SystemExit, match=f"bad.jsonl:2: {expected}"):
        review(tmp_path, folder, "--json")


def test_a_label_for_an_end_state_not_in_the_traces_is_refused(tmp_path):
    folder = traces(tmp_path, record("t", "a" * 16, 1.0))
    with pytest.raises(SystemExit, match="no end state t@zzz"):
        review(tmp_path, folder, "--label", "t@zzz=correct")
    with pytest.raises(SystemExit, match="expected TASK@SIGNATURE=LABEL"):
        review(tmp_path, folder, "--label", f"t@{'a' * 16}=fine")


def test_a_change_and_a_call_name_their_app():
    from miroir.review import call, change

    one = {"op": "delete", "tbl": "deals", "pk": {"id": 1}, "before": None, "after": None}
    assert change(one) == 'delete deals {"id": 1}'
    assert change({**one, "app": "west"}) == 'delete west.deals {"id": 1}'
    request = action("api_request", {"app": "west", "method": "GET", "path": "/me"}, status=200)
    assert call({**request, "app": "west"}) == "west: GET /me → 200"
    assert (
        call(action("api_request", {"method": "GET", "path": "/me"}, status=200)) == "GET /me → 200"
    )
