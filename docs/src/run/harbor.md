# Run in Harbor

[Harbor](https://harborframework.com) runs agents such as Claude Code and Codex on tasks,
locally or in cloud sandboxes. `miroir export harbor` writes each task of an env as a Harbor
task: the agent works the product through the env's MCP server, and Miroir grades what it
did.

## Export

```sh
mkdir harbor
docker run --rm -v "$PWD/harbor:/out" my-env export harbor --image my-env:1.0 --out /out
```

`--image` names the env image as Harbor's Docker will find it: a tag on the machine that runs
Harbor, or an image in a registry it can pull. Each task goes in `harbor/<split>/<task-id>/`.

The export leaves out, and names, the tasks Harbor can't run: tasks with
[coworkers](../features/coworkers.md), and tasks whose golden run calls a tool the agent won't
have, such as `run_code` or one of the product's own MCP tools. The agent gets the `api`
surface: `api_request`, `api_docs`, `submit` and, for tasks with a clock, `wait`.

## Run

```sh
uv tool install harbor
harbor run -p harbor/test -a oracle                    # each reward should be 1
harbor run -p harbor/test -a claude-code -m <model>
```

The oracle agent replays each golden run through the MCP server, as an agent would. Each
trial's reward has two metrics: `reward`, the episode's, and `harm`, 1 when the episode did
harm, so a job's means are its mean reward and its harm rate. The trial's
`verifier/record.json` is the episode's [record](../reference/results.md#record), which
`miroir review` and `miroir calibrate` read:

```sh
miroir calibrate jobs/<job>/*/verifier/record.json --model <model> --out calibration.json
```

## How a task runs

| Service | Runs | Holds |
|---|---|---|
| `main` | the agent, as the user `agent`; then the verifier | the agent token, in the agent's MCP settings |
| `app` | the env image, on a network with no way out | both tokens |
| `start` | `POST /episode` for the task, before `main` starts | the control token |

The verifier's `tests/test.sh` calls `POST /episode/end`, which submits for the agent if it
didn't, and writes the reward. The agent's token opens `/mcp` alone, so it can't end its
episode or start another; see `MIROIR_AGENT_TOKEN` in [Serve an env](serve.md#security).

## Limits

- Each task holds random tokens. They keep the agent from the env's other routes, not
  whoever reads the task files, which also hold the golden runs.
- The agent's container reaches whatever the env listens on outside loopback. Keep the
  product and its datastores on 127.0.0.1, as the examples do, so the agent reaches only
  Miroir.
- Run the tasks in Harbor's default `public` network mode: the agent's container joins the
  env's network, which the `no-network` and `allowlist` modes take away or bypass.
- The tasks are Docker Compose tasks: they run on Harbor's `docker` environment and its
  cloud environments that run Compose, such as Daytona and Modal.
