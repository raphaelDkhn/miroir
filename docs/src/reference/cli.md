# CLI

Two programs are called `miroir`: the Rust binary inside the env image, and the Python CLI on
the host.

## In the image

| Command | Does |
|---|---|
| `miroir run` | serves the env; the image's default command. See [Serve an env](../run/serve.md) |
| `miroir bake` | seeds every world and keeps its state, at image build. See [the manifest](../make/manifest.md#what-bake-does) |
| `miroir check` | checks that the env and its tasks grade reliably. See [Check the env](../make/check.md) |
| `miroir sandbox` | serves the sandbox that runs the agent's code, in a container of its own |
| `miroir import` | scrubs production dumps and traces into seed data. See [Import](../production/import.md) |

`check` takes `--json`, `--mutate` and `--world W`.

`import` takes `--dump APP=FILE` (one per app), `--scrub FILE`, `--key FILE`, `--out DIR`
and, optionally, `--traces FILE`.

`miroir --version` prints the version. Every command must run as the container's entrypoint,
or as a child of its init.

## On the host

Install with `uv pip install miroir`.

| Command | Does |
|---|---|
| `miroir pool --size N --urls FILE -- COMMAND...` | keeps `N` containers healthy, listed in `FILE`. See [Pools](../run/pool.md) |
| `miroir review TRACES... --tasks DIR` | labels end states; wrong grades become probes. See [Review grades](../train/review.md) |
| `miroir calibrate TRACES... --model M --out FILE` | classes tasks as solved, learnable or out of reach. See [Calibrate tasks](../train/calibrate.md) |
| `miroir draft TRACES --world W --tasks DIR` | writes a draft task for each scrubbed trace. See [Tasks from traces](../production/traces.md) |
| `miroir derive URL TRACES --world W --tasks DIR` | replays the drafts and writes their checks |

| Option | Of | What |
|---|---|---|
| `--json` | `review` | print the queue as JSON, and ask nothing |
| `--label TASK@SIGNATURE=LABEL` | `review` | label without asking: `correct`, `should_fail` or `should_pass` |
| `-k K` | `calibrate` | `k` for pass@k and pass^k; by default the most rollouts of a task |
| `--split S` | `draft` | the drafts' split; `train` by default |
| `--token-var VAR` | `derive` | the variable that holds the env's `MIROIR_TOKEN` |

`TRACES` are the record files the adapters save, or directories of them. For `draft` and
`derive`, it is the `traces.jsonl` that `miroir import` writes.
