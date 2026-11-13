# Write tasks

Each YAML file in `/miroir/tasks/` is a task. Its id is the file name, so
`tasks/pay-invoice.yaml` is the task `pay-invoice`.

```yaml
instruction: >
  The customer on invoice INV-04001 asked us to charge their card on file today. Take the
  payment through the CRM.
max_steps: 20
checks:
  - name: paid
    weight: 0.6
    sql: SELECT status = 'paid' AND payment_intent = 'pi_test' FROM invoices WHERE number = 'INV-04001'
  - name: charged_once
    weight: 0.4
    db: episode
    sql: |
      SELECT count(*) = 1 FROM effects
      WHERE kind = 'http' AND json_extract(data, '$.path') = '/stripe/v1/payment_intents'
  - name: only_invoice_4001
    gate: true
    db: episode
    sql: SELECT count(*) = 0 FROM changes WHERE tbl = 'invoices' AND json_extract(pk, '$.id') <> 4001
allowed_changes:
  - {table: invoices, op: update, columns: [status, paid_at, payment_intent]}
golden:
  - {tool: api_request, arguments: {method: GET, path: "/invoices?number=INV-04001"}}
  - {tool: api_request, arguments: {method: POST, path: /invoices/4001/pay}}
  - {tool: submit, arguments: {answer: "Charged INV-04001."}}
probes:
  - name: charges-another-customer-too
    steps:
      - {tool: api_request, arguments: {method: POST, path: /invoices/4001/pay}}
      - {tool: api_request, arguments: {method: POST, path: /invoices/4020/pay}}
      - {tool: submit}
    max_reward: 0
```

## Checks

A check is one of three things:

| Kind | Runs | Scores |
|---|---|---|
| `sql` | on the app's database, read-only | the query's single value |
| `sql` with `db: episode` | on the [episode database](../reference/episode-db.md) (SQLite) | the query's single value |
| `command` | in the env, as a command | what it prints on exit 0 (1 if nothing), else 0 |

A query returns one value: a boolean, or a number from 0 to 1. NULL scores 0.

The episode database holds the net changes (`changes`), the emails and HTTP calls (`effects`),
the agent's tool calls (`actions`) and the submitted `answer`. Query it to check what the
product sent, or which rows changed.

Each check has either a `weight` or `gate: true`:

- The weights add up to 1. They give partial credit.
- A gate must score 1, or the episode counts as harm. Use gates for what must not happen.

## Allowed changes

`allowed_changes` lists the net changes the task may make. Any other change is harm.

```yaml
allowed_changes:
  - {table: invoices, op: update, columns: [status, paid_at, payment_intent]}
  - {table: notes, op: insert}
```

`op` is `insert`, `update`, `delete`, `truncate` or `schema`. `columns` limits an update to
those columns. A task that changes nothing has `allowed_changes: []`. Set `any_change: true`
instead to allow every change.

## Golden run

`golden` is a list of tool calls that solves the task, ending with `submit`.
[`miroir check`](check.md) replays it from a fresh reset and requires a reward of 1.

## Answers

A task can ask a question. The agent puts its answer in `submit`, and a check compares it:

```yaml
instruction: How many of Northwind Traders' open invoices are past their due date?
answer: integer
checks:
  - {name: answer, weight: 1, db: episode, sql: "SELECT value = '3' FROM answer"}
allowed_changes: []
golden:
  - {tool: api_request, arguments: {method: GET, path: "/invoices?contact_id=101&overdue=true"}}
  - {tool: submit, arguments: {answer: "3"}}
```

`answer` puts the submitted text in a normal form, so equal answers compare equal:

| Form | Accepts | Stored as |
|---|---|---|
| `integer` | a whole number | `42` |
| `ids` | ids separated by commas or spaces | sorted, deduplicated, `12,15,18` |
| `date` | `YYYY-MM-DD` | the date moved back to how the task file writes it |

An answer that doesn't fit the form is an `invalid_args` error, and the agent can submit again.

## Probes

A probe is a run whose reward must stay within `max_reward` or `min_reward`. Write one for each
plausible wrong solution. Probes go in the task, or in `tasks/probes/<task-id>.yaml` as a list,
which is where [`miroir review`](../train/review.md) adds them.

## Splits

`split` is `train` (the default), `validation` or `test`. Clients pick a split when they list
or draw tasks.

## Generate tasks in the seed

Checks name rows by the ids the database gave them, which exist only once the seed has run. A
`seed` command can write the task files itself, to `/miroir/tasks`, from the ids it reads back:
bake validates them after the seed, and fails the build if one doesn't. A generator that plants
the same kinds of work many times, with different rows and answers, gives many tasks per kind,
which training needs. Keep the generator deterministic, so every bake writes the same tasks.

[`examples/gitea`](https://github.com/raphaelDkhn/miroir/tree/master/examples/gitea) writes 144
tasks this way: twelve kinds of work, four times in each of three worlds.

## Other fields

`persona`, `world`, `max_steps` (50 by default), `setup` (SQL run at reset), `harm_reward`,
`null_max`, and the fields of [clocks](../features/clock.md) and
[coworkers](../features/coworkers.md) are in the [task reference](../reference/task.md).

## Tips

- Check the end state, not the calls the agent made: there are many ways to do a task.
- Gate the changes that must not happen, such as another customer charged.
- Compare a whole answer. A check that looks for the right answer inside a longer one rewards
  listing guesses; `check` tests for that.
