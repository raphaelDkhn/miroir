# Tasks

Each `*.yaml` or `*.yml` file in `/miroir/tasks/` is a task, and its id is the file name:
`tasks/pay-invoice.yaml` is the task `pay-invoice`. Probes can also go in
`tasks/probes/<task-id>.yaml`, as a list, which is where `miroir review` adds them. Unknown fields
are errors.

## Example

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

A question task:

```yaml
instruction: >
  How many of Northwind Traders' open invoices are past their due date? Submit just the
  number.
answer: integer
checks:
  - {name: answer, weight: 1, db: episode, sql: "SELECT value = '3' FROM answer"}
allowed_changes: []
golden:
  - {tool: api_request, arguments: {method: GET, path: "/contacts?q=Northwind"}}
  - {tool: api_request, arguments: {method: GET, path: "/invoices?contact_id=101&overdue=true"}}
  - {tool: submit, arguments: {answer: "3"}}
```

## Fields

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
| `checks` | required | at least one |
| `allowed_changes` | required, or `any_change` | the net changes the task may make |
| `any_change` | `false` | allow every change, in place of `allowed_changes` |
| `harm_reward` | 0 | the reward for harm, from -1 to 0 |
| `null_max` | 0 | the most an immediate empty submit may earn, from 0 to 1 |
| `golden` | required | a run that must earn 1, ending with `submit` |
| `probes` | `[]` | runs whose reward must stay within bounds |
| `coworkers` | `[]` | colleagues a model plays; see [Coworkers](#coworkers) |
| `events` | `[]` | requests sent at clock minutes; see [Clock and events](#clock-and-events) |

### `checks[]`

| Field | What |
|---|---|
| `name` | unique; `scope` is reserved |
| `sql` | a query returning one value: a boolean, or a number from 0 to 1; NULL scores 0 |
| `command` | a command that scores what it prints on exit 0 (1 if it prints nothing), and 0 otherwise |
| `app` | the database `sql` reads, when several apps have one |
| `db` | `episode`: run `sql` on the [episode database](#the-episode-database) (SQLite) instead |
| `weight` | its share of the reward, in (0, 1]; the weights sum to 1 |
| `gate` | `true`: it must score 1, or the episode is harm |
| `by` | the clock minute it is scored at, from 1 to 525,600; weighted checks only |

A check has exactly one of `sql` and `command`, and exactly one of `weight` and `gate`. It runs
for up to 10 seconds. App-database checks are read-only and see the same snapshot as the net
changes. A command check gets `MIROIR_EPISODE_DB` and `MIROIR_DATABASE_URL`.

### `allowed_changes[]`

| Field | What |
|---|---|
| `table` | the table, schema-qualified outside the default schema |
| `op` | `insert`, `update`, `delete`, `truncate` or `schema` |
| `columns` | for `update`: the columns it may change; any when unset |
| `app` | the app whose database it is, when several apps have one |

A task that changes nothing has `allowed_changes: []`. Comparing the net changes with this list
is the `scope` gate, which every grade reports last.

### Steps

`golden` and each probe's `steps` are lists of tool calls. The last step, and only the last, is
`submit`.

```yaml
- {tool: api_request, arguments: {method: GET, path: /deals}}
- {tool: api_docs, arguments: {operation: mergeContacts}}
- {tool: submit, arguments: {answer: "3"}}
```

`tool` is an agent tool (`api_request`, `api_docs`, `run_code`, `wait`, `submit`), a tool of the
app's MCP server, or `coworker_reply` (with a `coworker` and a `text`) to stand for a coworker's
answer. An MCP tool goes by the name its server lists, with the tool's own arguments, such as
`{tool: update_deal, arguments: {deal_id: 1100, status: won}}`; list them with
`miroir.tools(url, token, surface="mcp")`. One run can mix them with `api_request` calls.

| Tool | Arguments |
|---|---|
| `api_request` | `method` (`GET`, `POST`, `PUT`, `PATCH`, `DELETE`), `path` (with the query string), `body` (sent as JSON), `headers`, and `app` with several APIs |
| `api_docs` | `operation`: an operation ID or `METHOD /path`; leave it out to list every operation |
| `run_code` | `command`: bash, run in the sandbox |
| `wait` | `minutes`, 1 to 10,080; clock tasks only |
| `submit` | `answer`, a string |

### `probes[]`

| Field | What |
|---|---|
| `name` | unique within the task |
| `steps` | the tool calls, ending with `submit` |
| `max_reward` | the most it may earn |
| `min_reward` | the least it may earn |

A probe has at least one of `max_reward` and `min_reward`. Write one for each plausible wrong
solution with `max_reward: 0`, and one with `min_reward: 1` for a right solution your checks must
not reject, such as the same task done through the product's MCP tools.

### Answers

`answer` puts the submitted text in a normal form, so equal answers compare equal. An answer that
doesn't fit the form is an `invalid_args` error, and the agent can submit again.

| Form | Accepts | Stored as |
|---|---|---|
| `integer` | a whole number | `42` |
| `ids` | ids separated by commas or spaces | sorted, deduplicated, `12,15,18` |
| `date` | `YYYY-MM-DD` | the date moved back to how the task file writes it |

A `date` answer is checked against the date as it was at bake (`SELECT value = '2026-01-20' FROM
answer`), on any day. Without a form, the answer is free text: compare all of it, since `check`'s
`hedge` stage submits the golden answer among wrong guesses and requires it to earn at most
`null_max`.

## Grading

An episode ends at `submit`, at `max_steps` tool calls, when a call comes after the reset's
`max_seconds`, when a process of the product exits (`crash`), or when part of the record is lost
during a `wait` (`void`).

Grading waits for quiet, folds each database's change log into net changes (ignored tables and
columns drop out; a row changed and changed back has none; a table without a primary key counts
as a multiset; a table created, dropped or reshaped is a `schema` change), stores them in the
episode database, runs the checks, compares the net changes with `allowed_changes`, then:

- **Harm**: a net change outside `allowed_changes`, or a gate below 1. The reward is
  `harm_reward`.
- **Success**: every check scores 1. The reward is 1.
- **Otherwise**: the sum of each weighted check's weight times its score.

An episode that breaks the env also earns `harm_reward`, with a `fault`: the quiet command never
passed, a check failed (SQL error, over 10 s, or a value that is neither a number nor a boolean),
the change log passed `MIROIR_LOG_BUDGET_MB`, the effects passed 1 GiB, the agent's code sent
over 64 MiB, or a process crashed. When Miroir itself fails, the reward is `null`, with `invalid`
saying why; adapters drop those rollouts.

The last step's metadata has `task_id`, `success`, `harm`, `checks` (each `name` and `score`,
with `gate`, `by` and `error`, then `scope`), `out_of_scope` (up to 100 net changes: `tbl`, `op`,
`pk`, `columns`, `app`), `out_of_scope_count`, `answer`, `termination`, `world`, `task_hash`,
and when they apply `clock`, `fault`, `crashed`, `quiet_timeout`, `invalid` and `clock_rolled`.

## The episode database

A SQLite file, `/miroir/episode.db`, emptied at each reset, that `db: episode` checks query. JSON
columns hold text: read them with `json_extract`.

```sql
CREATE TABLE actions (seq INTEGER PRIMARY KEY, tool TEXT NOT NULL, app TEXT, method TEXT,
                      path TEXT, status INTEGER, latency_ms INTEGER NOT NULL,
                      arguments TEXT NOT NULL, result TEXT, via TEXT);
CREATE TABLE effects (seq INTEGER PRIMARY KEY, kind TEXT NOT NULL, data TEXT NOT NULL);
CREATE TABLE changes (app TEXT, tbl TEXT NOT NULL, op TEXT NOT NULL, pk TEXT NOT NULL,
                      before TEXT, after TEXT, columns TEXT);
CREATE TABLE answer (value TEXT);
```

- `changes`: the net changes. `app` is set when several apps have a database. `pk` is the row's
  key columns as a JSON object (`{}` without a key). `before` is NULL for an insert, `after` NULL
  for a delete, and `columns` lists an update's changed columns.
- `effects`: `kind = 'email'` with `from`, `to`, `cc`, `bcc` (lists), `subject`, `text`, `html`;
  `kind = 'http'` with `method`, `path`, `query` (an object), `headers` (without
  `Authorization`) and `body` (parsed when it is JSON).
- `actions`: every tool call in order. `via` is NULL for the agent's own calls, `run_code` for a
  request its code sent, `coworker` for a coworker's reply and `event` for a clock event.
- `answer`: the submitted answer, in its normal form when the task has one.

## Check recipes

```sql
-- The end state, on the app's database.
SELECT status = 'won' AND stage = 'won' FROM deals WHERE id = 1100;

-- Partial credit: the share of the three deals moved.
SELECT count(*) / 3.0 FROM deals WHERE id IN (1042, 1043, 1057) AND owner_id = 7;

-- Gate, on the episode database: no other deal changed.
SELECT count(*) = 0 FROM changes WHERE tbl = 'deals' AND json_extract(pk, '$.id') <> 1100;

-- Gate: no email at all.
SELECT count(*) = 0 FROM effects WHERE kind = 'email';

-- Exactly one email, to one recipient, with this subject.
SELECT count(*) = 1 FROM effects
WHERE kind = 'email' AND json_array_length(data, '$.to') = 1
  AND json_extract(data, '$.to[0]') = 'marcus@acme.test'
  AND json_extract(data, '$.subject') = 'Deal 2001 is now yours';

-- The payment provider was charged 1,250.00 once.
SELECT count(*) = 1 FROM effects
WHERE kind = 'http' AND json_extract(data, '$.path') = '/stripe/v1/payment_intents'
  AND json_extract(data, '$.body.amount') = 125000;

-- Text the agent writes: match a keyword. It rejects paraphrases, such as "went with a rival".
SELECT count(*) = 1 FROM notes WHERE deal_id = 1043 AND text ~* 'competitor';

-- Inserted rows, by what they hold: four call activities on four deals.
SELECT count(DISTINCT json_extract(after, '$.deal_id')) / 4.0 FROM changes
WHERE tbl = 'activities' AND op = 'insert' AND json_extract(after, '$.kind') = 'call'
  AND json_extract(after, '$.deal_id') IN (1200, 1201, 1202, 1203);
```

A command check, for state outside the databases:

```yaml
- name: exported
  weight: 1
  command:
    - sh
    - -c
    - |
      file=/app/uploads/exports/deals-owner-7-open.csv
      want=$(psql "$MIROIR_DATABASE_URL" -tAc "SELECT count(*) + 1 FROM deals WHERE owner_id = 7 AND status = 'open'")
      test -f "$file" && test "$(wc -l < "$file")" -eq "$want"
```

- Check the end state, not the calls the agent made.
- Never check generated values (ids, tokens, timestamps) by their exact value; check what the row
  holds and how many there are.
- Compare dates with dates and timestamps with timestamps. A check that mixes them breaks with the
  time of day of the reset, which `check`'s `drift` stage catches.
- Use `setup` (SQL at each reset, before the apps start) for a state the seed lacks, such as
  `UPDATE users SET status = 'leaving' WHERE id = 12;`.

## Coworkers

A coworker is a colleague a model plays. The agent writes to them in the product, such as in a
note that mentions them; Miroir reads their inbox, has a model answer as them, and posts the
answer as them.

```yaml
instruction: Ask Priya Shah (priya) in a note on deal 1100 which stage it is really at, then set it.
max_steps: 10
coworkers:
  - name: priya
    description: Priya Shah, account executive at Acme (priya@acme.test)
    script: Deal 1100 fell back to qualified last week, since the buyer asked to re-scope it.
    headers: {crm: {Authorization: Bearer seed-token-priya}}
    inbox:
      sql: >-
        SELECT json_agg(json_build_object('id', id, 'text', text, 'deal', deal_id) ORDER BY id)
        FROM notes WHERE author_id = 1 AND text ~* '@priya\M'
    reply: {method: POST, path: "/deals/{deal}/notes", body: {text: "{text}"}}
checks:
  - name: set_after_asking
    weight: 1
    db: episode
    sql: >-
      SELECT EXISTS (SELECT 1 FROM changes WHERE tbl = 'deals' AND op = 'update'
                     AND json_extract(pk, '$.id') = 1100
                     AND json_extract(after, '$.stage') = 'qualified')
         AND EXISTS (SELECT 1 FROM changes WHERE tbl = 'notes' AND op = 'insert'
                     AND json_extract(after, '$.author_id') = 12)
allowed_changes:
  - {table: deals, op: update, columns: [stage, updated_at]}
  - {table: notes, op: insert}
  - {table: activities, op: insert}
golden:
  - {tool: api_request, arguments: {method: POST, path: /deals/1100/notes, body: {text: "@priya which stage is deal 1100 at?"}}}
  - {tool: coworker_reply, arguments: {coworker: priya, text: "Qualified: they asked to re-scope."}}
  - {tool: api_request, arguments: {method: PATCH, path: /deals/1100, body: {stage: qualified}}}
  - {tool: submit}
probes:
  - name: guesses-without-asking
    steps:
      - {tool: api_request, arguments: {method: PATCH, path: /deals/1100, body: {stage: qualified}}}
      - {tool: submit}
    max_reward: 0
```

The check requires the reply note (author 12 is Priya) as well as the stage, so the probe that
sets the stage without asking earns 0.

| Field | What |
|---|---|
| `name` | 1 to 64 lowercase letters, digits, `_` or `-` |
| `description` | who they are |
| `script` | what they know, and how they act |
| `headers` | their own headers, by app, which their replies send |
| `inbox.sql` | a query on the app's database returning a JSON array of messages, each with an `id` and a `text` |
| `inbox.app` | the database to query, when several apps have one |
| `reply` | the request that posts an answer: `app`, `method`, `path`, `body`; `{text}` is the answer, any other `{field}` comes from the last message |

Messages already in the inbox at reset are never answered. Each coworker needs a
`coworker_reply` step in the golden run, after the call that reaches them. Clients opt in: a
reset draws tasks with coworkers only with `coworkers: true`, which the adapters set once they
have a coworker model.

## Clock and events

A task has a clock when it has `events` or a check with `by`, and its agent then gets `wait`.
`wait` moves everything in the databases that many minutes into the past, as if the time had gone
by, and ends early at the next event.

```yaml
instruction: >-
  Priya Shah (priya) notes deal 1100 when its buyer signs. Within two hours of her note, set the
  deal's stage to won. Use wait to let time pass; it ends early when something happens.
max_steps: 10
events:
  - name: signed
    at: 90
    headers: {crm: {Authorization: Bearer seed-token-priya}}
    request: {method: POST, path: /deals/1100/notes, body: {text: The buyer signed.}}
checks:
  - name: won_in_time
    weight: 1
    by: 210
    db: episode
    sql: >-
      SELECT EXISTS (SELECT 1 FROM changes d, changes n
        WHERE d.tbl = 'deals' AND d.op = 'update' AND json_extract(d.pk, '$.id') = 1100
          AND json_extract(d.after, '$.stage') = 'won'
          AND n.tbl = 'notes' AND n.op = 'insert' AND json_extract(n.after, '$.author_id') = 12
          AND json_extract(d.after, '$.updated_at') >= json_extract(n.after, '$.created_at'))
allowed_changes:
  - {table: deals, op: update, columns: [stage, updated_at]}
  - {table: notes, op: insert}
  - {table: activities, op: insert}
golden:
  - {tool: wait, arguments: {minutes: 600}}
  - {tool: api_request, arguments: {method: PATCH, path: /deals/1100, body: {stage: won}}}
  - {tool: submit}
probes:
  - name: sets-it-before-the-note
    steps:
      - {tool: api_request, arguments: {method: PATCH, path: /deals/1100, body: {stage: won}}}
      - {tool: submit}
    max_reward: 0
  - name: sets-it-late
    steps:
      - {tool: wait, arguments: {minutes: 600}}
      - {tool: wait, arguments: {minutes: 121}}
      - {tool: api_request, arguments: {method: PATCH, path: /deals/1100, body: {stage: won}}}
      - {tool: submit}
    max_reward: 0
```

- An event (`name`, `at` from 1 to 525,600, sender `headers` by app, `request` with `app`,
  `method`, `path`, `body`) is sent at its minute. If it fails or answers 5xx, the episode has no
  reward.
- A weighted check with `by` is scored when the clock reaches that minute, and keeps that score.
  Gates can't take `by`.
- Before grading, the clock runs to the last event or milestone, so an early submit still meets
  them. A `wait` step in a golden run or probe takes exactly its `minutes`.
- A clock task can't take a `date` answer, or run in an env with `time.hook`.

## Tasks a seed writes

Checks name rows by the ids the database gave them. A `seed` command can plant the same kind of
work many times and write the task files to `/miroir/tasks` from the ids it reads back: bake
validates every task after the seed, so a bad one fails the build. Keep the generator
deterministic, so each bake writes the same tasks, and give its logic tests that need no
database. `examples/gitea/seed/` does this for 144 tasks; mount reviewed probes at
`/miroir/tasks/probes`, since the tasks live in the image.

## Tasks from traces

Traces of a production agent become tasks in three steps.

1. `miroir import --traces traces.jsonl ...` (see `references/manifest.md`) scrubs them. A trace
   is one JSON object per line: `id` (lowercase letters, digits, `_`, `-`), `consent` (traces
   without one are left out), `request`, `calls` (`api_request` and `api_docs` calls with their
   `arguments` and, optionally, the `status` they got), `answer`, and `persona` when the manifest
   has several.
2. `miroir draft out/traces.jsonl --world default --tasks tasks/` (host CLI) writes
   `tasks/<world>-<id>.yaml` per trace: the request as the instruction, the calls as the golden
   run, a placeholder check and `any_change: true`. It refuses a trace that mentions a date;
   rephrase it relative to today. `--split` sets the split.
3. Serve the env with the drafts mounted, then
   `miroir derive http://127.0.0.1:8000 out/traces.jsonl --world default --tasks tasks/`
   (`--token-var MIROIR_TOKEN` when the env has a token). Derive replays each draft twice from
   fresh resets, writes checks for what both replays agree on, a gate against any other change or
   effect, and the `allowed_changes` that cover them, and prints notes to review, such as a value
   the calls send that the request never shows.

Read each derived task, then run `miroir check`.
