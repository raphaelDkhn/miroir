# Python client

The `miroir` Python package (Python 3.11 or later) leases containers and runs episodes. The
adapters build on it; use it directly for your own loop.

```sh
uv pip install miroir               # the client and the host CLI
uv pip install 'miroir[trl]'        # with the TRL adapter
uv pip install 'miroir[verifiers]'  # with the verifiers adapter
```

## An episode

```python
import datetime
import miroir
from miroir import Session

url, token = "http://127.0.0.1:8000", "secret"
task = miroir.tasks(url, "train", token)[0]

session = Session.lease([url], token, wait_busy=True)
try:
    start = session.reset(task["id"])
    today = datetime.date.fromisoformat(start["now"][:10])
    print(miroir.prompt(start, today))
    step = session.step("api_request", {"method": "GET", "path": "/invoices"})
    print(step["observation"]["result"]["status"])
    outcome = session.finish()
    print(outcome["reward"], miroir.metrics(outcome["record"]))
finally:
    session.close()
```

## `Session`

| Call | Does |
|---|---|
| `Session.lease(urls, token=None, *, wait_busy, timeout_s=600)` | opens a session on a free, healthy container; `urls` is a list or the path of a URL file |
| `reset(task_id, max_seconds=None, coworkers=False)` | starts an episode; returns the reset observation |
| `step(tool, arguments)` | calls a tool; returns `observation`, `reward` and `done` |
| `finish()` | submits if the episode still runs; returns `reward`, `success`, `metadata` and `record` |
| `state()` | the env's state frame |
| `coworker_turns()` | the turns [coworkers](../features/coworkers.md) owe |
| `coworker_reply(coworker, text)` | posts a coworker's answer |
| `close()` | frees the container |

`lease` with `wait_busy=False` fails at once when every container is leased; otherwise it
waits up to `timeout_s`.

## Helpers

| Function | Returns |
|---|---|
| `tasks(url, split="train", token=None)` | a split's tasks, as the env lists them |
| `tools(url, token=None, surface=None)` | the tools on a surface: `api`, `mcp` or `code` |
| `prompt(task, today)` | the user prompt: the instruction, who the agent acts as, its step budget and today's date |
| `read_urls(path)` | the URLs of a file `miroir pool` writes |
| `answer_coworkers(session, model)` | answers every owed coworker turn with a `CoworkerModel` |
| `metrics(record)` | `success`, `harm`, the diagnostics and `episode_s`, to log |
| `save_record(directory, record)` | appends a record to this process's JSONL file there |

`CoworkerModel(base_url, model, api_key=None, temperature=None)` is an OpenAI-compatible chat
endpoint.

## Errors

Everything raises `MiroirError`, with a `code` and a `message`. Codes include the env's frame
errors, `NO_FREE_CONTAINER`, `CONNECTION_FAILED`, `CONNECTION_LOST`, `NOT_GRADED` and
`COWORKER_FAILED`.
