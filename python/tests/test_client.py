import base64
import datetime
import hashlib
import http.server
import json
import os
import re
import socket
import threading
import time
import urllib.request

import pytest
from conftest import golden

import miroir
from miroir import MiroirError, Session, save_record

TODAY = datetime.date(2026, 11, 2)
PERSONA = {"name": "sales_manager", "description": "Dana Lee, sales manager at Acme"}
TASK = {
    "id": "count",
    "split": "train",
    "instruction": "Count the overdue invoices.",
    "persona": PERSONA,
    "max_steps": 20,
}
NOTHING_LISTENS = "http://127.0.0.1:9"
WEBSOCKET_GUID = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11"


def test_a_task_and_its_reset_observation_open_with_the_same_prompt():
    observation = {
        "task_id": "count",
        "instruction": TASK["instruction"],
        "persona": PERSONA,
        "now": "2026-11-02T09:30:00Z",
        "max_steps": 20,
        "metadata": {"reset_s": {}},
    }
    expected = (
        "Count the overdue invoices.\n\n"
        "You act as Dana Lee, sales manager at Acme. You have 20 tool calls; "
        "call submit when you are done.\n\n"
        "Today is 2026-11-02 (UTC)."
    )
    assert miroir.prompt(TASK, TODAY) == expected
    assert miroir.prompt(observation, TODAY) == expected


def test_a_persona_without_a_description_is_left_out_of_the_prompt():
    task = {**TASK, "persona": {"name": "x", "description": ""}}
    assert "You act as" not in miroir.prompt(task, TODAY)


@pytest.mark.parametrize("urls", ["http://127.0.0.1:8000", []])
def test_a_lease_needs_a_list_of_urls(urls):
    with pytest.raises(ValueError, match="non-empty list"):
        Session.lease(urls, wait_busy=False)


def test_a_lease_gives_up_on_containers_that_never_come_up():
    with pytest.raises(MiroirError, match="0 of 1 containers leased, 1 not healthy") as refused:
        Session.lease([NOTHING_LISTENS], wait_busy=False, timeout_s=0.5)
    assert refused.value.code == "NO_FREE_CONTAINER"


def test_hosts_that_never_answer_cost_a_lease_one_timeout_not_one_each():
    silent = [socket.create_server(("127.0.0.1", 0)) for _ in range(10)]
    try:
        urls = [f"http://127.0.0.1:{listener.getsockname()[1]}" for listener in silent]
        started = time.monotonic()
        with pytest.raises(MiroirError, match="0 of 10 containers leased, 10 not healthy"):
            Session.lease(urls, wait_busy=False, timeout_s=0.1)
        assert time.monotonic() - started < 3 * miroir.HEALTH_TIMEOUT_S
    finally:
        for listener in silent:
            listener.close()


def test_a_server_that_stops_answering_pings_breaks_the_session(monkeypatch):
    monkeypatch.setattr(miroir, "PING_INTERVAL_S", 0.1)
    monkeypatch.setattr(miroir, "PING_TIMEOUT_S", 0.3)
    listener = socket.create_server(("127.0.0.1", 0))
    threading.Thread(target=_answer_once_then_hang, args=(listener,), daemon=True).start()
    url = f"http://127.0.0.1:{listener.getsockname()[1]}"
    connection = miroir._open_session(url, None)
    assert connection is not None
    connection.close_timeout = 0.1
    with pytest.raises(MiroirError) as lost:
        Session(url, connection).state()
    assert lost.value.code == "CONNECTION_LOST", "no reply deadline, yet a dead server is noticed"


def _answer_once_then_hang(listener: socket.socket) -> None:
    connection, _ = listener.accept()
    request = b""
    while b"\r\n\r\n" not in request:
        request += connection.recv(4096)
    key = re.search(rb"(?i)sec-websocket-key: (\S+)", request)
    assert key is not None
    accept = base64.b64encode(hashlib.sha1(key[1] + WEBSOCKET_GUID).digest())
    connection.sendall(
        b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
        b"Sec-WebSocket-Accept: " + accept + b"\r\n\r\n"
    )
    connection.recv(4096)
    state = b'{"type": "state", "data": {}}'
    connection.sendall(bytes([0x81, len(state)]) + state)
    threading.Event().wait()


def test_tools_asks_for_a_surface_and_says_when_the_env_has_none(monkeypatch):
    asked = []

    def answer(url, path, body, token):
        asked.append(path)
        if "code" in path:
            refused = {"code": -32602, "message": "surface 'code' needs a sandbox (MIROIR_SANDBOX)"}
            return {"jsonrpc": "2.0", "id": 1, "error": refused}
        return {"jsonrpc": "2.0", "id": 1, "result": {"tools": [{"name": "submit"}]}}

    monkeypatch.setattr(miroir, "_request", answer)
    assert miroir.tools("http://env") == [{"name": "submit"}]
    with pytest.raises(MiroirError, match="needs a sandbox"):
        miroir.tools("http://env", surface="code")
    assert asked == ["/mcp", "/mcp?surface=code"]


@pytest.mark.integration
def test_a_lease_skips_containers_that_are_held_or_down(urls):
    held = Session.lease([urls[0]], wait_busy=False)
    try:
        other = Session.lease([NOTHING_LISTENS, *urls], wait_busy=False)
        other.close()
        assert other.url == urls[1]
        with pytest.raises(MiroirError, match="1 of 1 containers leased, 0 not healthy") as refused:
            Session.lease([urls[0]], wait_busy=False)
        assert refused.value.code == "NO_FREE_CONTAINER"
    finally:
        held.close()
    Session.lease([urls[0]], wait_busy=False).close()


@pytest.mark.integration
def test_the_golden_run_earns_full_reward(urls):
    session = Session.lease(urls, wait_busy=False)
    try:
        observation = session.reset("reassign-stale-deals")
        assert observation["instruction"].startswith("Priya Shah leaves the team")
        for tool, arguments in golden("reassign-stale-deals"):
            step = session.step(tool, arguments)
            assert step["observation"]["error"] is None, step
        outcome = session.finish()
    finally:
        session.close()
    assert (outcome["reward"], outcome["success"]) == (1.0, True)
    assert outcome["metadata"]["termination"] == "submit"
    record = outcome["record"]
    calls = [(action["tool"], action["arguments"]) for action in record["actions"]]
    assert calls == golden("reassign-stale-deals")
    assert record["diagnostics"]["calls"] == len(calls)
    assert len(record["signature"]) == 16 and record["metadata"] == outcome["metadata"]
    listed = {task["id"]: task["task_hash"] for task in miroir.tasks(urls[0])}
    assert outcome["metadata"]["task_hash"] == listed["reassign-stale-deals"], "calibrate's join"


@pytest.mark.integration
def test_an_episode_an_mcp_harness_ended_finishes_with_its_whole_grade(urls):
    session = Session.lease(urls, wait_busy=False)
    try:
        session.reset("count-overdue-invoices")
        call = {"name": "submit", "arguments": {"answer": "3"}, "episode_id": session.episode_id}
        rpc = {"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": call}
        body = json.dumps(rpc).encode()
        headers = {"Content-Type": "application/json"}
        request = urllib.request.Request(f"{session.url}/mcp", data=body, headers=headers)
        with urllib.request.urlopen(request, timeout=30) as response:
            assert json.loads(response.read())["result"]["done"] is True
        outcome = session.finish()
    finally:
        session.close()
    assert (outcome["reward"], outcome["metadata"]["termination"]) == (1.0, "submit")
    assert outcome["record"]["actions"][0]["arguments"] == {"answer": "3"}


def test_records_append_to_one_trace_file_per_process(tmp_path):
    traces = tmp_path / "traces"
    save_record(traces, {"episode_id": "a", "signature": "1" * 16})
    save_record(str(traces), {"episode_id": "b", "signature": None})
    [written] = list(traces.glob("*.jsonl"))
    assert written.name.endswith(f"-{os.getpid()}.jsonl")
    lines = [json.loads(line) for line in written.read_text().splitlines()]
    assert [line["episode_id"] for line in lines] == ["a", "b"]


@pytest.mark.integration
def test_an_episode_that_never_submits_is_graded_when_it_finishes(urls):
    session = Session.lease(urls, wait_busy=False)
    try:
        session.reset("count-overdue-invoices")
        session.step("api_request", {"method": "GET", "path": "/contacts?q=Northwind"})
        outcome = session.finish()
        assert session.finish() == outcome, "a finished episode keeps its outcome"
        assert session.state()["done"]
    finally:
        session.close()
    assert (outcome["reward"], outcome["success"]) == (0.0, False)
    assert outcome["metadata"]["termination"] == "submit"


@pytest.mark.integration
def test_an_unknown_task_is_refused_and_leaves_no_episode(urls):
    session = Session.lease(urls, wait_busy=False)
    try:
        with pytest.raises(MiroirError, match="unknown task") as refused:
            session.reset("no-such-task")
        assert refused.value.code == "VALIDATION_ERROR"
        with pytest.raises(MiroirError, match="reset first"):
            session.step("submit", {})
    finally:
        session.close()


@pytest.mark.integration
def test_a_dropped_connection_breaks_the_session_for_good(urls):
    session = Session.lease(urls, wait_busy=False)
    session.reset("count-overdue-invoices")
    session._socket.close()
    for _ in range(2):
        with pytest.raises(MiroirError) as lost:
            session.step("api_request", {"method": "GET", "path": "/me"})
        assert lost.value.code == "CONNECTION_LOST"


class FakeEnv:
    def __init__(self, routes: dict[str, tuple[int, bytes]]) -> None:
        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def do_GET(self) -> None:
                status, body = routes.get(self.path, (404, b"{}"))
                self.send_response(status)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, format: str, *args: object) -> None:
                pass

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}"
        threading.Thread(target=self.server.serve_forever, daemon=True).start()


def test_a_url_file_lists_one_url_a_line_without_comments(tmp_path):
    listed = tmp_path / "urls.txt"
    listed.write_text("# the pool's\nhttp://127.0.0.1:8001  # a1b2\n\n  https://env.example/x\n")
    assert miroir.read_urls(listed) == ["http://127.0.0.1:8001", "https://env.example/x"]


@pytest.mark.parametrize(
    ("text", "error"),
    [
        ("", "lists no URL"),
        ("# only\n", "lists no URL"),
        ("127.0.0.1:8001\n", "is not an http"),
        ("http://\n", "is not an http"),
    ],
)
def test_a_url_file_without_urls_is_refused(tmp_path, text, error):
    listed = tmp_path / "urls.txt"
    listed.write_text(text)
    with pytest.raises(MiroirError, match=error) as refused:
        miroir.read_urls(listed)
    assert refused.value.code == "URL_FILE"


def test_a_missing_url_file_says_to_start_a_pool(tmp_path):
    with pytest.raises(MiroirError, match="start `miroir pool`"):
        miroir.read_urls(tmp_path / "urls.txt")


def test_health_is_none_unless_miroir_answers():
    env = FakeEnv({"/health": (200, b"<html>a gateway</html>")})
    assert miroir._health(env.url) is None
    assert miroir._health(NOTHING_LISTENS) is None
    unhealthy = FakeEnv({"/health": (503, b'{"leased": false, "status": "unhealthy"}')})
    assert miroir._health(unhealthy.url) == {"leased": False, "status": "unhealthy"}
    gone = FakeEnv({"/health": (502, b'{"status": "healthy"}')})
    assert miroir._health(gone.url) is None, "a gateway's error is no answer from miroir"


class FakeSocket:
    def close(self) -> None:
        pass


def test_a_lease_opens_no_session_on_a_container_a_session_holds(monkeypatch):
    held, free = "http://held:1", "http://free:1"
    leased = {held: True, free: False}
    monkeypatch.setattr(miroir, "_health", lambda url: {"status": "healthy", "leased": leased[url]})
    opened = []
    monkeypatch.setattr(
        miroir, "_open_session", lambda url, token: opened.append(url) or FakeSocket()
    )
    for _ in range(10):
        assert Session.lease([held, free], wait_busy=False).url == free
    assert held not in opened
    leased[free] = True
    with pytest.raises(MiroirError, match="2 of 2 containers leased"):
        Session.lease([held, free], wait_busy=False)


HEALTHY = b'{"status": "healthy", "leased": false}'


def test_a_gateway_error_at_the_handshake_sends_the_lease_to_another_container(monkeypatch):
    erring = FakeEnv({"/health": (200, HEALTHY), "/ws": (502, b"<html>bad gateway</html>")})
    free = "http://free:1"
    monkeypatch.setattr(miroir, "_health", lambda url: json.loads(HEALTHY))
    real = miroir._open_session
    monkeypatch.setattr(
        miroir,
        "_open_session",
        lambda url, token: FakeSocket() if url == free else real(url, token),
    )
    for _ in range(5):
        assert Session.lease([erring.url, free], wait_busy=False).url == free
    with pytest.raises(MiroirError, match="0 of 1 containers leased, 1 not healthy") as refused:
        Session.lease([erring.url], wait_busy=False, timeout_s=0.5)
    assert refused.value.code == "NO_FREE_CONTAINER", "retried until the lease's timeout"


def test_a_refused_handshake_fails_the_lease_at_once():
    refusing = FakeEnv({"/health": (200, HEALTHY), "/ws": (401, b'{"detail": "wrong token"}')})
    started = time.monotonic()
    with pytest.raises(MiroirError, match="401") as refused:
        Session.lease([refusing.url], "wrong", wait_busy=True, timeout_s=60)
    assert refused.value.code == "CONNECTION_FAILED"
    assert time.monotonic() - started < 10, "a wrong token is no reason to wait"


def test_a_lease_reads_its_url_file_again_while_it_waits(monkeypatch, tmp_path):
    listed = tmp_path / "urls.txt"
    listed.write_text("http://replaced:1  # old\n")
    monkeypatch.setattr(
        miroir, "_health", lambda url: {"status": "healthy", "leased": url == "http://replaced:1"}
    )
    monkeypatch.setattr(miroir, "_open_session", lambda url, token: FakeSocket())
    monkeypatch.setattr(miroir, "RETRY_EVERY_S", 0)
    monkeypatch.setattr(time, "sleep", lambda _: listed.write_text("http://new:1  # new\n"))
    assert Session.lease(listed, wait_busy=True, timeout_s=5).url == "http://new:1"


def test_concurrent_leases_share_one_pool_of_health_check_threads(monkeypatch):
    def slow_health(url: str) -> dict[str, object]:
        time.sleep(0.05)
        return {"status": "healthy", "leased": False}

    monkeypatch.setattr(miroir, "_health", slow_health)
    monkeypatch.setattr(miroir, "_open_session", lambda url, token: FakeSocket())
    urls = [f"http://env-{i}:1" for i in range(64)]
    peak, done = [threading.active_count()], threading.Event()

    def sample() -> None:
        while not done.is_set():
            peak.append(threading.active_count())
            time.sleep(0.005)

    sampler = threading.Thread(target=sample)
    sampler.start()
    leases = [
        threading.Thread(target=Session.lease, args=(urls,), kwargs={"wait_busy": False})
        for _ in range(200)
    ]
    for lease in leases:
        lease.start()
    for lease in leases:
        lease.join()
    done.set()
    sampler.join()
    assert max(peak) < 200 + miroir.HEALTH_WORKERS + 20, "a per-lease pool would start 6,400"


def test_tasks_are_listed_from_the_first_env_that_answers():
    env = FakeEnv({"/list_environments": (200, b'["acme-crm"]')})
    assert miroir.answering([NOTHING_LISTENS, env.url]) == env.url
    with pytest.raises(MiroirError) as none:
        miroir.answering([NOTHING_LISTENS])
    assert none.value.code == "CONNECTION_FAILED"


@pytest.mark.integration
def test_an_episode_past_its_max_seconds_is_graded_as_it_stands(urls):
    session = Session.lease(urls, wait_busy=False)
    try:
        session.reset("count-overdue-invoices", max_seconds=1)
        time.sleep(1.5)
        outcome = session.finish()
    finally:
        session.close()
    assert (outcome["reward"], outcome["metadata"]["termination"]) == (0.0, "max_seconds")
    assert outcome["record"]["actions"] == [], "the finishing submit came too late to run"


@pytest.mark.integration
def test_an_episode_an_mcp_call_ends_while_it_finishes_keeps_its_grade(urls, monkeypatch):
    session = Session.lease(urls, wait_busy=False)
    try:
        session.reset("count-overdue-invoices")
        call = {"name": "submit", "arguments": {"answer": "3"}, "episode_id": session.episode_id}
        rpc = {"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": call}
        request = urllib.request.Request(
            f"{session.url}/mcp",
            data=json.dumps(rpc).encode(),
            headers={"Content-Type": "application/json"},
        )
        real_state = session.state
        stale = iter([{"done": False}])
        monkeypatch.setattr(session, "state", lambda: next(stale, None) or real_state())
        with urllib.request.urlopen(request, timeout=30) as response:
            assert json.loads(response.read())["result"]["done"] is True
        outcome = session.finish()
    finally:
        session.close()
    assert (outcome["reward"], outcome["metadata"]["termination"]) == (1.0, "submit")
