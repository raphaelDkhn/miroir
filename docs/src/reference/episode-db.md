# Episode database

The episode database is a SQLite file, `/miroir/episode.db`, emptied at each reset. Checks
with `db: episode` query it, and command checks find its path in `MIROIR_EPISODE_DB`. JSON
columns hold text; read them with SQLite's `json_extract`.

```sql
CREATE TABLE actions (seq INTEGER PRIMARY KEY, tool TEXT NOT NULL, app TEXT, method TEXT,
                      path TEXT, status INTEGER, latency_ms INTEGER NOT NULL,
                      arguments TEXT NOT NULL, result TEXT, via TEXT);
CREATE TABLE effects (seq INTEGER PRIMARY KEY, kind TEXT NOT NULL, data TEXT NOT NULL);
CREATE TABLE changes (app TEXT, tbl TEXT NOT NULL, op TEXT NOT NULL, pk TEXT NOT NULL,
                      before TEXT, after TEXT, columns TEXT);
CREATE TABLE answer (value TEXT);
```

## `changes`

The episode's net changes, written before the checks run.

| Column | What |
|---|---|
| `app` | the app, when several have a database; else NULL |
| `tbl` | the table, schema-qualified outside the default schema |
| `op` | `insert`, `update`, `delete`, `truncate` or `schema` |
| `pk` | the row's key columns, as a JSON object; `{}` for a table without a key |
| `before` | the row before, as a JSON object; NULL for an insert |
| `after` | the row after, as a JSON object; NULL for a delete |
| `columns` | for an update, the columns that changed, as a JSON array |

## `effects`

The emails and HTTP calls the [sinks](../features/sinks.md) caught, in order.

| `kind` | `data` |
|---|---|
| `email` | `from`, `to`, `cc`, `bcc` (lists of addresses), `subject`, `text`, `html` |
| `http` | `method`, `path`, `query` (an object), `headers` (without `Authorization`), `body` |

## `actions`

Every tool call, in order. `arguments` and `result` are JSON; `result` holds the error when
the call failed. Calls the agent didn't make itself have a `via`:

| `via` | `tool` | What |
|---|---|---|
| NULL | the tool | a call of the agent |
| `run_code` | `api_request` | a request the agent's code sent |
| `coworker` | `coworker_reply` | a coworker's reply |
| `event` | `event` | a clock event |

## `answer`

The submitted answer, in its normal form when the task has an `answer` form.

## Examples

```sql
-- Only invoice 4001 changed.
SELECT count(*) = 0 FROM changes WHERE tbl = 'invoices' AND json_extract(pk, '$.id') <> 4001;

-- Marcus got exactly one email.
SELECT count(*) = 1 FROM effects
WHERE kind = 'email' AND json_extract(data, '$.to[0]') = 'marcus@acme.test';

-- The payment provider was charged 1,250.00 once.
SELECT count(*) = 1 FROM effects
WHERE kind = 'http' AND json_extract(data, '$.path') = '/stripe/v1/payment_intents'
  AND json_extract(data, '$.body.amount') = 125000;

-- The answer is 3.
SELECT value = '3' FROM answer;
```
