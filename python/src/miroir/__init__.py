"""Client of Miroir envs: lease a container, run episodes, and what the adapters share."""

from __future__ import annotations

import dataclasses
import datetime
import functools
import json
import os
import random
import socket
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid
import warnings
from collections.abc import Mapping, Sequence
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from typing import Any

from websockets.exceptions import ConnectionClosed, InvalidHandshake
from websockets.sync.client import ClientConnection, connect

__all__ = [
    "CoworkerModel",
    "MiroirError",
    "Session",
    "answer_coworkers",
    "answerable",
    "answering",
    "metrics",
    "prompt",
    "read_urls",
    "save_record",
    "tasks",
    "tools",
]

PING_INTERVAL_S = 20.0
PING_TIMEOUT_S = 60.0
HTTP_TIMEOUT_S = 30.0
HEALTH_TIMEOUT_S = 2.0
HEALTH_CHECKS = 32
HEALTH_WORKERS = 512
RETRY_EVERY_S = 1.0
COWORKER_TIMEOUT_S = 30.0
TRACES_STARTED = datetime.datetime.now(datetime.UTC).strftime("%Y%m%dT%H%M%SZ")
_traces_lock = threading.Lock()


class MiroirError(Exception):
    """An error from the env or from reaching it, with its code."""

    def __init__(self, code: str, message: str) -> None:
        super().__init__(f"{code}: {message}")
        self.code = code
        self.message = message


class Session:
    """A WebSocket session on one env container, which it holds until closed."""

    def __init__(self, url: str, socket: ClientConnection) -> None:
        self.url = url
        self._socket = socket
        self._broken: MiroirError | None = None
        self._episode_id: str | None = None
        self._last: dict[str, Any] | None = None
        self._lock = threading.Lock()

    @classmethod
    def lease(
        cls,
        urls: Sequence[str] | os.PathLike[str],
        token: str | None = None,
        *,
        wait_busy: bool,
        timeout_s: float = 600.0,
    ) -> Session:
        """Open a session on a free, healthy container among `urls`, a list or a URL file.

        Raises `NO_FREE_CONTAINER` at `timeout_s`, or at once if all are leased and not
        `wait_busy`.
        """
        if isinstance(urls, str) or not urls:
            raise ValueError("urls must be a non-empty list of base URLs, or a file of them")
        deadline = time.monotonic() + timeout_s
        while True:
            listed = read_urls(urls) if isinstance(urls, os.PathLike) else list(urls)
            start = random.randrange(len(listed))
            order = [*listed[start:], *listed[:start]]
            busy, down = 0, []
            for at in range(0, len(order), HEALTH_CHECKS):
                batch = order[at : at + HEALTH_CHECKS]
                for url, health in zip(batch, _check_pool().map(_health, batch), strict=True):
                    if health is None or health.get("status") != "healthy":
                        down.append(url)
                        continue
                    if health.get("leased"):
                        busy += 1
                        continue
                    socket = _open_session(url, token)
                    if socket is not None:
                        return cls(url, socket)
                    busy += 1
            if (busy and not down and not wait_busy) or time.monotonic() >= deadline:
                shown = f" ({', '.join(down[:3])})" if down else ""
                raise MiroirError(
                    "NO_FREE_CONTAINER",
                    f"{busy} of {len(listed)} containers leased, {len(down)} not healthy"
                    f"{shown}; start more containers, or run fewer rollouts at once",
                )
            time.sleep(RETRY_EVERY_S)

    @property
    def episode_id(self) -> str | None:
        """The running episode's id, or None."""
        return self._episode_id

    def reset(
        self, task_id: str, max_seconds: int | None = None, coworkers: bool = False
    ) -> dict[str, Any]:
        """Start an episode on a task; returns the reset observation."""
        self._episode_id, self._last = None, None
        episode_id = uuid.uuid4().hex
        data = {
            "task_id": task_id,
            "episode_id": episode_id,
            "max_seconds": max_seconds,
            "coworkers": coworkers,
        }
        reply = self._exchange({"type": "reset", "data": data})
        self._episode_id = episode_id
        return reply["observation"]

    def step(self, tool: str, arguments: Mapping[str, Any]) -> dict[str, Any]:
        """Call a tool; returns the step's observation, reward and done."""
        if self._episode_id is None:
            raise MiroirError("NO_EPISODE", "no episode is running; reset first")
        action = {
            "type": "call_tool",
            "tool_name": tool,
            "arguments": dict(arguments),
            "metadata": {"episode_id": self._episode_id},
        }
        self._last = self._exchange({"type": "step", "data": action})
        return self._last

    def coworker_turns(self) -> list[dict[str, Any]]:
        """The coworkers who owe a reply, each with the chat to answer."""
        return self._exchange({"type": "coworkers"})["turns"]

    def coworker_reply(self, coworker: str, text: str) -> int | None:
        """Post a coworker's reply; returns the app's HTTP status."""
        data = {"coworker": coworker, "text": text}
        return self._exchange({"type": "coworker_reply", "data": data})["status"]

    def state(self) -> dict[str, Any]:
        """The env's state frame."""
        return self._exchange({"type": "state"})

    def finish(self) -> dict[str, Any]:
        """Submit if the episode still runs; returns its reward, success, metadata and record."""
        if self._episode_id is None:
            raise MiroirError("NO_EPISODE", "no episode is running; reset first")
        if (self._last is None or not self._last["done"]) and not self.state()["done"]:
            try:
                last = self.step("submit", {})
            except MiroirError:
                if not self.state()["done"]:
                    raise
            else:
                if not last["done"]:
                    error = last["observation"].get("error") or {}
                    message = error.get("message", error)
                    raise MiroirError("NOT_GRADED", f"submit failed: {message}")
        record = self._exchange({"type": "record", "data": {"episode_id": self._episode_id}})
        return {
            "reward": record["reward"],
            "success": bool(record["metadata"].get("success")),
            "metadata": record["metadata"],
            "record": record,
        }

    def close(self) -> None:
        """Close the session, which frees the container."""
        self._socket.close()

    def _exchange(self, frame: dict[str, Any]) -> dict[str, Any]:
        with self._lock:
            if self._broken is not None:
                raise self._broken
            try:
                self._socket.send(json.dumps(frame))
                reply = json.loads(self._socket.recv())
            except (ConnectionClosed, TimeoutError, OSError) as e:
                self._broken = MiroirError("CONNECTION_LOST", f"{self.url}: {e!r}")
                self._socket.close()
                raise self._broken from e
        if reply["type"] == "error":
            raise MiroirError(reply["data"]["code"], reply["data"]["message"])
        return reply["data"]


@dataclasses.dataclass(frozen=True)
class CoworkerModel:
    """An OpenAI-compatible chat model that plays the coworkers."""

    base_url: str
    model: str
    api_key: str | None = dataclasses.field(default=None, repr=False)
    temperature: float | None = None


def answerable(tasks: list[dict[str, Any]], coworkers: bool) -> list[dict[str, Any]]:
    """Leave out the tasks with coworkers unless a coworker model will answer them."""
    if coworkers:
        return tasks
    kept = [task for task in tasks if not task.get("coworkers")]
    if len(kept) < len(tasks):
        warnings.warn(
            f"left out the tasks with coworkers ({len(tasks) - len(kept)}), which need a coworker "
            "model to answer for them",
            stacklevel=2,
        )
    return kept


def answer_coworkers(session: Session, model: CoworkerModel) -> int:
    """Answer every owed coworker turn with the model; returns how many."""
    turns = session.coworker_turns()
    for turn in turns:
        session.coworker_reply(turn["coworker"], _complete(model, turn["messages"]))
    return len(turns)


def _complete(model: CoworkerModel, messages: list[dict[str, Any]]) -> str:
    body: dict[str, Any] = {"model": model.model, "messages": messages}
    if model.temperature is not None:
        body["temperature"] = model.temperature
    headers = {"content-type": "application/json"}
    if model.api_key:
        headers["authorization"] = f"Bearer {model.api_key}"
    url = f"{model.base_url.rstrip('/')}/chat/completions"
    request = urllib.request.Request(url, data=json.dumps(body).encode(), headers=headers)
    try:
        with urllib.request.urlopen(request, timeout=COWORKER_TIMEOUT_S) as response:
            reply = json.load(response)
    except (OSError, ValueError) as e:
        raise MiroirError("COWORKER_FAILED", f"{url} with {model.model}: {e}") from e
    try:
        text = reply["choices"][0]["message"]["content"]
    except (KeyError, IndexError, TypeError) as e:
        raise MiroirError("COWORKER_FAILED", f"{model.model} sent no answer: {reply!r:.300}") from e
    if not isinstance(text, str) or not text.strip():
        raise MiroirError("COWORKER_FAILED", f"{model.model} answered with no text")
    return text


def read_urls(path: str | os.PathLike[str]) -> list[str]:
    """Read a URL file, as `miroir pool` writes it: a base URL per line, `#` comments."""
    try:
        text = Path(path).read_text(encoding="utf-8")
    except OSError as e:
        raise MiroirError("URL_FILE", f"cannot read {path}: {e}; start `miroir pool`") from e
    urls = [line.split("#", 1)[0].strip() for line in text.splitlines()]
    urls = [url for url in urls if url]
    for url in urls:
        if not is_base_url(url):
            raise MiroirError("URL_FILE", f"{path}: {url!r} is not an http:// or https:// URL")
    if not urls:
        raise MiroirError("URL_FILE", f"{path} lists no URL; start `miroir pool`")
    return urls


def is_base_url(url: str) -> bool:
    """Whether `url` is an http(s) URL with a host."""
    parts = urllib.parse.urlsplit(url)
    return parts.scheme in ("http", "https") and bool(parts.hostname)


def answering(urls: Sequence[str], token: str | None = None) -> str:
    """The first of `urls` whose env answers."""
    failure: MiroirError | None = None
    for url in urls:
        try:
            _request(url, "/list_environments", None, token)
        except MiroirError as e:
            failure = e
            continue
        return url
    if failure is None:
        raise ValueError("urls must be a non-empty list of base URLs")
    raise failure


def tasks(url: str, split: str = "train", token: str | None = None) -> list[dict[str, Any]]:
    """A split's tasks, as the env lists them."""
    name = _request(url, "/list_environments", None, token)[0]
    return _request(url, f"/{name}/tasks", {"split": split}, token)["tasks"]


def tools(url: str, token: str | None = None, surface: str | None = None) -> list[dict[str, Any]]:
    """The tools the env's MCP endpoint lists on a surface: api (the default), mcp or code."""
    rpc = {"jsonrpc": "2.0", "id": 1, "method": "tools/list"}
    query = f"?surface={urllib.parse.quote(surface)}" if surface else ""
    reply = _request(url, f"/mcp{query}", rpc, token)
    if "error" in reply:
        raise MiroirError("TOOLS", f"{url}: {reply['error']['message']}")
    return reply["result"]["tools"]


def prompt(task: Mapping[str, Any], today: datetime.date) -> str:
    """The user prompt for a task: instruction, persona, step budget and today's date."""
    persona = task["persona"]["description"]
    acting = f"You act as {persona}. " if persona else ""
    return (
        f"{task['instruction']}\n\n"
        f"{acting}You have {task['max_steps']} tool calls; call submit when you are done.\n\n"
        f"Today is {today.isoformat()} (UTC)."
    )


def metrics(record: Mapping[str, Any]) -> dict[str, float]:
    """A record's metrics to log: success, harm, diagnostics and episode time."""
    metadata = record["metadata"]
    return {
        "success": float(bool(metadata.get("success"))),
        "harm": float(bool(metadata.get("harm"))),
        **record["diagnostics"],
        "episode_s": record["timings"]["episode_s"],
    }


def save_record(directory: str | os.PathLike[str], record: Mapping[str, Any]) -> None:
    """Append a record to this process's JSONL file in `directory`."""
    folder = Path(directory)
    path = folder / f"{TRACES_STARTED}-{socket.gethostname()}-{os.getpid()}.jsonl"
    line = json.dumps(record) + "\n"
    with _traces_lock:
        folder.mkdir(parents=True, exist_ok=True)
        with path.open("a", encoding="utf-8") as traces:
            traces.write(line)


def _headers(token: str | None) -> dict[str, str]:
    return {"Authorization": f"Bearer {token}"} if token else {}


@functools.cache
def _check_pool() -> ThreadPoolExecutor:
    return ThreadPoolExecutor(HEALTH_WORKERS, thread_name_prefix="miroir-health")


os.register_at_fork(after_in_child=_check_pool.cache_clear)


def _health(url: str) -> dict[str, Any] | None:
    try:
        try:
            with urllib.request.urlopen(f"{url}/health", timeout=HEALTH_TIMEOUT_S) as response:
                body = response.read()
        except urllib.error.HTTPError as e:
            if e.code != 503:
                return None
            body = e.read()
        health = json.loads(body)
    except (OSError, ValueError):
        return None
    return health if isinstance(health, dict) else None


def _open_session(url: str, token: str | None) -> ClientConnection | None:
    if not url.startswith(("http://", "https://")):
        raise ValueError(f"{url!r} must start with http:// or https://")
    ws_url = "ws" + url.removeprefix("http") + "/ws"
    try:
        socket = connect(
            ws_url,
            additional_headers=_headers(token),
            open_timeout=HTTP_TIMEOUT_S,
            ping_interval=PING_INTERVAL_S,
            ping_timeout=PING_TIMEOUT_S,
            max_size=None,
        ).__enter__()
    except (InvalidHandshake, OSError, TimeoutError) as e:
        raise MiroirError("CONNECTION_FAILED", f"{ws_url}: {e!r}") from e
    try:
        socket.send(json.dumps({"type": "state"}))
        reply = json.loads(socket.recv(timeout=HTTP_TIMEOUT_S))
    except (ConnectionClosed, TimeoutError, OSError):
        socket.close()
        return None
    if reply["type"] == "error":
        socket.close()
        if reply["data"]["code"] == "CAPACITY_REACHED":
            return None
        raise MiroirError(reply["data"]["code"], reply["data"]["message"])
    return socket


def _request(url: str, path: str, body: Any, token: str | None) -> Any:
    data = None if body is None else json.dumps(body).encode()
    headers = {**_headers(token), **({"Content-Type": "application/json"} if data else {})}
    request = urllib.request.Request(f"{url}{path}", data=data, headers=headers)
    try:
        with urllib.request.urlopen(request, timeout=HTTP_TIMEOUT_S) as response:
            return json.loads(response.read())
    except urllib.error.HTTPError as e:
        raise MiroirError(
            f"HTTP_{e.code}", f"{url}{path}: {e.read().decode(errors='replace')}"
        ) from e
    except OSError as e:
        raise MiroirError("CONNECTION_FAILED", f"{url}{path}: {e!r}") from e
