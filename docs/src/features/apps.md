# Several apps

An env can hold several apps, such as two offices' CRMs and a ledger. Each app has its own
API, database or both.

```yaml
apps:
  east:
    description: the East office's CRM
    api: {base_url: "http://127.0.0.1:3000", openapi: /app/openapi.yaml}
    database: {url: "postgres://postgres@127.0.0.1:5432/east"}
  west:
    description: the West office's CRM
    api: {base_url: "http://127.0.0.1:3001"}
    database: {url: "postgres://postgres@127.0.0.1:5432/west"}
  ledger:
    database: {url: "postgres://postgres@127.0.0.1:5432/ledger"}
personas:
  sales_manager:
    description: Dana Lee in the East office, Alex Kim in the West
    headers:
      east: {Authorization: Bearer seed-token-dana}
      west: {Authorization: Bearer seed-token-alex}
```

[`fixtures/acme-apps`](https://github.com/raphaelDkhn/miroir/tree/master/fixtures/acme-apps)
is the full example.

## Naming the app

Name `app` wherever several apps could be meant, and only there:

| When several apps have | `app` is required in |
|---|---|
| an API | `api_request` and `api_docs` calls, golden and probe steps, event and reply requests |
| a database | `sql` checks, `allowed_changes` entries, coworker inboxes |

Each app with an API then needs a `description`: the tools show it to the agent.

```yaml
checks:
  - {name: won, weight: 0.5, app: east, sql: "SELECT status = 'won' FROM deals WHERE id = 1100"}
  - {name: noted, weight: 0.5, db: episode,
     sql: "SELECT count(*) = 1 FROM changes WHERE app = 'west' AND tbl = 'notes'"}
allowed_changes:
  - {app: east, table: deals, op: update, columns: [status, stage, updated_at]}
  - {app: west, table: notes, op: insert}
golden:
  - {tool: api_request, arguments: {app: east, method: PATCH, path: /deals/1100, body: {status: won}}}
  - {tool: api_request, arguments: {app: west, method: POST, path: /deals/1100/notes, body: {text: East closed it.}}}
  - {tool: submit}
```

With several databases, the `app` column of the `changes` table names each change's app. A
command check gets the database of its `app` as `MIROIR_DATABASE_URL`.

## Rules

- Two apps can't name the same database. Give the second app only an `api`; its changes count
  under the first.
- Two MySQL databases on one server can't be told apart. Give each its own server.
- Only one app may have an MCP server.
- A task's `setup` runs on the only database, so it can't be used with several.
- In the [sandbox](sandbox.md), each app's API is at `$API_<APP>`: the name in capitals, with
  `-` as `_`. Two apps can't map to the same variable.
