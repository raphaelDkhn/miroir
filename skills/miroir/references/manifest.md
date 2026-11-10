# The image and the manifest

## The image

An env is the product's own image with the `miroir` binary, a manifest and a tasks directory
added. Miroir runs every process of the product itself, so the datastores go in the same image as
the apps.

```dockerfile
ARG PRODUCT_IMAGE=ghcr.io/acme/crm:1.4.0
FROM ghcr.io/raphaeldkhn/miroir:0.1 AS miroir

FROM ${PRODUCT_IMAGE}
ARG PRODUCT_IMAGE
USER root
RUN apt-get update \
 && apt-get install -y --no-install-recommends postgresql-15 tini \
 && rm -rf /var/lib/apt/lists/*
ENV PATH=/usr/lib/postgresql/15/bin:$PATH MIROIR_ENV_VERSION=${PRODUCT_IMAGE}
COPY --from=miroir /miroir /usr/local/bin/miroir
COPY miroir.yaml /miroir/miroir.yaml
COPY tasks/ /miroir/tasks/
RUN install -d -o postgres -g postgres /var/lib/postgresql/data \
 && su postgres -c "initdb -D /var/lib/postgresql/data --auth=trust" \
 && miroir bake
EXPOSE 8000
ENTRYPOINT ["tini", "--", "miroir"]
CMD ["run"]
```

- `ghcr.io/raphaeldkhn/miroir` holds one static binary, `/miroir`. Tags are `X.Y.Z` and `X.Y`.
- Install the product's datastores and an init. Without `apt`, copy `tini-static` from a Debian
  stage, as `fixtures/mini-shop/Dockerfile.miroir` does.
- Initialize each datastore's data directory, owned by the user it runs as. For MySQL:
  `mysqld --initialize-insecure --user=mysql --datadir=...`, with an `--init-file` that creates
  the users.
- `miroir bake` reads `/miroir/miroir.yaml`, validates the tasks, seeds the data and keeps the
  state.
- The entrypoint is `miroir` under `tini` (or `dumb-init`, `catatonit`, `docker run --init`): a
  reset kills every process but Miroir and PID 1.
- `MIROIR_ENV_VERSION`, such as the product image's tag, is what `/metadata` reports.

Files under `/miroir`: `miroir.yaml`, `tasks/` (with probes in `tasks/probes/`),
`pristine/<world>/` (the baked state), `bake.json`, and at run time `shifted/<world>/` and
`episode.db`.

## Example manifest

```yaml
miroir: 1
name: acme-crm
description: Acme CRM, a sales CRM for small teams
processes:                          # started in this order, datastores first
  - {name: db, command: [postgres, -D, /var/lib/postgresql/data], user: postgres,
     datastore: true, ready: {tcp: 5432}}
  - {name: cache, command: [redis-server, --dir, /var/lib/redis, --bind, 127.0.0.1],
     user: redis, datastore: true, ready: {tcp: 6379}}
  - {name: web, command: [node, dist/server.js], user: node, cwd: /app,
     env: {DATABASE_URL: "postgres://crm_app@127.0.0.1:5432/crm"},
     ready: {http: "http://127.0.0.1:3000/healthz"}}
  - {name: worker, command: [node, dist/worker.js], user: node, cwd: /app,
     env: {DATABASE_URL: "postgres://crm_app@127.0.0.1:5432/crm"}}
env:                                # for every process and command
  DATABASE_URL: postgres://postgres@127.0.0.1:5432/crm
  SMTP_URL: smtp://127.0.0.1:2525
  STRIPE_API_BASE: http://127.0.0.1:9000/stripe
  SCHEDULER_ENABLED: "false"
bake:                               # only the datastores run
  - [createdb, -h, 127.0.0.1, -U, postgres, crm]
  - [node, dist/migrate.js]
  - [node, dist/seed.js]
  - [psql, -h, 127.0.0.1, -U, postgres, -d, crm, -v, ON_ERROR_STOP=1, -c,
     "CREATE ROLE crm_app LOGIN; GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO crm_app;
      GRANT USAGE ON ALL SEQUENCES IN SCHEMA public TO crm_app"]
state: [/var/lib/postgresql/data, /var/lib/redis, /app/uploads]
apps:
  crm:
    api: {base_url: "http://127.0.0.1:3000", openapi: /app/openapi.yaml}
    database:
      url: postgres://postgres@127.0.0.1:5432/crm
      ignore: [api_tokens.last_used_at]
personas:
  sales_manager:
    description: Dana Lee, sales manager at Acme (dana@acme.test)
    headers: {crm: {Authorization: Bearer seed-token-dana}}
sinks:
  smtp: 2525
  http: 9000
  responses:
    "POST /stripe/v1/payment_intents": {status: 200, json: {id: pi_test, status: succeeded}}
quiet:
  command: [sh, -c, 'test "$(redis-cli llen bull:email:wait)$(redis-cli llen bull:email:active)" = 00']
```

## What bake and reset do

Bake, for each world: start the datastores and wait until each database answers; run the `bake`
commands; start the apps and wait for quiet; run the `seed` commands and wait for quiet; install
the triggers that log row changes; stop everything and move the state to
`/miroir/pristine/<world>`.

Reset: kill every process; restore the state, copying only the files that changed, and empty
`/tmp`, `/var/tmp` and `/dev/shm`; start the datastores and move every date forward by the time
since bake; run the task's `setup` SQL; start the apps and wait for quiet; clear the change log
and the episode database.

## Fields

The manifest is `/miroir/miroir.yaml`. Unknown fields are errors. A command is a list of
strings, `[program, arg, ...]`, run without a shell unless you call one.

### Top level

| Field | Default | What |
|---|---|---|
| `miroir` | required | format version; must be `1` |
| `name` | required | letters, digits, `.`, `_` and `-`; the env's name in routes |
| `description` | required | what the product is; `/metadata` shows it |
| `processes` | required | what Miroir starts, in order |
| `env` | `{}` | variables for every process and command Miroir runs |
| `bake` | `[]` | commands that create the seed data, with only the datastores up |
| `seed` | `[]` | commands run at bake once the apps are up, such as calls to the API |
| `worlds` | `[default]` | the [worlds](#worlds), each baked apart |
| `state` | derived | the directories each reset restores |
| `apps` | required | the apps, by name |
| `personas` | required | the users the agent can act as, by name |
| `sinks` | none | the [sinks](#sinks) |
| `quiet` | none | the command that tells when background work is done |
| `time` | `anchor: bake` | how the data's [dates](#dates) follow the clock |
| `system_prompt` | none | a system prompt the task listings carry, for the adapters |

`bake` and `seed` commands get `MIROIR_WORLD`, and may run for up to an hour each. None of the
commands or processes sees `MIROIR_TOKEN`.

### `processes[]`

| Field | Default | What |
|---|---|---|
| `name` | required | unique |
| `command` | required | the command to run |
| `user` | root | the user it runs as; root fails `check` unless written `user: root` |
| `cwd` | none | an absolute working directory |
| `env` | `{}` | variables for this process |
| `datastore` | `false` | started first, and alone while a reset restores and moves the data |
| `ready` | none | `tcp: PORT` (a loopback port that accepts connections) or `http: URL` (answers 2xx or 3xx), and `timeout_s` (60 by default, up to 86,400) |

Datastores come before every other process, and there is at least one. Miroir waits for each
`ready` probe before it starts the next process.

### `state`

Absolute directories or files, not nested in each other and not inside `/miroir`: the
datastores' data, uploads, caches. Each reset restores them. Left out, the state is every path the
processes' users can write, and every process then needs a non-root `user`. A file that changes
outside the state lets one episode see another: `check` fails, and a served env refuses resets.

### `apps.<name>`

App names are lowercase letters, digits, `_` and `-`, starting with a letter or a digit. Each
app has an `api`, an `mcp` server, a `database`, or several. At least one app has an `api`, and
at least one has a `database`.

| Field | What |
|---|---|
| `description` | what the app is; required when several apps have an `api` |
| `api.base_url` | the API's `http://` URL on loopback, where the agent's requests go |
| `api.openapi` | the absolute path of its OpenAPI spec, JSON or YAML, which `api_docs` serves |
| `mcp.url` | the `http://` URL on loopback of the app's MCP server; one app at most |
| `database.url` | a `postgres://`, `postgresql://` or `mysql://` URL that names a database |
| `database.schemas` | the schemas to read; all of them when unset |
| `database.ignore` | tables, or `table.column`s, whose changes never count, such as a `last_used_at` |
| `database.time.exclude` | `table.column`s whose times never move, such as birthdays |
| `database.time.epoch` | `table.column` globs of integer Unix-time columns, each with its unit, `s` or `ms` |

The database user must be able to create triggers. A key column can't be ignored, and an
`epoch` pattern must match a 32- or 64-bit integer column.

### `personas.<name>`

| Field | What |
|---|---|
| `description` | who the agent is, as the prompt says |
| `headers` | headers by app, pinned on every request to that app's API or MCP server |

Pinned headers win over the agent's. When a persona pins headers for an app, the agent's own
`Cookie` header to that app is dropped. Tasks pick a persona; with one persona, every task uses
it. Seed the credential the headers carry, such as an API token row, in `bake`.

### Sinks

The sinks stand in for the outside world: a local SMTP server and HTTP server, both on
127.0.0.1, that record each email and call as an effect checks can query. Point the product's
outbound settings at them through `env`.

| Field | What |
|---|---|
| `smtp` | the SMTP sink's port; it accepts every message, with or without login |
| `http` | the HTTP sink's port, not the same as `smtp`; it answers 200 and `{}` unless a response matches |
| `responses` | canned answers by `/path` or `METHOD /path`; a trailing `*` matches a prefix |

A canned answer has `status` (200 by default), `json` or `body` (not both), and `headers`. An
exact path beats a prefix, a longer prefix beats a shorter one, and a key with a method beats one
without. A message can be up to 25 MiB with up to 1,000 recipients; an episode's effects can take
up to 1 GiB. Calls the product makes past the sinks are neither recorded nor blocked.

### `quiet`

| Field | Default | What |
|---|---|---|
| `command` | required | exits 0 once the apps' background work is done |
| `timeout_s` | 30 | how long to wait for two passes in a row, up to 86,400 |

Grading waits for quiet so queued work lands first. Without `quiet`, Miroir waits one second.

### Dates

Each reset moves every date and timestamp column of every app database forward by the time since
the anchor, so the data keeps its age: timestamps by whole days, keeping their time of day, and
dates by calendar days (UTC). Values from `9000-01-01` on, and `database.time.exclude` columns,
stay. Integer Unix-time columns move only when listed in `database.time.epoch`.

| `time` field | Default | What |
|---|---|---|
| `anchor` | `bake` | when the seed data is dated: `bake`, or an RFC 3339 instant for data written for a fixed day |
| `hook` | none | a command each reset runs after moving the databases, before the apps start, to move dates elsewhere (files, a search index); it gets `MIROIR_SHIFT_DAYS` and `MIROIR_SHIFT_DATE_DAYS`, and a failing hook fails the reset |
| `captured` | `{}` | when each captured world's snapshot was taken, by world; bake moves its dates from then to the anchor |

An env with a `hook` can't have clock tasks or captured worlds.

### Worlds

A world is a seed snapshot; most envs have one, `default`. With `worlds: [north, south]`, bake
seeds each in turn from the same unbaked state, and the `bake` and `seed` commands branch on
`MIROIR_WORLD`. Every task then names its `world`. A world's tasks are all `test`, or none are, so
test scores come from data training never saw. `check --world north` checks one world.

### Several apps

```yaml
apps:
  east:
    description: the East office's CRM
    api: {base_url: "http://127.0.0.1:3000", openapi: /app/openapi.yaml}
    database: {url: "postgres://postgres@127.0.0.1:5432/east"}
  west:
    description: the West office's CRM
    api: {base_url: "http://127.0.0.1:3001"}
    database: {url: "postgres://postgres@127.0.0.1:5432/west"}
personas:
  sales_manager:
    description: Dana Lee in the East office, Alex Kim in the West
    headers:
      east: {Authorization: Bearer seed-token-dana}
      west: {Authorization: Bearer seed-token-alex}
```

When several apps have an API, `app` is required in `api_request` and `api_docs` calls, golden
and probe steps, and event and reply requests; each such app needs a `description`. When several
have a database, `app` is required in `sql` checks, `allowed_changes` entries and coworker
inboxes. Two apps can't name the same database (give the second only an `api`), two MySQL
databases on one server can't be told apart, only one app may have an MCP server, and a task's
`setup` can't be used with several databases.

### The product's MCP server

`apps.<name>.mcp.url` names the product's own MCP server, plain `http://` on loopback. Clients on
the `mcp` surface get its tools, plus `submit` and, for clock tasks, `wait`, in place of
`api_request` and `api_docs`. Its tools can't be named `api_request`, `api_docs`, `submit`,
`run_code`, `wait` or `coworker_reply`. `check` fails its `mcp` stage if the server lists no
tools.

### The code sandbox

`run_code` runs the agent's bash in a separate container, the sandbox
(`ghcr.io/raphaeldkhn/miroir-sandbox`: Debian with `python3`, `requests`, `curl` and `jq`), which
reaches each app's API as the persona at `$API_<APP>` (the app name in capitals, `-` as `_`). The
env enables it when `MIROIR_SANDBOX` is set to the sandbox's `http://HOST:PORT`; nothing in the
manifest. Put the env and its sandbox alone on an internal network:

```sh
docker network create --internal -o com.docker.network.bridge.inhibit_ipv4=true ep1
docker run -d --rm --name ep1-sandbox --network ep1 --network-alias sandbox \
  --read-only --tmpfs /work:exec,size=256m --tmpfs /tmp:exec,size=256m \
  --pids-limit 256 --memory 1g --security-opt no-new-privileges \
  ghcr.io/raphaeldkhn/miroir-sandbox:0.1
docker create --rm --name ep1 -p 127.0.0.1::8000 -e MIROIR_TOKEN \
  -e MIROIR_SANDBOX=http://sandbox:8000 my-env
docker network connect ep1 ep1
docker start ep1
```

A call runs for up to 60 s, returns up to 64 KiB, and the code's requests count in grading like
the agent's own (`via: run_code` in `actions`).

## Production dumps

`miroir import` runs inside the env image (which has the datastores). It restores plain SQL
dumps, replaces names, emails, phones and secrets with keyed fakes of the same shape, fails if any
original is left, and writes scrubbed dumps a `bake` command can load.

```sh
mkdir -p in out
head -c 32 /dev/urandom > in/key
docker run --rm -v "$PWD/in:/in:ro" -v "$PWD/out:/out" my-env import \
  --dump crm=/in/crm.sql --scrub /in/scrub.yaml --key /in/key --out /out
```

- `--dump APP=FILE`, one per app: `pg_dump` without `--create`, or `mysqldump` without
  `--databases`. Mount inputs and output outside the state directories and `/tmp`.
- `--key FILE`, at least 32 bytes. Under one key an original always gets the same fake, so rows
  that matched still match. Keep it secret and out of the image.
- `--traces FILE` scrubs production agent traces too (see `references/tasks.md`).
- The scrub spec gives every text column a kind, by app and table. The import names the columns
  you missed; list every column that is not a number, a truth value, a timestamp or a UUID.

```yaml
apps:
  crm:
    contacts: {name: name, email: email, phone: phone, notes: text}
    users: {name: name, email: email, password_hash: secret}
    deals: {title: keep, stage: keep}
fields:                  # trace argument fields to scrub everywhere they appear
  contact_email: email
```

| Kind | Becomes |
|---|---|
| `name` | each word replaced by a fake word, keeping its case |
| `email` | a fake address, such as `3f9a1c2e4b7d8a90@1c2e4b7d.example` |
| `phone` | fake digits in the same format |
| `secret` | a fake secret |
| `text` | free text with its emails, and the known names, phones and secrets, replaced; also takes JSON and arrays |
| `blank` | NULL |
| `keep` | unchanged |

The output is `<app>.sql` per app, `traces.jsonl` with `--traces`, and `report.json`. Copy the
scrubbed dump into the image, load it at bake, and declare when it was taken so bake moves its
dates to the anchor:

```yaml
bake:
  - [createdb, -h, 127.0.0.1, -U, postgres, crm]
  - [psql, -h, 127.0.0.1, -U, postgres, -d, crm, -v, ON_ERROR_STOP=1, -f, /seed/crm.sql]
time:
  captured: {default: "2026-03-02T00:00:00Z"}
```

With several worlds, a `bake` command branches on `MIROIR_WORLD` to load each world's dump.
