# Gitea: A Real-World RL Environment

This example demonstrates how **Miroir turns an existing application into a reproducible environment for training and evaluating AI agents.**

The application is stock [Gitea](https://about.gitea.com) 28.3, running from its official Docker image, pinned by digest and without patches.

The environment tests realistic engineering workflows: issue triage, pull requests, access management, offboarding, webhooks, releases, and incident response.

## What's included

Three independent company worlds, each with realistic users, teams, repositories, issues, pull requests, reviews, and permissions.

| World     | Company             | Split              |
| --------- | ------------------- | ------------------ |
| `orbit`   | Orbit Payments      | Train / Validation |
| `ferry`   | Ferrylane Logistics | Train / Validation |
| `kestrel` | Kestrel Health      | Test               |

The test world has entirely separate users, repositories, and labels to prevent training-data leakage.

**12 task families** cover everyday Gitea operations, including:

- Issue counting and stale PR detection
- Issue triage and assignment using CODEOWNERS
- Issue cleanup and employee offboarding
- Fine-grained repository access
- Webhook configuration and delivery
- Safe PR merging and incident response
- Release management
- Secret redaction
- Multi-step tasks involving another colleague

Tasks are varied across worlds, repositories, people, and expected outcomes.

The dataset contains 144 tasks:

| Split      | Tasks |
| ---------- | ----: |
| Train      |    54 |
| Validation |    42 |
| Test       |    48 |

It also includes 360 adversarial probes to test incorrect actions and unintended side effects.


## Run it

Requirements: Docker with buildx and [uv](https://docs.astral.sh/uv/).

### Build the environment

```bash
docker buildx bake gitea-env
```

### Verify the environment

```bash
for w in orbit ferry kestrel; do
  docker run --rm --network none gitea-env check \
    --world $w --json > check-$w.json 2> check-$w.log &
done
wait

python examples/gitea/evidence.py \
  check-orbit.json check-ferry.json check-kestrel.json
```

This replays the tasks, golden solutions, repeated episodes, and adversarial probes.

### Play an episode

```bash
docker run --rm -d -p 127.0.0.1:8000:8000 \
  -e MIROIR_TOKEN=secret gitea-env

uv venv
uv pip install ./python
```

```python
from miroir import Session

session = Session.lease(
    ["http://127.0.0.1:8000"],
    token="secret",
)

task = session.reset("triage.orbit.1")
print(task["instruction"])

result = session.step(
    "api_request",
    {
        "method": "GET",
        "path": "/repos/orbit/payments-gateway/issues",
    },
)

outcome = session.finish()
print(outcome["reward"])

session.close()
```

### Evaluate an agent

Use the included Verifiers integration with any OpenAI-compatible model.

```bash
uv pip install './python[verifiers]'

export MIROIR_TOKEN=secret

miroir pool --size 8 --urls urls.txt -- \
  python/tests/docker.sh gitea-env

vf-run miroir-verifiers -m <model> \
  --client.base_url <url> \
  --client.api_key_var <VAR> \
  -r 4 \
  --env.taskset.urls_file urls.txt \
  --env.taskset.token_var MIROIR_TOKEN \
  --env.taskset.split test \
  --env.taskset.traces traces/
```

### Train with TRL

```bash
uv pip install './python[trl]' 'trl[vllm]'

miroir pool --size 20 --urls urls.txt -- \
  python/tests/docker.sh gitea-env

python examples/gitea/train.py --urls urls.txt
```

### Review and improve grading

Inspect agent traces and turn corrected grades into regression probes:

```bash
miroir review traces/ --tasks examples/gitea
```

Re-run the environment checks to ensure future agents don't repeat known failures.

See [`rl.toml`](rl.toml) for the training configuration and the [Miroir documentation](../../docs/src/) for the full API and integrations.

## Limitations

- Worlds are generated rather than imported from production data. For real application data, see [production imports](../../docs/src/production/import.md).
- Tasks cover relatively short workflows (1–20 API calls), not long-running engineering projects.
- Agents interact through Gitea's REST API. The web UI and MCP server are not integrated.

---

**The point of this example:** Miroir can take a real, unmodified application, create realistic workflows, expose it to agents, restore its state, and objectively measure their behavior. Gitea is the proof of that capability.
