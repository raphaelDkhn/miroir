import concurrent.futures
import re
import sys
import threading
import time
from collections.abc import Callable
from pathlib import Path
from typing import Any

import pytest
from conftest import ROOT, docker, golden

import miroir
from miroir import Session, pool
from miroir.pool import Pool, PoolError

HEALTHY = {"status": "healthy", "leased": False}
UNHEALTHY = {"status": "unhealthy", "leased": False}

FAKE = """
import signal, sys, time, uuid
from pathlib import Path

state = Path(sys.argv[1])
if sys.argv[2] == "stop":
    with open(state / "stopped", "a") as stopped:
        stopped.write(sys.argv[3] + "\\n")
    sys.exit(0)
if (state / "bad").exists():
    print("bad-1 http://")
    sys.exit(0)
if (state / "slow").exists():
    time.sleep(float((state / "slow").read_text()))
if (state / "hang").exists():
    signal.signal(signal.SIGTERM, lambda *_: ((state / "terminated").touch(), sys.exit(1)))
    time.sleep(60)
failing = state / "fail"
if failing.exists() and int(failing.read_text()) > 0:
    failing.write_text(str(int(failing.read_text()) - 1))
    sys.exit(1)
name = uuid.uuid4().hex[:8]
print("starting", name)
print(f"{name} http://{name}:1")
"""


class Clock:
    def __init__(self) -> None:
        self.now = 1000.0

    def __call__(self) -> float:
        return self.now


@pytest.fixture
def command(tmp_path: Path) -> list[str]:
    script = tmp_path / "fake.py"
    script.write_text(FAKE)
    (tmp_path / "state").mkdir()
    return [sys.executable, str(script), str(tmp_path / "state")]


def settle(running: Pool) -> None:
    concurrent.futures.wait(running.starting, timeout=30)
    running.tick()


def stopped(state: Path, wanted: set[str]) -> set[str]:
    deadline = time.monotonic() + 10
    while True:
        seen = (
            set((state / "stopped").read_text().split()) if (state / "stopped").exists() else set()
        )
        if wanted <= seen or time.monotonic() > deadline:
            return seen
        time.sleep(0.05)


def make(
    command: list[str], size: int, path: Path, health: Callable[[str], Any], clock: Clock
) -> Pool:
    made = Pool(command, size, path, health=health, clock=clock)
    made.tick()
    settle(made)
    return made


def test_the_url_file_lists_every_container_and_its_id(command, tmp_path, capsys):
    path = tmp_path / "urls.txt"
    held = {"status": "healthy", "leased": True}
    made = make(command, 3, path, lambda url: held, Clock())
    lines = path.read_text().splitlines()
    assert len(lines) == 3
    assert all(re.fullmatch(r"http://(\w+):1  # \1", line) for line in lines), lines
    assert miroir.read_urls(path) == [line.split()[0] for line in lines]
    assert "ready 3 of 3, 3 leased" in capsys.readouterr().err
    made.close()
    assert not path.exists()


def test_a_container_never_healthy_is_replaced(command, tmp_path):
    lock, good = threading.Lock(), set[str]()

    def health(url: str) -> dict[str, Any] | None:
        with lock:
            if not good:
                good.add(url)
            return HEALTHY if url in good else None

    clock = Clock()
    made = make(command, 2, tmp_path / "urls.txt", health, clock)
    [never] = [found for found, container in made.containers.items() if not container.ready]
    clock.now += pool.READY_LIMIT_S - 1
    made.tick()
    assert never in made.containers, "still in time"
    clock.now += 2
    made.tick()
    assert never not in made.containers
    assert never in stopped(tmp_path / "state", {never})
    made.close()


def test_a_container_that_dies_or_stays_unhealthy_is_replaced(command, tmp_path):
    table: dict[str, dict[str, Any] | None] = {}
    clock = Clock()
    made = make(command, 3, tmp_path / "urls.txt", lambda url: table.get(url, HEALTHY), clock)
    dead, sick, held = made.containers.values()
    table[dead.url], table[sick.url] = None, UNHEALTHY
    table[held.url] = {"status": "healthy", "leased": True}
    made.tick()
    clock.now += pool.DEAD_LIMIT_S + 1
    made.tick()
    seen = stopped(tmp_path / "state", {dead.id})
    assert dead.id in seen and sick.id not in seen, "a 503 may mend; no answer at all is death"
    clock.now += pool.SICK_LIMIT_S
    made.tick()
    seen = stopped(tmp_path / "state", {sick.id})
    assert sick.id in seen and held.id not in seen, "a leased, healthy container is kept"
    made.close()


def test_a_failed_start_is_tried_again(command, tmp_path, capsys):
    (tmp_path / "state" / "fail").write_text("1")
    made = make(command, 1, tmp_path / "urls.txt", lambda url: HEALTHY, Clock())
    assert made.containers == {}
    settle(made)
    assert len(made.containers) == 1
    assert "a start failed" in capsys.readouterr().err
    made.close()


def test_a_start_past_its_limit_gets_sigterm_first(command, tmp_path, monkeypatch):
    monkeypatch.setattr(pool, "START_LIMIT_S", 0.5)
    (tmp_path / "state" / "hang").touch()
    made = make(command, 1, tmp_path / "urls.txt", lambda url: HEALTHY, Clock())
    assert (tmp_path / "state" / "terminated").exists(), "the command could clean up"
    assert made.containers == {} and made.failed_starts == 1
    (tmp_path / "state" / "hang").unlink()
    made.close()


def test_a_start_that_prints_no_url_is_dropped_with_its_container(command, tmp_path):
    (tmp_path / "state" / "bad").touch()
    made = make(command, 1, tmp_path / "urls.txt", lambda url: HEALTHY, Clock())
    assert made.containers == {} and made.failed_starts == 1
    assert "bad-1" in stopped(tmp_path / "state", {"bad-1"}), "the container it named is dropped"
    assert (tmp_path / "urls.txt").read_text() == "", "a bad URL would stop every lease"
    (tmp_path / "state" / "bad").unlink()
    made.close()


def test_a_command_that_never_starts_one_stops_the_pool(command, tmp_path):
    (tmp_path / "state" / "fail").write_text("99")
    made = make(command, 1, tmp_path / "urls.txt", lambda url: HEALTHY, Clock())
    settle(made)
    with pytest.raises(PoolError, match="start never worked"):
        settle(made)
    made.close()


def test_a_pool_whose_containers_never_become_healthy_stops(command, tmp_path):
    clock = Clock()
    made = make(command, 1, tmp_path / "urls.txt", lambda url: None, clock)
    clock.now += pool.READY_LIMIT_S + 1
    with pytest.raises(PoolError, match="no container became healthy"):
        made.tick()
    made.close()


def test_leftovers_an_earlier_pool_listed_are_stopped_first(command, tmp_path):
    path = tmp_path / "urls.txt"
    path.write_text("http://a:1  # old-a\nhttp://b:1  # old-b\nhttp://hand-written:1\n")
    Pool(command, 1, path, health=lambda url: HEALTHY, clock=Clock()).recover()
    assert stopped(tmp_path / "state", {"old-a", "old-b"}) == {"old-a", "old-b"}


def test_a_second_pool_on_one_file_is_refused(tmp_path):
    path = tmp_path / "urls.txt"
    with pool._lock(path), pytest.raises(PoolError, match="another miroir pool keeps"):
        pool._lock(path)


def test_closing_waits_for_starts_under_way_then_stops_everything(command, tmp_path):
    (tmp_path / "state" / "slow").write_text("1")
    path = tmp_path / "urls.txt"
    made = Pool(command, 2, path, health=lambda url: HEALTHY, clock=Clock())
    made.tick()
    made.close()
    assert len(stopped(tmp_path / "state", set())) == 2, "both starts finished, then stopped"
    assert not path.exists()


@pytest.mark.integration
def test_a_docker_pool_serves_episodes_and_replaces_a_container_that_dies(tmp_path, monkeypatch):
    monkeypatch.setattr(pool, "DEAD_LIMIT_S", 1.0)
    path = tmp_path / "urls.txt"
    made = Pool([str(ROOT / "python" / "tests" / "docker.sh"), "acme-crm-env"], 2, path)

    def tick_until(done: Callable[[], bool]) -> None:
        deadline = time.monotonic() + 180
        while not done():
            assert time.monotonic() < deadline, path.read_text()
            made.tick()
            time.sleep(1)

    try:
        tick_until(lambda: sum(c.healthy for c in made.containers.values()) == 2)
        session = Session.lease(path, wait_busy=False)
        try:
            session.reset("reassign-stale-deals")
            for tool, arguments in golden("reassign-stale-deals"):
                session.step(tool, arguments)
            assert session.finish()["reward"] == 1.0
        finally:
            session.close()
        killed = next(iter(made.containers))
        docker("kill", killed)
        tick_until(
            lambda: (
                killed not in made.containers
                and sum(c.healthy for c in made.containers.values()) == 2
            )
        )
        assert killed not in path.read_text()
    finally:
        made.close()
    assert not path.exists()
    assert docker("ps", "-q", "--filter", f"label=miroir.pool={pool.pool_name(path)}") == ""
