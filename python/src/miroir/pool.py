"""`miroir pool`: keep a number of env containers healthy, listed in a URL file for leases."""

from __future__ import annotations

import fcntl
import hashlib
import os
import shlex
import signal
import subprocess
import sys
import threading
import time
from collections.abc import Callable, Sequence
from concurrent.futures import Future, ThreadPoolExecutor
from dataclasses import dataclass
from pathlib import Path
from typing import IO, Any

from miroir import _check_pool, _health, is_base_url

__all__ = ["Pool", "PoolError", "run_pool"]

POLL_S = 10.0
START_LIMIT_S = 600.0
STOP_LIMIT_S = 120.0
KILL_GRACE_S = 10.0
READY_LIMIT_S = 900.0
DEAD_LIMIT_S = 60.0
SICK_LIMIT_S = 600.0
STARTS_AT_ONCE = 64
FAILED_STARTS = 3

Health = Callable[[str], dict[str, Any] | None]


class PoolError(Exception):
    """A pool that can't keep going."""


@dataclass
class Container:
    """A container the pool started, and what its health checks found."""

    id: str
    url: str
    started: float
    ready: bool = False
    healthy: bool = False
    leased: bool = False
    silent_since: float | None = None
    sick_since: float | None = None


class Pool:
    """Starts, checks and replaces containers through `COMMAND start` and `COMMAND stop ID`."""

    def __init__(
        self,
        command: Sequence[str],
        size: int,
        path: Path,
        *,
        health: Health = _health,
        clock: Callable[[], float] = time.monotonic,
    ) -> None:
        self.command, self.size, self.path = list(command), size, path
        self.health, self.clock = health, clock
        self.containers: dict[str, Container] = {}
        self.starting: list[Future[tuple[str, str, float]]] = []
        self.commands = ThreadPoolExecutor(STARTS_AT_ONCE, thread_name_prefix="miroir-pool")
        self.environ = {**os.environ, "MIROIR_POOL": pool_name(path)}
        self.began = clock()
        self.failed_starts, self.ever_started, self.ever_ready = 0, False, False
        self.listed: list[str] | None = None
        self.shown: tuple[int, int] | None = None

    def recover(self) -> None:
        """Stop the containers an earlier pool on this file left."""
        try:
            lines = self.path.read_text(encoding="utf-8").splitlines()
        except FileNotFoundError:
            return
        left = [line.split("#", 1)[1].strip() for line in lines if "#" in line]
        left = [found for found in left if found]
        if left:
            _log(f"stopping {len(left)} containers an earlier pool on {self.path} left")
            list(self.commands.map(self._stop, left))

    def tick(self) -> None:
        """Collect started containers, replace dead ones, start missing ones, publish the file."""
        now = self.clock()
        self._take_started()
        self._check(now)
        missing = self.size - len(self.containers) - len(self.starting)
        self.starting += [self.commands.submit(self._start) for _ in range(max(missing, 0))]
        self._publish()

    def close(self) -> None:
        """Stop every container, those still starting too, and remove the file."""
        for future in self.starting:
            future.cancel()
        running = [future for future in self.starting if not future.cancelled()]
        if running:
            _log(f"waiting for {len(running)} starts under way, to stop what they start")
        for future in running:
            try:
                found, url, started = future.result()
            except PoolError:
                continue
            self.containers[found] = Container(found, url, started)
        list(self.commands.map(self._stop, list(self.containers)))
        self.containers.clear()
        self.commands.shutdown(wait=True)
        self.path.unlink(missing_ok=True)

    def _take_started(self) -> None:
        for future in [future for future in self.starting if future.done()]:
            self.starting.remove(future)
            try:
                found, url, started = future.result()
            except PoolError as e:
                self.failed_starts += 1
                _log(f"a start failed: {e}")
                if self.failed_starts >= FAILED_STARTS and not self.ever_started:
                    raise PoolError(f"{shlex.join(self.command)} start never worked: {e}") from e
                continue
            self.failed_starts, self.ever_started = 0, True
            self.containers[found] = Container(found, url, started)
            _log(f"started {found} at {url}")

    def _check(self, now: float) -> None:
        listed = list(self.containers.values())
        healths = _check_pool().map(self.health, [container.url for container in listed])
        for container, health in zip(listed, healths, strict=True):
            why = self._judge(container, health, now)
            if why is not None:
                _log(f"replacing {container.id}: {why}")
                del self.containers[container.id]
                self.commands.submit(self._stop, container.id)
        if not self.ever_ready and now - self.began > READY_LIMIT_S:
            raise PoolError(
                f"no container became healthy in {READY_LIMIT_S:.0f} s; see the output of "
                f"{shlex.join(self.command)} start and the containers' logs"
            )

    def _judge(self, container: Container, health: dict[str, Any] | None, now: float) -> str | None:
        if health is None:
            container.healthy = container.leased = False
            container.silent_since = _since(container.silent_since, now)
        else:
            container.healthy = health.get("status") == "healthy"
            container.leased = bool(health.get("leased"))
            container.silent_since = None
            container.sick_since = None if container.healthy else _since(container.sick_since, now)
        if container.healthy and not container.ready:
            container.ready = self.ever_ready = True
            _log(f"{container.id} is healthy, {now - container.started:.0f} s after its start")
        if not container.ready:
            late = now - container.started > READY_LIMIT_S
            return f"not healthy {READY_LIMIT_S:.0f} s after its start" if late else None
        if container.silent_since is not None and now - container.silent_since > DEAD_LIMIT_S:
            return f"no answer for {DEAD_LIMIT_S:.0f} s"
        if container.sick_since is not None and now - container.sick_since > SICK_LIMIT_S:
            return f"unhealthy for {SICK_LIMIT_S:.0f} s"
        return None

    def _publish(self) -> None:
        listed = sorted(
            f"{container.url}  # {container.id}" for container in self.containers.values()
        )
        if listed != self.listed:
            temporary = self.path.with_name(f".{self.path.name}.tmp")
            temporary.write_text("".join(f"{line}\n" for line in listed), encoding="utf-8")
            os.replace(temporary, self.path)
            self.listed = listed
        healthy = sum(container.healthy for container in self.containers.values())
        leased = sum(container.leased for container in self.containers.values())
        if (healthy, leased) != self.shown:
            _log(f"ready {healthy} of {self.size}, {leased} leased")
            if healthy == self.size and (self.shown is None or self.shown[0] < self.size):
                _log(f"ready: {healthy} of {self.size}; start the trainer")
            self.shown = (healthy, leased)

    def _start(self) -> tuple[str, str, float]:
        out = _run([*self.command, "start"], START_LIMIT_S, self.environ)
        lines = [line for line in out.splitlines() if line.strip()]
        last = lines[-1].split() if lines else []
        if len(last) == 2 and is_base_url(last[1]):
            return last[0], last[1].rstrip("/"), self.clock()
        if len(last) == 2:
            self._stop(last[0])
        shown = lines[-1] if lines else ""
        raise PoolError(f"start printed {shown!r} last, not 'ID URL'")

    def _stop(self, found: str) -> None:
        try:
            _run([*self.command, "stop", found], STOP_LIMIT_S, self.environ)
        except PoolError as e:
            _log(f"cannot stop {found}: {e}; stop it by hand")


def pool_name(path: Path) -> str:
    """The pool's name from its file's path, which the command gets as `MIROIR_POOL`."""
    return "miroir-" + hashlib.sha256(str(path.resolve()).encode()).hexdigest()[:8]


def run_pool(command: Sequence[str], size: int, path: Path) -> None:
    """Run a pool until SIGINT or SIGTERM, with a lock on its file."""
    if size < 1:
        raise PoolError(f"--size must be at least 1, not {size}")
    path = path.resolve()
    path.parent.mkdir(parents=True, exist_ok=True)
    with _lock(path):
        pool = Pool(command, size, path)
        stopping = threading.Event()
        for signum in (signal.SIGINT, signal.SIGTERM):
            signal.signal(signum, lambda *_: stopping.set())
        try:
            pool.recover()
            pool.tick()
            while not stopping.wait(1.0 if pool.starting else POLL_S):
                pool.tick()
        finally:
            _log("stopping every container")
            pool.close()


def _lock(path: Path) -> IO[str]:
    lock = path.with_name(f"{path.name}.lock").open("w", encoding="utf-8")
    try:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        lock.close()
        raise PoolError(f"another miroir pool keeps {path}; stop it, or use another file") from None
    return lock


def _run(argv: list[str], limit_s: float, environ: dict[str, str]) -> str:
    process = subprocess.Popen(argv, stdout=subprocess.PIPE, text=True, env=environ)
    try:
        out, _ = process.communicate(timeout=limit_s)
    except subprocess.TimeoutExpired:
        process.terminate()
        try:
            process.communicate(timeout=KILL_GRACE_S)
        except subprocess.TimeoutExpired:
            process.kill()
            process.communicate()
        raise PoolError(f"{shlex.join(argv)} took over {limit_s:.0f} s") from None
    if process.returncode != 0:
        raise PoolError(f"{shlex.join(argv)} exited with code {process.returncode}")
    return out


def _since(since: float | None, now: float) -> float:
    return now if since is None else since


def _log(message: str) -> None:
    print(f"{time.strftime('%H:%M:%S')} {message}", file=sys.stderr, flush=True)
