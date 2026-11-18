---
title: "Manifest"
description: "Every key of /miroir/miroir.yaml, and how dates, sinks and quiet work."
---

The manifest is `/miroir/miroir.yaml` in the world image. For a worked example, see
[Write the manifest](/environment#write-the-manifest).

It is YAML, read strictly:

- An unknown key is an error. Every error names the field path and the fix, such as
  `processes[2].cwd: 'app' must be an absolute path, such as /app`.
- Commands are lists, `[program, arg, ...]`, run without a shell. Write `[sh, -c, "..."]` when
  you need one.

## Top-level keys

| Key | Required | What it is |
|---|---|---|
| `expose` | yes | The TCP ports the agent may reach, at least one. The only ports that may listen off loopback. |
| `processes` | yes | The product's datastores and processes, started in order ([Processes](#processes)). |
| `env` | no | Variables layered over the container's environment for every process and every `bake`, `seed` and `quiet` command, at bake, boot and collect. |
| `bake` | no | Commands run at build with only the datastores up: create databases, migrate, restore a dump. One hour each. |
| `seed` | no | Commands run at build with every process up, to build the world through the product's own API. One hour each. |
| `database` | yes | The graded database and what capture leaves out ([Database](#database)). |
| `sinks` | no | Recorders for the product's outgoing mail and HTTP ([Sinks](#sinks)). |
| `quiet` | no | A command that exits 0 when background work is done ([Quiet](#quiet)). |

## Processes

Each entry of `processes` is a long-running process. At least one must have `datastore: true`,
and every datastore comes before the other processes.

| Key | Required | What it is |
|---|---|---|
| `name` | yes | Lower-case letters, digits and `-`; unique. |
| `command` | yes | The command, as a list. |
| `user` | no | A user in `/etc/passwd`. The process runs as it, with `HOME` set to its home directory unless `env` or the process's `env` sets `HOME`. Without it the process runs as root. |
| `cwd` | no | An absolute path. |
| `env` | no | Variables for this process, over the top-level `env`. |
| `datastore` | no | Started first, before the date shift. |
| `ready` | no | `{tcp: PORT}` (a connection to 127.0.0.1) or `{http: URL}` (a `GET` on `http://127.0.0.1…` answering 2xx or 3xx). It must pass within 60 s. Leave it out for workers. |

## Database

| Key | Required | What it is |
|---|---|---|
| `database.url` | yes | `postgres://…/<database>`: the graded database, reached as a superuser. |
| `database.ignore` | no | Entries left out of capture: a table, named as the evidence names it (`schema.table` outside `public`), or `table.column`. A primary-key column cannot be ignored; ignore its table. |
| `database.time.exclude` | no | `table.column` entries never shifted, such as birthdays ([Dates](#dates)). |
| `database.time.epoch` | no | A map of `table.column` globs (`*` for any run of characters) to `s` or `ms`: `integer` or `bigint` columns holding Unix time, shifted like timestamps. Each glob must match a column. |
| `database.time.captured` | no | An RFC 3339 timestamp: when the data the `bake` commands restore was captured ([Restored data](#restored-data)). |

## Dates

A world is seeded once, at build time, but tasks talk about "overdue" invoices or deals "stale for
30 days". To keep those true, every boot moves the database's dates from the bake to the day of
the trial:

| Columns | Move by |
|---|---|
| `timestamp` and `timestamptz`, and the Unix-time columns of `database.time.epoch` | Whole days elapsed since the bake |
| `date` | UTC calendar days since the bake |

These values stay where they are:

- columns listed in `database.time.exclude`;
- generated columns;
- values on or after `9000-01-01`, such as `9999-12-31` for "never";
- Unix times of 0 or less.

The shift runs with triggers disabled, so a product trigger that stamps `updated_at` does not
fire.

Whole days and calendar days differ for part of each day. An image baked at 23:00 UTC and booted
at 01:00 the next day moves dates by 1 and timestamps by 0. That is why a check never compares a
date column with a timestamp column ([authoring rules](/spec#authoring-rules)).

If a constraint refuses a moved value, the boot fails and names the table. List the column in
`database.time.exclude`.

### Restored data

`database.time.captured` is for data restored from a dump. The bake moves the restored rows from
that time to the bake's start, so a dump taken weeks before the build keeps its relative ages. The
seed's writes come after and are not moved. A captured time later than the bake's start is
refused.

## Sinks

The product cannot reach the internet: its compose network is `internal: true`. Instead, point its
outgoing mail and HTTP at the sinks, recorders that listen on loopback. What they record becomes
the `sent_emails` and `sent_requests` tables of the [evidence](/evidence).

| Key | Required | What it is |
|---|---|---|
| `sinks.smtp` | no | Port of the SMTP recorder. |
| `sinks.http` | no | Port of the HTTP recorder; not the SMTP port. |
| `sinks.responses` | no | Canned answers for HTTP requests. |

- **SMTP** records each email: the sender, the `to` and `cc` recipients, the envelope recipients
  missing from the headers (as `bcc`), the subject, the text and the HTML. Point the product's
  SMTP settings at `127.0.0.1:<port>`; the fixture sets `SMTP_URL=smtp://127.0.0.1:2525`.
- **HTTP** records each request: method, path, query parameters, headers (names lower-case,
  `authorization` dropped) and body (JSON as JSON, other text as a JSON string, an empty body as
  `null`). Point base URLs at `http://127.0.0.1:<port>`. The fixture sets
  `STRIPE_API_BASE=http://127.0.0.1:9000/stripe`, and Gitea allows webhooks to `127.0.0.1:9000`.
- **Responses** are keyed `"[METHOD ]/path[*]"`, each `{status, json | body, headers}`. An exact
  path beats a prefix (a key ending in `*`), a longer prefix beats a shorter one, and a key with a
  method beats one without. Unmatched requests get 200 with an empty body.

An email over 25 MiB is recorded truncated, and a request body over 25 MiB is refused; both add a
`sink` fault. The sinks answer during the bake but record only from the boot, and the boot clears
what start-up sent.

## Quiet

Products do work in the background: a queued email, a webhook delivery, a search index. `quiet` is
a command that exits 0 when that work is done, so the evidence holds everything the agent's
requests set off.

Miroir waits until `quiet` passes twice in a row, within 30 s, at bake, at boot and at collect. At
bake and boot a timeout fails; at collect it is recorded as a `quiet_timeout` fault. The fixture
checks two Redis queue lengths; Gitea runs `gitea manager flush-queues`.

- **Turn schedulers and cron off** in `env` (`SCHEDULER_ENABLED: "false"`), so nothing changes
  while the world idles: a run where the agent does nothing must change nothing.
- **Don't leave a background process holding the command's output open.** Miroir reads the
  command's stderr to explain a failure, so such a run waits out the full 30 s.
