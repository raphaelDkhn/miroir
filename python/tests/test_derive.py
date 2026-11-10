import json
import sqlite3
import subprocess
from pathlib import Path
from typing import Any

import pytest
import yaml
from conftest import IMAGE, ROOT, env_with_tasks

from miroir import derive
from miroir.cli import main

TRACE: dict[str, Any] = {
    "id": "inc-1004",
    "consent": "support terms 4.2, opt-in",
    "request": "Deal 2001 should move to Marcus Webb, with the usual email.",
    "calls": [
        {"tool": "api_request", "arguments": {"method": "GET", "path": "/users?role=rep"}},
        {
            "tool": "api_request",
            "arguments": {"method": "PATCH", "path": "/deals/2001", "body": {"owner_id": 7}},
            "status": 200,
        },
    ],
    "answer": "Done.",
}
NOW = "2026-11-09T12:00:00.123456+00:00"
LATER = "2026-11-09T12:00:04.654321+00:00"


def write_traces(path: Path, *traces: dict[str, Any]) -> Path:
    path.write_text("".join(json.dumps(trace) + "\n" for trace in traces), encoding="utf-8")
    return path


def update(table: str, pk: dict[str, Any], before: dict[str, Any], after: dict[str, Any]) -> dict:
    columns = sorted(key for key in after if before.get(key) != after[key])
    return {
        "tbl": table,
        "op": "update",
        "pk": pk,
        "before": before,
        "after": after,
        "columns": columns,
    }


def insert(table: str, pk: dict[str, Any], after: dict[str, Any]) -> dict:
    return {"tbl": table, "op": "insert", "pk": pk, "before": None, "after": after, "columns": None}


def email(to: list[str], subject: str) -> dict:
    return {
        "kind": "email",
        "data": {
            "from": "noreply@acme.test",
            "to": to,
            "cc": [],
            "bcc": [],
            "subject": subject,
            "text": "Hi",
        },
    }


def record(changes: list[dict], effects: list[dict], **extra: Any) -> dict[str, Any]:
    return {
        "reward": 0.0,
        "metadata": {"termination": "submit"},
        "changes": changes,
        "effects": effects,
        **extra,
    }


def reassigned(stamp: str) -> dict[str, Any]:
    deal = update(
        "deals",
        {"id": 2001},
        {"id": 2001, "owner_id": 4, "updated_at": "2026-11-01T10:00:00+00:00"},
        {"id": 2001, "owner_id": 7, "updated_at": stamp},
    )
    return record([deal], [email(["marcus@acme.test"], "Deal 2001 is now yours")])


def score(derived: derive.Derived, episode: dict[str, Any]) -> dict[str, int]:
    db = sqlite3.connect(":memory:")
    db.execute(
        "CREATE TABLE changes (app TEXT, tbl TEXT, op TEXT, pk TEXT, before TEXT, after TEXT, columns TEXT)"
    )
    db.execute("CREATE TABLE effects (seq INTEGER PRIMARY KEY, kind TEXT, data TEXT)")
    for c in episode["changes"]:
        row = (
            c.get("app"),
            c["tbl"],
            c["op"],
            json.dumps(c["pk"]),
            json.dumps(c["before"]),
            json.dumps(c["after"]),
            json.dumps(c["columns"]),
        )
        db.execute("INSERT INTO changes VALUES (?, ?, ?, ?, ?, ?, ?)", row)
    for e in episode["effects"]:
        db.execute(
            "INSERT INTO effects (kind, data) VALUES (?, ?)", (e["kind"], json.dumps(e["data"]))
        )
    return {check["name"]: db.execute(check["sql"]).fetchone()[0] for check in derived.checks}


def test_a_reassignment_derives_the_hand_written_checks_and_scope() -> None:
    derived = derive.derive(TRACE["request"], reassigned(NOW), reassigned(LATER))
    assert derived.allowed_changes == [
        {"table": "deals", "op": "update", "columns": ["owner_id", "updated_at"]}
    ], "as fixtures/acme-crm/tasks/notify-new-owner.yaml writes it by hand"
    by_name = {check["name"]: check for check in derived.checks}
    assert set(by_name) == {"update_deals_2001", "email", "only_these_changes"}
    assert by_name["update_deals_2001"]["sql"] == (
        "SELECT count(*) = 1 FROM changes WHERE tbl = 'deals' AND op = 'update' "
        "AND json_extract(pk, '$.id') = 2001 AND json_extract(after, '$.owner_id') = 7"
    ), "updated_at is a time, and differs between the replays"
    assert "'marcus@acme.test'" in by_name["email"]["sql"]
    assert "subject" not in by_name["email"]["sql"], "the instruction doesn't quote it"
    assert by_name["update_deals_2001"]["weight"] == by_name["email"]["weight"] == 0.5
    assert by_name["only_these_changes"]["gate"] is True
    assert derived.notes == []


def test_the_derived_sql_holds_any_path_to_the_outcome_and_nothing_more() -> None:
    derived = derive.derive(TRACE["request"], reassigned(NOW), reassigned(LATER))
    golden = score(derived, reassigned(LATER))
    assert golden == {"update_deals_2001": 1, "email": 1, "only_these_changes": 1}
    nothing = score(derived, record([], []))
    assert nothing == {"update_deals_2001": 0, "email": 0, "only_these_changes": 1}
    other_path = score(derived, reassigned("2026-11-10T08:00:00+00:00"))
    assert other_path == golden, "another time, the same outcome"
    extra_row = reassigned(LATER)
    extra_row["changes"].append(update("deals", {"id": 1100}, {"owner_id": 9}, {"owner_id": 7}))
    assert score(derived, extra_row)["only_these_changes"] == 0
    extra_email = reassigned(LATER)
    extra_email["effects"].append(email(["dana@acme.test"], "Deal 1100 is now yours"))
    assert score(derived, extra_email)["only_these_changes"] == 0
    two_recipients = reassigned(LATER)
    two_recipients["effects"][0]["data"]["to"].append("priya@acme.test")
    assert score(derived, two_recipients)["email"] == 0, "an extra recipient fails it"


def test_an_insert_checks_what_any_path_would_write_the_same() -> None:
    instruction = 'Log a call on deal 1200 with the summary "Quarterly check-in call".'

    def logged(row_id: int, stamp: str) -> dict[str, Any]:
        activity = {
            "id": row_id,
            "deal_id": 1200,
            "user_id": 1,
            "kind": "call",
            "summary": "Quarterly check-in call",
            "note": "an aside",
            "extra": {"a": 1},
            "occurred_at": stamp,
        }
        rate = {"currency": "eur", "day": "2026-11-09", "rate": 0.92}
        return record(
            [
                insert("activities", {"id": row_id}, activity),
                insert("exchange_rates", {"currency": "eur", "day": "2026-11-09"}, rate),
            ],
            [],
        )

    derived = derive.derive(instruction, logged(20001, NOW), logged(20001, LATER))
    sql = {check["name"]: check["sql"] for check in derived.checks}
    activity = sql["insert_activities"]
    for kept in (
        "'$.deal_id') = 1200",
        "'$.user_id') = 1",
        "'$.kind') = 'call'",
        "'Quarterly check-in call'",
    ):
        assert kept in activity, activity
    for skipped in ("'$.id'", "occurred_at", "an aside", "'$.extra'"):
        assert skipped not in activity, f"{skipped}: {activity}"
    rate = sql["insert_exchange_rates"]
    assert "'$.currency') = 'eur'" in rate and "'$.rate') = 0.92" in rate, (
        "a composite key is content"
    )
    assert "'$.day'" not in rate, "a date is skipped"


def test_an_id_the_replay_drew_and_a_touch_only_update_are_left_unchecked() -> None:
    def created() -> dict[str, Any]:
        deal = insert("deals", {"id": 8001}, {"id": 8001, "name": "Pilot", "owner_id": 3})
        note = insert("notes", {"id": 9001}, {"id": 9001, "deal_id": 8001, "author_id": 2})
        touched = update(
            "contacts",
            {"id": 41},
            {"id": 41, "updated_at": "2026-11-01T00:00:00Z"},
            {"id": 41, "updated_at": NOW},
        )
        return record([deal, note, touched], [])

    derived = derive.derive("Open a pilot deal and note it.", created(), created())
    notes = next(check["sql"] for check in derived.checks if check["name"] == "insert_notes")
    assert "8001" not in notes and "'$.author_id') = 2" in notes, notes
    assert not any(check["name"].startswith("update_contacts") for check in derived.checks)
    assert {
        "table": "contacts",
        "op": "update",
        "columns": ["updated_at"],
    } in derived.allowed_changes


def test_identical_rows_count_as_one_check_that_wants_both() -> None:
    def noted() -> dict[str, Any]:
        rows = [
            insert("notes", {"id": n}, {"id": n, "deal_id": 401, "text": f"free text {n}"})
            for n in (1, 2)
        ]
        return record(rows, [])

    derived = derive.derive("Note deal 401 twice.", noted(), noted())
    assert [check["name"] for check in derived.checks] == ["insert_notes", "only_these_changes"]
    assert derived.checks[0]["sql"].startswith("SELECT count(*) = 2 FROM changes")
    assert derived.checks[0]["weight"] == 1.0
    half = noted()
    half["changes"].pop()
    assert score(derived, half)["insert_notes"] == 0, "one row can't pass for two"


@pytest.mark.parametrize(
    ("second", "expected"),
    [
        (record([], [], reward=None), "couldn't be graded"),
        (record([], [], metadata={"fault": "crash"}), "broke the env"),
        (record([], [], truncated={"changes": 3}), "was cut"),
        (record([], []), "nothing changed and nothing was sent"),
        (
            record(
                [
                    {
                        "tbl": "t",
                        "op": "schema",
                        "pk": {},
                        "before": {},
                        "after": {},
                        "columns": None,
                    }
                ],
                [],
            ),
            "schema change",
        ),
        (record([update("logs", {}, {"a": 1}, {"a": 2})], []), "no primary key"),
        (
            record([update("rates", {"day": "2026-11-09"}, {"r": 1}, {"r": 2})], []),
            "keyed by a time",
        ),
    ],
)
def test_a_replay_that_cant_make_a_task_is_refused(second: dict[str, Any], expected: str) -> None:
    with pytest.raises(derive.Refused, match=expected):
        derive.derive("x", second, second)


def test_replays_that_differ_are_refused() -> None:
    first = reassigned(NOW)
    second = reassigned(NOW)
    second["changes"][0]["pk"] = {"id": 2002}
    with pytest.raises(derive.Refused, match="changed different rows"):
        derive.derive("x", first, second)


def test_strings_the_calls_send_but_the_request_never_shows_are_noted() -> None:
    calls = [
        {
            "tool": "api_request",
            "arguments": {
                "body": {"email": "a1@b2.example", "kind": "call", "n": 7, "tags": ["Urgent"]}
            },
        }
    ]
    assert derive.unseen("Log a call for a1@b2.example", calls) == ["Urgent"]


def test_content_hash_is_fnv1a_as_miroir_hashes_task_files() -> None:
    assert derive.content_hash(b"") == "cbf29ce484222325"
    assert derive.content_hash(b"a") == "af63dc4c8601ec8c"
    assert derive.content_hash(b"foobar") == "85944171f73967e8"


def test_draft_writes_a_task_the_env_serves_and_check_fails(tmp_path: Path) -> None:
    traces = write_traces(tmp_path / "traces.jsonl", TRACE)
    main(
        [
            "draft",
            str(traces),
            "--world",
            "week1",
            "--tasks",
            str(tmp_path / "tasks"),
            "--split",
            "test",
        ]
    )
    path = tmp_path / "tasks" / "week1-inc-1004.yaml"
    text = path.read_text(encoding="utf-8")
    assert text.splitlines()[0] == derive.DRAFT
    assert text.splitlines()[1] == '# Trace "inc-1004", consent: "support terms 4.2, opt-in"'
    task = yaml.safe_load(text)
    assert task == {
        "split": "test",
        "world": "week1",
        "instruction": TRACE["request"],
        "max_steps": 20,
        "checks": [{"name": "draft", "weight": 1.0, "db": "episode", "sql": "SELECT 0"}],
        "any_change": True,
        "golden": [
            {"tool": "api_request", "arguments": {"method": "GET", "path": "/users?role=rep"}},
            {
                "tool": "api_request",
                "arguments": {"method": "PATCH", "path": "/deals/2001", "body": {"owner_id": 7}},
            },
            {"tool": "submit", "arguments": {"answer": "Done."}},
        ],
    }


def test_draft_refuses_dates_repeats_and_finished_tasks(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    tasks = tmp_path / "tasks"
    tasks.mkdir()
    (tasks / "week1-done.yaml").write_text("split: train\n", encoding="utf-8")
    dated = {**TRACE, "id": "dated", "request": "Push the close to 2026-11-30."}
    done = {**TRACE, "id": "done"}
    traces = write_traces(tmp_path / "traces.jsonl", TRACE, TRACE, dated, done)
    with pytest.raises(SystemExit) as exited:
        main(["draft", str(traces), "--world", "week1", "--tasks", str(tasks)])
    assert exited.value.code == 1
    err = capsys.readouterr().err
    assert ":2: trace 'inc-1004' refused: its id is used by an earlier trace" in err
    assert ":3: trace 'dated' refused: it holds the date 2026-11-30" in err
    assert ":4: trace 'done' refused:" in err and "a finished task" in err
    assert (tasks / "week1-inc-1004.yaml").exists(), "the others are still drafted"
    assert (tasks / "week1-done.yaml").read_text(encoding="utf-8") == "split: train\n"
    with pytest.raises(SystemExit, match="a world is lowercase"):
        main(["draft", str(traces), "--world", "Week 1", "--tasks", str(tasks)])


class FakeSession:
    def __init__(self, statuses: dict[str, int], records: list[dict[str, Any]]) -> None:
        self.statuses, self.records, self.runs = statuses, records, []

    def reset(self, task_id: str) -> dict[str, Any]:
        self.runs.append([])
        return {}

    def step(self, tool: str, arguments: dict[str, Any]) -> dict[str, Any]:
        self.runs[-1].append(arguments.get("path"))
        status = self.statuses.get(arguments.get("path", ""), 200)
        return {"observation": {"result": {"status": status}, "error": None}}

    def finish(self) -> dict[str, Any]:
        return {"record": self.records.pop(0)}

    def close(self) -> None:
        pass


def serve(
    monkeypatch: pytest.MonkeyPatch, tasks_dir: Path, session: FakeSession, stale: bool = False
) -> None:
    def listing(url: str, split: str, token: str | None) -> list[dict[str, Any]]:
        if split != "test":
            return []
        data = (tasks_dir / "week1-inc-1004.yaml").read_bytes()
        digest = "0" * 16 if stale else derive.content_hash(data)
        return [
            {
                "id": "week1-inc-1004",
                "split": "test",
                "world": "week1",
                "persona": {"name": "support"},
                "max_steps": 20,
                "task_hash": digest,
            }
        ]

    monkeypatch.setattr(derive, "tasks", listing)
    monkeypatch.setattr(
        derive.Session, "lease", classmethod(lambda cls, urls, token, wait_busy: session)
    )


def test_derive_replays_twice_and_writes_the_task(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    trace = {
        **TRACE,
        "calls": [
            {"tool": "api_request", "arguments": {"method": "GET", "path": "/deals/2002"}},
            *TRACE["calls"],
        ],
    }
    traces = write_traces(tmp_path / "traces.jsonl", trace)
    tasks_dir = tmp_path / "tasks"
    main(["draft", str(traces), "--world", "week1", "--tasks", str(tasks_dir), "--split", "test"])
    session = FakeSession({"/deals/2002": 404}, [reassigned(NOW), reassigned(LATER)])
    serve(monkeypatch, tasks_dir, session)
    main(["derive", "http://env", str(traces), "--world", "week1", "--tasks", str(tasks_dir)])
    assert session.runs == [
        ["/deals/2002", "/users?role=rep", "/deals/2001"],
        ["/users?role=rep", "/deals/2001"],
    ]
    text = (tasks_dir / "week1-inc-1004.yaml").read_text(encoding="utf-8")
    assert text.splitlines()[0] == derive.DERIVED
    task = yaml.safe_load(text)
    assert task["split"] == "test" and task["max_steps"] == 20
    assert [step["arguments"].get("path") for step in task["golden"]] == [
        "/users?role=rep",
        "/deals/2001",
        None,
    ]
    assert [check["name"] for check in task["checks"]] == [
        "update_deals_2001",
        "email",
        "only_these_changes",
    ]
    assert task["allowed_changes"] == [
        {"table": "deals", "op": "update", "columns": ["owner_id", "updated_at"]}
    ]


def test_derive_refuses_a_stale_draft_and_a_call_that_worked_in_production(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    traces = write_traces(tmp_path / "traces.jsonl", TRACE)
    tasks_dir = tmp_path / "tasks"
    main(["draft", str(traces), "--world", "week1", "--tasks", str(tasks_dir)])
    draft = (tasks_dir / "week1-inc-1004.yaml").read_text(encoding="utf-8")
    serve(monkeypatch, tasks_dir, FakeSession({}, []), stale=True)
    with pytest.raises(SystemExit):
        main(["derive", "http://env", str(traces), "--world", "week1", "--tasks", str(tasks_dir)])
    assert "the env serves another version of its draft" in capsys.readouterr().err
    serve(monkeypatch, tasks_dir, FakeSession({"/deals/2001": 404}, [reassigned(NOW)]))
    with pytest.raises(SystemExit):
        main(["derive", "http://env", str(traces), "--world", "week1", "--tasks", str(tasks_dir)])
    assert "call 2 answered 200 in production but fails on the snapshot" in capsys.readouterr().err
    assert (tasks_dir / "week1-inc-1004.yaml").read_text(encoding="utf-8") == draft, (
        "the draft stays"
    )


def scopes(rules: list[dict[str, Any]]) -> list[str]:
    return sorted(
        json.dumps({**r, "columns": sorted(r.get("columns", []))}, sort_keys=True) for r in rules
    )


def golden_trace(task: str, before: list[dict[str, Any]] | None = None) -> dict[str, Any]:
    written = yaml.safe_load((ROOT / "fixtures/acme-crm/tasks" / f"{task}.yaml").read_text("utf-8"))
    *calls, submit = written["golden"]
    trace: dict[str, Any] = {
        "id": task,
        "consent": "the fixture's",
        "request": written["instruction"],
    }
    trace["calls"] = [*(before or []), *calls]
    if submit.get("arguments", {}).get("answer"):
        trace["answer"] = submit["arguments"]["answer"]
    return trace


@pytest.mark.integration
def test_derive_rederives_acme_crms_scopes_and_check_passes_them(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    paid = {
        "tool": "api_request",
        "arguments": {"method": "POST", "path": "/invoices/4015/pay"},
        "status": 409,
    }
    names = ["notify-new-owner", "merge-duplicate-contacts", "log-call-activities", "pay-invoice"]
    traces = [golden_trace(name, [paid] if name == "pay-invoice" else None) for name in names]
    traces.append(golden_trace("count-overdue-invoices"))
    path = write_traces(tmp_path / "traces.jsonl", *traces)
    tasks_dir = tmp_path / "tasks"
    main(["draft", str(path), "--world", "default", "--tasks", str(tasks_dir)])
    with env_with_tasks(tasks_dir) as url, pytest.raises(SystemExit) as exited:
        main(["derive", url, str(path), "--world", "default", "--tasks", str(tasks_dir)])
    assert exited.value.code == 1, "the question is refused"
    err = capsys.readouterr().err
    assert "default-count-overdue-invoices refused: nothing changed and nothing was sent" in err
    assert "call 1 (POST /invoices/4015/pay) failed on the snapshot; left out" in err
    for name in names:
        derived = yaml.safe_load((tasks_dir / f"default-{name}.yaml").read_text("utf-8"))
        written = yaml.safe_load(
            (ROOT / "fixtures/acme-crm/tasks" / f"{name}.yaml").read_text("utf-8")
        )
        assert scopes(derived["allowed_changes"]) == scopes(written["allowed_changes"]), name
    pay = yaml.safe_load((tasks_dir / "default-pay-invoice.yaml").read_text("utf-8"))
    assert "/invoices/4015/pay" not in json.dumps(pay["golden"]), (
        "the failed call left the golden run"
    )
    (tasks_dir / "default-count-overdue-invoices.yaml").unlink()
    mount = f"{tasks_dir.resolve()}:/miroir/tasks:ro"
    checked = subprocess.run(
        ["docker", "run", "--rm", "--network", "none", "-v", mount, IMAGE, "check", "--json"],
        capture_output=True,
        text=True,
        check=False,
    )
    report = json.loads(checked.stdout)
    failed = {task["id"]: task for task in report["tasks"] if not task["passed"]}
    assert report["passed"], json.dumps(failed, indent=1)[:4000]
