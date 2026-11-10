# TRL

`miroir.trl` plugs an env into TRL's `GRPOTrainer`: a dataset of tasks, an environment
factory whose methods are the agent's tools, and a reward function.

```sh
uv pip install './python[trl]'
```

```python
from trl import GRPOConfig, GRPOTrainer
from miroir import read_urls
from miroir.trl import dataset, environment_factory, reward

urls = read_urls("urls.txt")  # as `miroir pool` writes it
trainer = GRPOTrainer(
    model="<model>",
    train_dataset=dataset(urls[0], token="secret"),
    environment_factory=environment_factory(urls_file="urls.txt", token="secret"),
    reward_funcs=reward,
    args=GRPOConfig(output_dir="out"),
)
trainer.train()
```

## `dataset`

`dataset(url, split="train", token=None, calibration=None, keep=("learnable",), coworkers=False)`

A row per task, with `task_id`, `instruction`, `persona` and `max_steps`, and a `prompt` with
the system message when the manifest has a `system_prompt`. With `calibration`, only the
tasks whose [class](calibrate.md) is in `keep`. Tasks with coworkers are left out unless
`coworkers=True`.

## `environment_factory`

`environment_factory(urls=None, token=None, traces=None, *, harm_reward=None, urls_file=None, max_seconds=None, surface="api", coworker=None)`

| Option | What |
|---|---|
| `urls` or `urls_file` | the containers: a list, or a file `miroir pool` writes |
| `token` | the env's `MIROIR_TOKEN` |
| `traces` | a directory to save each episode's record in, for [review](review.md) and [calibration](calibrate.md) |
| `harm_reward` | caps the reward of a harmful episode, from -1 to 0 |
| `max_seconds` | a time limit per episode, 1 to 86,400 |
| `surface` | `api`, or `code` for `run_code` in place of `api_request` |
| `coworker` | a `CoworkerModel` that answers for [coworkers](../features/coworkers.md) |

Each environment leases a container, and its public methods are the tools the env offers:
`api_request`, `api_docs`, `run_code`, `wait` and `submit`. With several apps, `api_request`
and `api_docs` take an `app`.

TRL makes `per_device_train_batch_size * steps_per_generation` environments per process, more
for evaluation, and each needs a container of its own. Size the [pool](../run/pool.md) to
match.

## `reward`

`reward` finishes each episode and returns its reward, or `None` for an episode that could
not be graded. With `log_metric`, it logs the means of `miroir/success`, `miroir/harm`, the
call diagnostics and `miroir/episode_s`.

The prompt tells the agent today's date. If the env's date differs, as when an episode starts
across midnight, the episode has no reward.
