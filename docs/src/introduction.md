# Miroir

Miroir turns a product's container into a reinforcement-learning environment for agents.

You add a manifest and a few tasks to the product's Docker image. Miroir then runs the real
product, gives an agent its API, and grades what the agent did from what changed in the
product's database and from the emails and HTTP calls the product sent.

## How it works

1. **Bake.** At image build, `miroir bake` starts the product, runs your seed commands and
   keeps the resulting state as a pristine copy.
2. **Reset.** Before every episode, Miroir stops every process, restores that copy, moves
   every date forward so the data keeps its age, and starts the product again.
3. **Episode.** The agent acts as a user of the product through tools: HTTP requests to the
   API, the API's docs, the product's own MCP tools, code in a sandbox, and `submit`. Emails
   and outbound HTTP calls land in local sinks.
4. **Grade.** Miroir folds every row change into net changes, runs the task's checks on them,
   and returns a reward from 0 to 1. A change the task does not allow counts as harm.

Agents connect over [OpenEnv](https://github.com/openenv/OpenEnv) (a WebSocket) or MCP.
The Python package has a client and adapters for [TRL](https://github.com/huggingface/trl)
and [verifiers](https://github.com/PrimeIntellect-ai/verifiers).

## Two programs

| Program | Runs | Does |
|---|---|---|
| `miroir` (Rust) | inside the env image | bakes, resets, serves episodes, grades, checks, scrubs dumps |
| `miroir` (Python) | on the host | client, TRL and verifiers adapters, container pools, review, calibration |

Both are called `miroir`. Inside the image it is always the Rust binary.

## Where to go next

- New here: run the [quick start](quick-start.md), then read the [concepts](concepts.md).
- Making an env for your product: start at [Add Miroir to the image](make/image.md).
- Training on an env: see [Serve an env](run/serve.md) and [TRL](train/trl.md) or
  [verifiers](train/verifiers.md).
- Seeing it on a real product:
  [`examples/gitea`](https://github.com/raphaelDkhn/miroir/tree/master/examples/gitea) makes the
  stock Gitea image an env, with tasks, probes and a training script.
- Looking up a field: see the [reference](reference/manifest.md).
