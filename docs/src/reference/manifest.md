# Manifest

The manifest is `/miroir/miroir.yaml`. Unknown fields are errors. A command is a list of
strings, `[program, arg, ...]`, run without a shell unless you call one.

## Top level

| Field | Default | What |
|---|---|---|
| `miroir` | required | format version; must be `1` |
| `name` | required | letters, digits, `.`, `_` and `-`; the env's name in routes |
| `description` | required | what the product is; `/metadata` shows it |
| `processes` | required | what Miroir starts, in order |
| `env` | `{}` | variables for every process and command Miroir runs |
| `bake` | `[]` | commands that create the seed data, with only the datastores up |
| `seed` | `[]` | commands run at bake once the apps are up |
| `worlds` | `[default]` | the [worlds](../features/worlds.md), each baked apart |
| `state` | derived | the directories each reset restores |
| `apps` | required | the apps, by name |
| `personas` | required | the users the agent can act as, by name |
| `sinks` | none | the [sinks](../features/sinks.md) |
| `quiet` | none | the command that tells when background work is done |
| `time` | `anchor: bake` | how the data's [dates](../features/dates.md) follow the clock |
| `system_prompt` | none | a system prompt the task listings carry, for the adapters |

Bake and seed commands get `MIROIR_WORLD`, and may run for up to an hour each.

## `processes[]`

| Field | Default | What |
|---|---|---|
| `name` | required | unique |
| `command` | required | the command to run |
| `user` | root | the user it runs as; root fails `check` unless written `user: root` |
| `cwd` | none | an absolute working directory |
| `env` | `{}` | variables for this process |
| `datastore` | `false` | started first, and alone while a reset restores and moves the data |
| `ready` | none | how Miroir tells it is ready |

Datastores come before every other process, and there is at least one.

`ready` takes exactly one of:

| Field | What |
|---|---|
| `tcp` | a port that accepts connections on loopback |
| `http` | an `http://` URL that answers 2xx or 3xx |

and `timeout_s`, the seconds to wait: 60 by default, up to 86,400.

## `state`

Absolute directories or files, not nested in each other and not inside `/miroir`. Each reset
restores them, and empties `/tmp`, `/var/tmp` and `/dev/shm`.

Left out, the state is every path the processes' users can write. Every process then needs a
non-root `user`.

## `apps.<name>`

App names are lowercase letters, digits, `_` and `-`, starting with a letter or a digit. Each
app has an `api`, an `mcp` server, a `database`, or several. At least one app has an `api`,
and at least one has a `database`.

| Field | What |
|---|---|
| `description` | what the app is; required when several apps have an `api` |
| `api.base_url` | the API's `http://` URL on loopback |
| `api.openapi` | the absolute path of its OpenAPI spec, JSON or YAML, which `api_docs` serves |
| `api.omit` | keys dropped from its JSON answers, at any depth: `key`, or `parent.key` for a key of an object under `parent`; `*` matches any run of characters, and `{a,b}` either |
| `mcp.url` | the `http://` URL on loopback of its [MCP server](../features/mcp.md); one app at most |
| `database.url` | a `postgres://`, `postgresql://` or `mysql://` URL that names a database |
| `database.schemas` | the schemas to read; all of them when unset |
| `database.ignore` | tables, or `table.column`s, whose changes never count |
| `database.time.exclude` | `table.column`s whose times never move |
| `database.time.epoch` | `table.column` globs of integer Unix-time columns, each with its unit, `s` or `ms` |

The database user must be able to create triggers. A key column can't be ignored, and an
`epoch` pattern must match a 32- or 64-bit integer column.

## `personas.<name>`

| Field | What |
|---|---|
| `description` | who the agent is, as the prompt says |
| `headers` | headers by app, pinned on every request to that app's API or MCP server |

Pinned headers win over the agent's. When a persona pins headers for an app, the agent's own
`Cookie` header to that app is dropped.

## `sinks`

| Field | What |
|---|---|
| `smtp` | the SMTP sink's port |
| `http` | the HTTP sink's port, not the same as `smtp` |
| `responses` | canned answers by `/path` or `METHOD /path`, where a trailing `*` matches a prefix |

A canned answer has `status` (200 by default), `json` or `body` (not both), and `headers`.

## `quiet`

| Field | Default | What |
|---|---|---|
| `command` | required | a command that exits 0 once the apps' background work is done |
| `timeout_s` | 30 | how long to wait for two passes in a row, up to 86,400 |

## `time`

| Field | Default | What |
|---|---|---|
| `anchor` | `bake` | when the seed data is dated: `bake`, or an RFC 3339 instant |
| `hook` | none | a command each reset runs to move dates outside the databases |
| `captured` | `{}` | when each captured world's snapshot was taken, by world |

`hook` gets `MIROIR_SHIFT_DAYS` and `MIROIR_SHIFT_DATE_DAYS`, and can't go with `captured`.
