import datetime
import inspect
import json
import time
from typing import Any, cast
from unittest.mock import patch

import pytest
from conftest import ROOT, FakeModel, golden
from transformers import PreTrainedTokenizerBase
from transformers.utils import get_json_schema

import miroir
from miroir import CoworkerModel, MiroirError, Session
from miroir.trl import MiroirEnv, _env_class, dataset, environment_factory, reward

TOOLS = json.loads((ROOT / "src" / "tools.json").read_text())


def listed(tool: dict) -> dict:
    return {
        "name": tool["name"],
        "description": tool["description"],
        "inputSchema": tool["input_schema"],
    }


LISTED = [listed(tool) for tool in TOOLS]


def exposed(env_class: type) -> list[str]:
    functions = inspect.getmembers(env_class, inspect.isfunction)
    return sorted(name for name, _ in functions if name != "reset" and not name.startswith("_"))


@pytest.mark.parametrize("tool", TOOLS, ids=lambda tool: tool["name"])
def test_each_tool_shows_the_model_miroirs_schema(tool):
    env = MiroirEnv.__new__(MiroirEnv)
    schema = get_json_schema(getattr(env, tool["name"]))["function"]
    expected = tool["input_schema"]
    assert schema["description"] == tool["description"]
    parameters = schema["parameters"]
    assert parameters["properties"].keys() == expected["properties"].keys()
    assert set(parameters.get("required", [])) == set(expected.get("required", []))
    for name, spec in expected["properties"].items():
        for key, value in spec.items():
            assert parameters["properties"][name][key] == value, (name, key)


def test_trl_sees_only_the_tools_the_env_offers():
    assert exposed(MiroirEnv) == sorted(tool["name"] for tool in TOOLS)
    assert _env_class(LISTED) is MiroirEnv
    no_docs = [tool for tool in LISTED if tool["name"] not in ("api_docs", "run_code", "wait")]
    assert exposed(_env_class(no_docs)) == ["api_request", "submit"]
    code = [tool for tool in LISTED if tool["name"] not in ("api_request", "wait")]
    assert exposed(_env_class(code)) == ["api_docs", "run_code", "submit"], "the code surface"
    clock = [tool for tool in LISTED if tool["name"] not in ("api_docs", "run_code")]
    assert exposed(_env_class(clock)) == ["api_request", "submit", "wait"], "an env with a clock"
    with pytest.raises(MiroirError, match="upgrade the miroir package"):
        _env_class([*LISTED, {"name": "teleport"}])


def with_apps(tool: dict, names: list[str]) -> dict:
    tool = json.loads(json.dumps(tool))
    schema = tool["inputSchema"]
    schema["properties"]["app"] = {
        "type": "string",
        "enum": names,
        "description": "The app to call: " + "; ".join(f"{n}, the {n} CRM" for n in names),
    }
    schema["required"] = ["app", *schema.get("required", [])]
    return tool


def test_with_several_apps_the_api_tools_name_one():
    apps = {"api_request": ["east", "west"], "api_docs": ["east"]}
    tools = [
        with_apps(tool, apps[tool["name"]]) if tool["name"] in apps else tool for tool in LISTED
    ]
    env_class = _env_class(tools)
    assert exposed(env_class) == exposed(MiroirEnv)
    env = env_class.__new__(env_class)
    for tool in tools:
        schema = get_json_schema(getattr(env, tool["name"]))["function"]
        expected = tool["inputSchema"]
        assert schema["name"] == tool["name"]
        assert schema["description"] == tool["description"]
        parameters = schema["parameters"]
        assert parameters["properties"] == {
            name: {**parameters["properties"][name], **spec}
            for name, spec in expected["properties"].items()
        }
        assert parameters["properties"].get("app") == expected["properties"].get("app")
        assert set(parameters.get("required", [])) == set(expected.get("required", []))
    sent = []
    with patch.object(
        env_class, "_call", lambda self, tool, arguments: sent.append((tool, arguments)) or "ok"
    ):
        dynamic = cast(Any, env)
        dynamic.api_request(app="west", method="GET", path="/me")
        dynamic.api_docs(app="east")
    assert sent == [
        (
            "api_request",
            {"app": "west", "method": "GET", "path": "/me", "body": None, "headers": None},
        ),
        ("api_docs", {"app": "east", "operation": None}),
    ]


def test_a_non_string_answer_is_refused_before_it_reaches_the_env():
    env = MiroirEnv.__new__(MiroirEnv)
    with pytest.raises(ValueError, match="answer must be a string"):
        env.submit(answer=3)  # ty: ignore[invalid-argument-type]


class FlakySession:
    def __init__(
        self,
        code: str | None = None,
        now: str | None = None,
        earned: float = 1.0,
        *,
        harm: bool = False,
    ) -> None:
        self.code, self.now, self.resets, self.calls = code, now, 0, 0
        self.earned, self.harm = earned, harm

    def reset(self, task_id: str, max_seconds: int | None = None, coworkers: bool = False) -> dict:
        self.resets += 1
        self.max_seconds, self.coworkers = max_seconds, coworkers
        if self.code and self.resets == 1:
            raise MiroirError(self.code, "reset failed at step 'quiet': not quiet after 30 s")
        now = self.now or datetime.datetime.now(datetime.UTC).isoformat()
        return {"now": now, "metadata": {"reset_s": {}}}

    def finish(self) -> dict:
        self.calls += 2
        metadata = {"success": self.earned == 1.0, "harm": self.harm}
        record = {
            "episode_id": f"e{self.calls}",
            "reward": self.earned,
            "metadata": metadata,
            "diagnostics": {"calls": self.calls, "client_errors": 0},
            "timings": {"episode_s": self.calls / 2},
        }
        outcome = {"reward": self.earned, "success": metadata["success"], "metadata": metadata}
        return {**outcome, "record": record}

    def close(self) -> None:
        pass


ROW = {"task_id": "t", "instruction": "Do it.", "persona": {"description": ""}, "max_steps": 5}


def test_a_calibrated_dataset_has_only_the_kept_tasks(tmp_path, monkeypatch):
    task = {"split": "train", "instruction": "Do it.", "persona": ROW["persona"], "max_steps": 5}
    listed = [{"id": task_id, "task_hash": "a" * 16, **task} for task_id in ("t1", "t2")]
    monkeypatch.setattr(miroir, "tasks", lambda url, split, token: listed)
    tasks = {
        "t1": {"task_hash": "a" * 16, "class": "learnable"},
        "t2": {"task_hash": "a" * 16, "class": "solved"},
    }
    path = tmp_path / "calibration.json"
    path.write_text(json.dumps({"model": "m", "k": 8, "tasks": tasks}), encoding="utf-8")
    assert dataset("http://127.0.0.1:9", calibration=str(path))["task_id"] == ["t1"]
    assert len(dataset("http://127.0.0.1:9")) == 2, "no calibration, every task"


def test_the_envs_system_prompt_opens_each_rows_prompt(monkeypatch):
    task = {"split": "train", "instruction": "Do it.", "persona": ROW["persona"], "max_steps": 5}
    listed = [{"id": "t1", "task_hash": "a" * 16, **task}]
    monkeypatch.setattr(miroir, "tasks", lambda url, split, token: listed)
    assert "prompt" not in dataset("http://127.0.0.1:9").column_names, "no system prompt"
    listed[0]["system_prompt"] = "Use only the tools."
    [row] = dataset("http://127.0.0.1:9")
    assert row["prompt"] == [
        {"role": "system", "content": "Use only the tools."},
        {"role": "user", "content": ""},
    ]
    assert row["instruction"] == "Do it.", "reset still renders the task's own text"


def test_a_dataset_leaves_tasks_with_coworkers_out_unless_asked(monkeypatch):
    task = {"split": "train", "instruction": "Do it.", "persona": ROW["persona"], "max_steps": 5}
    listed = [
        {"id": "t1", "task_hash": "a" * 16, **task},
        {"id": "t2", "task_hash": "b" * 16, "coworkers": ["priya"], **task},
    ]
    monkeypatch.setattr(miroir, "tasks", lambda url, split, token: listed)
    with pytest.warns(UserWarning, match=r"left out the tasks with coworkers \(1\)"):
        assert dataset("http://127.0.0.1:9")["task_id"] == ["t1"]
    assert dataset("http://127.0.0.1:9", coworkers=True)["task_id"] == ["t1", "t2"]


class CoworkerSession(FlakySession):
    def __init__(self) -> None:
        super().__init__()
        self.log: list[str] = []
        self.owed = False

    def reset(self, task_id: str, max_seconds: int | None = None, coworkers: bool = False) -> dict:
        observation = super().reset(task_id, max_seconds, coworkers)
        self.log.append("reset")
        observation["metadata"]["coworkers"] = ["priya"]
        return observation

    def step(self, tool: str, arguments: dict) -> dict:
        self.log.append(f"{tool} {arguments.get('method', '')}".strip())
        self.owed = self.owed or arguments.get("method") == "POST"
        return {"observation": {"result": {"status": 200}, "error": None}, "done": False}

    def coworker_turns(self) -> list[dict]:
        self.log.append("coworkers")
        return [{"coworker": "priya", "messages": [{"role": "user", "content": "?"}]}] * self.owed

    def coworker_reply(self, coworker: str, text: str) -> int:
        self.log.append(f"{coworker}: {text}")
        self.owed = False
        return 201


def test_a_coworker_answers_after_the_call_that_reached_them_and_before_the_next():
    model = FakeModel("Qualified.")
    try:
        lease = CoworkerSession()
        with patch.object(Session, "lease", return_value=lease):
            env = MiroirEnv(["http://127.0.0.1:9"], coworker=CoworkerModel(model.url, "m"))
            env.reset(**ROW)
            env.api_request("POST", "/deals/1100/notes", {"text": "@priya which stage?"})
            env.api_request("GET", "/deals/1100/notes")
            assert reward([env]) == [1.0]
        assert lease.coworkers is True, "the reset says this env answers coworkers"
        assert lease.log == [
            "reset",
            "api_request POST",
            "coworkers",
            "priya: Qualified.",
            "api_request GET",
            "coworkers",
        ]
    finally:
        model.close()


def test_without_a_coworker_model_an_env_answers_for_nobody():
    lease = CoworkerSession()
    with patch.object(Session, "lease", return_value=lease):
        env = MiroirEnv(["http://127.0.0.1:9"])
        env.reset(**ROW)
        env.api_request("POST", "/deals/1100/notes", {"text": "@priya which stage?"})
        assert reward([env]) == [1.0]
    assert lease.coworkers is False
    assert "coworkers" not in lease.log


class AccentedSession(FlakySession):
    def step(self, tool: str, arguments: dict) -> dict:
        result = {"status": 200, "body": [{"login": "ines", "full_name": "Inès Moreau"}]}
        return {"observation": {"result": result, "error": None}, "done": False}


def test_a_result_reaches_the_model_as_compact_json_with_its_own_letters():
    with patch.object(Session, "lease", return_value=AccentedSession()):
        env = MiroirEnv(["http://127.0.0.1:9"])
        env.reset(**ROW)
        shown = env.api_request("GET", "/orgs/orbit/members")
    assert shown == '{"status":200,"body":[{"login":"ines","full_name":"Inès Moreau"}]}'


def test_a_coworker_the_model_cant_answer_for_voids_the_rollout():
    model = FakeModel(status=500)
    try:
        with patch.object(Session, "lease", return_value=CoworkerSession()):
            env = MiroirEnv(["http://127.0.0.1:9"], coworker=CoworkerModel(model.url, "m"))
            env.reset(**ROW)
            env.api_request("POST", "/deals/1100/notes", {"text": "@priya which stage?"})
            with pytest.raises(MiroirError, match="COWORKER_FAILED"):
                env.api_request("GET", "/deals/1100/notes")
            assert reward([env]) == [None]
        assert "COWORKER_FAILED" in env.metadata["invalid"]
    finally:
        model.close()


def test_reward_logs_the_batchs_metrics_and_saves_each_record(tmp_path):
    sessions = [FlakySession(), FlakySession()]
    sessions[1].calls = 2
    logged: list[tuple[str, float]] = []
    with patch.object(Session, "lease", side_effect=sessions):
        envs = [MiroirEnv(["http://127.0.0.1:9"], traces=str(tmp_path)) for _ in sessions]
        for env in envs:
            env.reset(**ROW)
        rewards = reward(envs, log_metric=lambda name, value: logged.append((name, value)))
    assert rewards == [1.0, 1.0]
    assert logged == [
        ("miroir/success", 1.0),
        ("miroir/harm", 0.0),
        ("miroir/calls", 3.0),
        ("miroir/client_errors", 0.0),
        ("miroir/episode_s", 1.5),
    ], "the batch's means"
    [saved] = list(tmp_path.glob("*.jsonl"))
    episodes = [json.loads(line)["episode_id"] for line in saved.read_text().splitlines()]
    assert episodes == ["e2", "e4"]
    assert [env.record and env.record["episode_id"] for env in envs] == ["e2", "e4"]


@pytest.mark.parametrize(
    ("earned", "harm", "trained"),
    [(0.0, True, -0.5), (-1.0, True, -1.0), (0.0, False, 0.0), (0.4, False, 0.4)],
)
def test_harm_earns_the_training_harm_reward_unless_its_task_sets_a_lower_one(
    earned, harm, trained
):
    with patch.object(Session, "lease", return_value=FlakySession(earned=earned, harm=harm)):
        env = MiroirEnv(["http://127.0.0.1:9"], harm_reward=-0.5)
        env.reset(**ROW)
        assert reward([env]) == [trained]
    assert (env.reward, env.record and env.record["reward"]) == (trained, earned), "the env's own"


@pytest.mark.parametrize("wrong", [0.5, -1.5])
def test_a_harm_reward_out_of_range_is_refused(wrong):
    with pytest.raises(ValueError, match="between -1 and 0"):
        environment_factory(["http://127.0.0.1:9"], harm_reward=wrong)


@pytest.mark.parametrize(("code", "leases"), [("EXECUTION_ERROR", 1), ("CONNECTION_LOST", 2)])
def test_only_a_lost_connection_gives_up_the_lease(code, leases):
    with patch.object(Session, "lease", return_value=FlakySession(code)) as lease:
        env = MiroirEnv(["http://127.0.0.1:9"])
        env.reset(**ROW)
        assert reward([env]) == [None], "the failed reset voids its rollout"
        env.reset(**ROW)
        assert reward([env]) == [1.0], "and the next reset recovers"
    assert lease.call_count == leases, "a broken container is one no other env would lease"


def test_a_reset_on_another_day_than_the_prompt_voids_its_rollout():
    with patch.object(Session, "lease", return_value=FlakySession(now="2001-01-01T00:00:01Z")):
        env = MiroirEnv(["http://127.0.0.1:9"])
        prompt = env.reset(**ROW)
        assert reward([env]) == [None], "the model was told another day than the data's"
    assert "CLOCK_ROLLED" in env.metadata["invalid"] and "2001-01-01" not in prompt


@pytest.fixture
def envs(urls):
    make = environment_factory(urls)
    made = [make(), make()]
    yield made
    for env in made:
        env._close()


@pytest.fixture
def rows(urls):
    return {row["task_id"]: row for row in dataset(urls[0])}


@pytest.mark.integration
def test_two_envs_play_the_golden_run_and_earn_full_reward(envs, rows):
    for env in envs:
        started = time.monotonic()
        prompt = env.reset(**rows["reassign-stale-deals"])
        assert time.monotonic() - started < 1, "the reset runs in the background"
        assert prompt.startswith("Priya Shah leaves the team")
    for env in envs:
        for tool, arguments in golden("reassign-stale-deals"):
            shown = getattr(env, tool)(**arguments)
        assert shown == "Submitted; the episode is over."
        with pytest.raises(RuntimeError, match="the episode is over"):
            env.api_request(method="GET", path="/me")
    assert reward(envs) == [1.0, 1.0]
    assert [(env.success, env.metadata["termination"]) for env in envs] == [(True, "submit")] * 2
    signatures = {env.record and env.record["signature"] for env in envs}
    assert len(signatures) == 1, "two containers, one golden run, one end state"


@pytest.mark.integration
def test_a_rollout_that_never_submits_is_graded(envs, rows):
    env = envs[0]
    env.reset(**rows["count-overdue-invoices"])
    shown = json.loads(env.api_request(method="GET", path="/contacts?q=Northwind"))
    assert shown["status"] == 200
    assert reward([env, object()]) == [0.0, None], "only Miroir envs get a Miroir reward"
    assert env.metadata["termination"] == "submit"


@pytest.mark.integration
def test_a_failed_reset_scores_none_and_the_next_reset_recovers(envs, rows):
    env = envs[0]
    env.reset(**{**rows["count-overdue-invoices"], "task_id": "no-such-task"})
    with pytest.raises(MiroirError, match="unknown task"):
        env.api_request(method="GET", path="/me")
    assert reward([env]) == [None]
    assert "unknown task" in env.metadata["invalid"]
    env.reset(**rows["count-overdue-invoices"])
    env.submit(answer="3")
    assert reward([env]) == [1.0]


@pytest.mark.integration
def test_trl_wants_a_container_per_env(urls, envs):
    with pytest.raises(MiroirError, match="needs a container of its own"):
        environment_factory(urls)()


@pytest.mark.integration
def test_grpo_plays_a_scripted_golden_run_through_trls_tool_loop(urls, tmp_path, monkeypatch):
    import torch
    from trl import GRPOConfig, GRPOTrainer

    monkeypatch.setenv("TRL_EXPERIMENTAL_SILENCE", "1")
    make = environment_factory(urls)
    made: list[MiroirEnv] = []

    def factory() -> MiroirEnv:
        made.append(make())
        return made[-1]

    task = dataset(urls[0]).filter(lambda row: row["task_id"] == "reassign-stale-deals")
    trainer = GRPOTrainer(
        model="trl-internal-testing/tiny-Qwen3MoeForCausalLM",
        train_dataset=task,
        environment_factory=factory,
        reward_funcs=reward,
        args=GRPOConfig(
            output_dir=str(tmp_path),
            per_device_train_batch_size=2,
            num_generations=2,
            max_completion_length=8192,
            max_steps=1,
            logging_steps=1,
            report_to="none",
            dataloader_pin_memory=False,
            use_cpu=True,
        ),
    )
    replies = [
        f"<tool_call>\n{json.dumps({'name': tool, 'arguments': arguments})}\n</tool_call><|im_end|>"
        for tool, arguments in golden("reassign-stale-deals")
    ] + ["Done.<|im_end|>"]
    tokenizer = cast(PreTrainedTokenizerBase, trainer.processing_class)

    def scripted_generate(input_ids, **_):
        ids = tokenizer.encode(replies.pop(0), add_special_tokens=False)
        completion = torch.tensor([ids] * input_ids.shape[0], device=input_ids.device)
        return torch.cat([input_ids, completion], dim=-1)

    try:
        with patch.object(trainer.model, "generate", side_effect=scripted_generate):
            trainer.train()
    finally:
        for env in made:
            env._close()
    assert replies == [], "every golden call was generated"
    assert len(made) == 2, "one env per rollout of the batch"
    assert [(env.reward, env.success) for env in made] == [(1.0, True)] * 2
    assert trainer.state.log_history[0]["rewards/reward/mean"] == 1.0
    assert trainer.state.log_history[0]["miroir/calls"] == len(golden("reassign-stale-deals"))


@pytest.mark.parametrize(
    ("given", "error"),
    [
        ({"max_seconds": 1.5}, "max_seconds must be a whole number"),
        ({"max_seconds": True}, "max_seconds must be a whole number"),
        ({"max_seconds": 0}, "max_seconds must be a whole number"),
        ({"urls": None}, "one of urls and urls_file"),
        ({"urls_file": "urls.txt"}, "one of urls and urls_file"),
        ({"surface": "mcp"}, "surface must be api or code"),
    ],
)
def test_a_factory_refuses_a_bad_time_limit_or_container_list(given, error):
    arguments: dict[str, Any] = {"urls": ["http://127.0.0.1:9"], **given}
    with pytest.raises(ValueError, match=error):
        environment_factory(**arguments)


def test_a_factory_lists_the_tools_of_its_surface(monkeypatch):
    asked = []

    def tools(url, token, surface):
        asked.append(surface)
        return [tool for tool in LISTED if tool["name"] != "api_request"]

    monkeypatch.setattr(miroir, "answering", lambda urls, token: urls[0])
    monkeypatch.setattr(miroir, "tools", tools)
    make = environment_factory(["http://127.0.0.1:9"], surface="code")
    assert asked == ["code"]
    with patch.object(Session, "lease", return_value=FlakySession()):
        env = make()
    offered = cast(Any, env)
    assert offered.api_request is None and callable(offered.run_code)


def test_an_env_leases_from_its_url_file_and_sends_its_time_limit(tmp_path):
    listed = tmp_path / "urls.txt"
    flaky = FlakySession()
    with patch.object(Session, "lease", return_value=flaky) as lease:
        env = MiroirEnv(listed, max_seconds=60)
        env.reset(**ROW)
        assert reward([env]) == [1.0]
    assert lease.call_args.args[0] == listed, "the file, read again at every lease"
    assert flaky.max_seconds == 60
