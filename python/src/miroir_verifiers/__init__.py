"""verifiers adapter: the `miroir-verifiers` taskset and env, which `vf-eval` and training load."""

from __future__ import annotations

import asyncio
import datetime
import functools
import os
from collections.abc import Awaitable, Callable, Iterator, Sequence
from concurrent.futures import Future, ThreadPoolExecutor
from pathlib import Path
from typing import Any, Literal, cast
from urllib.parse import urlencode

import verifiers.v1 as vf
from pydantic import Field
from verifiers.v1.mcp import SharedToolServer

import miroir
from miroir import CoworkerModel, MiroirError, Session
from miroir.calibrate import calibrated

__all__ = ["MiroirEnv", "MiroirEnvConfig", "MiroirTaskset", "MiroirTasksetConfig"]

LEASE_TIMEOUT_S = 3600.0
UNPREFIXED = ("null", "bash")


class MiroirTasksetConfig(vf.TasksetConfig):
    """Options under `--env.taskset`: the containers, the split, and how episodes run."""

    urls: list[str] = Field(default_factory=list)
    urls_file: str | None = None
    split: str = "train"
    token_var: str | None = None
    traces: str | None = None
    calibration: str | None = None
    keep: list[str] = Field(default_factory=lambda: ["learnable"])
    harm_reward: float | None = Field(default=None, ge=-1.0, le=0.0)
    surface: Literal["api", "mcp", "code"] = "api"
    max_seconds: int | None = Field(default=None, ge=1, le=86_400)
    coworker_model: str | None = None
    coworker_base_url: str | None = None
    coworker_api_key_var: str | None = None


class MiroirTask(vf.Task[vf.TaskData]):
    """A task of the env, by id."""


class CoworkerTask(MiroirTask):
    """A task whose coworkers answer before each model request."""

    answer: Callable[[], Awaitable[None]] | None = None

    @vf.intercept
    async def coworkers_answer(self, request: vf.Request) -> None:
        """Answer the owed coworker turns before the request reaches the model."""
        if self.answer is not None:
            await self.answer()


class MiroirTaskset(vf.Taskset[MiroirTask, MiroirTasksetConfig]):
    """The split's tasks, as the env serves them."""

    def load(self) -> Iterator[MiroirTask]:
        """Yield the tasks, leaving out those with coworkers unless a coworker model is set."""
        config = self.config
        token = _token(config)
        tasks = miroir.tasks(miroir.answering(_urls(config), token), config.split, token)
        tasks = miroir.answerable(tasks, config.coworker_model is not None)
        if config.calibration is not None:
            tasks = calibrated(tasks, config.calibration, config.keep)
        timeout = vf.TaskTimeout(agent=config.max_seconds)
        for task in tasks:
            data = vf.TaskData(
                id=task["id"],
                name=task["id"],
                system_prompt=task.get("system_prompt"),
                timeout=timeout,
            )
            yield MiroirTask(data, config.task)


class MiroirEnvConfig(vf.EnvConfig):
    """The env's config: the player runs with no harness of its own, in a subprocess."""

    player: vf.AgentConfig = vf.AgentConfig(harness={"id": "null"}, runtime=vf.SubprocessConfig())


class MiroirEnv(vf.Env[MiroirEnvConfig]):
    """Leases a container per rollout and serves the env's tools to the player over MCP."""

    def __init__(self, config: MiroirEnvConfig) -> None:
        super().__init__(config)
        taskset = cast(MiroirTasksetConfig, self.taskset.config)
        urls, self._token = _urls(taskset), _token(taskset)
        self._source: Sequence[str] | Path = Path(taskset.urls_file) if taskset.urls_file else urls
        self._traces, self._harm_reward = taskset.traces, taskset.harm_reward
        self._surface, self._max_seconds = taskset.surface, taskset.max_seconds
        self._coworker = _coworker(taskset)
        harness = self.config.player.harness
        self._server = _server_name(harness.id if harness else None)
        self._leases = asyncio.Semaphore(len(urls))
        self._calls = ThreadPoolExecutor(len(urls), thread_name_prefix="miroir")

    async def run(self, task: vf.Task, agents: vf.Agents) -> None:
        """Play one rollout on a leased container, and record its reward and metrics."""
        async with self._leases:
            leasing = self._calls.submit(
                Session.lease, self._source, self._token, wait_busy=True, timeout_s=LEASE_TIMEOUT_S
            )
            try:
                session = await asyncio.wrap_future(leasing)
            except asyncio.CancelledError:
                leasing.add_done_callback(_close_late_lease)
                raise
            try:
                await self._play(session, task, agents)
            finally:
                await asyncio.shield(self._call(session.close))

    async def _play(self, session: Session, task: vf.Task, agents: vf.Agents) -> None:
        assert task.data.id is not None
        coworkers = self._coworker is not None
        observation = await self._call(session.reset, task.data.id, self._max_seconds, coworkers)
        today = datetime.date.fromisoformat(observation["now"][:10])
        mcp_url = _mcp_url(session.url, session.episode_id, self._token, self._surface)
        tools = {self._server: SharedToolServer(url=mcp_url, local=True, external=True)}
        failures: list[MiroirError] = []
        if self._coworker is not None and observation["metadata"].get("coworkers"):
            task = CoworkerTask(task.data, task.config)
            task.answer = self._answering(session, self._coworker, failures)
        async with agents.player.interaction(task, tools=tools) as interaction:
            await interaction.turn(miroir.prompt(observation, today))
            if failures:
                raise failures[0]
            outcome = await self._call(session.finish)
            record = outcome["record"]
            if self._traces is not None:
                await self._call(miroir.save_record, self._traces, record)
            reward = outcome["reward"]
            if reward is None:
                reason = outcome["metadata"].get("invalid", "see the env's logs")
                raise MiroirError("INVALID_GRADE", f"the episode has no valid reward: {reason}")
            if self._harm_reward is not None and outcome["metadata"].get("harm"):
                reward = min(reward, self._harm_reward)
            interaction.trace.record_reward("miroir", reward)
            interaction.trace.record_metrics(miroir.metrics(record))

    def _answering(
        self, session: Session, model: CoworkerModel, failures: list[MiroirError]
    ) -> Callable[[], Awaitable[None]]:
        lock = asyncio.Lock()

        async def answer() -> None:
            if failures:
                return
            async with lock:
                try:
                    await self._call(miroir.answer_coworkers, session, model)
                except MiroirError as e:
                    failures.append(e)

        return answer

    async def _call(self, function: Callable[..., Any], *args: Any, **kwargs: Any) -> Any:
        loop = asyncio.get_running_loop()
        return await loop.run_in_executor(self._calls, functools.partial(function, *args, **kwargs))


def _mcp_url(url: str, episode_id: str | None, token: str | None, surface: str) -> str:
    query = {
        "episode_id": episode_id,
        **({"token": token} if token else {}),
        **({"surface": surface} if surface != "api" else {}),
    }
    return f"{url}/mcp?{urlencode(query)}"


def _server_name(harness: str | None) -> str:
    return "" if harness is None or harness in UNPREFIXED else "miroir"


def _close_late_lease(leasing: Future[Session]) -> None:
    if not leasing.cancelled() and leasing.exception() is None:
        leasing.result().close()


def _urls(config: MiroirTasksetConfig) -> list[str]:
    if bool(config.urls) == (config.urls_file is not None):
        raise ValueError(
            "set one of --env.taskset.urls (the containers' base URLs, as a JSON list) and "
            "--env.taskset.urls_file (a file of them, which `miroir pool` writes)"
        )
    return miroir.read_urls(config.urls_file) if config.urls_file else config.urls


def _coworker(config: MiroirTasksetConfig) -> CoworkerModel | None:
    if config.coworker_model is None:
        return None
    if not config.coworker_base_url:
        raise ValueError(
            "--env.taskset.coworker_model needs --env.taskset.coworker_base_url, the "
            "OpenAI-compatible endpoint that serves it"
        )
    key = None
    if config.coworker_api_key_var is not None:
        key = os.environ.get(config.coworker_api_key_var)
        if not key:
            var = config.coworker_api_key_var
            raise ValueError(f"--env.taskset.coworker_api_key_var names {var}, which is not set")
    return CoworkerModel(config.coworker_base_url, config.coworker_model, key)


def _token(config: MiroirTasksetConfig) -> str | None:
    if config.token_var is None:
        return None
    token = os.environ.get(config.token_var)
    if not token:
        raise ValueError(f"--env.taskset.token_var names {config.token_var}, which is not set")
    return token
