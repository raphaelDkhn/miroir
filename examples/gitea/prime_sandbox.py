"""Run env containers as Prime Intellect sandboxes: a command for `miroir pool`.

    export MIROIR_TOKEN=secret
    miroir pool --size 16 --urls urls.txt -- \\
        python examples/gitea/prime_sandbox.py prime/<you>/gitea-env:<tag>

`IMAGE start` creates a sandbox from the image, publishes its port 8000 at a public HTTPS URL
and prints `ID URL`; `IMAGE stop ID` deletes the sandbox. Prime does not authenticate that URL,
so MIROIR_TOKEN is all that guards the env: `start` refuses to run without it. Needs
`prime_sandboxes` (verifiers depends on it) and a `prime login`.
"""

import os
import sys
import time
import uuid

from prime_sandboxes import APIClient, CreateSandboxRequest, LanRule, SandboxClient, StartCommand
from prime_sandboxes.core.client import APIError

PORT = 8000
GATEWAY = "sandbox-v2-prod.pinfra.io"
# A sandbox is a VM, not a container, and each reset kills every process but PID 1 and miroir:
# miroir runs in a PID namespace of its own, under tini, as it would as a container's entrypoint.
START = "exec unshare --pid --fork --mount-proc tini -- miroir run"
LIFETIME_MIN = 24 * 60
USAGE = "usage: prime_sandbox.py IMAGE start | prime_sandbox.py IMAGE stop ID"


def start(client: SandboxClient, image: str) -> str:
    """Create a sandbox from `image` on a network of its own that publishes its port 8000.

    Args:
        client: The Prime sandbox client.
        image: The env's Prime image, such as prime/<you>/gitea-env:<tag>.

    Returns:
        The line `miroir pool` reads: the sandbox and network IDs, and the env's URL.
    """
    token = os.environ.get("MIROIR_TOKEN")
    if not token:
        raise SystemExit("set MIROIR_TOKEN: it is all that guards the sandbox's public URL")
    pool = os.environ.get("MIROIR_POOL", "miroir")
    name = f"{pool}-{uuid.uuid4().hex[:8]}"
    exposed = LanRule(from_="internet", to="*", ports=[PORT])
    network = client.create_lan_network(name, rules=[exposed])
    sandbox_id = None
    try:
        request = CreateSandboxRequest(
            name=name,
            docker_image=image,
            start_command=StartCommand(executable="/bin/sh", args=["-c", START]),
            cpu_cores=2,
            memory_gb=2,
            disk_size_gb=10,
            timeout_minutes=LIFETIME_MIN,
            secrets={"MIROIR_TOKEN": token},
            # The env needs no egress: the product's mail and webhooks go to miroir's sinks.
            network_denylist=["0.0.0.0/0"],
            labels=[pool],
            lan_network_id=network.id,
        )
        sandbox_id = client.create(request).id
        client.wait_for_creation(sandbox_id)
    except BaseException:
        stop(client, f"{sandbox_id}:{network.id}")
        raise
    return f"{sandbox_id}:{network.id} https://{PORT}-{sandbox_id}.{GATEWAY}"


def stop(client: SandboxClient, found: str) -> None:
    """Delete a sandbox and its network, as `start` named them: `SANDBOX:NETWORK`.

    Args:
        client: The Prime sandbox client.
        found: The ID `start` printed.
    """
    sandbox_id, _, network_id = found.partition(":")
    if sandbox_id and sandbox_id != "None":
        _gone(lambda: client.delete(sandbox_id))
    # The network refuses deletion until the deleted sandbox has left it.
    for _ in range(30):
        try:
            _gone(lambda: client.delete_lan_network(network_id))
            return
        except APIError as e:
            print(f"waiting for {sandbox_id} to leave network {network_id}: {e}", file=sys.stderr)
            time.sleep(2)
    raise SystemExit(f"cannot delete network {network_id}; delete it with `prime sandbox network`")


def _gone(delete) -> None:
    try:
        delete()
    except APIError as e:
        if "404" not in str(e):
            raise


def main() -> None:
    """Run `start` or `stop` from the command line."""
    args = sys.argv[1:]
    client = SandboxClient(APIClient())
    if len(args) == 2 and args[1] == "start":
        print(start(client, args[0]))
    elif len(args) == 3 and args[1] == "stop":
        stop(client, args[2])
    else:
        raise SystemExit(USAGE)


if __name__ == "__main__":
    main()
