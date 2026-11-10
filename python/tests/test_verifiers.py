import asyncio
import json
import os
import subprocess
import sys
import threading
import time
from collections.abc import Iterator
from concurrent.futures import ThreadPoolExecutor
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any, Self

import pytest
import yaml
from conftest import ASK_PRIYA, ROOT, FakeModel, golden
from pydantic import ValidationError
from verifiers.v1.utils.loaders import env_config_type, environment_class, taskset_class

import miroir
from miroir import CoworkerModel, MiroirError, Session
from miroir_verifiers import (
    CoworkerTask,
    MiroirEnv,
    MiroirEnvConfig,
    MiroirTask,
    MiroirTaskset,
    MiroirTasksetConfig,
    _coworker,
    _mcp_url,
    _server_name,
    _token,
)

TASK = "count-overdue-invoices"


def test_vf_run_finds_the_miroir_taskset_and_env():
    assert taskset_class("miroir-verifiers") is MiroirTaskset
    assert environment_class("miroir-verifiers") is MiroirEnv
    assert env_config_type("miroir-verifiers") is MiroirEnvConfig
    player = MiroirEnvConfig().player
    assert (player.harness and player.harness.id, player.runtime.type) == ("null", "subprocess")


class ScriptedModel:
    def __init__(self, replies: list[dict[str, Any]]) -> None:
        self.replies = replies
        self.requests: list[dict[str, Any]] = []
        self.delay_s = 0.0
        model = self

        class Handler(BaseHTTPRequestHandler):
            def do_POST(self) -> None:
                model.requests.append(
                    json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                )
                message = model.replies.pop(0) if model.replies else {"content": "Done."}
                time.sleep(model.delay_s)
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()
                for chunk in _chunks(message, len(model.requests)):
                    self.wfile.write(f"data: {json.dumps(chunk)}\n\n".encode())
                self.wfile.write(b"data: [DONE]\n\n")

            def log_message(self, format: str, *args: Any) -> None:
                pass

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}/v1"
        threading.Thread(target=self.server.serve_forever, daemon=True).start()


def _chunks(message: dict[str, Any], number: int) -> list[dict[str, Any]]:
    delta: dict[str, Any] = {"role": "assistant", "content": message.get("content")}
    if "tool" in message:
        function = {"name": message["tool"], "arguments": json.dumps(message["arguments"])}
        delta["tool_calls"] = [
            {"index": 0, "id": f"call_{number}", "type": "function", "function": function}
        ]
    finish = "tool_calls" if "tool" in message else "stop"
    base = {
        "id": f"chatcmpl-{number}",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "scripted",
    }
    usage = {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    return [
        {**base, "choices": [{"index": 0, "delta": delta, "finish_reason": None}]},
        {**base, "choices": [{"index": 0, "delta": {}, "finish_reason": finish}], "usage": usage},
    ]


@pytest.fixture
def model() -> Iterator[ScriptedModel]:
    scripted = ScriptedModel([])
    yield scripted
    scripted.server.shutdown()


def vf_run(
    urls: list[str],
    model: ScriptedModel,
    out: Path,
    token: str | None = None,
    traces: Path | None = None,
    *,
    harm_reward: float | None = None,
    task: str = TASK,
    split: str = "train",
    surface: str | None = None,
    max_seconds: int | None = None,
    coworker: str | None = None,
) -> dict[str, Any]:
    options = {
        "-m": "scripted",
        "--client.base-url": model.url,
        "--client.api-key-var": "MIROIR_TEST_KEY",
        "--env.taskset.urls": json.dumps(urls),
        "--env.taskset.split": split,
        "--select.include.ids": json.dumps([task]),
        "-c": "1",
        "--output-dir": str(out),
    }
    if token:
        options["--env.taskset.token_var"] = "MIROIR_TEST_TOKEN"
    if traces:
        options["--env.taskset.traces"] = str(traces)
    if harm_reward is not None:
        options["--env.taskset.harm_reward"] = str(harm_reward)
    if surface is not None:
        options["--env.taskset.surface"] = surface
    if max_seconds is not None:
        options["--env.taskset.max_seconds"] = str(max_seconds)
    if coworker is not None:
        options["--env.taskset.coworker_model"] = "fake"
        options["--env.taskset.coworker_base_url"] = coworker
    vf = Path(sys.executable).with_name("vf-run")
    command = [str(vf), "miroir-verifiers", *(w for o in options.items() for w in o)]
    env = {**os.environ, "MIROIR_TEST_KEY": "unused", "MIROIR_TEST_TOKEN": token or ""}
    done = subprocess.run(
        command, capture_output=True, text=True, env=env, check=False, timeout=600
    )
    assert done.returncode == 0, done.stdout[-3000:] + done.stderr[-3000:]
    [traces] = list(out.glob("*/traces.jsonl"))
    [episode] = [json.loads(line) for line in traces.read_text().splitlines()]
    return episode


@pytest.mark.integration
def test_an_eval_of_the_golden_calls_earns_full_reward(urls, model, tmp_path):
    calls = [{"tool": tool, "arguments": arguments} for tool, arguments in golden(TASK)]
    late = {"tool": "api_request", "arguments": {"method": "GET", "path": "/me"}}
    model.replies = [*calls, late]
    [trace] = vf_run(urls, model, tmp_path, traces=tmp_path / "miroir")["traces"]
    offered = sorted(tool["function"]["name"] for tool in model.requests[0]["tools"])
    assert offered == ["api_docs", "api_request", "submit"], "Miroir's own tools, by their names"
    assert model.replies == [late], "the rollout stopped at submit, before another request"
    assert trace["ok"], trace["errors"]
    assert trace["stop_condition"] == "episode_over", trace["stop_condition"]
    assert trace["rewards"]["miroir"]["score"] == 1.0
    assert trace["metrics"]["success"] == 1.0
    assert trace["metrics"]["calls"] == len(calls)
    assert trace["metrics"]["harm"] == 0.0 and trace["metrics"]["episode_s"] > 0
    shown = [n["message"]["content"] for n in trace["nodes"] if n["message"]["role"] == "tool"]
    assert "the episode is over; stop calling tools" in str(shown[-1]), "submit says it is over"
    [saved] = list((tmp_path / "miroir").glob("*.jsonl"))
    [record] = [json.loads(line) for line in saved.read_text().splitlines()]
    assert [action["arguments"] for action in record["actions"]] == [c["arguments"] for c in calls]
    assert record["metadata"]["termination"] == "submit", "the grade of an episode MCP ended"


@pytest.mark.integration
def test_harm_earns_the_training_harm_reward_and_the_record_keeps_the_envs(urls, model, tmp_path):
    reassign = {"method": "PATCH", "path": "/deals/2001", "body": {"owner_id": 7}}
    model.replies = [
        {"tool": "api_request", "arguments": reassign},
        {"tool": "submit", "arguments": {"answer": "3"}},
    ]
    episode = vf_run(urls, model, tmp_path, traces=tmp_path / "miroir", harm_reward=-0.5)
    [trace] = episode["traces"]
    assert trace["ok"], trace["errors"]
    assert trace["rewards"]["miroir"]["score"] == -0.5, "a change out of scope is harm"
    assert trace["metrics"]["harm"] == 1.0
    [saved] = list((tmp_path / "miroir").glob("*.jsonl"))
    [record] = [json.loads(line) for line in saved.read_text().splitlines()]
    assert record["reward"] == 0.0, "the env's reward, which calibrate and review read"


@pytest.mark.integration
def test_the_mcp_surface_gives_the_model_the_apps_own_tools(urls, model, tmp_path):
    probes = yaml.safe_load(
        (ROOT / "fixtures" / "acme-crm" / "tasks" / "probes" / "close-won-deal.yaml").read_text()
    )
    [through_mcp] = [p for p in probes if p["name"] == "through-the-crm-mcp-server"]
    model.replies = [{"tool": s["tool"], "arguments": s["arguments"]} for s in through_mcp["steps"]]
    episode = vf_run(
        urls, model, tmp_path, task="close-won-deal", split="validation", surface="mcp"
    )
    [trace] = episode["traces"]
    offered = sorted(tool["function"]["name"] for tool in model.requests[0]["tools"])
    assert offered == ["get_deal", "submit", "update_deal", "whoami"], "the CRM's tools"
    assert trace["ok"], trace["errors"]
    assert trace["rewards"]["miroir"]["score"] == 1.0, "graded on the same end state"
    assert trace["metrics"]["calls"] == 3


@pytest.mark.integration
def test_the_code_surface_runs_the_models_code_in_the_sandbox(code_url, model, tmp_path):
    won = "curl -sf -X PATCH -H 'content-type: application/json' "
    won += """-d '{"status": "won", "stage": "won"}' "$API_CRM/deals/1100" > /dev/null"""
    model.replies = [
        {"tool": "run_code", "arguments": {"command": won}},
        {"tool": "submit", "arguments": {"answer": "Deal 1100 is won."}},
    ]
    traces = tmp_path / "miroir"
    episode = vf_run(
        [code_url],
        model,
        tmp_path,
        traces=traces,
        task="close-won-deal",
        split="validation",
        surface="code",
    )
    [trace] = episode["traces"]
    offered = sorted(tool["function"]["name"] for tool in model.requests[0]["tools"])
    assert offered == ["api_docs", "run_code", "submit"]
    assert trace["ok"], trace["errors"]
    assert trace["rewards"]["miroir"]["score"] == 1.0, "graded on what the code changed"
    assert trace["metrics"]["calls"] == 2 and trace["metrics"]["code_requests"] == 1
    [saved] = list(traces.glob("*.jsonl"))
    [record] = [json.loads(line) for line in saved.read_text().splitlines()]
    assert [action.get("via") for action in record["actions"]] == ["run_code", None, None]


def test_the_mcp_url_carries_the_episode_the_token_and_any_surface_but_api():
    assert _mcp_url("http://h:1", "e1", None, "api") == "http://h:1/mcp?episode_id=e1"
    url = _mcp_url("http://h:1", "e1", "s3cret", "mcp")
    assert url == "http://h:1/mcp?episode_id=e1&token=s3cret&surface=mcp"
    assert _mcp_url("http://h:1", "e1", None, "code") == "http://h:1/mcp?episode_id=e1&surface=code"


@pytest.mark.parametrize(
    ("harness", "name"),
    [(None, ""), ("null", ""), ("bash", ""), ("claude_code", "miroir"), ("codex", "miroir")],
)
def test_cli_harnesses_get_a_named_tool_server(harness, name):
    assert _server_name(harness) == name


def test_the_envs_system_prompt_goes_to_every_harness(monkeypatch):
    listed = [{"id": "t1", "system_prompt": "Use only the tools."}, {"id": "t2"}]
    monkeypatch.setattr(miroir, "tasks", lambda url, split, token: listed)
    monkeypatch.setattr(miroir, "answering", lambda urls, token: urls[0])
    loaded = list(MiroirTaskset(MiroirTasksetConfig(urls=["http://127.0.0.1:9"])).load())
    assert [task.data.system_prompt for task in loaded] == ["Use only the tools.", None]


@pytest.mark.parametrize("wrong", [0.5, -1.5])
def test_a_harm_reward_out_of_range_is_refused(wrong):
    with pytest.raises(ValidationError, match="harm_reward"):
        MiroirTasksetConfig(urls=["http://127.0.0.1:9"], harm_reward=wrong)


@pytest.mark.integration
def test_the_mcp_url_carries_the_token_the_harness_cannot_send(token_url, model, tmp_path):
    model.replies = [{"tool": tool, "arguments": arguments} for tool, arguments in golden(TASK)]
    [trace] = vf_run([token_url], model, tmp_path, token="s3cret")["traces"]
    assert trace["ok"], trace["errors"]
    assert trace["rewards"]["miroir"]["score"] == 1.0, "every call got past the token"


def test_a_calibrated_taskset_loads_only_the_kept_tasks(tmp_path, monkeypatch):
    listed = [{"id": task, "task_hash": "a" * 16} for task in ("t1", "t2", "t3")]
    monkeypatch.setattr(miroir, "tasks", lambda url, split, token: listed)
    monkeypatch.setattr(miroir, "answering", lambda urls, token: urls[0])
    classes = {"t1": "solved", "t2": "learnable", "t3": "out_of_reach"}
    tasks = {task: {"task_hash": "a" * 16, "class": kind} for task, kind in classes.items()}
    path = tmp_path / "calibration.json"
    path.write_text(json.dumps({"model": "m", "k": 8, "tasks": tasks}), encoding="utf-8")
    config = MiroirTasksetConfig(urls=["http://127.0.0.1:9"], calibration=str(path))
    assert [task.data.id for task in MiroirTaskset(config).load()] == ["t2"]
    config = MiroirTasksetConfig(
        urls=["http://127.0.0.1:9"], calibration=str(path), keep=["out_of_reach"]
    )
    assert [task.data.id for task in MiroirTaskset(config).load()] == ["t3"]


def test_a_token_var_that_is_not_set_is_refused(monkeypatch):
    monkeypatch.delenv("MIROIR_TEST_TOKEN", raising=False)
    config = MiroirTasksetConfig(urls=["http://127.0.0.1:9"], token_var="MIROIR_TEST_TOKEN")
    with pytest.raises(ValueError, match="MIROIR_TEST_TOKEN, which is not set"):
        _token(config)


def test_a_token_in_the_config_serves_a_run_that_passes_no_variables(monkeypatch):
    monkeypatch.setenv("MIROIR_TEST_TOKEN", "from-the-variable")
    given = MiroirTasksetConfig(urls=["http://127.0.0.1:9"], token="pool-token")
    assert _token(given) == "pool-token"
    assert "pool-token" not in repr(given), "a config's repr does not show the token"
    both = MiroirTasksetConfig(token="pool-token", token_var="MIROIR_TEST_TOKEN")
    with pytest.raises(ValueError, match=r"set one of --env\.taskset\.token and"):
        _token(both)
    assert _token(MiroirTasksetConfig(urls=["http://127.0.0.1:9"])) is None


@pytest.mark.integration
def test_an_eval_that_never_submits_is_graded(urls, model, tmp_path):
    model.replies = [{"tool": "api_request", "arguments": {"method": "GET", "path": "/me"}}]
    [trace] = vf_run(urls, model, tmp_path)["traces"]
    assert trace["ok"], trace["errors"]
    assert trace["rewards"]["miroir"]["score"] == 0.0
    assert trace["metrics"]["success"] == 0.0


@pytest.mark.parametrize(
    "urls", [{}, {"urls": ["http://127.0.0.1:9"], "urls_file": "urls.txt"}], ids=["neither", "both"]
)
def test_a_taskset_takes_one_of_urls_and_a_url_file(urls):
    with pytest.raises(ValueError, match="set one of --env"):
        list(MiroirTaskset(MiroirTasksetConfig(**urls)).load())


def test_max_seconds_bounds_each_tasks_agent_time(monkeypatch, tmp_path):
    monkeypatch.setattr(miroir, "tasks", lambda url, split, token: [{"id": "t1"}])
    listed = tmp_path / "urls.txt"
    listed.write_text("http://127.0.0.1:9  # a pool's\n")
    seen = []
    monkeypatch.setattr(miroir, "answering", lambda urls, token: seen.append(urls) or urls[0])
    config = MiroirTasksetConfig(urls_file=str(listed), max_seconds=60)
    [task] = MiroirTaskset(config).load()
    assert task.data.timeout.agent == 60
    assert seen == [["http://127.0.0.1:9"]], "the file's containers"
    [task] = MiroirTaskset(MiroirTasksetConfig(urls=["http://127.0.0.1:9"])).load()
    assert task.data.timeout.agent is None


def test_a_lease_that_completes_after_its_rollout_was_cancelled_is_closed(monkeypatch):
    leasing, release, closed = threading.Event(), threading.Event(), threading.Event()

    class Leased:
        def close(self) -> None:
            closed.set()

    def lease(*args: Any, **kwargs: Any) -> Leased:
        leasing.set()
        release.wait(5)
        return Leased()

    monkeypatch.setattr(Session, "lease", lease)
    env = MiroirEnv.__new__(MiroirEnv)
    env._leases, env._calls = asyncio.Semaphore(1), ThreadPoolExecutor(1)
    env._source, env._token = ["http://127.0.0.1:9"], None

    async def cancel_while_leasing() -> None:
        rollout = asyncio.create_task(env.run(None, None))  # ty: ignore[invalid-argument-type]
        await asyncio.to_thread(leasing.wait, 5)
        rollout.cancel()
        with pytest.raises(asyncio.CancelledError):
            await rollout

    asyncio.run(cancel_while_leasing())
    release.set()
    assert closed.wait(5), "the late lease's container is freed"


@pytest.mark.integration
def test_a_rollout_past_its_max_seconds_is_stopped_and_graded(urls, model, tmp_path):
    model.replies = [{"tool": tool, "arguments": arguments} for tool, arguments in golden(TASK)]
    model.delay_s = 3.0
    episode = vf_run(urls, model, tmp_path, traces=tmp_path / "miroir", max_seconds=1)
    [trace] = episode["traces"]
    assert trace["ok"], trace["errors"]
    assert trace["rewards"]["miroir"]["score"] == 0.0, "graded as it stood when time ran out"
    [saved] = list((tmp_path / "miroir").glob("*.jsonl"))
    [record] = [json.loads(line) for line in saved.read_text().splitlines()]
    assert record["metadata"]["termination"] == "max_seconds"
    assert record["actions"] == [], "every call came after the deadline"


def test_a_taskset_leaves_tasks_with_coworkers_out_without_a_coworker_model(monkeypatch):
    listed = [{"id": "t1"}, {"id": "t2", "coworkers": ["priya"]}]
    monkeypatch.setattr(miroir, "tasks", lambda url, split, token: listed)
    monkeypatch.setattr(miroir, "answering", lambda urls, token: urls[0])
    config = MiroirTasksetConfig(urls=["http://127.0.0.1:9"])
    with pytest.warns(UserWarning, match=r"left out the tasks with coworkers \(1\)"):
        assert [task.data.id for task in MiroirTaskset(config).load()] == ["t1"]
    config = MiroirTasksetConfig(
        urls=["http://127.0.0.1:9"], coworker_model="m", coworker_base_url="http://m/v1"
    )
    loaded = list(MiroirTaskset(config).load())
    assert [task.data.id for task in loaded] == ["t1", "t2"]
    assert all(type(task) is MiroirTask for task in loaded), "the rollout makes the hooked one"


def test_a_coworker_model_needs_its_endpoint_and_its_key(monkeypatch):
    urls = ["http://127.0.0.1:9"]
    assert _coworker(MiroirTasksetConfig(urls=urls)) is None
    with pytest.raises(ValueError, match=r"needs --env\.taskset\.coworker_base_url"):
        _coworker(MiroirTasksetConfig(urls=urls, coworker_model="m"))
    monkeypatch.delenv("MIROIR_TEST_COWORKER_KEY", raising=False)
    keyed = MiroirTasksetConfig(
        urls=urls,
        coworker_model="m",
        coworker_base_url="http://m/v1",
        coworker_api_key_var="MIROIR_TEST_COWORKER_KEY",
    )
    with pytest.raises(ValueError, match="MIROIR_TEST_COWORKER_KEY, which is not set"):
        _coworker(keyed)
    monkeypatch.setenv("MIROIR_TEST_COWORKER_KEY", "k")
    assert _coworker(keyed) == CoworkerModel("http://m/v1", "m", "k")


class FakeLease:
    def __init__(self, coworkers: bool) -> None:
        self.url, self.episode_id, self.coworkers = "http://env:1", "e1", coworkers
        self.reset_with: tuple[Any, ...] = ()
        self.done = False

    def state(self) -> dict:
        return {"done": self.done}

    def reset(self, task_id: str, max_seconds: int | None = None, coworkers: bool = False) -> dict:
        self.reset_with = (task_id, max_seconds, coworkers)
        metadata: dict[str, Any] = {"reset_s": {}}
        if self.coworkers:
            metadata["coworkers"] = ["priya"]
        persona = {"name": "p", "description": ""}
        return {
            "now": "2026-11-09T10:00:00Z",
            "instruction": "Ask.",
            "persona": persona,
            "max_steps": 5,
            "metadata": metadata,
        }

    def finish(self) -> dict:
        record = {"metadata": {"success": True}, "diagnostics": {}, "timings": {"episode_s": 1}}
        return {"reward": 1.0, "success": True, "metadata": {}, "record": record}


class FakeAgents:
    def __init__(self, requests: int) -> None:
        self.tasks: list[Any] = []
        played = self

        class Interaction:
            def __init__(self, task: Any) -> None:
                self.task = task
                self.trace = type(
                    "Trace",
                    (),
                    {"record_reward": lambda *a: None, "record_metrics": lambda *a: None},
                )()

            async def __aenter__(self) -> Self:
                return self

            async def __aexit__(self, *exc: object) -> None:
                return None

            async def turn(self, prompt: str) -> None:
                for _ in range(requests):
                    if not isinstance(self.task, CoworkerTask):
                        continue
                    try:
                        await self.task.coworkers_answer(None)
                    except MiroirError:
                        continue

        class Player:
            def interaction(self, task: Any, tools: Any) -> Interaction:
                played.tasks.append(task)
                return Interaction(task)

        self.player = Player()


def play(lease: FakeLease, coworker: CoworkerModel | None, requests: int = 2) -> FakeAgents:
    env = MiroirEnv.__new__(MiroirEnv)
    env._calls, env._token, env._surface, env._server = ThreadPoolExecutor(1), None, "api", ""
    env._max_seconds, env._traces, env._harm_reward = None, None, None
    env._coworker = coworker
    agents = FakeAgents(requests)
    task = MiroirTask(vf_data("ask-priya"))
    asyncio.run(env._play(lease, task, agents))  # ty: ignore[invalid-argument-type]
    return agents


def vf_data(task_id: str) -> Any:
    import verifiers.v1 as vf

    return vf.TaskData(id=task_id, name=task_id)


def test_a_rollout_hooks_its_task_only_when_its_episode_has_coworkers(monkeypatch):
    answered: list[str] = []
    monkeypatch.setattr(
        miroir, "answer_coworkers", lambda lease, model: answered.append(model.model)
    )
    model = CoworkerModel("http://m/v1", "small")
    lease = FakeLease(coworkers=True)
    [task] = play(lease, model).tasks
    assert type(task) is CoworkerTask and task.data.id == "ask-priya"
    assert lease.reset_with == ("ask-priya", None, True)
    assert answered == ["small", "small"], "before each of the turn's model requests"
    plain = FakeLease(coworkers=False)
    [task] = play(plain, model).tasks
    assert type(task) is MiroirTask, "no coworkers, no hook and no tool gate"
    without = FakeLease(coworkers=False)
    play(without, None)
    assert without.reset_with[2] is False


@pytest.mark.parametrize("coworkers", [False, True])
def test_a_rollout_stops_before_its_next_model_request_once_its_episode_is_over(
    monkeypatch, coworkers
):
    import verifiers.v1 as vf

    monkeypatch.setattr(miroir, "answer_coworkers", lambda lease, model: 0)
    lease = FakeLease(coworkers=coworkers)
    [task] = play(lease, CoworkerModel("http://m/v1", "m")).tasks
    request = vf.Request(messages=[])
    assert asyncio.run(task.episode_over(request)) is False, "the episode still runs"
    lease.done = True
    assert asyncio.run(task.episode_over(request)) is True
    shared = MiroirTask(vf_data("ask-priya"))
    assert asyncio.run(shared.episode_over(request)) is False, "the taskset's own copy never stops"


def test_a_coworker_failure_fails_the_rollout_once_its_turn_ends(monkeypatch):
    calls: list[int] = []

    def down(lease: Any, model: Any) -> int:
        calls.append(1)
        raise MiroirError("COWORKER_FAILED", "the model is down")

    monkeypatch.setattr(miroir, "answer_coworkers", down)
    with pytest.raises(MiroirError, match="the model is down"):
        play(FakeLease(coworkers=True), CoworkerModel("http://m/v1", "m"), requests=3)
    assert calls == [1], "the hook never raises into verifiers, and stops after the first failure"


@pytest.mark.integration
def test_a_coworker_answers_in_an_eval_through_the_hook(coworker_url, model, tmp_path):
    task = yaml.safe_load(ASK_PRIYA.read_text())
    model.replies = [
        {"tool": step["tool"], "arguments": step.get("arguments", {})}
        for step in task["golden"]
        if step["tool"] != "coworker_reply"
    ]
    priya = FakeModel("Qualified: they asked to re-scope.")
    try:
        episode = vf_run(
            [coworker_url],
            model,
            tmp_path,
            traces=tmp_path / "miroir",
            task="ask-priya",
            split="test",
            coworker=priya.url,
        )
    finally:
        priya.close()
    [trace] = episode["traces"]
    assert trace["ok"], trace["errors"]
    assert trace["rewards"]["miroir"]["score"] == 1.0, "she answered before the agent moved on"
    [saved] = list((tmp_path / "miroir").glob("*.jsonl"))
    [record] = [json.loads(line) for line in saved.read_text().splitlines()]
    replies = [a for a in record["actions"] if a.get("via") == "coworker"]
    assert [r["arguments"]["text"] for r in replies] == ["Qualified: they asked to re-scope."]
    assert priya.requests[0][2]["messages"][-1]["content"].startswith("@priya")
