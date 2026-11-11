# Agent tools

Every call counts toward the task's `max_steps`, and is recorded in the `actions` table of the
[episode database](episode-db.md). With several apps, `api_request` and `api_docs` take a
required `app`, listed with each app's description.

## `api_request`

Sends an HTTP request to the app's API as the persona.

| Argument | What |
|---|---|
| `method` | `GET`, `POST`, `PUT`, `PATCH` or `DELETE` |
| `path` | the path and query string, starting with `/`, such as `/deals?owner_id=12` |
| `body` | sent as JSON; with a `content-type` header, a string is sent as-is, and an object is form-encoded for `application/x-www-form-urlencoded` |
| `headers` | extra headers, as an object of strings; at most 100 |

Returns the `status`, the `content-type`, `location` and `link` `headers`, and the `body`:
parsed when it is JSON, without the keys `api.omit` names, else text, or `null` when empty.
A body over 64 KiB is cut to fit. A JSON array keeps its first items, and `truncated` says how
many of how many; any other body is cut as text, with `truncated: true`.

The persona's headers win over the agent's. Paths can't leave the API, such as with `..`.
Redirects are not followed, and a request times out after 30 seconds.

## `api_docs`

Looks up the API in its OpenAPI spec. Listed when an app sets `api.openapi`.

| Argument | What |
|---|---|
| `operation` | an operation ID, or `METHOD /path` such as `GET /deals/1100`; leave it out to list every operation |

Without `operation`, returns `operations`: each operation's `operation` ID, `method`, `path`
and `summary`. With one, returns its `summary`, `description`, `parameters`, `request_body`
and `responses`, with `$ref`s resolved.

## `run_code`

Runs a bash command in the [sandbox](../features/sandbox.md). Listed on the `code` surface, in
place of `api_request`, when the env has a sandbox.

| Argument | What |
|---|---|
| `command` | the bash command, up to 1 MiB |

Returns `exit_code` and `output` (stdout and stderr), with `timed_out` or `truncated` when
the 60-second or 64 KiB limit cut it.

## `wait`

Lets time pass in the apps. Listed for tasks with a [clock](../features/clock.md).

| Argument | What |
|---|---|
| `minutes` | a whole number from 1 to 10,080 |

Returns `waited_minutes` and `elapsed_minutes`. It ends early when an event fires.

## `submit`

Ends the episode, which is then graded.

| Argument | What |
|---|---|
| `answer` | the answer the task asks for, if any, as a string |

Returns `submitted: true`. When the task has an `answer` form and the answer doesn't fit it,
the call fails with `invalid_args`, and the episode goes on.

## The app's MCP tools

On the `mcp` surface, the app's [MCP server](../features/mcp.md) tools replace `api_request`
and `api_docs`. Miroir passes the arguments through, with the persona's headers.

## Errors

A failed call returns `result: null` and an `error`:

| `error_type` | When |
|---|---|
| `invalid_args` | the arguments are wrong |
| `tool_not_found` | the tool doesn't exist in this env or task |
| `execution_error` | the call failed, such as a refused connection |
| `transport_error` | the sandbox could not be reached |
| `timeout` | the call took too long, or the episode's `max_seconds` ran out |
