import http from "node:http";
import { randomUUID } from "node:crypto";

const PORT = Number(process.env.MCP_PORT ?? 3100);
const API = process.env.CRM_API ?? "http://127.0.0.1:3000";
const VERSIONS = ["2025-06-18", "2025-03-26"];
const sessions = new Set();

const deal = { deal_id: { type: "integer", description: "The deal's id" } };
const tools = [
  { name: "whoami", description: "The CRM user this server acts as.",
    inputSchema: { type: "object", properties: {} } },
  { name: "get_deal", description: "One deal.",
    inputSchema: { type: "object", properties: deal, required: ["deal_id"] } },
  { name: "update_deal", description: "Change a deal's status or stage.",
    inputSchema: { type: "object", required: ["deal_id"],
      properties: { ...deal, status: { type: "string" }, stage: { type: "string" } } } },
];

async function api(method, path, authorization, body) {
  const headers = { authorization: authorization ?? "", "content-type": "application/json" };
  const response = await fetch(`${API}${path}`, { method, headers, body: JSON.stringify(body) });
  return { ok: response.ok, text: await response.text() };
}

async function call(name, args, authorization) {
  const id = Number(args.deal_id);
  const calls = {
    whoami: () => api("GET", "/me", authorization),
    get_deal: () => api("GET", `/deals/${id}`, authorization),
    update_deal: () => api("PATCH", `/deals/${id}`, authorization,
      { status: args.status, stage: args.stage }),
  };
  if (!calls[name]) return { error: { code: -32602, message: `Unknown tool: ${name}` } };
  const { ok, text } = await calls[name]();
  return { result: { content: [{ type: "text", text }], isError: !ok } };
}

function send(res, status, body, headers = {}) {
  res.writeHead(status, { "content-type": "application/json", ...headers });
  res.end(JSON.stringify(body));
}

http.createServer(async (req, res) => {
  if (req.url === "/healthz") return send(res, 200, { ok: true });
  if (req.method !== "POST" || req.url !== "/mcp") return send(res, 405, { error: "POST /mcp" });
  const chunks = [];
  for await (const chunk of req) chunks.push(chunk);
  const message = JSON.parse(Buffer.concat(chunks).toString("utf8"));
  const reply = (fields) => ({ jsonrpc: "2.0", id: message.id, ...fields });
  if (message.method === "initialize") {
    const asked = message.params?.protocolVersion;
    const session = randomUUID();
    sessions.add(session);
    const result = { protocolVersion: VERSIONS.includes(asked) ? asked : VERSIONS[0],
      capabilities: { tools: {} }, serverInfo: { name: "acme-crm-mcp", version: "1.0.0" } };
    return send(res, 200, reply({ result }), { "mcp-session-id": session });
  }
  if (!sessions.has(req.headers["mcp-session-id"])) return send(res, 404, { error: "no session" });
  if (message.id === undefined) {
    res.writeHead(202);
    return res.end();
  }
  if (message.method === "tools/list") return send(res, 200, reply({ result: { tools } }));
  if (message.method !== "tools/call") {
    return send(res, 200, reply({ error: { code: -32601, message: "Method not found" } }));
  }
  const answer = await call(message.params.name, message.params.arguments ?? {},
    req.headers.authorization);
  res.writeHead(200, { "content-type": "text/event-stream" });
  res.end(`event: message\ndata: ${JSON.stringify(reply(answer))}\n\n`);
}).listen(PORT, "127.0.0.1", () => console.log(`acme-crm MCP server listening on ${PORT}`));
