---
title: "World lifecycle"
description: "What Miroir does at build, at each boot and at collect, what keeps the agent out, and what each failure message means."
---

A world goes through three stages. `miroir bake` runs once, when the image is built. `miroir run`
boots the world at the start of every trial. `miroir collect` writes the evidence after the agent
stops. Failure messages name the step that failed, such as `boot step 5`, so you can look it up
below.

## Bake

`RUN miroir bake`, the last `RUN` of the world image's Dockerfile, at `docker build`:

1. Load the manifest. An image that is already baked (`/miroir/bake.json` exists) is refused.
2. Start the sinks. They answer but record nothing, so mail and webhooks sent while seeding
   succeed.
3. Start the datastores in order, each until its ready probe passes, then wait until
   `database.url` answers.
4. Run each `bake` command, with the build's environment under the manifest `env`, so Docker
   `ARG`s reach it.
5. With `database.time.captured` set, move the restored data from that time to the bake's start.
6. Start the other processes in order until ready, then wait for quiet.
7. Run each `seed` command, then wait for quiet. A process that exited fails the bake.
8. Install change capture on every table not ignored: a `miroir` schema holding the change log,
   and on each table a row trigger, `miroir_capture`, and a statement trigger, `miroir_truncate`.
   The bake refuses:
   - a `database.url` role that is not a superuser;
   - captured names the grader's SQLite copy cannot hold, in any ASCII case: two tables, or two
     columns of one table, with the same name; a table whose evidence name starts with `sqlite_`;
     and a column named `_op` or `_change`, which the grader adds.
9. Stop every process in reverse order: SIGTERM, then SIGKILL after 30 s.
10. Refuse a bake that ran past midnight UTC, since each boot moves dates from the bake's day.
11. Write `/miroir/bake.json`, create the empty `/miroir/evidence/`, and make `/miroir` private
    to root (mode 0700).

A failure prints `bake step N, <what>: <why and the fix>` and exits 1, which fails
`docker build`.

## Boot

`miroir run`, the world container's command, once per trial:

1. Read `/miroir/bake.json`.
2. Create `/miroir/run/`. If it is already there, the container booted before: its dates may have
   moved already and its database holds that run's changes. The boot fails and leaves that run's
   files as they are. A world boots once per container.
3. Start the sinks, recording to `/miroir/run/effects.jsonl` (and faults to `faults.jsonl`).
4. Start the datastores until ready, then wait until the database answers.
5. Shift dates from the bake to today ([Dates](/manifest#dates)).
6. Start the other processes until ready, then wait for quiet.
7. Clear the change log and the journals, since start-up writes and mail are not the agent's, and
   record the tables at the start of the episode in `/miroir/run/catalog.json`. A process that
   exited fails the boot.
8. Audit the listeners ([The listener rule](#the-listener-rule)).
9. Write `/miroir/run/ready`, which the compose healthcheck tests, and log the step timings:
   `booted in 0.8 s: datastores 0.2, shift 0.0, apps 0.4, quiet 0.2` for the fixture.
10. Supervise. A process that exits is recorded as a `crash` fault, and the world keeps serving.
    Every child gets `oom_score_adj` 1000, so under memory pressure the kernel kills a product
    process (a recorded crash) before `miroir` itself.
11. On SIGTERM, stop the processes in reverse order and exit 0.

A failure in steps 1 to 8 prints `boot step N, <what>: <why and the fix>` and exits 1. The
container stops, `main` never starts, and Harbor records an environment error before any agent
runs.

## Collect

`miroir collect`, run by the task's collect hook after `main` has stopped. Harbor runs
`docker compose exec <service> sh -c 'miroir collect'` as root with an almost empty environment,
so collect applies the manifest `env` itself.

1. Fail if `/miroir/run/ready` is missing: the world never finished booting.
2. Wait for quiet. A timeout is recorded as a `quiet_timeout` fault.
3. Read the change log and the live table list in one consistent read. Either of these leaves the
   evidence with no changes:
   - a database that does not answer, recorded as a `database` fault;
   - a change log over 64 MiB, counting each row's text plus 1 KiB, recorded as a `log_budget`
     fault.
4. Fold the log into net changes: one per changed row, with the row as the episode found it and as
   it left it. A table created, dropped or reshaped since boot, or one whose capture trigger is
   gone, is a `schema` change; a re-created table is a `truncate`.
5. Read the effects and faults journals. An effects journal over 16 MiB is left unread, and
   recorded as an `evidence_budget` fault giving its size.
6. Write `/miroir/evidence/evidence.db` through a temporary file and a rename. Evidence over
   16 MiB is written again with no changes and no effects, plus an `evidence_budget` fault.
7. Print a summary, such as `evidence: 1 changes, 1 effects, 36864 bytes; faults: []`, and exit 0.

Collect exits 1 only for Miroir's own failures: the world never booted, the fold failed, or no
file could be written. The grader then finds no evidence, and the trial voids. Faults never fail
collect: they are harm, measured.

## The listener rule

At boot, once every process is ready, Miroir reads every TCP listener in the world's network
namespace (`/proc/net/tcp` and `/proc/net/tcp6`). Two things fail the boot:

- a listener off loopback (not `127.0.0.0/8` or `::1`) on a port outside `expose`:
  `port 5432 (db) listens on 0.0.0.0: bind it to 127.0.0.1, or list it in expose`;
- an `expose` port with no listener off loopback:
  `expose lists 3000 (web), but nothing listens on it off loopback: check the product's bind address`.

The name in parentheses is the process whose ready probe uses the port. Bind datastores (which use
trust authentication) and everything else to `127.0.0.1`, and the product's API to `0.0.0.0` on
its exposed port.

A runtime that runs each compose service as a VM also lists the VM's own daemons there, so such
runtimes fail this audit and are not supported.

## Security model

The agent is untrusted, and runs as root in `main`.

- **The product cannot reach the internet.** `main` sits on Harbor's default network (for its
  model API) and on `world`. The world service sits only on `world`, which is `internal: true`, so
  its mail and webhooks land in the sinks.
- **Over TCP, `main` reaches only the `expose` ports.** Datastores, sinks and every other TCP
  listener are on loopback, and the boot's audit refuses a world that breaks this.
- **Capture happens below the API**, in triggers the superuser owns. The agent never shares a
  container with the datastores, the change log or the journals, and a root process writes the
  evidence after Harbor has stopped `main`. The product's roles cannot read `/miroir` (mode 0700),
  and a disabled capture trigger shows as a `schema` change, which is harm.
- **The spec never reaches `main`.** It lives in `[verifier.env]`, which only the verifier
  receives, and `main` holds no grader, tests or Miroir binary.
- **Grader inputs are bounded.** Only the first 64 KiB of a regular answer file are read, and
  every SQL statement of the spec runs under a fixed budget of SQLite steps, not a clock. So an
  agent cannot turn a scored trial into a void, and grading never depends on machine load. The
  grader runs in its own container.
- **The task's credential is test data**, valid only inside one trial's private network, and
  written in `instruction.md` so that every harness sees it.
- **Identity is not a security boundary.** The agent may use any credential it finds, and scope
  measures every change, whoever made it. What the product can reach through its own features
  (SSRF) is limited by its own configuration, such as Gitea's `ALLOWED_HOST_LIST`.

## Failure messages

| Message | Fix |
|---|---|
| `<field path>: <problem>` at bake or boot | The manifest: the message names the field and the fix. |
| `bake step 1, /miroir/bake.json: this image is already baked` | Build on the product's image, not on a world image. |
| ``this image was not baked: add `RUN miroir bake` to its Dockerfile`` | Make `miroir bake` the last `RUN`. |
| `boot step 2, /miroir/run: this container booted before` | A world boots once per container: start a new one from the image (`docker run`, not `docker start`). |
| `bake step 3, datastores: …` or `boot step 4, datastores: …` | A datastore never became ready, or `database.url` does not reach it as a superuser. |
| ``bake step 4, bake command `…`: `…` exited with status 1; its output is above`` | The command failed: its output is above the message. |
| `process '<name>' was not ready within 60 s: … never passed` | Check the process's command, its output and its ready probe. |
| `process '<name>' exited with status 1 before it was ready` | The process failed at start: its output is above the message. |
| `boot step 7, process '<name>': it exited during boot (status 1)` | A worker exited once started: its output is above. |
| `the quiet command … did not pass twice in a row within 30 s (its last run …)` | Background work never settles: fix the command, or turn the scheduler off. The parenthesis says how the last run ended, with the last line it printed on stderr. |
| `boot step 5, date shift: …: if a constraint refuses a moved value, list its column in database.time.exclude` | As it says. |
| `database.time.epoch: '<glob>' matches no integer or bigint column` | Name the table as the evidence does, with `*` for any run of characters. |
| `database.ignore: no table or column '<entry>'` | Name a table as the evidence does, or a column as `table.column`. |
| `bake step 8, change capture: … role <name> is not a superuser …: database.url must connect as a superuser` | Connect `database.url` as a superuser, such as `postgres`. |
| `port 5432 (db) listens on 0.0.0.0: bind it to 127.0.0.1, or list it in expose` | Bind the process to loopback. |
| `expose lists 3000 (web), but nothing listens on it off loopback` | Make the product listen on `0.0.0.0` (the fixture sets `HOST: 0.0.0.0`). |
| `bake step 10, the clock: the bake ran past midnight UTC` | Build again. |
| `the world never finished booting` (from collect) | The trial voids: the world container's log says why the boot stopped. |
