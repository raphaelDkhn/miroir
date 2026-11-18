---
title: "Wrap a product"
description: "Turn your product's container image into a world image that Harbor can run and Miroir can grade."
---

A world image is your product's own image with a few things added: Postgres (and any other
datastore the product uses), `tini`, the `miroir` binary, and a manifest at `/miroir/miroir.yaml`.
Its data, the world, is seeded once at `docker build`, and every trial boots a fresh copy of it.

Harbor runs the world image as a compose service next to the agent's container, `main`. The agent
reaches it only on the ports the manifest exposes.

Two working worlds to copy from:

- [`fixtures/acme-crm`](https://github.com/raphaelDkhn/miroir/tree/master/fixtures/acme-crm), a
  small CRM with Postgres, Redis, a web server, a worker, an email queue and a payment API. This
  page uses it as the example.
- [`examples/gitea`](https://github.com/raphaelDkhn/miroir/tree/master/examples/gitea), stock
  Gitea wrapped with configuration only, one image per world.

## Map the product

Start by listing every long-running process (web servers, workers, schedulers) and every
datastore, with its command, port, user and environment. The product's compose file usually holds
most of this. Then find out:

- **Where its data comes from**: migrations, seed scripts, or a scrubbed dump.
- **How its API authenticates**, so the world can seed a test account and a credential for it.
- **What it sends out**: email, webhooks, payment or other HTTP calls, and the base URLs that
  will point them at Miroir's recorders.
- **What runs in the background**: queues, cron, schedulers, and how to tell when they are done.
- **Which columns change on every request**, such as a token's `last_used_at`. Those go in
  `database.ignore`; otherwise every request that touches them shows up as a change.

## Write the Dockerfile

The world's Dockerfile starts from the product's image and adds the rest. The fixture's
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

To adapt it to another product, keep four things:

1. **`RUN miroir bake` is the last `RUN`.** Declare the build arguments the seed needs (such as
   `ARG WORLD`) right before it, so every layer above is shared between worlds.
2. **The image ends as `root`, with `tini` as PID 1** in place of the product's entrypoint. That
   way `docker run IMAGE cat /path` works, and zombie processes are reaped.
3. **`/bin/sh` stays in the image.** Harbor runs the collect hook through `sh -c`.
4. **`database.url` connects as a Postgres superuser**, because change capture installs triggers
   and the date shift disables them while it runs. The product's own roles must not be
   superusers: give them only the privileges they need, as the fixture's `crm_app` role shows.

Locally the Miroir image is `miroir:dev`, which `docker buildx bake` builds in the Miroir
repository. Releases publish `ghcr.io/raphaeldkhn/miroir:<major>.<minor>`.

## Write the manifest

The manifest, `/miroir/miroir.yaml`, tells Miroir how to run the product: which processes to
start, how to seed the world, which database to grade, and where outgoing mail and HTTP calls go.
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

From top to bottom:

- **`expose`** lists the ports the agent may reach: the CRM's API, on 3000. Only these ports may
  listen off loopback, so the datastores bind to `127.0.0.1` and the web server to `0.0.0.0`.
- **`processes`** start in order, datastores first. Each server has a `ready` probe that must pass
  before the next process starts. Workers have none.
- **`env`** applies to every process and command. It points the product at the datastores, at the
  SMTP recorder on port 2525 and at the HTTP recorder on 9000. It also turns the scheduler off,
  so nothing changes while the world sits idle.
- **`bake`** commands run at build with only the datastores up: create the database, migrate,
  seed, and create the product's own role.
- **`seed`** commands run at build with every process up, to build data through the product's own
  API.
- **`database`** names the graded database and the column left out of capture. `time` names a
  column whose dates never move, and an integer column that holds Unix time.
- **`sinks`** are the recorders for outgoing mail and HTTP. `responses` gives the payment API a
  canned answer.
- **`quiet`** passes once the email queues are empty, so Miroir knows background work is done.

The [manifest reference](/manifest) lists every key and its rules, and explains how dates, sinks
and quiet work.

## Build and boot

The bake runs inside `docker build`, so building the image seeds the world, and a failed bake
fails the build. In the Miroir repository, `docker buildx bake` builds the fixture's world,
`acme-crm-env:dev`.

Then boot the image in the foreground. The log shows each process's output, then the boot's timing
line, `booted in …`. Ctrl-C stops it:

```bash
docker run --rm -p 3000:3000 acme-crm-env:dev
```

A failed boot stops the container. Either way, the message names the step that failed and how to
fix it. [World lifecycle](/lifecycle) lists every step and
[every failure message](/lifecycle#failure-messages).

## Debug a world

Try a manifest change without rebuilding, by mounting it over the baked one. This works for
everything that acts at boot (processes, `env`, `expose`, sinks, `quiet`, dates). Changes to
`bake` and `seed` need a rebuild.

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

Open a shell in `main` with the world running, to see what an agent sees (Harbor's
[`harbor task start-env`](https://docs.harborframework.com/tutorials/create-a-task)):

```bash
harbor task start-env -p fixtures/acme-crm/tasks/pay-invoice
```

## From production

A production snapshot becomes a world with no import or scrub code in Miroir:

1. **Scrub the dump before it leaves the customer's network.** Miroir does not scrub.
2. **Restore it in a `bake` command** (`pg_restore`), and set `database.time.captured` to the
   time the dump was taken. The bake moves every time column from then to the bake's start, so
   relative ages hold however late the image is built. Each boot then moves the world to the
   trial's day, as for any world.
3. **Build one image per snapshot.** Put the `COPY` of the dump and `RUN miroir bake` last in the
   Dockerfile, so the product and datastore layers are shared and each snapshot adds one layer.
4. **Keep a private snapshot in a private image.** See
   [where images must be public](/publish#where-images-must-be-public).
5. **Turn tickets into tasks.** The ticket becomes `instruction.md`: the request in the
   requester's words, and the account the agent acts as. The API calls that resolved it become
   `solution/solve.sh`, using `curl -fsS`. Then [draft the spec](/tasks#draft-the-spec) from
   reference runs.
