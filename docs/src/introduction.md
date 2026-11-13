# Miroir

Miroir turns a product's Docker image into a reinforcement-learning environment for agents.

You add a manifest and a few tasks to that image. Miroir then runs the real product, gives an
agent its API, and grades what the agent did from what changed in the product's database and
from the emails and HTTP calls the product sent.
[`examples/gitea`](https://github.com/raphaelDkhn/miroir/tree/master/examples/gitea) does this
for the stock Gitea image, with 144 tasks and the measurements that show it grades right.

## How it works

At image build, `miroir bake` starts the product, runs your seed commands and keeps the
resulting state as a pristine copy. Before every episode, a reset stops every process, restores
that copy, moves every date forward so the data keeps its age, and starts the product again.

During an episode, the agent acts as a user of the product through tools: HTTP requests to the
API, the API's docs, the product's own MCP tools, code in a sandbox, and `submit`. Emails and
outbound HTTP calls land in local sinks. Grading then folds every row change into net changes,
runs the task's checks on them, and returns a reward from 0 to 1. A change the task does not
allow counts as harm.

Agents connect over [OpenEnv](https://github.com/openenv/OpenEnv) (a WebSocket) or MCP.
The Python package has a client and adapters for [TRL](https://github.com/huggingface/trl)
and [verifiers](https://github.com/PrimeIntellect-ai/verifiers).

## Two programs

| Program | Runs | Does |
|---|---|---|
| `miroir` (Rust) | inside the env image | bakes, resets, serves episodes, grades, checks, scrubs dumps |
| `miroir` (Python) | on the host | client, TRL and verifiers adapters, container pools, review, calibration |

Both are called `miroir`. Inside the image it is always the Rust binary.
