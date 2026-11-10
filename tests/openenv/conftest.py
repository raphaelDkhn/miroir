import os
import subprocess
import time
import urllib.request
from collections.abc import Iterator

import pytest

IMAGE = "acme-crm-env"


def docker(*args: str) -> str:
    done = subprocess.run(["docker", *args], capture_output=True, text=True, check=False)
    if done.returncode != 0:
        pytest.fail(f"docker {' '.join(args)}: {done.stderr.strip()}")
    return done.stdout.strip()


@pytest.fixture
def env_url() -> Iterator[str]:
    label = f"miroir-test-pid={os.getpid()}"
    container = docker("run", "-d", "--label", label, "-p", "127.0.0.1::8000", IMAGE)
    try:
        port = docker("port", container, "8000/tcp").rsplit(":", 1)[1]
        url = f"http://127.0.0.1:{port}"
        deadline = time.monotonic() + 120
        while not _healthy(url):
            if time.monotonic() > deadline:
                pytest.fail(f"the env never became ready:\n{_stderr_logs(container)}")
            time.sleep(0.5)
        yield url
    finally:
        subprocess.run(["docker", "rm", "-f", container], capture_output=True, check=False)


def _healthy(url: str) -> bool:
    try:
        with urllib.request.urlopen(f"{url}/health", timeout=2) as response:
            return response.status == 200
    except OSError:
        return False


def _stderr_logs(container: str) -> str:
    done = subprocess.run(
        ["docker", "logs", container], capture_output=True, text=True, check=False
    )
    return done.stderr
