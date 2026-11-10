# MCP servers

If the product has an MCP server, the agent can use its tools in place of the HTTP API. Name
the server under its app:

```yaml
apps:
  crm:
    api: {base_url: "http://127.0.0.1:3000", openapi: /app/openapi.yaml}
    mcp: {url: "http://127.0.0.1:3100/mcp"}
    database: {url: "postgres://postgres@127.0.0.1:5432/crm"}
```

The URL must be plain `http://` on loopback. Only one app may have an MCP server, and the
manifest still needs an app with an `api`.

## How the agent uses it

Clients pick the `mcp` surface: the env's MCP endpoint at `/mcp?surface=mcp` then lists the
server's tools, plus `submit` and, for tasks with a clock, `wait`. Miroir calls the server
with the persona's headers for that app, and records each call in `actions` like any other.
Over the WebSocket, the agent can call the server's tools by name.

Miroir lists the server's tools once, and opens a new MCP session after every reset. A tool
can't be named like one of Miroir's own: `api_request`, `api_docs`, `submit`, `run_code`,
`wait` or `coworker_reply`.

## In tasks

Golden runs and probes can call the server's tools:

```yaml
probes:
  - name: through-the-crm-mcp-server
    steps:
      - {tool: get_deal, arguments: {deal_id: 1100}}
      - {tool: update_deal, arguments: {deal_id: 1100, status: won, stage: won}}
      - {tool: submit, arguments: {answer: "Deal 1100 is won."}}
    min_reward: 1
```

`check` lists the server's tools in its `mcp` stage, and fails if it lists none.
