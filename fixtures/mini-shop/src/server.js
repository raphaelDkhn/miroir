import http from "node:http";
import { pool, query } from "./db.js";

class HttpError extends Error {
  constructor(status, message) {
    super(message);
    this.status = status;
  }
}
const fail = (status, message) => {
  throw new HttpError(status, message);
};

async function one(sql, params, what) {
  const [row] = await query(sql, params);
  return row ?? fail(404, `${what} not found`);
}

async function orderWithItems(id) {
  const order = await one("SELECT * FROM orders WHERE id = ?", [id], `order ${id}`);
  order.items = await query("SELECT * FROM order_items WHERE order_id = ? ORDER BY id", [id]);
  return order;
}

const routes = [];
const on = (method, pattern, handler) => routes.push({
  method, handler, pattern: new RegExp(`^${pattern.replace(/:(\w+)/g, "(?<$1>\\d+)")}$`),
});

on("GET", "/products", () => query("SELECT * FROM products ORDER BY id"));
on("GET", "/products/:id", ({ params }) =>
  one("SELECT * FROM products WHERE id = ?", [params.id], `product ${params.id}`));
on("PATCH", "/products/:id", async ({ params, body }) => {
  const changes = Object.fromEntries(Object.entries(body ?? {}).filter(([k]) => ["price", "stock"].includes(k)));
  if (Object.keys(changes).length === 0) fail(422, "send price or stock");
  if (changes.stock !== undefined && !(Number.isInteger(changes.stock) && changes.stock >= 0)) {
    fail(422, "stock must be a whole number, zero or more");
  }
  await one("SELECT id FROM products WHERE id = ?", [params.id], `product ${params.id}`);
  await query("UPDATE products SET ? WHERE id = ?", [changes, params.id]);
  return one("SELECT * FROM products WHERE id = ?", [params.id], `product ${params.id}`);
});
on("GET", "/customers", () => query("SELECT * FROM customers ORDER BY id"));
on("GET", "/orders", ({ url }) => {
  const status = url.searchParams.get("status");
  const customer = url.searchParams.get("customer_id");
  return query(`SELECT * FROM orders WHERE (? IS NULL OR status = ?) AND (? IS NULL OR customer_id = ?)
    ORDER BY id`, [status, status, customer, customer]);
});
on("GET", "/orders/:id", ({ params }) => orderWithItems(params.id));
on("POST", "/orders/:id/cancel", async ({ params }) => {
  const order = await orderWithItems(params.id);
  if (order.status !== "pending") fail(409, `order ${order.id} is ${order.status}`);
  const conn = await pool.getConnection();
  try {
    await conn.beginTransaction();
    await conn.query("UPDATE orders SET status = 'cancelled' WHERE id = ?", [order.id]);
    for (const item of order.items) {
      await conn.query("UPDATE products SET stock = stock + ? WHERE id = ?", [item.quantity, item.product_id]);
    }
    await conn.commit();
  } catch (error) {
    await conn.rollback();
    throw error;
  } finally {
    conn.release();
  }
  return orderWithItems(order.id);
});
on("DELETE", "/orders/:id", async ({ params }) => {
  await orderWithItems(params.id);
  await query("DELETE FROM orders WHERE id = ?", [params.id]);
  return { deleted: Number(params.id) };
});

async function handle(req) {
  const url = new URL(req.url, "http://localhost");
  if (url.pathname === "/healthz") return { ok: true };
  const key = req.headers["x-api-key"] ?? "";
  const [staff] = await query("SELECT name FROM staff_keys WHERE key_value = ?", [key]);
  if (!staff) fail(401, "send a valid X-Api-Key");
  const chunks = [];
  for await (const chunk of req) chunks.push(chunk);
  let body = null;
  try {
    body = chunks.length ? JSON.parse(Buffer.concat(chunks).toString("utf8")) : null;
  } catch {
    fail(400, "the body must be JSON");
  }
  for (const route of routes) {
    const match = route.method === req.method && route.pattern.exec(url.pathname);
    if (match) return route.handler({ params: match.groups ?? {}, url, body });
  }
  return fail(404, `no route for ${req.method} ${url.pathname}`);
}

http.createServer(async (req, res) => {
  let status = 200;
  let payload;
  try {
    payload = await handle(req);
  } catch (error) {
    status = error.status ?? 500;
    if (status === 500) console.error(error);
    payload = { error: status === 500 ? "internal error" : error.message };
  }
  res.writeHead(status, { "content-type": "application/json" });
  res.end(JSON.stringify(payload));
}).listen(3001, "127.0.0.1", () => console.log("mini-shop listening on 3001"));
