# miroir

Turn a product's container into a reinforcement-learning environment for agents.

You add a manifest and tasks to the product's image. `miroir bake` seeds the data once, at build
time. `miroir run` then restores that seed before every episode, serves the agent's tools over
OpenEnv (WebSocket) and MCP, and grades the end state from the rows that changed and the email and
HTTP calls the product sent.

The documentation is the book in [docs/](docs/src/introduction.md): `mdbook serve docs`.

## How it works

- **Bake** starts the datastores and runs the manifest's `bake` commands, then starts the apps and
  runs its `seed` commands, installs triggers that log every row change, and moves the state
  directories to `/miroir/pristine/<world>`.
- **Reset** kills every process, restores the state, moves every date forward by the days since
  bake (so the data keeps its age), runs the task's `setup`, starts the apps and waits for quiet.
- **Episode**: the agent acts as a persona through `api_request`, `api_docs`, `run_code` (in a
  separate sandbox), `wait` (lets time pass), the app's own MCP tools, and `submit`. Outbound email
  and HTTP land in local sinks.
- **Grade** folds the change log into net changes and runs the task's checks: SQL on the app's
  database, SQL on the episode database (`actions`, `effects`, `changes`, `answer`), or commands.
  A change outside `allowed_changes` or a failed gate is harm and earns `harm_reward` (0 by
  default). Otherwise the reward is 1 when every check scores 1, else the weighted sum.

## Quick start

With Docker and buildx:

```sh
docker buildx bake acme-crm-env                          # the example env in fixtures/acme-crm
docker run --rm --network none acme-crm-env check        # replay every task's golden run and probes
docker run --rm -d -p 127.0.0.1:8000:8000 -e MIROIR_TOKEN=secret acme-crm-env
curl -H 'Authorization: Bearer secret' http://127.0.0.1:8000/acme-crm/splits
```

## Make an env

Add miroir to the product's image, as in [fixtures/acme-crm/Dockerfile.miroir](fixtures/acme-crm/Dockerfile.miroir):

```dockerfile
FROM ghcr.io/raphaeldkhn/miroir:0.1 AS miroir
FROM your-product-image
USER root
# install the product's datastores, and tini
COPY --from=miroir /miroir /usr/local/bin/miroir
COPY miroir.yaml /miroir/miroir.yaml
COPY tasks/ /miroir/tasks/
RUN miroir bake
EXPOSE 8000
ENTRYPOINT ["tini", "--", "miroir"]
CMD ["run"]
```

`miroir.yaml` says what to run, what to restore and how to call the product:

```yaml
miroir: 1
name: acme-crm
description: Acme CRM, a sales CRM for small teams
processes:                  # datastores first; give each a non-root user
  - {name: db, command: [postgres, -D, /var/lib/postgresql/data], user: postgres, datastore: true,
     ready: {tcp: 5432}}
  - {name: web, command: [node, dist/server.js], user: node, cwd: /app,
     ready: {http: "http://127.0.0.1:3000/healthz"}}
bake:
  - [createdb, -h, 127.0.0.1, -U, postgres, crm]
  - [node, dist/seed.js]
state: [/var/lib/postgresql/data, /app/uploads]
apps:
  crm:
    api: {base_url: "http://127.0.0.1:3000", openapi: /app/openapi.yaml}
    database: {url: "postgres://postgres@127.0.0.1:5432/crm"}
personas:
  sales_manager:
    description: Dana Lee, sales manager at Acme
    headers: {crm: {Authorization: Bearer seed-token-dana}}
sinks: {smtp: 2525, http: 9000}
```

Each file in `tasks/` is a task: an instruction, weighted checks and gates, the changes it may make,
and a golden run that must earn 1.

```yaml
instruction: Charge the card on file for invoice INV-04001.
checks:
  - {name: paid, weight: 1, sql: "SELECT status = 'paid' FROM invoices WHERE number = 'INV-04001'"}
  - {name: only_4001, gate: true, db: episode,
     sql: "SELECT count(*) = 0 FROM changes WHERE tbl = 'invoices' AND json_extract(pk, '$.id') <> 4001"}
allowed_changes:
  - {table: invoices, op: update, columns: [status, paid_at, payment_intent]}
golden:
  - {tool: api_request, arguments: {method: POST, path: /invoices/4001/pay}}
  - {tool: submit}
```

Tasks can also have probes (runs whose reward must stay in bounds), a typed `answer`, `coworkers`
played by a model, and clock `events`. Every field is in the
[manifest](docs/src/reference/manifest.md) and [task](docs/src/reference/task.md) references, and
[fixtures/](fixtures) has full examples for Postgres, MySQL and several apps. Run `miroir check`
until it passes: it replays each golden run, an empty submit, repeats and probes, and checks that
the env idles, resets cleanly and writes only to its state.
Runs that call `run_code` need a sandbox: [sandbox/check.sh](sandbox/check.sh) runs the check
beside one.

## Serve

One container runs one episode at a time; for parallel rollouts, run several.

| Endpoint | |
|---|---|
| `GET /health` | 200 once the env is ready; `leased` says whether a session holds it |
| `GET /ws` | the OpenEnv session: `reset`, `step`, `state`, `record` and `close` frames |
| `POST /mcp` | MCP over JSON-RPC; `?surface=api`, `mcp` (the app's tools) or `code` |
| `GET /metadata`, `/schema`, `/openapi.json` | versions and hashes, frame schemas, routes |
| `GET /list_environments`, `/{name}/splits`; `POST /{name}/tasks` | task listings |

| Variable | |
|---|---|
| `MIROIR_TOKEN` | bearer token every route but `/health` requires; unset, anyone can act |
| `MIROIR_PORT` | port to serve on, 8000 by default |
| `MIROIR_SANDBOX` | `http://HOST:PORT` of a `miroir sandbox` container, which enables `run_code` |
| `MIROIR_LOG_BUDGET_MB` | change-log size past which an episode faults, 1024 by default |
| `MIROIR_ENV_VERSION` | version `/metadata` reports, such as the product image's tag; unset, null |

Run the env on an internal network, since nothing outside the sinks is recorded or blocked.
[python/tests/docker.sh](python/tests/docker.sh) starts an env with its sandbox, and cuts off only
the sandbox: the env stays on Docker's bridge, which publishes its port.

## Train and evaluate

The Python package (`python/`, Python 3.11+) has the client, the adapters and the host-side CLI:

```sh
uv pip install './python[trl]'     # or './python[verifiers]'
```

TRL:

```python
from trl import GRPOConfig, GRPOTrainer
from miroir import read_urls
from miroir.trl import dataset, environment_factory, reward

urls = read_urls("urls.txt")  # as `miroir pool` writes it
trainer = GRPOTrainer(
    model="<model>",
    train_dataset=dataset(urls[0]),
    environment_factory=environment_factory(urls_file="urls.txt"),
    reward_funcs=reward,
    args=GRPOConfig(output_dir="out"),
)
```

verifiers:

```sh
vf-run miroir-verifiers -m <model> --env.taskset.urls_file urls.txt
```

On the host, `miroir` is the Python CLI. Inside the image, it is the Rust binary.

| Command | |
|---|---|
| `miroir pool --size N --urls urls.txt -- COMMAND` | keep N containers healthy, listed in `urls.txt`; `COMMAND start` prints `ID URL`, `COMMAND stop ID` removes one |
| `miroir review TRACES --tasks DIR` | label end states; wrong grades become probes in `DIR/probes/` |
| `miroir calibrate TRACES --model M --out FILE` | class tasks as solved, learnable or out of reach, for `calibration=` |
| `miroir draft` / `miroir derive` | turn scrubbed production traces into tasks |

TRACES are the episode records the adapters save with `traces=` (TRL) or `--env.taskset.traces`.

## Production data

`miroir import` runs inside the image. It restores production dumps, replaces names, emails, phones
and secrets with keyed fakes of the same shape, fails if any original is left, and writes scrubbed
dumps that a `bake` command can load:

```sh
head -c 32 /dev/urandom > in/key
docker run --rm -v "$PWD/in:/in:ro" -v "$PWD/out:/out" acme-crm-env import \
  --dump crm=/in/crm.sql --scrub /in/scrub.yaml --key /in/key --out /out
```

`scrub.yaml` gives every text column a kind: `name`, `email`, `phone`, `secret`, `text`, `blank`
or `keep`. For example, `apps: {crm: {contacts: {name: name, email: email, phone: phone}}}`.

## Develop

```sh
cargo fmt --check && cargo clippy --all-targets --locked -- -D warnings
cargo test --locked                                    # unit tests
cargo test --locked --test integration -- --ignored    # builds the fixture images; needs Docker
(cd python && uv run ruff check . && uv run ty check && uv run pytest -q)   # -m integration: Docker
(cd tests/openenv && uv run pytest -q)                 # OpenEnv conformance, on acme-crm-env
```

| Path | |
|---|---|
| `src/` | the `miroir` binary |
| `python/` | client, TRL and verifiers adapters, host-side CLI |
| `sandbox/` | the image that runs `run_code` commands |
| `fixtures/` | example products and their envs |
| `tests/` | Docker integration tests and OpenEnv conformance |
| `tools/` | reset timing and pool soak benchmarks |
| `docs/` | the documentation, an mdBook |

Licensed under [Apache-2.0](LICENSE).
