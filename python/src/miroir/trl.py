"""TRL adapter: a task dataset, an environment factory for `GRPOTrainer`, and its reward."""

from __future__ import annotations

import datetime
import json
import os
import types
from collections.abc import Callable, Sequence
from concurrent.futures import Future, ThreadPoolExecutor
from pathlib import Path
from typing import Any, Literal

from datasets import Dataset

import miroir
from miroir import CoworkerModel, MiroirError, Session
from miroir.calibrate import calibrated

__all__ = ["MiroirEnv", "dataset", "environment_factory", "reward"]

EPISODE_OVER = "the episode is over; stop calling tools"
TOOLS = ("api_request", "api_docs", "run_code", "wait", "submit")


def dataset(
    url: str,
    split: str = "train",
    token: str | None = None,
    calibration: str | None = None,
    keep: Sequence[str] = ("learnable",),
    coworkers: bool = False,
) -> Dataset:
    """A split's tasks as a dataset; with `calibration`, only the classes in `keep`."""
    tasks = miroir.answerable(miroir.tasks(url, split, token), coworkers)
    if calibration is not None:
        tasks = calibrated(tasks, calibration, keep)
    fields = ("instruction", "persona", "max_steps")
    rows = [{"task_id": task["id"], **{field: task[field] for field in fields}} for task in tasks]
    for row, task in zip(rows, tasks, strict=True):
        if task.get("system_prompt"):
            system = {"role": "system", "content": task["system_prompt"]}
            row["prompt"] = [system, {"role": "user", "content": ""}]
    return Dataset.from_list(rows)


def environment_factory(
    urls: Sequence[str] | None = None,
    token: str | None = None,
    traces: str | None = None,
    *,
    harm_reward: float | None = None,
    urls_file: str | os.PathLike[str] | None = None,
    max_seconds: int | None = None,
    surface: Literal["api", "code"] = "api",
    coworker: CoworkerModel | None = None,
) -> Callable[[], MiroirEnv]:
    """A factory of `MiroirEnv`s, each leasing a container, with the tools the env offers."""
    if harm_reward is not None and not -1.0 <= harm_reward <= 0.0:
        raise ValueError(f"harm_reward must be between -1 and 0, not {harm_reward}")
    whole = isinstance(max_seconds, int) and not isinstance(max_seconds, bool)
    if max_seconds is not None and not (whole and 1 <= max_seconds <= 86_400):
        raise ValueError(
            f"max_seconds must be a whole number from 1 to 86,400, not {max_seconds!r}"
        )
    if (urls is None) == (urls_file is None):
        raise ValueError("pass one of urls and urls_file")
    if surface not in ("api", "code"):
        raise ValueError(f"surface must be api or code, not {surface!r}")
    source: Sequence[str] | Path = Path(urls_file) if urls_file is not None else urls or []
    listed = miroir.read_urls(source) if isinstance(source, Path) else source
    env_class = _env_class(miroir.tools(miroir.answering(listed, token), token, surface))
    return lambda: env_class(source, token, traces, harm_reward, max_seconds, coworker)


def reward(
    environments: Sequence[Any],
    log_metric: Callable[[str, float], None] | None = None,
    **_: Any,
) -> list[float | None]:
    """Finish each env's episode and return the rewards, logging the mean `miroir/*`
    metrics.
    """
    finishing = [
        env._calls.submit(env._finish) if isinstance(env, MiroirEnv) else None
        for env in environments
    ]
    rewards = [None if future is None else future.result() for future in finishing]
    records = []
    for env in environments:
        if isinstance(env, MiroirEnv) and env.record is not None:
            records.append(env.record)
            if env._traces is not None:
                miroir.save_record(env._traces, env.record)
    if log_metric is not None and records:
        rows = [miroir.metrics(record) for record in records]
        for name in rows[0]:
            log_metric(f"miroir/{name}", sum(row[name] for row in rows) / len(rows))
    return rewards


def _env_class(tools: Sequence[dict[str, Any]]) -> type[MiroirEnv]:
    offered = {tool["name"]: tool for tool in tools}
    unknown = offered.keys() - set(TOOLS)
    if unknown:
        raise MiroirError(
            "UNKNOWN_TOOLS",
            f"the env offers tools this client does not know: {sorted(unknown)}; "
            "upgrade the miroir package",
        )
    changed: dict[str, Any] = {name: None for name in TOOLS if name not in offered}
    templates = {"api_request": _api_request_in, "api_docs": _api_docs_in}
    for name, template in templates.items():
        app = offered.get(name, {}).get("inputSchema", {}).get("properties", {}).get("app")
        if app is not None:
            changed[name] = _with_app(template, name, app)
    return type("MiroirEnv", (MiroirEnv,), changed) if changed else MiroirEnv


def _with_app(template: types.FunctionType, name: str, app: dict[str, Any]) -> types.FunctionType:
    tool = types.FunctionType(template.__code__, template.__globals__, name, template.__defaults__)
    tool.__qualname__ = f"MiroirEnv.{name}"
    kept = {key: hint for key, hint in template.__annotations__.items() if key != "self"}
    names = Literal[tuple(app["enum"])]  # ty: ignore[invalid-type-form]
    tool.__annotations__ = {**kept, "app": names}
    tool.__doc__ = (template.__doc__ or "").replace(
        "app: The app to call.", f"app: {app['description']}"
    )
    return tool


def _api_request_in(
    self: MiroirEnv,
    app: str,
    method: Literal["GET", "POST", "PUT", "PATCH", "DELETE"],
    path: str,
    body: Any = None,
    headers: dict[str, str] | None = None,
) -> str:
    """Send an HTTP request to the product's API as the current user. Returns the status code, the content-type, location and link headers, and the body.

    Args:
        app: The app to call.
        method: The HTTP method.
        path: Path and query string relative to the API base URL, such as /deals?owner_id=12
        body: Request body, sent as JSON unless headers set a content-type: then a string is sent as-is, and for application/x-www-form-urlencoded an object is form-encoded
        headers: Extra request headers.
    """
    arguments = {"app": app, "method": method, "path": path, "body": body, "headers": headers}
    return self._call("api_request", arguments)


def _api_docs_in(self: MiroirEnv, app: str, operation: str | None = None) -> str:
    """Look up the product's API. With no arguments, lists every operation. With an operation ID or 'METHOD /path', returns that operation's parameters and schemas.

    Args:
        app: The app to call.
        operation: An operation ID or 'METHOD /path'; leave it out to list every operation.
    """
    return self._call("api_docs", {"app": app, "operation": operation})


class MiroirEnv:
    """A TRL environment on one leased container; its public methods are the agent's tools."""

    def __init__(
        self,
        urls: Sequence[str] | Path,
        token: str | None = None,
        traces: str | None = None,
        harm_reward: float | None = None,
        max_seconds: int | None = None,
        coworker: CoworkerModel | None = None,
    ) -> None:
        self._urls, self._token, self._traces = urls, token, traces
        self._harm_reward, self._max_seconds = harm_reward, max_seconds
        self._coworker = coworker
        self._coworkers = False
        self._calls = ThreadPoolExecutor(max_workers=1, thread_name_prefix="miroir")
        try:
            self._session: Session | None = Session.lease(urls, token, wait_busy=False)
        except MiroirError as e:
            raise MiroirError(
                e.code,
                f"{e.message}. TRL makes per_device_train_batch_size * steps_per_generation envs "
                "per process (more for eval), and each needs a container of its own",
            ) from e
        self._failure: MiroirError | None = None
        self._started: Future[None] | None = None
        self.done = False
        self.reward: float | None = None
        self.success = False
        self.metadata: dict[str, Any] = {}
        self.record: dict[str, Any] | None = None

    def reset(self, **row: Any) -> str:
        """Start the row's episode in the background; returns the prompt."""
        self.done, self.reward, self.success, self.metadata = False, None, False, {}
        self.record = None
        today = datetime.datetime.now(datetime.UTC).date()
        self._started = self._calls.submit(self._start, row["task_id"], today)
        return miroir.prompt(row, today)

    def api_request(
        self,
        method: Literal["GET", "POST", "PUT", "PATCH", "DELETE"],
        path: str,
        body: Any = None,
        headers: dict[str, str] | None = None,
    ) -> str:
        """Send an HTTP request to the product's API as the current user. Returns the status code, the content-type, location and link headers, and the body.

        Args:
            method: The HTTP method.
            path: Path and query string relative to the API base URL, such as /deals?owner_id=12
            body: Request body, sent as JSON unless headers set a content-type: then a string is sent as-is, and for application/x-www-form-urlencoded an object is form-encoded
            headers: Extra request headers.
        """
        return self._call(
            "api_request", {"method": method, "path": path, "body": body, "headers": headers}
        )

    def api_docs(self, operation: str | None = None) -> str:
        """Look up the product's API. With no arguments, lists every operation. With an operation ID or 'METHOD /path', returns that operation's parameters and schemas.

        Args:
            operation: An operation ID or 'METHOD /path'; leave it out to list every operation.
        """
        return self._call("api_docs", {"operation": operation})

    def run_code(self, command: str) -> str:
        """Run a bash command in a sandbox that has python3 (with requests), curl and jq, and return its exit code and output (stdout and stderr). Each app's API is at $API_<APP>, the app's name in capitals with '-' as '_' (env lists them); requests there act as the current user and take the paths api_docs lists. Files you write last until the episode ends; processes stop when the call returns. A call is cut off after 60 s, and its output after 64 KiB.

        Args:
            command: The bash command to run.
        """
        return self._call("run_code", {"command": command})

    def wait(self, minutes: int) -> str:
        """Let time pass in the apps, up to the given minutes: everything already in them moves that much into the past, as if that time had gone by, while their clock keeps reading the real time. The wait ends early when something happens, such as a new message or issue. Returns how many minutes it lasted, and how many have passed since the task began.

        Args:
            minutes: How many minutes to wait: a whole number from 1 to 10080 (a week).
        """
        return self._call("wait", {"minutes": minutes})

    def submit(self, answer: str | None = None) -> str:
        """Finish the task. Put any answer the task asks for in answer. This ends the episode.

        Args:
            answer: The answer the task asks for, if any.
        """
        if answer is not None and not isinstance(answer, str):
            raise ValueError(f"answer must be a string, not {type(answer).__name__}")
        if self.done:
            raise RuntimeError(EPISODE_OVER)
        self.done = True
        self._calls.submit(self._step, "submit", {"answer": answer})
        return "Submitted; the episode is over."

    def _call(self, tool: str, arguments: dict[str, Any]) -> str:
        if self.done:
            raise RuntimeError(EPISODE_OVER)
        step = self._calls.submit(self._step, tool, arguments).result()
        observation = step["observation"]
        error = observation["error"]
        # As the env's MCP server shows it: Python's spaces and \u escapes would cost tokens.
        shown = json.dumps(
            observation["result"] if error is None else error,
            separators=(",", ":"),
            ensure_ascii=False,
        )
        if step["done"]:
            self.done = True
            return f"{shown}\n\nThat was the last tool call: {EPISODE_OVER}."
        if self._coworkers:
            self._calls.submit(self._answer)
        return shown

    def _start(self, task_id: str, today: datetime.date) -> None:
        self._failure = None
        try:
            if self._session is None:
                self._session = Session.lease(self._urls, self._token, wait_busy=False)
            coworkers = self._coworker is not None
            observation = self._session.reset(task_id, self._max_seconds, coworkers)
        except MiroirError as e:
            self._fail(e)
            return
        self._coworkers = bool(observation["metadata"].get("coworkers"))
        reset_on = observation["now"][:10]
        if reset_on != today.isoformat():
            self._fail(MiroirError("CLOCK_ROLLED", f"the prompt says {today}, the env {reset_on}"))

    def _step(self, tool: str, arguments: dict[str, Any]) -> dict[str, Any]:
        if self._failure is not None:
            raise self._failure
        assert self._session is not None
        try:
            return self._session.step(tool, {k: v for k, v in arguments.items() if v is not None})
        except MiroirError as e:
            self._fail(e)
            raise

    def _answer(self) -> None:
        if self._failure is not None or self._session is None or self._coworker is None:
            return
        try:
            miroir.answer_coworkers(self._session, self._coworker)
        except MiroirError as e:
            self._fail(e)

    def _finish(self) -> float | None:
        if self._started is not None and self._failure is None:
            assert self._session is not None
            try:
                outcome = self._session.finish()
            except MiroirError as e:
                self._fail(e)
            else:
                self.done = True
                reward = outcome["reward"]
                if self._harm_reward is not None and outcome["metadata"].get("harm"):
                    reward = min(reward, self._harm_reward)
                self.reward, self.success = reward, outcome["success"]
                self.metadata, self.record = outcome["metadata"], outcome["record"]
                return self.reward
        self.reward, self.success = None, False
        self.metadata = {"invalid": str(self._failure or "the env was never reset")}
        return None

    def _fail(self, error: MiroirError) -> None:
        self._failure = error
        if error.code == "CONNECTION_LOST":
            self._close()

    def _close(self) -> None:
        if self._session is not None:
            self._session.close()
            self._session = None
