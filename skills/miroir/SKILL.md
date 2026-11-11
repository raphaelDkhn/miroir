---
name: miroir
description: Build, check, serve and train on Miroir envs, which turn a product's Docker image into a reinforcement-learning environment. Use to write a Dockerfile.miroir, a miroir.yaml manifest or tasks/*.yaml (instructions, SQL checks, gates, allowed_changes, golden runs, probes); to fix `miroir bake` or `miroir check` failures; to explore a running env or play episodes with the Python client; to run pools, TRL or verifiers rollouts, review grades or calibrate tasks; or to scrub production dumps and traces into seed data and tasks. Also use when a repo has a miroir.yaml or Dockerfile.miroir, or the user mentions Miroir.
license: Apache-2.0
compatibility: Needs Docker with buildx. The host CLI and Python client need Python 3.11+ and uv.
---

# Miroir

Miroir turns a product's Docker image into a reinforcement-learning environment, an *env*. You
add a manifest (`miroir.yaml`) and tasks to the image. `miroir bake` seeds the data at build
time. `miroir run` restores that seed before every episode, gives the agent the product's API as
tools over OpenEnv (WebSocket) and MCP, and grades the end state from the rows that changed and
the emails and HTTP calls the product sent.

Two programs are called `miroir`:

| Program | Runs | Commands |
|---|---|---|
| Rust binary | inside the env image, as its entrypoint under `tini` | `bake`, `check`, `run`, `sandbox`, `import` |
| Python package | on the host | `pool`, `review`, `calibrate`, `draft`, `derive`; the `Session` client; TRL and verifiers adapters |

Run the image's commands as `docker run --rm IMAGE COMMAND`. They refuse to run under
`docker exec`: Miroir must be the container's entrypoint, or a child of its init.

The Python package is not on PyPI. Install it from a Miroir checkout (`uv pip install ./python`,
`'./python[trl]'` or `'./python[verifiers]'`), or from git:
`uv pip install 'miroir @ git+https://github.com/raphaelDkhn/miroir#subdirectory=python'`.

## Choose the job

| The user wants to | Follow | Then read |
|---|---|---|
| turn a product into an env | [Make an env](#make-an-env) | `references/manifest.md` |
| add or fix tasks | [Write a task](#write-a-task) | `references/tasks.md` |
| make `miroir check` pass | [The check loop](#the-check-loop) | `references/check.md` |
| serve an env, play episodes, train or evaluate, review grades, calibrate | [Explore a running env](#explore-a-running-env) | `references/run-and-train.md` |
| seed from production dumps | | `references/manifest.md`, "Production dumps" |
| turn production traces into tasks | | `references/tasks.md`, "Tasks from traces" |

Look before you write. Find `miroir.yaml`, `Dockerfile.miroir` and `tasks/` if they exist, and
the product's own Dockerfile or compose file, migrations, seed scripts, OpenAPI spec and auth
code. The Miroir repository's
[`fixtures/`](https://github.com/raphaelDkhn/miroir/tree/master/fixtures) are envs that pass
`check`: `acme-crm` (Postgres, Redis, a worker, an MCP server), `mini-shop` (MySQL, two worlds)
and `acme-apps` (several apps). Its
[`examples/gitea`](https://github.com/raphaelDkhn/miroir/tree/master/examples/gitea) makes a
stock product image an env: Gitea with Postgres, Unix-time columns, mail and webhooks in the
sinks, coworkers and clocks, three worlds, and a seed that writes the tasks from the ids its
rows got.

## Make an env

1. **Map the product.** List every long-running process (web servers, workers, schedulers) and
   every datastore, with its command, port, user and environment; a compose file holds most of
   this. Find how the seed data is made (migrations, seed scripts, fixtures), how the API
   authenticates (a token or key you can seed for a persona), the OpenAPI spec, what the product
   sends out (email, payment or webhook calls), what runs in the background (queues, cron), and
   every directory it writes.
2. **Write `Dockerfile.miroir`** on top of the product's image: install its datastores and
   `tini`, copy in the `miroir` binary, the manifest and `tasks/`, initialize each datastore's
   data directory as its user, then `RUN miroir bake`. Everything runs in this one container.
   The template is in `references/manifest.md`.
3. **Write `miroir.yaml`**: the processes in order, datastores first with `datastore: true`, each
   with a non-root `user` and a `ready` probe; an `env` that points email and outbound base URLs
   at the sinks and turns schedulers off; `bake` commands that create, migrate and seed the
   database; the `state` directories; the `apps` (API base URL and spec, and a database URL whose
   user can create triggers); a persona whose headers carry a seeded credential; the `sinks`; and
   a `quiet` command if the product queues work.
4. **Write one read-only task first**, such as a count with `answer: integer` and
   `allowed_changes: []`, so the first check tests the env, not the task.
5. **Build** with `docker build -f Dockerfile.miroir -t NAME-env .`. Bake runs during the build,
   so its errors are in the build log.
6. **Check until it passes**: [the check loop](#the-check-loop).
7. **Add tasks** a few at a time, and check after each.

## Write a task

A task is `tasks/<id>.yaml`: an instruction, weighted checks and gates, the changes it may make,
and a golden run that must earn 1. Fields and check recipes are in `references/tasks.md`.

1. **Find facts** in the seed data and the API ([explore a running env](#explore-a-running-env)):
   the ids, names and amounts the instruction can name, and the calls that solve it.
2. **Write the instruction** as a request the persona gets at work, such as from a colleague or
   a customer. Name the records, and say what must stay as it is. Never write an absolute date:
   every reset moves the data's dates forward, so write "today", "last week" or "overdue". When
   the answer is a number, a list of ids or a date, set `answer`.
3. **Write the golden run**: the calls that solve the task, ending with `submit` (with an
   `answer` when the task asks for one).
4. **Learn what it changes.** Play the calls on the scratch task and read `record["changes"]`:
   each net change's table, op and columns. Without a scratch run, set `allowed_changes: []` and
   run the check; the golden stage then lists each change, such as
   `outside allowed_changes: update invoices {"id":4001} (paid_at, payment_intent, status)`.
   Allow exactly those tables, ops and columns.
5. **Write the checks** on the end state, not on the calls: there are many ways to do a task.
   The weights sum to 1 and give partial credit. Add a gate for each thing that must not happen,
   such as another row changed or an email sent. Compare a submitted answer whole
   (`value = '3'`), never search inside it. For text the agent writes into a row, such as a
   note, match a keyword (`text ~* 'competitor'`), knowing it rejects paraphrases.
6. **Add probes**, one per plausible wrong solution (does too much, picks the wrong record,
   sends the email twice), with `max_reward: 0`.
7. **Check.** A probe that earns too much means the checks are too weak: fix the checks, never
   the probe. A probe whose calls fail earns 0 and passes without testing anything, so confirm in
   the `--json` report that its calls worked and the gate you meant caught it
   (`references/check.md`).

## The check loop

`miroir check` replays each task's golden run, an empty submit, two repeats and the probes, and
checks that the env idles, resets cleanly and writes only to its state. It prints a table, each
failure's reason under its task, then `PASSED` (exit 0) or `FAILED` (exit 1).

```sh
docker build -f Dockerfile.miroir -t my-env .   # after changing the manifest, Dockerfile or seed
docker run --rm --network none my-env check 2>check.log
```

- The report goes to stdout; the datastores' and the product's logs go to stderr, hundreds of
  lines, hence `2>check.log`. An `ERROR` in those logs, such as a constraint conflict while
  dates move (Miroir retries those), fails nothing: the report decides.
- Tasks are read when the container starts, so task edits need no rebuild. Mount them over the
  baked ones: `docker run --rm --network none -v "$PWD/tasks:/miroir/tasks:ro" my-env check`.
  Edits to the manifest, the Dockerfile or the seed need a rebuild, since bake uses them.
- To check one task, mount a directory that holds only that task (and its `probes/<id>.yaml`).
- A check idles for 30 s, then resets several times per task. On `acme-crm`, 8 tasks take under
  two minutes. Run it in the background on large envs, or check one world with `--world W`.
- `--json` prints the whole report: each stage's `passed`, and for the golden run its
  `statuses`, its `checks` with their scores, and its `metadata` (`out_of_scope`, `fault`,
  `termination`).
- `--mutate` also replays the golden run without each of its calls, to find calls it doesn't
  need and checks too weak to notice. Its lines are advice: they never fail the check.
- Tasks whose runs call `run_code` need a sandbox; see `references/check.md`.

Each stage's usual causes and fixes are in `references/check.md`.

## Explore a running env

To try calls before you write a task, serve the env with a directory that holds only a scratch
task, which allows any change. Keep it out of `tasks/`: `check` fails on it.

```yaml
# explore/scratch.yaml
instruction: Scratch task for trying calls.
checks: [{name: any, weight: 1, sql: SELECT true}]
any_change: true
golden: [{tool: submit}]
```

```sh
docker run --rm -d --name my-env -p 127.0.0.1:8000:8000 -e MIROIR_TOKEN=secret \
  -v "$PWD/explore:/miroir/tasks:ro" my-env
curl -s http://127.0.0.1:8000/health   # {"status":"healthy"} once ready
```

Play calls with the Python client, and read what changed and what the product sent. The
product's MCP tools, if it has an MCP server, are stepped by name too; list them with
`miroir.tools("http://127.0.0.1:8000", "secret", surface="mcp")`.

```python
import json
from miroir import Session

session = Session.lease(["http://127.0.0.1:8000"], token="secret", wait_busy=False)
try:
    session.reset("scratch")
    for tool, arguments in [
        ("api_docs", {}),
        ("api_request", {"method": "PATCH", "path": "/deals/2001", "body": {"owner_id": 7}}),
    ]:
        print(json.dumps(session.step(tool, arguments)["observation"])[:2000])
    outcome = session.finish()  # submits, grades and returns the record
    record = outcome["record"]
    print(json.dumps({"changes": record["changes"], "effects": record["effects"]}, indent=1))
finally:
    session.close()
```

After a run, the data stays as the episode left it until the next reset, so you can try a
check's SQL on it:

- On the app's database:
  `docker exec my-env psql -h 127.0.0.1 -U postgres -d DB -tAc 'SELECT ...'`, or
  `mysql -h 127.0.0.1 -u root DB -e '...'`. Keep these non-interactive: a file written outside
  the state, such as a shell history, makes the env refuse resets, and the container must then
  be replaced.
- On the episode database, for `db: episode` checks: the image may lack `sqlite3`, so copy the
  database out with its WAL files and query it on the host.

```sh
d=$(mktemp -d); for f in episode.db episode.db-wal episode.db-shm; do
  docker cp "my-env:/miroir/$f" "$d/" 2>/dev/null; done
sqlite3 "$d/episode.db" "SELECT tbl, op, columns FROM changes"
```

## Rules

- Every process has a non-root `user`. Datastores come first, with `datastore: true`.
- Everything the product writes is under `state`, or under `/tmp`, `/var/tmp`, `/dev/shm`,
  `/var/log` or `/run`. A write anywhere else fails `writes`, and a served env refuses resets.
- Nothing happens while the env idles: turn schedulers and cron off, wait for queues with
  `quiet`, and list in `database.ignore` the columns the product touches on every request.
- The env never needs the internet. Email and outbound HTTP go to the sinks, with canned
  `responses` where the product reads a reply. Run `check` with `--network none`.
- Commands are lists, `[program, arg, ...]`, run without a shell unless you write
  `[sh, -c, "..."]`. Unknown fields are errors.
- `database.url` connects as a user that can create triggers. The fixtures give the app's own
  processes a role with only `SELECT, INSERT, UPDATE, DELETE`.
- Dates move at every reset. List integer Unix-time columns in `database.time.epoch` and dates
  that never move, such as birthdays, in `database.time.exclude`; move dates outside the
  databases with `time.hook`.
- Never weaken a probe or the golden run to pass `check`: fix the checks.
- Never bake `MIROIR_TOKEN`, an import key or production data that is not scrubbed into an
  image.
- Serve on `127.0.0.1` with `MIROIR_TOKEN` set, and train on an internal network.
