"""Print rl.toml with the pool's URLs and token in every source, for `prime train`.

    export MIROIR_TOKEN=...   # the pool's token
    python examples/gitea/with_pool.py urls.txt > rl.local.toml
    prime train rl.local.toml

Prime's full-parameter runs pass no variables to the env, so the URLs and the token go in the
config, under each train and eval source. Keep rl.local.toml out of version control: it holds
the token.
"""

import json
import os
import sys
from pathlib import Path

import miroir

SOURCES = ("[[orchestrator.train.source]]", "[[orchestrator.eval.source]]")


def with_pool(config: str, urls: list[str], token: str) -> str:
    """The config with the URLs and token added after each source's header."""
    lines = []
    for line in config.splitlines():
        lines.append(line)
        if line.strip() in SOURCES:
            lines.append(f"env.taskset.urls = {json.dumps(urls)}")
            lines.append(f"env.taskset.token = {json.dumps(token)}")
    return "\n".join(lines) + "\n"


def main() -> None:
    """Print the config for the URL file given as the only argument."""
    if len(sys.argv) != 2:
        raise SystemExit("usage: with_pool.py URLS_FILE > rl.local.toml")
    token = os.environ.get("MIROIR_TOKEN")
    if not token:
        raise SystemExit("set MIROIR_TOKEN to the pool's token")
    config = Path(__file__).with_name("rl.toml").read_text(encoding="utf-8")
    sys.stdout.write(with_pool(config, miroir.read_urls(sys.argv[1]), token))


if __name__ == "__main__":
    main()
