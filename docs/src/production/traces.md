# Tasks from traces

Traces of a production agent show real requests and how they were solved. Three steps turn
them into tasks:

1. `miroir import --traces` scrubs them, along with the dumps.
2. `miroir draft` writes a draft task for each trace.
3. `miroir derive` replays each draft on the env and writes the checks its outcome holds to.

## Trace format

A JSON object per line:

```json
{"id": "t-1042", "consent": "support-terms-2026", "persona": "sales_manager",
 "request": "Charge the card on file for invoice INV-04001.",
 "calls": [{"tool": "api_request", "arguments": {"method": "POST", "path": "/invoices/4001/pay"}, "status": 200}],
 "answer": "Charged INV-04001."}
```

| Field | What |
|---|---|
| `id` | the trace's id: lowercase letters, digits, `_` and `-` |
| `consent` | the basis on which the trace may be used; traces without one are left out |
| `request` | what the user asked |
| `calls` | the `api_request` and `api_docs` calls, each with its `arguments` and, optionally, the `status` it got |
| `answer` | the agent's answer, if any |
| `persona` | the persona that made the calls, if the manifest has several |

## Scrub

Import drops credential headers from the calls and scrubs every string as text. Name the
argument fields that hold a name, an email, a phone or a secret under `fields` in the scrub
spec, so their values are scrubbed everywhere they appear:

```yaml
apps:
  crm: {contacts: {name: name, email: email}}
fields:
  contact_email: email
```

## Draft

```sh
miroir draft out/traces.jsonl --world default --tasks tasks/
```

This writes `tasks/<world>-<id>.yaml` for each trace: the request as the instruction, the
calls as the golden run, a placeholder check and `any_change: true`. `--split` sets the split,
`train` by default. A trace that mentions a date is refused, since a reset moves the data's
dates but not the task's text; rephrase it relative to today.

## Derive

Serve the env with the drafts, then derive:

```sh
docker run --rm -d -p 127.0.0.1:8000:8000 -v "$PWD/tasks:/miroir/tasks:ro" my-env
miroir derive http://127.0.0.1:8000 out/traces.jsonl --world default --tasks tasks/
```

For each draft, derive:

1. Replays the calls, and keeps those that work on the snapshot.
2. Replays the kept calls again, from a fresh reset.
3. Writes checks for what both replays agree on, a gate against any other change or effect,
   and the `allowed_changes` that cover them.

The finished task replaces the draft. Derive prints notes to review, such as a value the calls
send that the request never shows, which an agent could not know. It refuses a trace whose
calls worked in production but fail on the snapshot, or the reverse. `--token-var` names the
variable that holds the env's `MIROIR_TOKEN`.

Read each derived task, then run [`miroir check`](../make/check.md).
