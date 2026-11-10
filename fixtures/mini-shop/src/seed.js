import { pool } from "./db.js";

const DAY = 86_400_000;
const NOW = Date.now();
const at = (days) => new Date(NOW - days * DAY).toISOString().slice(0, 19).replace("T", " ");
const day = (days) => new Date(NOW - days * DAY).toISOString().slice(0, 10);

const SEEDS = { north: 7, south: 11 };
let seed = SEEDS[process.env.MIROIR_WORLD];
if (seed === undefined) {
  throw new Error(`MIROIR_WORLD is '${process.env.MIROIR_WORLD}', not one of ${Object.keys(SEEDS)}`);
}
function rand() {
  seed = (seed * 1103515245 + 12345) % 2147483648;
  return seed / 2147483648;
}
const int = (lo, hi) => lo + Math.floor(rand() * (hi - lo + 1));

const FIRST = ["Ada", "Ben", "Cleo", "Dev", "Eve", "Finn", "Gia", "Hal", "Ines", "Jon"];
const GOODS = ["Mug", "Lamp", "Kettle", "Chair", "Desk", "Shelf", "Rug", "Clock", "Vase", "Fan"];

const customers = [];
for (let id = 1; id <= 30; id++) {
  const name = `${FIRST[id % 10]} ${String.fromCharCode(65 + (id % 26))}.`;
  customers.push([id, name, `customer${id}@shop.test`, at(int(100, 900))]);
}
const products = [];
for (let id = 1; id <= 20; id++) {
  const stock = id === 4 || id === 9 ? 0 : int(5, 80);
  products.push([id, `${GOODS[id % 10]} ${id}`, int(5, 200) + 0.99, stock, at(int(3, 60))]);
}
const FIXED = { 1007: [3, "pending", 12], 1012: [3, "pending", 3], 1013: [3, "delivered", 60] };
const orders = [];
const items = [];
for (let id = 1001; id <= 1060; id++) {
  let customer = int(1, 30);
  if (customer === 3) customer = 4;
  let placed = int(2, 120);
  let status = placed < 15 ? "pending" : placed < 40 ? "shipped" : "delivered";
  if (FIXED[id]) [customer, status, placed] = FIXED[id];
  let total = 0;
  for (let lines = int(1, 4); lines > 0; lines--) {
    const product = products[int(0, 19)];
    const quantity = int(1, 3);
    total += product[2] * quantity;
    items.push([items.length + 1, id, product[0], quantity, product[2]]);
  }
  orders.push([id, customer, status, Math.round(total * 100) / 100, at(placed), day(placed - 5),
    at(placed - 1)]);
}

await pool.query("INSERT INTO staff_keys (key_value, name) VALUES ?", [[["seed-key-ops", "Ops team"]]]);
await pool.query("INSERT INTO customers (id, name, email, created_at) VALUES ?", [customers]);
await pool.query("INSERT INTO products (id, name, price, stock, updated_at) VALUES ?", [products]);
await pool.query(
  "INSERT INTO orders (id, customer_id, status, total, placed_at, ship_by, updated_at) VALUES ?", [orders]);
await pool.query("INSERT INTO order_items (id, order_id, product_id, quantity, price) VALUES ?", [items]);
const priceDays = [0, 1, 2, 3, 4].map((days) => [day(days), 1.1]);
await pool.query("INSERT INTO price_days (day, factor) VALUES ?", [priceDays]);
await pool.query("UPDATE price_days SET stamped_unix = UNIX_TIMESTAMP(day) WHERE day <> ?", [day(4)]);
const lenient = await pool.getConnection();
await lenient.query("SET sql_mode = ''");
await lenient.query("UPDATE price_days SET retired_at = '0000-00-00 00:00:00' WHERE day = ?", [day(4)]);
lenient.release();
await pool.end();
console.log(`seeded ${customers.length + products.length + orders.length + items.length} rows`);
