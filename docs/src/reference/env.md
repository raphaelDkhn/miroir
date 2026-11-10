# Environment variables

## You set

| Variable | Read by | What |
|---|---|---|
| `MIROIR_TOKEN` | `run` | the bearer token every route but `/health` requires; unset, anyone can act |
| `MIROIR_PORT` | `run`, `sandbox` | the port to serve on; 8000 by default |
| `MIROIR_SANDBOX` | `run`, `check` | `http://HOST:PORT` of a sandbox, which enables `run_code` |
| `MIROIR_LOG_BUDGET_MB` | `run`, `check` | the change-log size past which an episode faults, shared by the databases; 1024 by default |
| `MIROIR_ENV_VERSION` | `run` | the version `/metadata` reports, such as the product image's tag |
| `SANDBOX_IMAGE` | `sandbox/check.sh`, `python/tests/docker.sh` | the sandbox image to start |

## Miroir sets

| Variable | For | What |
|---|---|---|
| `MIROIR_WORLD` | `bake` and `seed` commands | the world being baked |
| `MIROIR_SHIFT_DAYS` | `time.hook` | whole days the timestamps move |
| `MIROIR_SHIFT_DATE_DAYS` | `time.hook` | calendar days the dates move |
| `MIROIR_EPISODE_DB` | command checks | the path of the episode database |
| `MIROIR_DATABASE_URL` | command checks | the URL of the check's app database, or the only one |
| `API_<APP>` | the agent's code in the sandbox | each app's API, valid during one `run_code` call |
| `MIROIR_POOL` | the `miroir pool` command | the pool's name, to label what it starts |

Every process and command also gets the manifest's `env`, and a process its own `env`. None
of them sees `MIROIR_TOKEN`.
