# verifiers

The `miroir-verifiers` environment runs an env's tasks under
[verifiers](https://github.com/PrimeIntellect-ai/verifiers), for evaluation with `vf-run` and
for training.

```sh
uv pip install 'miroir[verifiers]'
export MIROIR_TOKEN=secret
vf-run miroir-verifiers -m <model> \
  --env.taskset.urls_file urls.txt --env.taskset.token_var MIROIR_TOKEN
```

Each rollout leases a free container, waiting up to an hour for one, resets its task, and
serves the env's tools to the model over MCP. The reward is recorded as `miroir`, along with
the [metrics](client.md#helpers) of the episode. A rollout that could not be graded fails
with `INVALID_GRADE` instead of earning a reward.

## Options

All under `--env.taskset.`:

| Option | What |
|---|---|
| `urls` or `urls_file` | the containers: a JSON list of base URLs, or a file `miroir pool` writes |
| `split` | the split to run; `train` by default |
| `token_var` | the variable that holds the env's `MIROIR_TOKEN` |
| `token` | the token itself, for a run that passes no variables to the env |
| `traces` | a directory to save each episode's record in |
| `calibration`, `keep` | a [calibration](calibrate.md) file, and the classes to run; `["learnable"]` by default |
| `harm_reward` | caps the reward of a harmful episode, from -1 to 0 |
| `surface` | `api` (default), `mcp` for the app's MCP tools, or `code` for `run_code` |
| `max_seconds` | a time limit per episode, 1 to 86,400 |
| `coworker_model` | the model that plays [coworkers](../features/coworkers.md) |
| `coworker_base_url` | its OpenAI-compatible endpoint |
| `coworker_api_key_var` | the variable that holds its API key |

Tasks with coworkers run only when `coworker_model` is set.

The number of rollouts in flight is capped at the number of URLs the file lists when the run
starts.

## Prime Intellect Hosted Training

Hosted Training runs the env on Prime's servers, which install it from the Environments Hub.
`python/push_hub.py` pushes it there, private, as `<you>/miroir-verifiers`: a wheel of the
adapter and the client, versioned with the push's time, since the Hub never reuses a version.

```sh
python python/push_hub.py
```

The containers must be reachable from Prime, such as a [pool of Prime
sandboxes](../run/pool.md#on-prime-intellect). The training config names the taskset by its
Hub id, `env.taskset.id = "<you>/miroir-verifiers"`. Full-parameter runs pass neither
`prime train -e` variables nor environment secrets to the env, so the pool's URLs and token go
in the config: with the source table last, append them to a copy and dispatch that:

```sh
{ cat rl.toml
  python -c 'import json, miroir; print("env.taskset.urls =", json.dumps(miroir.read_urls("urls.txt")))'
  echo "env.taskset.token = \"$MIROIR_TOKEN\""
} > rl.local.toml
prime train rl.local.toml
```

A config with several sources needs them under each, as the train and eval sources of
[`examples/gitea/rl.toml`](https://github.com/raphaelDkhn/miroir/blob/master/examples/gitea/rl.toml)
do; [`examples/gitea/with_pool.py`](https://github.com/raphaelDkhn/miroir/blob/master/examples/gitea/with_pool.py)
adds them.

The run's config, and its logs (verifiers logs each rollout's MCP URL, which carries the
token), then hold the token: use a token for the pool alone, and delete the sandboxes when the
run ends.
