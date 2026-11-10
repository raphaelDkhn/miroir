# Task file

Each `*.yaml` or `*.yml` file in `/miroir/tasks/` is a task. Probes can also go in
`/miroir/tasks/probes/<task-id>.yaml`, as a list. Unknown fields are errors.

## Top level

| Field | Default | What |
|---|---|---|
| `id` | the file name | unique among the tasks |
| `split` | `train` | `train`, `validation` or `test` |
| `world` | the only world | the world each reset restores; required with several |
| `persona` | the only persona | who the agent acts as; required with several |
| `instruction` | required | what the agent is asked to do |
| `answer` | none | the normal form of the submitted answer: `integer`, `ids` or `date` |
| `max_steps` | 50 | the tool calls the episode allows |
| `setup` | none | SQL run on the only database at each reset, before the apps start |
| `checks` | required | the [checks](#checks), at least one |
| `allowed_changes` | required, or `any_change` | the [net changes](#allowed_changes) the task may make |
| `any_change` | `false` | allow every change, in place of `allowed_changes` |
| `harm_reward` | 0 | the reward for harm, from -1 to 0 |
| `null_max` | 0 | the most an immediate empty submit may earn, from 0 to 1 |
| `golden` | required | a run that must earn 1, ending with `submit` |
| `probes` | `[]` | runs whose reward must stay within bounds |
| `coworkers` | `[]` | colleagues a model plays; see [Coworkers](../features/coworkers.md) |
| `events` | `[]` | requests sent at clock minutes; see [Clock and events](../features/clock.md) |

With several worlds, a world's tasks are all `test`, or none are.

## `checks`

| Field | What |
|---|---|
| `name` | unique; `scope` is reserved |
| `sql` | a query returning one value: a boolean, or a number from 0 to 1 |
| `command` | a command that scores what it prints on exit 0 (1 if nothing), and 0 otherwise |
| `app` | the database `sql` reads, when several apps have one |
| `db` | `episode`: run `sql` on the [episode database](episode-db.md) instead |
| `weight` | its share of the reward, in (0, 1]; the weights sum to 1 |
| `gate` | `true`: it must score 1, or the episode is harm |
| `by` | the clock minute it is scored at, from 1 to 525,600; weighted checks only |

A check has exactly one of `sql` and `command`, and exactly one of `weight` and `gate`. A
check runs for up to 10 seconds. A command check gets `MIROIR_EPISODE_DB` and
`MIROIR_DATABASE_URL`.

## `allowed_changes`

| Field | What |
|---|---|
| `table` | the table, schema-qualified outside the default schema |
| `op` | `insert`, `update`, `delete`, `truncate` or `schema` |
| `columns` | for `update`: the columns it may change; any when unset |
| `app` | the app whose database it is, when several apps have one |

## Steps

`golden` and each probe's `steps` are lists of tool calls:

```yaml
- {tool: api_request, arguments: {method: GET, path: /deals}}
- {tool: submit, arguments: {answer: "3"}}
```

`tool` is one of the [agent tools](tools.md), a tool of the app's MCP server, or
`coworker_reply` (with a `coworker` and a `text`) to stand for a coworker's answer. The last
step, and only the last, is `submit`.

## `probes`

| Field | What |
|---|---|
| `name` | unique within the task |
| `steps` | the tool calls, ending with `submit` |
| `max_reward` | the most it may earn |
| `min_reward` | the least it may earn |

A probe has at least one of `max_reward` and `min_reward`.

## `coworkers`

| Field | What |
|---|---|
| `name` | 1 to 64 lowercase letters, digits, `_` or `-` |
| `description` | who they are |
| `script` | what they know, and how they act |
| `headers` | their headers, by app |
| `inbox.sql` | a query returning a JSON array of messages, each with an `id` and a `text` |
| `inbox.app` | the database to query, when several apps have one |
| `reply` | the request that posts an answer: `app`, `method`, `path` and `body`, with `{text}` and `{field}` placeholders |

## `events`

| Field | What |
|---|---|
| `name` | 1 to 64 lowercase letters, digits, `_` or `-` |
| `at` | the clock minute it is sent at, from 1 to 525,600 |
| `headers` | the sender's headers, by app |
| `request` | `app`, `method`, `path` and `body` |
