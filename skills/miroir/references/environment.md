# Wrap a product as a world

A world image is the product's own image plus Postgres (and any other datastore it uses),
`tini`, the `miroir` binary, a manifest at `/miroir/miroir.yaml`, and a world seeded at
`docker build`. Harbor runs it as a compose service next to the agent's container (`main`); the
agent reaches it only on the ports the manifest exposes.

Two worlds to copy from:
[`fixtures/acme-crm`](https://github.com/raphaelDkhn/miroir/tree/master/fixtures/acme-crm), a
small CRM (Postgres, Redis, a web server, a worker, an email queue, a payment API), and
[`examples/gitea`](https://github.com/raphaelDkhn/miroir/tree/master/examples/gitea), stock Gitea
wrapped with configuration only, one image per world.

## Map the product first

List every long-running process (web servers, workers, schedulers) and datastore, with its
command, port, user and environment; the product's compose file holds most of this. Then find:

- how its data is made: migrations, seed scripts, or a scrubbed dump;
- how its API authenticates, so the world can seed a test account and a credential for it;
- what it sends out (email, webhooks, payment or other HTTP calls), and the base URLs to point
  at the sinks;
- what runs in the background (queues, cron, schedulers), and how to tell when it is done;
- which columns it touches on every request (a token's `last_used_at`): those go in
  `database.ignore`.

## The Dockerfile

`fixtures/acme-crm/Dockerfile.miroir`:

```dockerfile
# Acme CRM as a Miroir world: the product image plus Postgres, Redis, tini and miroir, baked.
ARG PRODUCT_IMAGE=acme-crm:dev
ARG MIROIR_IMAGE=miroir:dev
FROM ${MIROIR_IMAGE} AS miroir

FROM ${PRODUCT_IMAGE}
USER root
RUN apt-get update \
 && apt-get install -y --no-install-recommends postgresql-15 redis-server tini \
 && rm -rf /var/lib/apt/lists/*
ENV PATH=/usr/lib/postgresql/15/bin:$PATH
COPY --from=miroir /usr/local/bin/miroir /usr/local/bin/miroir
COPY miroir.yaml /miroir/miroir.yaml
RUN install -d -o postgres -g postgres /var/lib/postgresql/data \
 && su postgres -c "initdb -D /var/lib/postgresql/data --auth=trust --encoding=UTF8 --locale=C.UTF-8" \
 && chown node:node /app/uploads \
 && miroir bake
ENTRYPOINT ["tini", "--"]
CMD ["miroir", "run"]
```

The rules:

1. Install the datastores and `tini` with the product's own package manager.
2. Copy the binary from the Miroir image (`COPY --from=<miroir image> /usr/local/bin/miroir
   /usr/local/bin/miroir`), then the manifest to `/miroir/miroir.yaml`.
3. Initialize the data directories (`initdb --auth=trust`), then `RUN miroir bake` as the last
   `RUN`. Declare the build arguments the seed needs (such as `ARG WORLD`) right before it, so
   every layer above is shared between worlds.
4. End as `root` with `ENTRYPOINT ["tini", "--"]` and `CMD ["miroir", "run"]`, which replace the
   product's own entrypoint. With `tini` as PID 1, `docker run IMAGE cat /path` works, and zombies
   are reaped.
5. Keep `/bin/sh` in the image: Harbor runs the collect hook through `sh -c`.
6. `database.url` connects as a Postgres superuser, because capture installs triggers and the
   date shift disables them while it runs. The product's own roles must not be superusers: give
   them only the privileges they need, as the fixture's `crm_app` role shows.

Locally the Miroir image is `miroir:dev` (`docker buildx bake` in the Miroir repository). Releases
publish `ghcr.io/raphaeldkhn/miroir:<major>.<minor>`.

## The manifest

`/miroir/miroir.yaml` is YAML. Unknown keys are errors, and every error names the field path and
the fix, such as `processes[2].cwd: 'app' must be an absolute path, such as /app`. Commands are
lists, `[program, arg, ...]`, run without a shell unless you write `[sh, -c, "..."]`.

| Key | Required | Rule |
|---|---|---|
| `expose` | yes | TCP ports, at least one. The only ports that may listen off loopback: the agent's door. |
| `processes` | yes | At least one with `datastore: true`; every datastore comes before the other processes. |
| `processes[].name` | yes | Lower-case letters, digits and `-`; unique. |
| `processes[].command` | yes | An argv. |
| `processes[].user` | no | A user in `/etc/passwd`. Without it the process runs as root. |
| `processes[].cwd` | no | An absolute path. |
| `processes[].env` | no | Variables for this process, over `env`. |
| `processes[].datastore` | no | Started first, before the date shift. |
| `processes[].ready` | no | `{tcp: PORT}` (a connection to 127.0.0.1) or `{http: URL}` (a `GET` on `http://127.0.0.1…` answering 2xx or 3xx). It must pass within 60 s. Leave it out for workers. |
| `env` | no | Layered over the container's environment for every process and every `bake`, `seed` and `quiet` command, at bake, boot and collect. |
| `bake` | no | Commands run at build with only the datastores up: create databases, migrate, restore a dump. One hour each. |
| `seed` | no | Commands run at build with every process up: build the world through the product's own API. One hour each. |
| `database.url` | yes | `postgres://…/<database>`: the graded database, reached as a superuser. |
| `database.ignore` | no | Entries left out of capture: a table, named as the evidence names it (`schema.table` outside `public`), or `table.column`. A primary-key column cannot be ignored; ignore its table. |
| `database.time.exclude` | no | `table.column` entries never shifted, such as birthdays. |
| `database.time.epoch` | no | A map of `table.column` globs (`*` for any run of characters) to `s` or `ms`: `integer` or `bigint` columns holding Unix time, shifted like timestamps. Each glob must match a column. |
| `database.time.captured` | no | An RFC 3339 timestamp: when the data the `bake` commands restore was captured ([From production](#from-production)). |
| `sinks.smtp` | no | Port of the loopback SMTP recorder. |
| `sinks.http` | no | Port of the loopback HTTP recorder; not the SMTP port. |
| `sinks.responses` | no | Canned HTTP answers ([Sinks](#sinks)). |
| `quiet` | no | An argv that exits 0 when background work is done ([Quiet](#quiet)). |

The fixture's
[`miroir.yaml`](https://github.com/raphaelDkhn/miroir/blob/master/fixtures/acme-crm/miroir.yaml)
uses every key but `database.time.captured`:

```yaml
expose: [3000]
processes:
  - {name: db, command: [postgres, -D, /var/lib/postgresql/data, -c, listen_addresses=127.0.0.1],
     user: postgres, datastore: true, ready: {tcp: 5432}}
  - {name: cache, command: [redis-server, --dir, /var/lib/redis, --bind, 127.0.0.1],
     user: redis, datastore: true, ready: {tcp: 6379}}
  - name: web
    command: [node, dist/server.js]
    user: node
    cwd: /app
    env: {DATABASE_URL: "postgres://crm_app@127.0.0.1:5432/crm", HOST: 0.0.0.0}
    ready: {http: "http://127.0.0.1:3000/healthz"}
  - {name: worker, command: [node, dist/worker.js], user: node, cwd: /app,
     env: {DATABASE_URL: "postgres://crm_app@127.0.0.1:5432/crm"}}
env:
  DATABASE_URL: postgres://postgres@127.0.0.1:5432/crm
  REDIS_URL: redis://127.0.0.1:6379
  SMTP_URL: smtp://127.0.0.1:2525
  STRIPE_API_BASE: http://127.0.0.1:9000/stripe
  SCHEDULER_ENABLED: "false"
bake:
  - [createdb, -h, 127.0.0.1, -U, postgres, crm]
  - [node, dist/migrate.js]
  - [node, dist/seed.js]
  - [psql, -h, 127.0.0.1, -U, postgres, -d, crm, -v, ON_ERROR_STOP=1, -c,
     "CREATE ROLE crm_app LOGIN; GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO crm_app;
      GRANT USAGE ON ALL SEQUENCES IN SCHEMA public TO crm_app"]
seed:
  - [node, -e, "fetch('http://127.0.0.1:3000/deals/1100/notes', {method: 'POST',
     headers: {authorization: 'Bearer seed-token-dana', 'content-type': 'application/json'},
     body: JSON.stringify({text: 'Seeded through the API'})}).then((r) => process.exit(r.ok ? 0 : 1))"]
database:
  url: postgres://postgres@127.0.0.1:5432/crm
  ignore: [api_tokens.last_used_at]
  time:
    exclude: [contacts.birthday]
    epoch: {deals.synced_unix: s}
sinks:
  smtp: 2525
  http: 9000
  responses:
    "POST /stripe/v1/payment_intents": {status: 200, json: {id: pi_test, object: payment_intent, status: succeeded}}
quiet: [sh, -c, 'test "$(redis-cli -u "$REDIS_URL" llen bull:email:wait)$(redis-cli -u "$REDIS_URL" llen bull:email:active)" = 00']
```

## What runs when

### Bake: `RUN miroir bake`, at `docker build`

1. Load the manifest; refuse an image that is already baked (`/miroir/bake.json` exists).
2. Start the sinks, answering but recording nothing, so mail and webhooks sent while seeding
   succeed.
3. Start the datastores in order, each until its ready probe passes, then wait until
   `database.url` answers.
4. Run each `bake` command, with the build's environment (so Docker `ARG`s reach it) under the
   manifest `env`.
5. With `database.time.captured` set, move the restored data from that time to the bake's
   start.
6. Start the other processes in order until ready, then wait for quiet.
7. Run each `seed` command, then wait for quiet. A process that exited fails the bake.
8. Install change capture on every table not ignored: a `miroir` schema holding the change log,
   a row trigger `miroir_capture` and a statement trigger `miroir_truncate` on each table. A
   captured column named `_op` or `_change` is refused: the grader adds those names.
9. Stop every process in reverse order: SIGTERM, then SIGKILL after 30 s.
10. Refuse a bake that ran past midnight UTC: each boot moves dates from the bake's day.
11. Write `/miroir/bake.json`, create the empty `/miroir/evidence/`, and make `/miroir` private
    to root (mode 0700).

A failure prints `bake step N, <what>: <why and the fix>` and exits 1, which fails
`docker build`.

### Boot: `miroir run`, once per trial

1. Read `/miroir/bake.json`.
2. Replace `/miroir/run/` with an empty directory, so no stale ready marker survives.
3. Start the sinks, recording to `/miroir/run/effects.jsonl` (and faults to `faults.jsonl`).
4. Start the datastores until ready, then wait until the database answers.
5. Shift dates from the bake to today ([Dates](#dates)).
6. Start the other processes until ready, then wait for quiet.
7. Clear the change log and the journals (start-up writes and mail are not the agent's), and
   record the tables at the start of the episode in `/miroir/run/catalog.json`. A process that
   exited fails the boot.
8. Audit the listeners ([The listener rule](#the-listener-rule)).
9. Write `/miroir/run/ready`, which the compose healthcheck tests, and log the step timings:
   `booted in 0.8 s: datastores 0.2, shift 0.0, apps 0.4, quiet 0.2` for the fixture.
10. Supervise. A process that exits is recorded as a `crash` fault and the world keeps serving.
    Every child gets `oom_score_adj` 1000, so under memory pressure the kernel kills a product
    process (a recorded crash) before `miroir` itself.
11. On SIGTERM, stop the processes in reverse order and exit 0.

A failure in steps 1 to 8 prints `boot step N, <what>: <why and the fix>` and exits 1. The
container stops, `main` never starts, and Harbor records an environment error before any agent
runs.

### Collect: Harbor's collect hook, after `main` has stopped

Harbor runs `docker compose exec <service> sh -c 'miroir collect'` as root with an almost empty
environment, so collect applies the manifest `env` itself.

1. Fail if `/miroir/run/ready` is missing: the world never finished booting.
2. Wait for quiet. A timeout is recorded as a `quiet_timeout` fault.
3. Read the change log and the live table list in one consistent read. A database that does not
   answer is a `database` fault, and change-log text over 64 MiB is a `log_budget` fault; either
   way the evidence holds no changes.
4. Fold the log into net changes: one per changed row, with the row as the episode found it and
   as it left it. A table created, dropped or reshaped since boot, or one whose capture trigger
   is gone, is a `schema` change; a re-created table is a `truncate`.
5. Read the effects and faults journals. An effects journal over 16 MiB is left unread and
   recorded as an `evidence_budget` fault giving its size.
6. Write `/miroir/evidence/evidence.db` through a temporary file and a rename. Evidence over
   16 MiB is written again with no changes and no effects, plus an `evidence_budget` fault.
7. Print a summary, such as `evidence: 1 changes, 1 effects, 36864 bytes; faults: []`, and exit 0.

Collect exits 1 only for Miroir's own failures (the world never booted, the fold failed, no file
could be written); the grader then finds no evidence and the trial voids. Faults never fail
collect: they are harm, measured.

## The listener rule

At boot, after every process is ready, Miroir reads every TCP listener in the world's network
namespace (`/proc/net/tcp` and `/proc/net/tcp6`):

- a listener off loopback (not `127.0.0.0/8` or `::1`) on a port outside `expose` fails the
  boot: `port 5432 (db) listens on 0.0.0.0: bind it to 127.0.0.1, or list it in expose`;
- an `expose` port with no listener off loopback fails the boot:
  `expose lists 3000 (web), but nothing listens on it off loopback: check the product's bind address`.

The name in parentheses is the process whose ready probe uses the port. Bind datastores (which
use trust authentication) and everything else to `127.0.0.1`, and the product's API to
`0.0.0.0` on its exposed port. A runtime that runs each compose service as a VM also lists the
VM's own daemons there, so such runtimes fail this audit and are not supported.

## Dates

A world is seeded once, at build, but a task says "overdue" or "stale for 30 days". So every
boot moves the database's dates from the bake to the trial's day:

- `timestamp` and `timestamptz` columns, and the Unix-time columns of `database.time.epoch`, move
  by whole elapsed days;
- `date` columns move by UTC calendar days;
- `database.time.exclude` columns, generated columns, values on or after `9000-01-01` (such as
  `9999-12-31` for "never"), and Unix times of 0 or less stay;
- the shift runs with triggers disabled, so a product trigger that stamps `updated_at` does not
  fire.

For part of each day the two numbers differ: an image baked at 23:00 UTC and booted at 01:00 the
next day moves dates by 1 and timestamps by 0. Checks never compare a date column with a
timestamp column (authoring rule 5 in [Write tasks](https://raphaeldkhn.github.io/miroir/tasks.html#authoring-rules)).
If a constraint refuses a moved value, the boot fails naming the table: list the column in
`database.time.exclude`.

`database.time.captured` serves restored data: bake moves the restored rows from that time to
the bake's start, so a dump taken weeks before the build keeps its relative ages. The seed's
writes come after and are not moved. A captured time later than the bake's start is refused.

## Sinks

The product cannot reach the internet (its compose network is `internal: true`), so point its
outbound mail and HTTP at loopback recorders:

- `sinks.smtp` records each email: sender, `to` and `cc` recipients, the envelope recipients
  missing from the headers as `bcc`, subject, text and HTML. Point the product's SMTP settings
  at `127.0.0.1:<port>` (the fixture sets `SMTP_URL=smtp://127.0.0.1:2525`).
- `sinks.http` records each request: method, path, query parameters, headers (names lower-case,
  `authorization` dropped) and body (JSON as JSON, other text as a JSON string, an empty body as
  `null`). Point base URLs at `http://127.0.0.1:<port>` (the fixture sets
  `STRIPE_API_BASE=http://127.0.0.1:9000/stripe`; Gitea allows webhooks to `127.0.0.1:9000`).
- `sinks.responses` answers requests with canned responses, keyed `"[METHOD ]/path[*]"`, each
  `{status, json | body, headers}`. An exact path beats a prefix (a key ending in `*`), a longer
  prefix beats a shorter one, and a key with a method beats one without. Unmatched requests get
  200 with an empty body.

An email over 25 MiB is recorded truncated, and a request body over 25 MiB is refused; both add a
`sink` fault. The sinks answer during the bake but record only from the boot, and the boot clears
what start-up sent.

## Quiet

Products do work in the background: a queued email, a webhook delivery, a search index. `quiet`
is a command that exits 0 when that work is done. Miroir waits until it passes twice in a row,
within 30 s, at bake, at boot and at collect, so the evidence holds what the agent's requests
set off. At bake and boot a timeout fails; at collect it is a `quiet_timeout` fault.

The fixture checks two Redis queue lengths; Gitea runs
`gitea manager flush-queues`. Turn schedulers and cron off in `env`
(`SCHEDULER_ENABLED: "false"`), so nothing changes while the world idles: the null run must
change nothing.

## Security model

The agent is untrusted and runs as root in `main`.

- **Network.** `main` sits on Harbor's default network (for its model API) and on `world`; the
  world service sits only on `world`, which is `internal: true`. The product cannot reach the
  internet, and its mail and webhooks land in the sinks.
- **Reachable ports.** Over TCP, `main` reaches only the `expose` ports. Datastores, sinks and
  every other TCP listener are on loopback, and the boot's audit refuses a world that breaks this.
- **Evidence integrity.** Capture happens below the API, in triggers the superuser owns. The
  agent never shares a container with the datastores, the change log or the journals. A root
  process writes the evidence after Harbor has stopped `main`. The product's roles cannot read
  `/miroir` (mode 0700), and a disabled capture trigger shows as a `schema` change, which is
  harm.
- **The spec never reaches `main`.** It lives in `[verifier.env]`, which only the verifier
  receives, and `main` holds no grader, tests or Miroir binary.
- **Untrusted grader inputs.** Only the first 64 KiB of a regular answer file are read. Every
  SQL statement of the spec runs under a fixed budget of SQLite steps, not a clock, so an agent
  cannot turn a scored trial into a void and grading never depends on machine load. The grader
  runs in its own container.
- **Credentials.** The task's credential is test data, valid only inside one trial's private
  network, and written in `instruction.md` so that every harness sees it.
- **Not a boundary.** Identity is not a security boundary: the agent may use any credential it
  finds, and scope measures every change, whoever made it. What the product can reach through
  its own features (SSRF) is limited by its own configuration, such as Gitea's
  `ALLOWED_HOST_LIST`.

## Debug a world

Boot it in the foreground; the log shows each process's output and the boot's timing line.
Ctrl-C stops it:

```bash
docker run --rm -p 3000:3000 acme-crm-env:dev
```

Try a manifest change without rebuilding by mounting it over the baked one. It works for what
acts at boot (processes, `env`, `expose`, sinks, `quiet`, dates); `bake` and `seed` changes need
a rebuild:

```bash
docker run --rm -v "$PWD/miroir.yaml:/miroir/miroir.yaml:ro" acme-crm-env:dev
```

Read a file from the image, collect from a running world as Harbor does, and read the evidence:

```bash
docker run --rm acme-crm-env:dev cat /miroir/bake.json
docker exec <container> sh -c 'miroir collect'
docker cp <container>:/miroir/evidence/evidence.db .
sqlite3 evidence.db 'SELECT tbl, op, pk, columns FROM changes'
```

Open a shell in `main` with the world running, as an agent sees it (Harbor's
[`harbor task start-env`](https://docs.harborframework.com/tutorials/create-a-task)):

```bash
harbor task start-env -p fixtures/acme-crm/tasks/pay-invoice
```

## Failure messages

| Message | Fix |
|---|---|
| `<field path>: <problem>` at bake or boot | The manifest: the message names the field and the fix. |
| `bake step 1, /miroir/bake.json: this image is already baked` | Build on the product's image, not on a world image. |
| ``this image was not baked: add `RUN miroir bake` to its Dockerfile`` | Make `miroir bake` the last `RUN`. |
| `bake step 3, datastores: …` or `boot step 4, datastores: …` | A datastore never became ready, or `database.url` does not reach it as a superuser. |
| ``bake step 4, bake command `…`: `…` exited with status 1; its output is above`` | The command failed: its output is above the message. |
| `process '<name>' was not ready within 60 s: … never passed` | Check the process's command, its output and its ready probe. |
| `process '<name>' exited with status 1 before it was ready` | The process failed at start: its output is above the message. |
| `boot step 7, process '<name>': it exited during boot (status 1)` | A worker exited once started: its output is above. |
| `the quiet command … did not pass twice in a row within 30 s` | Background work never settles: fix the command, or turn the scheduler off. |
| `boot step 5, date shift: …: if a constraint refuses a moved value, list its column in database.time.exclude` | As it says. |
| `database.time.epoch: '<glob>' matches no integer or bigint column` | Name the table as the evidence does, with `*` for any run of characters. |
| `database.ignore: no table or column '<entry>'` | Name a table as the evidence does, or a column as `table.column`. |
| `port 5432 (db) listens on 0.0.0.0: bind it to 127.0.0.1, or list it in expose` | Bind the process to loopback. |
| `expose lists 3000 (web), but nothing listens on it off loopback` | Make the product listen on `0.0.0.0` (the fixture sets `HOST: 0.0.0.0`). |
| `bake step 10, the clock: the bake ran past midnight UTC` | Build again. |
| `the world never finished booting` (from collect) | The trial voids: the world container's log says why the boot stopped. |

## From production

A production snapshot becomes a world with no Miroir import or scrub code:

1. Scrub the dump before it leaves the customer's network. Miroir does not scrub.
2. Restore it in a `bake` command (`pg_restore`), and set `database.time.captured` to the time
   the dump was taken. Bake moves every time column from then to the bake's start, so relative
   ages hold however late the image is built, and each boot then moves the world to the trial's
   day, as for any world.
3. Build one image per snapshot. Put the `COPY` of the dump and `RUN miroir bake` last in the
   Dockerfile, so the product and datastore layers are shared and each snapshot adds one layer.
4. A private snapshot needs a private image: see where images must be public in
   [Validate and publish](https://raphaeldkhn.github.io/miroir/validate.html#where-images-must-be-public).
5. Turn a ticket into a task. The ticket becomes `instruction.md` (the request in the requester's
   words, and the account the agent acts as), and the API calls that resolved it become
   `solution/solve.sh` (`curl -fsS`). Run the oracle with `-k 2 -n 1`, then `miroir draft` the
   two trial directories, paste the draft into `MIROIR_SPEC`, and review it: drop checks the
   instruction does not ask for, turn invariants into gates, and add probes. Then run the task's
   `check.yaml`. [Write tasks](https://raphaeldkhn.github.io/miroir/tasks.html#draft-a-spec-from-reference-runs)
   shows the commands.
