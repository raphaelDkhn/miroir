import http from "node:http";
import { mkdir, writeFile } from "node:fs/promises";
import { Queue } from "bullmq";
import { query, transaction } from "./db.js";

const PORT = Number(process.env.PORT ?? 3000);
const UPLOADS = "/app/uploads";
const STRIPE = process.env.STRIPE_API_BASE ?? "https://api.stripe.com";
const emails = new Queue("email", { connection: { url: process.env.REDIS_URL } });

class HttpError extends Error {
  constructor(status, message) {
    super(message);
    this.status = status;
  }
}
const fail = (status, message) => {
  throw new HttpError(status, message);
};

const directory = new Map();
for (const user of await query("SELECT id, name, email, role, status FROM users ORDER BY id")) {
  directory.set(user.id, user);
}

const routes = [];
const on = (method, pattern, handler) => routes.push({
  method, handler, pattern: new RegExp(`^${pattern.replace(/:(\w+)/g, "(?<$1>\\d+)")}$`),
});

function page(url) {
  const limit = Math.min(Number(url.searchParams.get("limit") ?? 50) || 50, 200);
  const offset = Math.max(Number(url.searchParams.get("offset") ?? 0) || 0, 0);
  return { limit, offset };
}

function listed(url, rows, { limit, offset }) {
  if (rows.length <= limit) return [200, rows];
  const next = new URL(url);
  next.searchParams.set("offset", String(offset + limit));
  next.searchParams.set("limit", String(limit));
  return [200, rows.slice(0, limit), { link: `<${next.pathname}${next.search}>; rel="next"` }];
}

const param = (url, name) => url.searchParams.get(name) || null;
const int = (url, name) => (param(url, name) === null ? null : Number(param(url, name)));

function fields(body, allowed) {
  const picked = Object.fromEntries(Object.entries(body ?? {}).filter(([k]) => allowed.includes(k)));
  if (Object.keys(picked).length === 0) fail(422, `send at least one of: ${allowed.join(", ")}`);
  return picked;
}

async function update(table, id, values) {
  const keys = Object.keys(values);
  const sets = keys.map((key, i) => `${key} = $${i + 2}`).join(", ");
  const [row] = await query(`UPDATE ${table} SET ${sets} WHERE id = $1 RETURNING *`,
    [id, ...keys.map((key) => values[key])]);
  return row ?? fail(404, `${table.slice(0, -1)} ${id} not found`);
}

const withOwner = (deal) => ({ ...deal, owner_name: directory.get(deal.owner_id)?.name ?? null });
const DEAL = `SELECT d.*, (SELECT max(occurred_at) FROM activities a WHERE a.deal_id = d.id)
  AS last_activity_at FROM deals d`;

async function getDeal(id) {
  const [deal] = await query(`${DEAL} WHERE d.id = $1`, [id]);
  return deal ? withOwner(deal) : fail(404, `deal ${id} not found`);
}

on("GET", "/me", ({ user }) => [200, user]);
on("GET", "/users", ({ url }) => [200, [...directory.values()].filter((u) =>
  (!param(url, "role") || u.role === param(url, "role")) &&
  (!param(url, "status") || u.status === param(url, "status")))]);
on("GET", "/users/:id", ({ params }) =>
  [200, directory.get(Number(params.id)) ?? fail(404, `user ${params.id} not found`)]);

on("GET", "/contacts", async ({ url }) => {
  const p = page(url);
  const rows = await query(`SELECT * FROM contacts WHERE ($1::int IS NULL OR owner_id = $1)
    AND ($2::text IS NULL OR name ILIKE '%' || $2 || '%' OR company ILIKE '%' || $2 || '%')
    ORDER BY id LIMIT $3 OFFSET $4`, [int(url, "owner_id"), param(url, "q"), p.limit + 1, p.offset]);
  return listed(url, rows, p);
});
on("GET", "/contacts/:id", async ({ params }) => {
  const [contact] = await query("SELECT * FROM contacts WHERE id = $1", [params.id]);
  return [200, contact ?? fail(404, `contact ${params.id} not found`)];
});
on("POST", "/contacts", async ({ body }) => {
  const c = fields(body, ["name", "email", "company", "phone", "owner_id"]);
  if (!c.name) fail(422, "name is required");
  const [row] = await query(`INSERT INTO contacts (name, email, company, phone, owner_id)
    VALUES ($1, $2, $3, $4, $5) RETURNING *`, [c.name, c.email, c.company, c.phone, c.owner_id]);
  return [201, row, { location: `/contacts/${row.id}` }];
});
on("PATCH", "/contacts/:id", async ({ params, body }) =>
  [200, await update("contacts", params.id, fields(body, ["name", "email", "company", "phone",
    "owner_id"]))]);
on("POST", "/contacts/:id/merge", async ({ params, body }) => {
  const keep = Number(params.id);
  const duplicate = Number(body?.duplicate_id);
  if (!duplicate || duplicate === keep) fail(422, "duplicate_id must name another contact");
  return [200, await transaction(async (client) => {
    const found = await client.query("SELECT id FROM contacts WHERE id = ANY($1)", [[keep, duplicate]]);
    if (found.rowCount !== 2) fail(404, "both contacts must exist");
    await client.query("UPDATE deals SET contact_id = $1 WHERE contact_id = $2", [keep, duplicate]);
    await client.query("DELETE FROM contacts WHERE id = $1", [duplicate]);
    return (await client.query("SELECT * FROM contacts WHERE id = $1", [keep])).rows[0];
  })];
});

on("GET", "/deals", async ({ url }) => {
  const p = page(url);
  const rows = await query(`${DEAL} WHERE ($1::int IS NULL OR d.owner_id = $1)
    AND ($2::text IS NULL OR d.status = $2) AND ($3::text IS NULL OR d.stage = $3)
    AND ($4::int IS NULL OR d.contact_id = $4)
    AND ($5::int IS NULL OR NOT EXISTS (SELECT 1 FROM activities a WHERE a.deal_id = d.id
      AND a.occurred_at > now() - make_interval(days => $5)))
    ORDER BY d.id LIMIT $6 OFFSET $7`, [int(url, "owner_id"), param(url, "status"),
    param(url, "stage"), int(url, "contact_id"), int(url, "inactive_days"), p.limit + 1, p.offset]);
  return listed(url, rows.map(withOwner), p);
});
on("GET", "/deals/:id", ({ params }) => getDeal(params.id).then((deal) => [200, deal]));
on("POST", "/deals", async ({ body, user }) => {
  const d = fields(body, ["name", "contact_id", "owner_id", "stage", "amount", "expected_close"]);
  if (!d.name) fail(422, "name is required");
  const [row] = await query(`INSERT INTO deals (name, contact_id, owner_id, stage, amount,
    expected_close) VALUES ($1, $2, $3, coalesce($4, 'lead'), coalesce($5, 0), $6) RETURNING id`,
  [d.name, d.contact_id, d.owner_id ?? user.id, d.stage, d.amount, d.expected_close]);
  return [201, await getDeal(row.id), { location: `/deals/${row.id}` }];
});
on("PATCH", "/deals/:id", async ({ params, body, url }) => {
  const changes = fields(body, ["name", "contact_id", "owner_id", "stage", "status", "amount",
    "expected_close"]);
  if (changes.owner_id !== undefined && !directory.has(changes.owner_id)) {
    fail(422, `user ${changes.owner_id} does not exist`);
  }
  const before = await getDeal(params.id);
  await update("deals", params.id, changes);
  const owner = directory.get(changes.owner_id);
  if (owner && owner.id !== before.owner_id && url.searchParams.get("notify") !== "false") {
    await emails.add("deal-assigned", {
      to: owner.email,
      subject: `Deal ${before.id} is now yours`,
      text: `Hi ${owner.name},\n\n${before.name} (deal ${before.id}) was assigned to you.\n`,
      html: `<p>Hi ${owner.name},</p><p><b>${before.name}</b> (deal ${before.id}) was assigned to you.</p>`,
    }, { removeOnComplete: true, removeOnFail: true });
  }
  return [200, await getDeal(params.id)];
});

on("GET", "/deals/:id/notes", async ({ params }) =>
  [200, await query("SELECT * FROM notes WHERE deal_id = $1 ORDER BY id", [params.id])]);
on("POST", "/deals/:id/notes", async ({ params, body, user }) => {
  if (typeof body?.text !== "string" || !body.text.trim()) fail(422, "text is required");
  await getDeal(params.id);
  return [201, await transaction(async (client) => {
    const note = await client.query(`INSERT INTO notes (deal_id, author_id, text)
      VALUES ($1, $2, $3) RETURNING *`, [params.id, user.id, body.text]);
    await client.query(`INSERT INTO activities (deal_id, user_id, kind, summary)
      VALUES ($1, $2, 'note', $3)`, [params.id, user.id, body.text.slice(0, 80)]);
    return note.rows[0];
  })];
});
on("GET", "/deals/:id/activities", async ({ params }) => [200, await query(
  "SELECT * FROM activities WHERE deal_id = $1 ORDER BY occurred_at DESC", [params.id])]);
on("POST", "/deals/:id/activities", async ({ params, body, user }) => {
  if (!["call", "email", "meeting", "note"].includes(body?.kind)) {
    fail(422, "kind must be call, email, meeting or note");
  }
  if (typeof body.summary !== "string" || !body.summary.trim()) fail(422, "summary is required");
  await getDeal(params.id);
  const [row] = await query(`INSERT INTO activities (deal_id, user_id, kind, summary)
    VALUES ($1, $2, $3, $4) RETURNING *`, [params.id, user.id, body.kind, body.summary]);
  return [201, row];
});

on("GET", "/invoices", async ({ url }) => {
  const p = page(url);
  const rows = await query(`SELECT i.* FROM invoices i JOIN deals d ON d.id = i.deal_id
    WHERE ($1::text IS NULL OR i.status = $1) AND ($2::int IS NULL OR i.deal_id = $2)
    AND ($3::int IS NULL OR d.contact_id = $3) AND ($4::text IS NULL OR i.number = $4)
    AND ($5::text IS NULL OR (i.status = 'open' AND i.due_date < current_date))
    ORDER BY i.id LIMIT $6 OFFSET $7`, [param(url, "status"), int(url, "deal_id"),
    int(url, "contact_id"), param(url, "number"), param(url, "overdue") === "true" ? "y" : null,
    p.limit + 1, p.offset]);
  return listed(url, rows, p);
});
on("GET", "/invoices/:id", async ({ params }) => {
  const [invoice] = await query("SELECT * FROM invoices WHERE id = $1", [params.id]);
  return [200, invoice ?? fail(404, `invoice ${params.id} not found`)];
});
on("POST", "/invoices/:id/pay", async ({ params }) => {
  const [invoice] = await query("SELECT * FROM invoices WHERE id = $1", [params.id]);
  if (!invoice) fail(404, `invoice ${params.id} not found`);
  if (invoice.status !== "open") fail(409, `invoice ${invoice.number} is ${invoice.status}`);
  const response = await fetch(`${STRIPE}/v1/payment_intents`, {
    method: "POST",
    headers: { "content-type": "application/json", authorization: "Bearer sk_test_acme" },
    body: JSON.stringify({ amount: Math.round(invoice.amount * 100), currency: invoice.currency,
      confirm: true, metadata: { invoice: invoice.number } }),
  });
  if (!response.ok) fail(502, `the payment provider answered ${response.status}`);
  const intent = await response.json();
  return [200, await update("invoices", params.id,
    { status: "paid", paid_at: new Date(), payment_intent: intent.id })];
});
on("POST", "/invoices/:id/void", async ({ params }) =>
  [200, await update("invoices", params.id, { status: "void" })]);

on("POST", "/exports", async ({ body }) => {
  if (body?.kind !== "deals") fail(422, "kind must be deals");
  const owner = body.owner_id ?? null;
  const status = body.status ?? null;
  const rows = await query(`SELECT id, name, owner_id, stage, status, amount, expected_close
    FROM deals WHERE ($1::int IS NULL OR owner_id = $1) AND ($2::text IS NULL OR status = $2)
    ORDER BY id`, [owner, status]);
  const name = ["deals", owner && `owner-${owner}`, status].filter(Boolean).join("-");
  const csv = ["id,name,owner_id,stage,status,amount,expected_close",
    ...rows.map((r) => [r.id, JSON.stringify(r.name), r.owner_id, r.stage, r.status, r.amount,
      r.expected_close].join(","))].join("\n");
  await mkdir(`${UPLOADS}/exports`, { recursive: true });
  await writeFile(`${UPLOADS}/exports/${name}.csv`, `${csv}\n`);
  return [201, { file: `exports/${name}.csv`, rows: rows.length }];
});

async function authenticate(req) {
  const token = /^Bearer (.+)$/.exec(req.headers.authorization ?? "")?.[1];
  if (!token) fail(401, "send Authorization: Bearer <token>");
  const [row] = await query(
    "UPDATE api_tokens SET last_used_at = now() WHERE token = $1 RETURNING user_id", [token]);
  const user = row && directory.get(row.user_id);
  if (!user || user.status === "inactive") fail(401, "invalid token");
  return user;
}

async function readBody(req) {
  const chunks = [];
  for await (const chunk of req) chunks.push(chunk);
  if (chunks.length === 0) return null;
  try {
    return JSON.parse(Buffer.concat(chunks).toString("utf8"));
  } catch {
    return fail(400, "the body must be JSON");
  }
}

async function handle(req) {
  const url = new URL(req.url, "http://localhost");
  if (url.pathname === "/healthz") return [200, { ok: true }];
  const user = await authenticate(req);
  const body = await readBody(req);
  for (const route of routes) {
    const match = route.method === req.method && route.pattern.exec(url.pathname);
    if (match) return route.handler({ params: match.groups ?? {}, url, body, user });
  }
  return fail(404, `no route for ${req.method} ${url.pathname}`);
}

http.createServer(async (req, res) => {
  let reply;
  try {
    reply = await handle(req);
  } catch (error) {
    const invalid = /^2[23]/.test(error.code ?? "");
    if (!(error instanceof HttpError) && !invalid) console.error(error);
    reply = error instanceof HttpError ? [error.status, { error: error.message }]
      : invalid ? [422, { error: error.detail ?? error.message }] : [500, { error: "internal error" }];
  }
  const [status, body, headers = {}] = reply;
  res.writeHead(status, { "content-type": "application/json", ...headers });
  res.end(JSON.stringify(body));
}).listen(PORT, "127.0.0.1", () => console.log(`acme-crm listening on ${PORT}`));
