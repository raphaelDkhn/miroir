# verifiers

The `miroir-verifiers` environment runs an env's tasks under
[verifiers](https://github.com/PrimeIntellect-ai/verifiers), for evaluation with `vf-run` and
for training.

```sh
uv pip install './python[verifiers]'
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
