# Episode results

## Step reply

Each `step` returns `observation`, `reward` and `done`. Until the episode ends, `reward` is 0
and `done` is `false`. The last step has `done: true`, the reward (or `null` when the episode
could not be graded) and the metadata below.

## Metadata

| Field | What |
|---|---|
| `task_id` | the task |
| `success` | every check scored 1, with no harm |
| `harm` | a change out of scope, a failed gate, or a fault |
| `checks` | each check's `name` and `score`, with `gate`, `by` and `error` when they apply, then the `scope` gate |
| `out_of_scope` | up to 100 net changes outside `allowed_changes`: `tbl`, `op`, `pk`, `columns` and `app` |
| `out_of_scope_count` | how many there are in all |
| `answer` | the stored answer, or `null` |
| `termination` | `submit`, `max_steps`, `max_seconds`, `void` or `crash` |
| `world`, `task_hash`, `manifest_hash`, `miroir_version` | what the episode ran on |
| `clock` | for clock tasks: the `minutes` elapsed, and the `events` fired with their `minute` and `status` |
| `fault` | why the episode broke the env, which made it harm |
| `crashed` | the `process` that exited, and its `exit_code` |
| `quiet_timeout` | `true` when the quiet command never passed before grading |
| `invalid` | why Miroir could not grade the episode, whose reward is `null` |
| `clock_rolled` | `true` when the date moved during the episode, as at midnight |

A faulted episode, or one that could not be graded, has no `checks`.

## Record

The `record` frame, or `Session.finish()["record"]`, returns the finished episode in full,
within 4 MiB. The adapters save it with `traces=`.

| Field | What |
|---|---|
| `episode_id`, `task_id` | which episode |
| `reward` | its reward |
| `null_max` | the task's `null_max` |
| `metadata` | as above |
| `signature` | a hash of the end state; `null` when the episode was not graded |
| `diagnostics` | counts: `calls`, `client_errors`, `server_errors`, `repeated_calls`, `docs_calls`, `code_requests`, `coworker_replies`, `events` |
| `actions` | each call's `tool`, `method`, `path`, `status`, `latency_ms`, `arguments` and `result`, with `app` and `via` when set |
| `changes` | each net change's `tbl`, `op`, `pk`, `before`, `after` and `columns` |
| `effects` | each effect's `kind` and `data` |
| `timings` | `reset_s` (each reset step, and the `total`), `episode_s` and `grading_s` |
| `limit_bytes` | the size limit, 4 MiB |
| `truncated` | what was left out to fit the limit, counted by part |

The signature reads the times the episode wrote relative to when it ran, so rollouts that end
alike share it on any day.

## State

The `state` frame returns `episode_id`, `step_count`, `task_id`, `done`, `reward` and
`success`. `reward` and `success` stay `null` until the episode is done.
