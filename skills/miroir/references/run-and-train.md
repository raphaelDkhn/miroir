# Serve, play, train and evaluate

## Serve

```sh
docker run --rm -d -p 127.0.0.1:8000:8000 -e MIROIR_TOKEN=secret my-env
curl -s http://127.0.0.1:8000/health
curl -s -H 'Authorization: Bearer secret' http://127.0.0.1:8000/acme-crm/splits  # /<manifest name>/
```

`miroir run`, the image's default command, restores the first world and serves on port 8000. Its
log says `env ready` once `/health` answers 200. One container runs one episode at a time and
holds one session: a second WebSocket is refused with `CAPACITY_REACHED`. Run several containers
for parallel rollouts, such as with a [pool](#pools).

| Route | What |
|---|---|
| `GET /health` | `{"status": "healthy", "leased": false}`, or 503 while the env can't serve |
| `GET /ws` | the OpenEnv session |
| `POST /mcp` | MCP over JSON-RPC |
| `GET /metadata` | name, description, `version`, `miroir_version`, `manifest_hash`, `tasks_hash` |
| `GET /schema`, `/openapi.json` | the frames' JSON schemas; these routes |
| `GET /list_environments`, `/{name}/splits` | the env's name; the splits that have tasks |
| `POST /{name}/tasks`, `/{name}/num_tasks` | a split's tasks, or how many: `{"split": "train"}` |
| `POST /{name}/task`, `/{name}/task_range` | `{"split": "train", "index": 0}`; `{"split": "train", "start": 0, "stop": 10}` |

A listed task has `id`, `split`, `world`, `instruction`, `persona` (`name`, `description`),
`max_steps` and `task_hash`, plus `system_prompt` and `coworkers` when there are some.

| Variable | What |
|---|---|
| `MIROIR_TOKEN` | the bearer token every route but `/health` requires; unset, anyone can act |
| `MIROIR_PORT` | the port to serve on; 8000 by default |
| `MIROIR_SANDBOX` | `http://HOST:PORT` of a sandbox, which enables `run_code` |
| `MIROIR_LOG_BUDGET_MB` | change-log size past which an episode faults; 1024 by default |
| `MIROIR_ENV_VERSION` | the version `/metadata` reports |

Requests that carry an `Origin` header, as web pages send, are refused. Publish the port on
127.0.0.1 and run the env on an internal network: it warns at start when it can reach the
internet. When a process crashes or a reset fails, `/health` turns 503 and, while no session
holds the env, Miroir restores it every 10 s, backing off to 5 minutes. When files change outside
the state, the env refuses every reset: replace the container and add the files to `state`.

## Protocol

Clients drive episodes over the WebSocket at `/ws` with JSON frames, `{"type": ..., "data": ...}`:

| Frame | `data` | Reply |
|---|---|---|
| `reset` | `task_id`, or `split` with `index` or `seed` (else a random task); `episode_id`; `max_seconds`; `coworkers` | `observation`: `task_id`, `instruction`, `persona`, `now`, `max_steps` |
| `step` | `{"type": "list_tools"}`, or `{"type": "call_tool", "tool_name": ..., "arguments": {...}, "metadata": {"episode_id": ...}}` | `observation` (`result`, `error`), `reward`, `done` |
| `state` | none | `episode_id`, `step_count`, `task_id`, `done`, `reward`, `success` |
| `record` | `episode_id`, optional | the finished episode's [record](#records) |
| `coworkers` | `{}` | the turns coworkers owe, each with the chat to answer |
| `coworker_reply` | `coworker`, `text` | the app's HTTP `status` |
| `close` | none | the server closes the socket |

Errors come back as `{"type": "error", "data": {"code", "message"}}`. A failed tool call has
`result: null` and an `error` with an `error_type`: `invalid_args`, `tool_not_found`,
`execution_error`, `transport_error` or `timeout`; the agent can go on.

`POST /mcp` serves the same episode's tools over MCP (`initialize`, `ping`, `tools/list`,
`tools/call`), while the WebSocket session still does the reset. Its query string takes
`surface` (`api`: `api_request`, `api_docs`, `wait`, `submit`; `code`: `run_code` in place of
`api_request`; `mcp`: the product's MCP tools, `wait` and `submit`), `episode_id` and `token`.

## Python client

```python
import datetime
import miroir
from miroir import Session

url, token = "http://127.0.0.1:8000", "secret"
task = miroir.tasks(url, "train", token)[0]
session = Session.lease([url], token, wait_busy=True)
try:
    start = session.reset(task["id"])
    print(miroir.prompt(start, datetime.date.fromisoformat(start["now"][:10])))
    step = session.step("api_request", {"method": "GET", "path": "/invoices"})
    outcome = session.finish()
    print(outcome["reward"], miroir.metrics(outcome["record"]))
finally:
    session.close()
```

| Call | Does |
|---|---|
| `Session.lease(urls, token=None, *, wait_busy, timeout_s=600)` | opens a session on a free, healthy container; `urls` is a list or a `pathlib.Path` to a URL file |
| `reset(task_id, max_seconds=None, coworkers=False)` | starts an episode; returns the reset observation |
| `step(tool, arguments)` | calls a tool; returns `observation`, `reward` and `done` |
| `finish()` | submits if the episode still runs; returns `reward`, `success`, `metadata` and `record` |
| `state()`, `coworker_turns()`, `coworker_reply(coworker, text)`, `close()` | the state frame; owed coworker turns; post a coworker's answer; free the container |

Helpers: `tasks(url, split="train", token=None)`, `tools(url, token=None, surface=None)`,
`prompt(task, today)`, `read_urls(path)`, `answer_coworkers(session, model)`, `metrics(record)`,
`save_record(directory, record)`, and `CoworkerModel(base_url, model, api_key=None,
temperature=None)` for any OpenAI-compatible chat endpoint. Everything raises `MiroirError` with
a `code`, such as `NO_FREE_CONTAINER`, `CONNECTION_FAILED`, `CONNECTION_LOST`, `NOT_GRADED` or
`COWORKER_FAILED`.

## Records

`finish()["record"]`, or the `record` frame, is the finished episode within 4 MiB: `episode_id`,
`task_id`, `reward`, `null_max`, `metadata` (see `references/tasks.md`, "Grading"), `signature`
(a hash of the end state; rollouts that end alike share it on any day), `diagnostics` (`calls`,
`client_errors`, `server_errors`, `repeated_calls`, `docs_calls`, `code_requests`,
`coworker_replies`, `events`), `actions`, `changes`, `effects`, `timings` (`reset_s`,
`episode_s`, `grading_s`) and `truncated`. The adapters save records with `traces=`; `review` and
`calibrate` read them.

## Pools

Each rollout needs a container of its own. `miroir pool` (host CLI) keeps N healthy and lists
their URLs in a file, which the adapters reread at each lease:

```sh
export MIROIR_TOKEN=secret
miroir pool --size 8 --urls urls.txt -- ./docker.sh my-env
```

The command after `--` starts or stops one container: `COMMAND start` prints `ID URL` as its last
line, and `COMMAND stop ID` removes it. It runs with `MIROIR_POOL` set to the pool's name, for
labels. `python/tests/docker.sh` in the Miroir repository is a full example: it starts the env
alone, or beside a sandbox when `SANDBOX_IMAGE` is set, and passes `MIROIR_TOKEN` through. Size
the pool to the rollouts in flight plus a few spare; it prints `ready: 8 of 8; start the trainer`.
It replaces containers that stay unhealthy, and on SIGINT or SIGTERM stops them all and removes
the file.

## TRL

```sh
uv pip install './python[trl]'
```

```python
from trl import GRPOConfig, GRPOTrainer
from miroir import read_urls
from miroir.trl import dataset, environment_factory, reward

urls = read_urls("urls.txt")
trainer = GRPOTrainer(
    model="<model>",
    train_dataset=dataset(urls[0], token="secret"),
    environment_factory=environment_factory(urls_file="urls.txt", token="secret", traces="traces/"),
    reward_funcs=reward,
    args=GRPOConfig(output_dir="out"),
)
trainer.train()
```

- `dataset(url, split="train", token=None, calibration=None, keep=("learnable",),
  coworkers=False)`: a row per task, keeping only the classes in `keep` with a calibration file.
- `environment_factory(urls=None, token=None, traces=None, *, harm_reward=None, urls_file=None,
  max_seconds=None, surface="api", coworker=None)`: each environment leases a container, and its
  public methods are the env's tools. `surface="code"` gives `run_code`; `coworker` takes a
  `CoworkerModel`.
- `reward` finishes each episode and returns its reward, or `None` when it could not be graded.
- TRL makes `per_device_train_batch_size * steps_per_generation` environments per process, more
  for evaluation; size the pool to match.

## verifiers

```sh
uv pip install './python[verifiers]'
export MIROIR_TOKEN=secret
vf-run miroir-verifiers -m <model> \
  --env.taskset.urls_file urls.txt --env.taskset.token_var MIROIR_TOKEN --env.taskset.traces traces/
```

Options under `--env.taskset.`: `urls` (a JSON list) or `urls_file`, `split`, `token_var` (or
`token`, for a run that passes the env no variables, as Prime's Hosted Training),
`traces`, `calibration` and `keep`, `harm_reward`, `surface` (`api`, `mcp` or `code`),
`max_seconds`, and `coworker_model`, `coworker_base_url`, `coworker_api_key_var`. A rollout that
could not be graded fails with `INVALID_GRADE`. Rollouts in flight are capped at the number of
URLs the file lists when the run starts.

## Review grades

```sh
miroir review traces/ --tasks tasks/
```

Review groups rollouts by task and end-state signature, shows each end state (rewards, net
changes, effects, check scores, one example's calls) and asks whether the grade is right:

| Label | Means | Writes |
|---|---|---|
| `correct` | the grade is right | the label |
| `should_fail` | it earned too much | a probe with `max_reward` set to the task's `null_max` |
| `should_pass` | it earned too little | a probe with `min_reward: 1` |

Probes go in `tasks/probes/<task-id>.yaml`, labels in `tasks/probes/labels.jsonl`. To review
without a prompt, read the queue with `--json`, then label with
`--label TASK@SIGNATURE=LABEL` (repeatable). Then fix the checks and run `miroir check`: the new
probes fail until the grade is right.

## Calibrate tasks

Run several rollouts per task (such as 8) with traces saved, then:

```sh
miroir calibrate traces/ --model my-model-v3 --out calibration.json
```

It prints `n`, `void`, `pass@1`, `pass@k`, `pass^k`, the mean reward, its variance and the harm
rate per task, and a class: `learnable` (rewards vary), `solved` (every rollout succeeds) or
`out_of_reach` (every rollout earns the same, short of success). Tasks with fewer than two graded
rollouts get no class. `-k` sets `k`. Pass the file to `dataset(..., calibration=...)` or
`--env.taskset.calibration` to train on learnable tasks only. The file records each task's
`task_hash`, and the adapters refuse it once a task file changes until that task is calibrated
again.
