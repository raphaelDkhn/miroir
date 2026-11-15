# Check the env

`miroir check` tells you whether the env grades reliably. Run it after every change to the
manifest or the tasks, until it passes:

```sh
docker run --rm --network none my-env check
```

```text
miroir check: acme-crm
static                       pass
users                        pass
idle                         pass
mcp                          pass (3 tools)
task                         golden null  repeat  reset  writes golden statuses
count-overdue-invoices       pass   pass  pass    pass   pass   200 200 ok
export-open-deals            pass   pass  pass    pass   pass   201 ok
pay-invoice                  pass   pass  pass    pass   pass   200 200 ok
reset p50 0.9 s, p95 1.07 s; grading p50 0.24 s, max 0.46 s
PASSED
```

A failed stage prints its reasons under the task, and the command exits non-zero.

## Env stages

| Stage | Passes when |
|---|---|
| `static` | the manifest and every task load and validate |
| `users` | every process has a `user` (write `user: root` to keep one as root) |
| `idle` | resets a day and 400 days later work, and 30 s after a reset with no agent action leave no change, no effect and no crash |
| `mcp` | the app's MCP server lists its tools (only with an MCP server) |
| `sandbox` | the sandbox reaches nothing of the env's but its proxy (only with `MIROIR_SANDBOX`) |

## Task stages

| Stage | Passes when |
|---|---|
| `golden` | the golden run succeeds, and every call works: HTTP below 400, code exits 0, no tool error |
| `null` | an immediate empty `submit` earns at most `null_max` (0 by default), and every gate holds |
| `repeat` | two more golden runs earn the same reward and the same check scores |
| `reset` | the data after reset and `setup` hashes the same in all of these runs, and as in the first run checked of the same world and `setup`, time columns aside |
| `writes` | no file changed outside the state directories |
| `probes` | each probe's reward stays within its bounds |
| `hedge` | submitting the golden answer among wrong guesses earns at most `null_max` |
| `drift` | the golden run earns the same when timestamps sit a day behind dates |

`hedge` runs for tasks whose golden run submits an answer, unless the answer form is
`integer` or `date`. `drift` catches checks that break depending on the time of day of the
reset.

## Options

| Option | Does |
|---|---|
| `--json` | print the full report as JSON |
| `--world W` | check only the tasks of world `W` |
| `--mutate` | also replay the golden run once without each of its calls |

`--mutate` shows calls the run still succeeds without (reads, or checks too weak to notice),
and calls whose removal still earns a reward.

## With a sandbox

Tasks whose runs call `run_code` need a [sandbox](../features/sandbox.md).
[`sandbox/check.sh`](https://github.com/raphaelDkhn/miroir/blob/master/sandbox/check.sh) runs
the check beside one, the two alone on an internal network. It uses the `miroir-sandbox`
image, or the one `SANDBOX_IMAGE` names:

```sh
SANDBOX_IMAGE=ghcr.io/raphaeldkhn/miroir-sandbox:0.0 sandbox/check.sh my-env
```

Arguments after the image go to `check`, such as `--json`.
