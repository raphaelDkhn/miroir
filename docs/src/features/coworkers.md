# Coworkers

A coworker is a colleague a model plays. The agent writes to them in the product, such as in a
note that mentions them. Miroir reads their inbox, has a model answer as them, and posts the
answer in the product as them.

```yaml
instruction: Ask Priya Shah (priya) in a note on deal 1100 which stage it is really at, then set it.
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
golden:
  - {tool: api_request, arguments: {method: POST, path: /deals/1100/notes, body: {text: "@priya which stage is deal 1100 at?"}}}
  - {tool: coworker_reply, arguments: {coworker: priya, text: "Qualified: they asked to re-scope."}}
  - {tool: api_request, arguments: {method: PATCH, path: /deals/1100, body: {stage: qualified}}}
  - {tool: submit}
```

## Fields

| Field | What |
|---|---|
| `name` | 1 to 64 lowercase letters, digits, `_` or `-` |
| `description` | who they are |
| `script` | what they know, and how they act |
| `headers` | their own headers, by app, which their replies send |
| `inbox.sql` | a query on the app's database that returns a JSON array of messages, each with an `id` and a `text` |
| `inbox.app` | the database to query, when several apps have one |
| `reply` | the request that posts an answer: `app`, `method`, `path`, `body` |

In `reply`, `{text}` is the answer, and any other `{field}` comes from the last message the
inbox returned. Above, `{deal}` comes from the message's `deal`.

## How a turn goes

1. Messages already in the inbox at reset are never answered.
2. The client asks for turns with a `coworkers` frame. Miroir reads each inbox. A coworker
   with new messages owes a reply, and their turn carries the chat to answer: a system prompt
   made from their `description` and `script`, then the last 20 messages.
3. The client sends that chat to a model and returns the answer in a `coworker_reply` frame.
   Miroir posts it through `reply`, as the coworker.

Messages and answers are cut at 4,000 characters. The adapters answer the owed turns between
the agent's calls. See the [protocol](../run/protocol.md#coworker-frames).

## Clients must opt in

A reset draws tasks with coworkers only when it says `coworkers: true`, which tells Miroir
the client answers them. Naming such a task without it is an error. Both adapters opt in once
they have a coworker model, any OpenAI-compatible chat endpoint:

- TRL: `environment_factory(..., coworker=CoworkerModel(base_url, model, api_key))`, and
  `dataset(..., coworkers=True)`.
- verifiers: `--env.taskset.coworker_model`, `--env.taskset.coworker_base_url` and
  `--env.taskset.coworker_api_key_var`.

## Golden runs

A `coworker_reply` step, with a `coworker` and a `text`, stands for the coworker's answer.
`check` posts that text as their reply. Each coworker needs one in the golden run, after the
call that reaches them.
