# Write the manifest

`/miroir/miroir.yaml` tells Miroir what to run, what to restore and how to reach the product.
This page walks through the manifest of the example CRM. Every field is in the
[manifest reference](../reference/manifest.md).

```yaml
miroir: 1
name: acme-crm
description: Acme CRM, a sales CRM for small teams

processes:                          # started in this order, datastores first
  - {name: db, command: [postgres, -D, /var/lib/postgresql/data], user: postgres,
     datastore: true, ready: {tcp: 5432}}
  - {name: web, command: [node, dist/server.js], user: node, cwd: /app,
     ready: {http: "http://127.0.0.1:3000/healthz"}}
  - {name: worker, command: [node, dist/worker.js], user: node, cwd: /app}

env:                                # for every process and command
  DATABASE_URL: postgres://postgres@127.0.0.1:5432/crm
  SMTP_URL: smtp://127.0.0.1:2525
  STRIPE_API_BASE: http://127.0.0.1:9000/stripe

bake:                               # create the seed data, with only the datastores up
  - [createdb, -h, 127.0.0.1, -U, postgres, crm]
  - [node, dist/migrate.js]
  - [node, dist/seed.js]

state: [/var/lib/postgresql/data, /app/uploads]

apps:
  crm:
    api: {base_url: "http://127.0.0.1:3000", openapi: /app/openapi.yaml}
    database: {url: "postgres://postgres@127.0.0.1:5432/crm"}

personas:
  sales_manager:
    description: Dana Lee, sales manager at Acme (dana@acme.test)
    headers: {crm: {Authorization: Bearer seed-token-dana}}

sinks: {smtp: 2525, http: 9000}

quiet:
  command: [sh, -c, 'test "$(redis-cli llen bull:email:wait)" = 0']
```

## Processes

Miroir starts the processes in order and waits for each `ready` probe before the next: a
loopback TCP port, or an `http://` URL that answers 2xx or 3xx. Mark the datastores with
`datastore: true` and list them first. During a reset they run alone while the data is
restored and its dates are moved. A datastore needs no durability, since each reset restores
its files: run Postgres with `-c fsync=off -c synchronous_commit=off -c full_page_writes=off`
to skip the fsyncs of each commit and checkpoint.

## Bake and seed

`bake` commands create the seed data while only the datastores run. `seed` commands run after
the apps start, for data that is easier to create through the product, such as through its
API. Both get `MIROIR_WORLD`, the name of the [world](../features/worlds.md) being baked.

## State

`state` lists the directories each reset restores: the datastores' data, uploads, caches.
Leave it out and Miroir restores every path the processes' users can write.

A file that changes outside the state lets one episode see another. `check` fails when that
happens, and a running env refuses resets once it finds such a file.

## Apps

Each app has an `api`, a `database`, an [MCP server](../features/mcp.md), or several.

- `api.base_url` is where the agent's requests go. `api.openapi` is the spec `api_docs`
  serves.
- `api.omit` lists keys to drop from the API's JSON answers before the agent sees them, such
  as links and avatars repeated in every object. Each answer's tokens stay in the agent's
  context for the rest of the episode, and in every training sample. A key matches at any
  depth; `parent.key` matches only in an object held under `parent`, so `head.repo` drops a
  pull request's copy of its repository and keeps an activity's `repo`. Braces give
  alternatives: `{user,assignee}.created` drops when a user's account was made, and keeps
  when a commit was.
- `database.url` is where grading reads net changes and runs checks. Connect as a user that
  can create triggers: bake installs triggers that log every row change.
- `database.ignore` lists tables, or `table.column`s, whose changes never count, such as a
  `last_used_at` the product touches on every request.

An env with [several apps](../features/apps.md) names the app in each request and check.

## Personas

A persona is the user the agent acts as. Its `headers` go on every request to that app, and
win over headers the agent sends. Tasks pick a persona; with one persona, every task uses it.

## Sinks

The [sinks](../features/sinks.md) are a local SMTP server and HTTP server that record what
the product sends. Point the product's outbound settings at them, as `SMTP_URL` and
`STRIPE_API_BASE` do above.

## Quiet

Grading must wait for background work, such as a queued email, to finish. `quiet.command`
exits 0 when the product is idle; Miroir waits until it passes twice in a row, for up to
`timeout_s` (30 s). Without it, Miroir waits one second.

## What bake does

Bake validates the tasks already in `/miroir/tasks`, so a broken task file fails the build before
the seeding. Then, for each world:

1. Start the datastores and wait until each database answers.
2. Run the `bake` commands.
3. Start the apps and wait for quiet.
4. Run the `seed` commands and wait for quiet.
5. Install the triggers that log row changes.
6. Stop everything and move the state to `/miroir/pristine/<world>`.

Last, it validates every task again, including those a seed wrote: see
[Generate tasks in the seed](tasks.md#generate-tasks-in-the-seed).

## What a reset does

1. Kill every process.
2. Restore the state, copying only the files that changed, and empty `/tmp`, `/var/tmp` and
   `/dev/shm`.
3. Start the datastores and move every date forward by the days since bake. See
   [Dates](../features/dates.md).
4. Run the task's `setup` SQL, if any.
5. Start the apps and wait for quiet.
6. Clear the change log and the episode database.
