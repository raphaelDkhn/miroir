# Protocol

Clients drive episodes over a WebSocket at `/ws`, in [OpenEnv](https://github.com/openenv/OpenEnv)
frames. Agents can also call the tools over MCP at `/mcp`, and a harness can start and end
their episodes [over HTTP](#episodes-over-http). The [Python client](../train/client.md) speaks
the WebSocket for you.

## Frames

Every frame is a JSON object with a `type` and, for most, `data`:

```json
{"type": "reset", "data": {"task_id": "pay-invoice"}}
```

| Frame | `data` | Reply |
|---|---|---|
| `reset` | which task, and options (below) | `observation`: the task the agent starts on |
| `step` | an action (below) | `observation`: the tool list or the call's result |
| `state` | none | `state`: `episode_id`, `step_count`, `task_id`, `done`, `reward`, `success` |
| `record` | `episode_id`, optional | `record`: the finished episode's [record](../reference/results.md#record) |
| `coworkers` | `{}` | `coworkers`: the turns coworkers owe |
| `coworker_reply` | `coworker`, `text` | `coworker_reply`: the app's HTTP `status` |
| `close` | none | the server closes the socket |

An error comes back as `{"type": "error", "data": {"code": "...", "message": "..."}}`. The
codes are `INVALID_JSON`, `VALIDATION_ERROR`, `UNKNOWN_TYPE`, `EXECUTION_ERROR`,
`SESSION_ERROR` and `CAPACITY_REACHED`.

## Reset

| Field | What |
|---|---|
| `task_id` | the task to run; wins over the others |
| `split` | the split to draw from; `train` by default |
| `index` | the split's task at this index, in id order |
| `seed` | the split's task at `seed` mod the number of tasks |
| `episode_id` | an id for the episode, 1 to 255 characters; a UUID by default |
| `max_seconds` | after this many seconds, the next call ends the episode as it stands |
| `coworkers` | `true` if the client answers [coworkers](../features/coworkers.md) |

With neither `task_id`, `index` nor `seed`, the task is drawn at random. Draws leave out
tasks with coworkers unless `coworkers` is `true`.

The reply:

```json
{"type": "observation", "data": {
  "observation": {
    "task_id": "pay-invoice",
    "instruction": "The customer on invoice INV-04001 asked us to charge their card...",
    "persona": {"name": "sales_manager", "description": "Dana Lee, sales manager at Acme"},
    "now": "2026-11-10T09:30:00Z",
    "max_steps": 20,
    "metadata": {"reset_s": {"kill": 0.05, "restore": 0.1, "total": 0.9}}
  },
  "reward": null,
  "done": false
}}
```

## Step

A step lists the tools or calls one:

```json
{"type": "step", "data": {"type": "list_tools"}}
{"type": "step", "data": {"type": "call_tool", "tool_name": "api_request",
  "arguments": {"method": "GET", "path": "/invoices?number=INV-04001"},
  "metadata": {"episode_id": "..."}}}
```

The reply carries the observation, the reward and `done`:

```json
{"type": "observation", "data": {
  "observation": {"tool_name": "api_request", "error": null, "metadata": {},
                  "result": {"status": 200, "headers": {"content-type": "application/json"},
                             "body": [{"id": 4001, "number": "INV-04001", "status": "open"}]}},
  "reward": 0.0,
  "done": false
}}
```

A failed call has `result: null` and an `error` with an `error_type` (`invalid_args`,
`execution_error`, `transport_error`, `tool_not_found` or `timeout`) and a `message`. The
agent can go on after an error.

The step that ends the episode has `done: true`, the reward, and the episode's metadata. The
reward is `null` when the episode could not be graded. See [Grading](../make/grading.md).

Pass the reset's `episode_id` in each call's `metadata`: a call for an earlier episode is
refused instead of acting on the current one. Arguments set to `null` are dropped.

## Coworker frames

`coworkers` returns `{"turns": [{"coworker": "priya", "messages": [...]}]}`, where `messages`
is a chat to send to a model: a system message, then user and assistant messages. Send the
model's answer back:

```json
{"type": "coworker_reply", "data": {"coworker": "priya", "text": "It's qualified."}}
```

## MCP

`POST /mcp` speaks MCP over JSON-RPC (protocol `2025-06-18` or `2025-03-26`): `initialize`,
`ping`, `tools/list` and `tools/call`. Its query string takes:

| Parameter | What |
|---|---|
| `surface` | `api` (default): `api_request`, `api_docs`, `wait`, `submit`; `code`: `run_code` in place of `api_request`; `mcp`: the app's MCP tools, `wait` and `submit`. `wait` is listed only for tasks with a clock |
| `episode_id` | the episode the calls are for |
| `token` | the bearer token, for clients that can't send headers |

MCP calls step the same episode as the WebSocket session, or as `POST /episode`, which does
the reset. A `tools/call` result is the step's observation, with MCP's `content` and
`isError`, and `done` and `reward`.

## Episodes over HTTP

A harness whose agent speaks MCP alone, such as Claude Code or Codex, can run an episode with
two HTTP requests in place of the WebSocket session:

```sh
curl -X POST http://127.0.0.1:8000/episode -H 'Authorization: Bearer secret' \
  --json '{"task_id": "pay-invoice"}'
# The agent works through http://127.0.0.1:8000/mcp ...
curl -X POST http://127.0.0.1:8000/episode/end -H 'Authorization: Bearer secret'
```

| Route | Body | Answer |
|---|---|---|
| `POST /episode` | a `reset` frame's `data`; empty draws a task | the reset's `observation`, `reward` and `done`, and the `episode_id` |
| `POST /episode/end` | none | the episode's [record](../reference/results.md#record) |

`POST /episode` holds the env as a session does: until `POST /episode/end`, `/health` says
`leased`, and a second `POST /episode` or a WebSocket session is refused. `POST /episode/end`
submits for the agent if the episode still runs, as the Python client's `finish()` does, then
answers the record and frees the env. Unlike a session, which ends when its socket closes, the
episode holds the env until someone ends it: a harness that stops first leaves it held, and
`POST /episode/end` frees it. Tasks with coworkers need the WebSocket session, whose frames
answer for them.

Errors answer `{"detail": "..."}`: 400 for data the reset refuses, 409 when the env is held,
or when no episode started over HTTP is running, and 500 when the reset or the end fails.

Give the agent `MIROIR_AGENT_TOKEN`, not `MIROIR_TOKEN`: it opens `/mcp` alone, so the agent
can't end its episode or start another. See [Serve an env](serve.md#security).
