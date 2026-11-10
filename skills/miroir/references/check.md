# Bake and check failures

## Bake

`miroir bake` runs in `docker build`, so its errors end the build log. Each names its step:

| Step | Usual cause | Fix |
|---|---|---|
| `datastores`, `database` | the data directory is missing or not owned by the datastore's user; a wrong port in `ready` or `database.url`; the database doesn't exist yet | initialize the directory in the Dockerfile as that user; create the database in the first `bake` command |
| `bake command`, `seed command` | the command failed; its output is just above | commands run without a shell and get the manifest's `env`. To iterate faster, build the image without `miroir bake`, then run `docker run --rm -it --entrypoint sh IMAGE` and `miroir bake` inside it |
| `processes` | a `ready` probe timed out, or a process exited | check its port or URL, its `cwd`, its `env`, and that its user can read its files |
| `quiet` | the quiet command never passed twice in a row | test the command by hand; raise `timeout_s` only for real long work |
| `change capture` | the `database.url` user can't create triggers | connect as the database owner or a superuser |
| `state` | a root process with no `state` listed; a writable path holding a system directory; a world changing what the processes can write | give each process a user, or list `state`; create the paths in the image before bake |
| `time` | bake ran past midnight UTC, or `time.captured` is after the bake | build again; fix the instant |

A task that doesn't validate also fails bake, since bake loads the tasks.

## Check

`docker run --rm --network none IMAGE check` prints one line per stage and task, and each failure's
reasons under it. `--json` prints the same report in full:

```text
{passed, env, shift_days, timings,
 stages: {static, users, idle, mcp?, sandbox?},
 tasks: [{id, passed, error?, golden, null, repeat, reset, writes, drift,
          probes?, hedge?, mutations?}]}
```

`golden` has `reward`, `success`, `statuses` (one per call: the HTTP status, `ok`, `exit N` or an
error type), `checks` (`name`, `score`, `gate`, `error`) and `metadata` (`out_of_scope`, `fault`,
`termination`, `clock`). For example:

```sh
docker run --rm --network none my-env check --json > check.json 2>check.log
jq '.tasks[] | select(.passed == false)
    | {id, error, golden: .golden | {statuses, checks, out: .metadata.out_of_scope}}' check.json
```

`probes.runs[]` has each probe's `name`, `reward`, `max_reward`, `min_reward`, `statuses`,
`checks` and `metadata`. The table shows no probe column, and a probe passes whenever its reward
is within bounds, even when its calls fail: a typo in a path earns 0. Confirm that each probe's
calls worked and that the gate you meant is the one that failed:

```sh
jq -c '.tasks[] | .id as $id | .probes.runs[]? | {task: $id, name, reward, statuses,
       failed: [(.checks // [])[] | select(.score < 1) | .name]}' check.json
```

### Env stages

| Stage | Message | Usual cause | Fix |
|---|---|---|---|
| `static` | a file, a line and the field | an unknown or wrong field | fix the field; see `references/manifest.md` and `references/tasks.md` |
| `users` | `process 'x' has no user` | a process runs as root, so it could read the tasks' checks | set its `user`, or `user: root` to keep it |
| `idle` | `30 s with no agent action produced N change(s)`, then the changes | a scheduler, cron or poller writes on its own; a `HEALTHCHECK` inherited from the product image hits an endpoint that writes | turn it off in `env`; add `HEALTHCHECK NONE` to the Dockerfile; `database.ignore` columns touched on every request |
| `idle` | `... effect(s)` | the product sends email or calls out on a timer | turn the timer off |
| `idle` | `process 'x' exited with code N while idle` | the process crashes after a restore | run the image and read its log; check its user can write what it needs, inside `state` |
| `idle` | `a reset a day later would fail` or `400 days later` | moving dates breaks something: a constraint, an integer epoch column that overflows, a `time.hook` | list the column in `database.time.exclude` or `database.time.epoch`; fix the hook |
| `mcp` | `the app's MCP server lists no tools` | wrong `mcp.url`, or the server needs headers the persona lacks | fix the URL or the persona's headers for that app |
| `sandbox` | `the sandbox reaches ...` | the sandbox shares a network with more than the env | run them alone on an internal network, as `sandbox/check.sh` does |

### Task stages

| Stage | Message | Usual cause | Fix |
|---|---|---|---|
| (any) | `error: ...` | a replay could not run: the reset or `setup` failed, a tool doesn't exist, the run doesn't end with `submit` | read the error; fix the setup SQL or the steps |
| `golden` | `call N returned 404` (or another status, `exit 1`) | a wrong path, body or method; the persona lacks access | look the operation up with `api_docs`; try the call on a scratch task |
| `golden` | `outside allowed_changes: update invoices {"id":4001} (paid_at, ...)` | the run changes what the task doesn't allow | add the table, op and columns to `allowed_changes`, or `database.ignore` noise such as `last_used_at` |
| `golden` | `check 'x' scored 0` | the check's SQL doesn't match the end state | play the golden run on a scratch task, then run the SQL by hand with `docker exec` |
| `golden` | a `fault`, such as `quiet_timeout` | background work never settles, or a check errors | fix `quiet`; read the check's `error` |
| `null` | `an immediate empty submit earned 1, above null_max 0` | a check is already true at reset, such as `count(*) = 0` of something already absent | make every weighted check require the change; move "nothing else changed" checks into gates |
| `null` | `gate 'x' fails on an empty submit` | the gate says what must happen; a gate says what must not | make it a weighted check |
| `repeat` | `rewards differ across runs` or `check 'x' scored 1, 0, 1` | background work not awaited; a check on generated ids, times or row order; random seed data | add `quiet`; check what rows hold, not generated values |
| `reset` | `the fingerprint after setup changed between resets in T` | something writes at startup: migrations or seeding on boot, a cache or session table; a `setup` that reads the clock or randomness | move the work into `bake`, make startup read-only, or `database.ignore` the table |
| `writes` | `changed outside the state directories: /path` | the product writes outside its state: logs, caches, a build directory | add the directory to `state`, or send logs to stdout or `/var/log` and temporary files to `/tmp` |
| `probes` | `'name': earned 1.00, above max_reward 0` | the checks don't catch that wrong solution | add a gate or tighten a check; keep the probe |
| `probes` | `'name': earned 0.00, below min_reward 1` | the checks reject a right solution | loosen the check that scored below 1 |
| `hedge` | `the golden answer among guesses ... earned 1.00, above null_max` | a check searches inside the answer (`LIKE '%3%'`, `instr`) | compare the whole value, or set an `answer` form |
| `drift` | `with timestamps a day behind dates ... earned 0, not 1` | a check mixes dates and timestamps, so it depends on the time of day of the reset | compare timestamps with timestamps (`now()`) and dates with dates (`CURRENT_DATE`) |

`repeat` and `reset` compare the golden run with two repeats, and `reset` ignores time columns.
`drift` replays the golden run with timestamps moved a day less than dates, as when a reset runs
earlier in the day than the bake did.

### `--mutate`

`check --mutate` replays the golden run once without each of its calls, other than `submit`.
Its lines are advice, and never fail the check:

- `mutate: it still succeeds without call 1, 2: reads, or weak checks`: those calls are reads,
  which is fine, or the checks don't notice what they do.
- `mutate: without call 3 (POST /invoices/4001/pay) it still earns 0.60`: the other calls still
  earn credit. That is expected when each call earns its own weight; it points to a weak check
  only when the credit left comes from a check on what call 3 did.

## Check with a sandbox

Tasks whose runs call `run_code` need a sandbox beside the env, alone on an internal network.
`sandbox/check.sh` in the Miroir repository does it; by hand:

```sh
docker network create --internal -o com.docker.network.bridge.inhibit_ipv4=true mycheck
docker run -d --rm --name mycheck-sandbox --network mycheck --network-alias sandbox \
  ghcr.io/raphaeldkhn/miroir-sandbox:0.1
docker run --rm --network mycheck -e MIROIR_SANDBOX=http://sandbox:8000 my-env check
docker rm -f mycheck-sandbox && docker network rm mycheck
```
