import contextlib
import http.server
import json
import os
import subprocess
import threading
import time
import urllib.request
from collections.abc import Generator, Iterator
from pathlib import Path
from typing import Any

import pytest
import yaml

ROOT = Path(__file__).parents[2]
IMAGE = "acme-crm-env"
READY_LIMIT_S = 180
ASK_PRIYA = ROOT / "fixtures" / "acme-crm" / "coworker-tasks" / "ask-priya.yaml"


class FakeModel:
    def __init__(
        self,
        content: object = "Qualified: they asked to re-scope.",
        status: int = 200,
        delay: float = 0.0,
    ) -> None:
        self.requests: list[tuple[str, dict[str, str], dict[str, Any]]] = []
        requests = self.requests

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_POST(self) -> None:
                length = int(self.headers["Content-Length"])
                body = json.loads(self.rfile.read(length))
                requests.append((self.path, dict(self.headers), body))
                time.sleep(delay)
                message = {"role": "assistant", "content": content}
                sent = json.dumps({"choices": [{"index": 0, "message": message}]}).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(sent)))
                self.end_headers()
                self.wfile.write(sent)

            def log_message(self, format: str, *args: object) -> None:
                pass

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}/v1"
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def close(self) -> None:
        self.server.shutdown()
        self.server.server_close()


def golden(task_id: str) -> list[tuple[str, dict[str, Any]]]:
    task = yaml.safe_load(
        (ROOT / "fixtures" / "acme-crm" / "tasks" / f"{task_id}.yaml").read_text()
    )
    return [(step["tool"], step.get("arguments", {})) for step in task["golden"]]


def docker(*args: str) -> str:
    done = subprocess.run(["docker", *args], capture_output=True, text=True, check=False)
    if done.returncode != 0:
        pytest.fail(f"docker {' '.join(args)}: {done.stderr.strip()}")
    return done.stdout.strip()


@pytest.fixture(scope="session")
def urls() -> Iterator[list[str]]:
    label = f"miroir-python-test={os.getpid()}"
    started = [docker("run", "-d", "--label", label, "-p", "127.0.0.1::8000", IMAGE) for _ in "ab"]
    try:
        found = []
        for container in started:
            url = f"http://127.0.0.1:{docker('port', container, '8000/tcp').rsplit(':', 1)[1]}"
            _wait_healthy(url, container)
            found.append(url)
        yield found
    finally:
        subprocess.run(["docker", "rm", "-f", *started], capture_output=True, check=False)


@pytest.fixture
def token_url() -> Iterator[str]:
    label = f"miroir-python-test={os.getpid()}"
    options = ["--label", label, "-e", "MIROIR_TOKEN=s3cret", "-p", "127.0.0.1::8000"]
    container = docker("run", "-d", *options, IMAGE)
    try:
        url = f"http://127.0.0.1:{docker('port', container, '8000/tcp').rsplit(':', 1)[1]}"
        _wait_healthy(url, container)
        yield url
    finally:
        subprocess.run(["docker", "rm", "-f", container], capture_output=True, check=False)


@pytest.fixture
def code_url() -> Iterator[str]:
    pool = f"miroir-python-test-{os.getpid()}"
    script = str(ROOT / "python" / "tests" / "docker.sh")
    env = {**os.environ, "SANDBOX_IMAGE": "miroir-sandbox", "MIROIR_POOL": pool}

    def run(*args: str) -> subprocess.CompletedProcess[str]:
        command = [script, IMAGE, *args]
        return subprocess.run(command, capture_output=True, text=True, env=env, check=False)

    started = run("start")
    if started.returncode != 0:
        pytest.fail(f"docker.sh start: {started.stderr.strip()}")
    pair, url = started.stdout.split()[-2:]
    try:
        _wait_healthy(url, pair)
        yield url
    finally:
        stopped = run("stop", pair)
        assert stopped.returncode == 0, stopped.stderr
        label = f"label=miroir.pool={pool}"
        containers = docker("ps", "-a", "-q", "--filter", label)
        networks = docker("network", "ls", "-q", "--filter", label)
        assert (containers, networks) == ("", ""), "stopping a pair removes all of it"


@pytest.fixture
def coworker_url() -> Iterator[str]:
    label = f"miroir-python-test={os.getpid()}"
    container = docker("create", "--label", label, "-p", "127.0.0.1::8000", IMAGE)
    try:
        docker("cp", str(ASK_PRIYA), f"{container}:/miroir/tasks/ask-priya.yaml")
        docker("start", container)
        url = f"http://127.0.0.1:{docker('port', container, '8000/tcp').rsplit(':', 1)[1]}"
        _wait_healthy(url, container)
        yield url
    finally:
        subprocess.run(["docker", "rm", "-f", container], capture_output=True, check=False)


@contextlib.contextmanager
def env_with_tasks(tasks_dir: Path) -> Generator[str]:
    label = f"miroir-python-test={os.getpid()}"
    mount = f"{tasks_dir.resolve()}:/miroir/tasks:ro"
    container = docker("run", "-d", "--label", label, "-p", "127.0.0.1::8000", "-v", mount, IMAGE)
    try:
        url = f"http://127.0.0.1:{docker('port', container, '8000/tcp').rsplit(':', 1)[1]}"
        _wait_healthy(url, container)
        yield url
    finally:
        subprocess.run(["docker", "rm", "-f", container], capture_output=True, check=False)


def _wait_healthy(url: str, container: str) -> None:
    deadline = time.monotonic() + READY_LIMIT_S
    while time.monotonic() < deadline:
        try:
            with urllib.request.urlopen(f"{url}/health", timeout=2) as response:
                if response.status == 200:
                    return
        except OSError:
            pass
        time.sleep(0.5)
    logs = subprocess.run(
        ["docker", "logs", container], capture_output=True, text=True, check=False
    )
    pytest.fail(f"{container} never became healthy:\n{logs.stderr[-3000:]}")
