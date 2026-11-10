# Clock and events

Some tasks need time to pass: "When the buyer signs, mark the deal won within two hours." A
task has a clock when it has `events`, or a check with `by`. Its agent then gets the `wait`
tool.

```yaml
instruction: >-
  Priya Shah notes deal 1100 when its buyer signs. Within two hours of her note, set the
  deal's stage to won. Use wait to let time pass; it ends early when something happens.
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
        WHERE d.tbl = 'deals' AND json_extract(d.after, '$.stage') = 'won'
          AND n.tbl = 'notes' AND n.op = 'insert' AND json_extract(n.after, '$.author_id') = 12
          AND json_extract(d.after, '$.updated_at') >= json_extract(n.after, '$.created_at'))
allowed_changes:
  - {table: deals, op: update, columns: [stage, updated_at]}
  - {table: notes, op: insert}
golden:
  - {tool: wait, arguments: {minutes: 600}}
  - {tool: api_request, arguments: {method: PATCH, path: /deals/1100, body: {stage: won}}}
  - {tool: submit}
```

## `wait`

`wait` takes `minutes`, from 1 to 10080 (a week). Everything in the databases moves that many
minutes into the past, as if the time had gone by, while the apps keep reading the real clock.
The wait ends early at the first event. It returns how long it lasted, `waited_minutes`, and
the minutes since the task began, `elapsed_minutes`.

## Events

An event is a request sent at clock minute `at`, as someone other than the agent. `request`
takes `app`, `method`, `path` and `body`; `headers` are the sender's, by app. If an event's
request fails or answers 5xx, the episode has no reward.

## Milestones

A weighted check with `by` is scored when the clock reaches that minute, and keeps that score.
Above, the deal must be won by minute 210: two hours after the note at minute 90. Gates can't
take `by`, since a gate says what must not happen by the end.

## Before grading

The clock runs to the last event or milestone before grading, so an agent that submits early
still meets every event, and every milestone gets a score. The episode's metadata has
`clock`: the minutes elapsed, and each event with its minute and status.

## Rules

- A `wait` step in a golden run or probe takes exactly `minutes`.
- Clock minutes go up to 525,600, a year.
- A task with a clock can't take a `date` answer, and can't run in an env with `time.hook`.
