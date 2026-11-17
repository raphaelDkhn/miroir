# Serve an env

```sh
docker run --rm -d -p 127.0.0.1:8000:8000 -e MIROIR_TOKEN=secret my-env
```

`miroir run`, the image's default command, restores the first world and serves on port 8000.
Its log says `env ready` once `/health` answers 200.

One container runs one episode at a time, and holds one session: a second WebSocket is refused
with `CAPACITY_REACHED`. For parallel rollouts, run several containers, such as with a
[pool](pool.md).

## Routes

| Route | What |
|---|---|
| `GET /health` | `{"status": "healthy", "leased": false}`, or 503 while the env can't serve |
| `GET /ws` | the OpenEnv session; see [Protocol](protocol.md) |
| `POST /mcp` | MCP over JSON-RPC; see [Protocol](protocol.md#mcp) |
| `POST /episode`, `POST /episode/end` | start and end an episode without a WebSocket; see [Protocol](protocol.md#episodes-over-http) |
| `GET /metadata` | name, description, `version`, `miroir_version`, `manifest_hash`, `tasks_hash` |
| `GET /schema` | JSON schemas of the frames |
| `GET /openapi.json` | these routes |
| `GET /list_environments` | the env's name, in a list |
| `GET /{name}/splits` | the splits that have tasks |
| `POST /{name}/tasks` | a split's tasks: `{"split": "train"}` |
| `POST /{name}/num_tasks` | how many tasks a split has: `{"split": "train"}` |
| `POST /{name}/task` | one task: `{"split": "train", "index": 0}` |
| `POST /{name}/task_range` | a range: `{"split": "train", "start": 0, "stop": 10}` |

`/health` answers 503 before the first restore, while the env is broken, while its sandbox
doesn't answer, and once it is shutting down. `leased` says whether a session holds the env.

A listed task has `id`, `split`, `world`, `instruction`, `persona` (`name` and
`description`), `max_steps` and `task_hash`, plus `system_prompt` when the manifest has one
and `coworkers` when the task has some.

## Settings

Besides `MIROIR_TOKEN` and `MIROIR_AGENT_TOKEN`, the env reads `MIROIR_PORT` (8000 by
default), `MIROIR_SANDBOX` (a [sandbox](../features/sandbox.md) for `run_code`),
`MIROIR_LOG_BUDGET_MB` and `MIROIR_ENV_VERSION`. See
[Environment variables](../reference/env.md).

## Security

- Set `MIROIR_TOKEN`. Clients send it as `Authorization: Bearer <token>`; `/mcp` also takes
  `?token=`. The product's processes don't see it.
- When an agent calls `/mcp` itself, as over [HTTP episodes](protocol.md#episodes-over-http),
  give it `MIROIR_AGENT_TOKEN` instead: a second token that opens `/mcp` alone, so the agent
  can't end its episode, start another or read the task listings. It needs `MIROIR_TOKEN`
  set, and must differ from it.
- Requests that carry an `Origin` header, as web pages send, are refused.
- Publish the port on 127.0.0.1, and run the env on an internal network. Calls the product
  makes past the [sinks](../features/sinks.md) are neither recorded nor blocked.

## Recovery

When a process crashes or a reset fails, `/health` turns 503. While no session holds the env,
Miroir restores it every 10 seconds, backing off to 5 minutes, until it works again.

When files change outside the state directories, one episode could see another, so the env
refuses every reset from then on. Replace the container, and add the files to `state`.

## Stop

`docker stop` sends SIGTERM. Miroir stops the processes, giving them 30 seconds, and exits.
