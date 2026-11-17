import { pool, transaction } from "./db.js";

const DAY = 86_400_000;
const NOW = Date.now();
const ago = (days, minutes = 0) => new Date(NOW - days * DAY - minutes * 60_000);
const dateAgo = (days) => new Date(NOW - days * DAY).toISOString().slice(0, 10);

let seed = 42;
function rand() {
  seed = (seed + 0x6d2b79f5) | 0;
  let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
  t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
  return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
}
const int = (lo, hi) => lo + Math.floor(rand() * (hi - lo + 1));
const pick = (items) => items[int(0, items.length - 1)];

const USERS = [
  "Dana Lee", "Alex Kim", "Sam Ortiz", "Jamie Chen", "Riley Brooks", "Morgan Diaz",
  "Marcus Webb", "Taylor Reed", "Jordan Price", "Casey Nguyen", "Avery Patel", "Priya Shah",
  "Drew Foster", "Quinn Harper", "Logan Rivera", "Parker Hayes", "Rowan Ellis",
  "Skyler Grant", "Emerson Cole", "Hayden Ward",
];
const FIRST = ["Ana", "Ben", "Chloe", "Dev", "Eli", "Fay", "Gus", "Hana", "Ivan", "Jade",
  "Kai", "Lena", "Milo", "Nia", "Omar", "Pia", "Rhys", "Sara", "Theo", "Uma", "Vik", "Wes"];
const LAST = ["Abbott", "Baker", "Cruz", "Doyle", "Evans", "Flynn", "Garcia", "Holt", "Ito",
  "Jensen", "Klein", "Lopez", "Moss", "Novak", "Okafor", "Park", "Quinn", "Rossi", "Silva"];
const BRAND = ["Blue", "Silver", "Granite", "Summit", "Harbor", "Pioneer", "Atlas", "Cedar",
  "Beacon", "Vertex", "Orchid", "Falcon", "Maple", "Copper", "Lumen"];
const TRADE = ["Logistics", "Labs", "Foods", "Systems", "Health", "Partners", "Robotics",
  "Media", "Energy", "Retail"];
const KINDS = ["renewal", "expansion", "pilot", "onboarding", "upgrade", "license"];
const SUMMARIES = { call: "Discovery call", email: "Sent pricing", meeting: "Demo meeting",
  note: "Internal note" };
const NOTES = ["Asked for a discount", "Waiting on legal review", "Champion moved teams",
  "Budget approved for next quarter", "Requested a security questionnaire"];
const REPS = USERS.map((_, i) => i + 1).filter((id) => id !== 1 && id !== 12);

const users = USERS.map((name, i) => {
  const email = `${name.split(" ")[0].toLowerCase()}@acme.test`;
  const status = i + 1 === 12 ? "leaving" : "active";
  return [i + 1, name, email, i === 0 ? "manager" : "rep", status, ago(900 - i)];
});
const tokens = users.map(([id, name]) => [
  id, id, `seed-token-${name.split(" ")[0].toLowerCase()}`, ago(400),
]);

const contacts = [];
for (let id = 1; id <= 5000; id++) {
  const [first, last] = [pick(FIRST), pick(LAST)];
  const born = new Date(Date.UTC(int(1960, 2000), int(0, 11), int(1, 28)));
  const created = int(30, 1000);
  contacts.push([id, `${first} ${last}`, `${first}.${last}${id}@example.test`.toLowerCase(),
    `${pick(BRAND)} ${pick(TRADE)}`, `+1-555-${String(int(0, 9999)).padStart(4, "0")}`,
    pick(REPS), born.toISOString().slice(0, 10), ago(created), ago(int(1, created))]);
}
const contact = (id, name, email, company) => {
  contacts[id - 1].splice(1, 3, name, email, company);
};
contact(101, "Elena Torres", "elena@northwind.test", "Northwind Traders");
contact(2045, "Jordan Avery", "jordan@initech.test", "Initech");
contact(2046, "Jordan Avery", "j.avery@initech.test", "Initech");
const SPECIAL_CONTACTS = new Set([101, 2045, 2046]);

const deals = [];
for (let id = 1; id <= 8000; id++) {
  let contactId = int(1, 5000);
  while (SPECIAL_CONTACTS.has(contactId)) contactId = int(1, 5000);
  const r = rand();
  const status = r < 0.5 ? "open" : r < 0.8 ? "won" : "lost";
  const created = int(60, 700);
  deals.push({
    id, contact_id: contactId, owner_id: pick(REPS), status,
    name: `${contacts[contactId - 1][3]} ${pick(KINDS)}`,
    stage: status === "open" ? pick(["lead", "qualified", "proposal"]) : status,
    amount: int(5, 500) * 100,
    expected_close: status === "open" ? dateAgo(-int(5, 120)) : dateAgo(int(5, created - 1)),
    created_at: ago(created), updated_at: ago(int(1, created)),
    synced_unix: id === 1100 ? 0 : Math.floor(ago(created).getTime() / 1000),
  });
}
const deal = (id, fields) => Object.assign(deals[id - 1], fields);
const open = (stage) => ({ status: "open", stage });
const closed = (status) => ({ status, stage: status });
deal(1042, { name: "Northwind renewal", contact_id: 101, owner_id: 12, ...open("proposal") });
deal(1043, { name: "Contoso pilot", owner_id: 12, ...open("qualified") });
deal(1057, { name: "Globex onboarding", owner_id: 12, ...open("lead") });
deal(1050, { name: "Umbrella upgrade", owner_id: 12, ...open("proposal") });
deal(1061, { name: "Hooli license", owner_id: 12, ...open("qualified") });
deal(1066, { name: "Initrode expansion", owner_id: 12, ...closed("won") });
deal(1071, { name: "Vehement renewal", owner_id: 12, ...closed("lost") });
deal(1100, { name: "Soylent rollout", owner_id: 9, ...open("proposal") });
for (const id of [1200, 1201, 1202, 1203]) deal(id, open("qualified"));
deal(2001, { name: "Globex expansion", owner_id: 4, ...open("proposal") });
deal(3001, { name: "Northwind expansion", contact_id: 101, ...closed("won") });
deal(3002, { name: "Northwind support", contact_id: 101, ...closed("won") });
deal(3100, { name: "Initech renewal", contact_id: 2045, owner_id: 5, ...open("lead") });
deal(3101, { name: "Initech upgrade", contact_id: 2046, owner_id: 5, ...open("qualified") });
deal(3102, { name: "Initech support", contact_id: 2046, owner_id: 5, ...closed("won") });
deal(3200, { name: "Stark license", owner_id: 3, ...closed("won") });

const PRIYA_ACTIVITY = { 1042: [40, 95], 1043: [52], 1057: [75, 130], 1050: [3, 44],
  1061: [9], 1066: [90], 1071: [120] };
const activities = [];
const activity = (dealRow, days, minutes) => {
  const kind = pick(Object.keys(SUMMARIES));
  activities.push([activities.length + 1, dealRow.id, dealRow.owner_id, kind, SUMMARIES[kind],
    ago(days, minutes)]);
};
for (const row of deals) {
  const fixed = PRIYA_ACTIVITY[row.id];
  if (fixed) fixed.forEach((days) => activity(row, days, 600));
  else for (let n = int(0, 5); n > 0; n--) activity(row, int(1, 400), int(0, 1439));
}

const notes = [];
for (let id = 1; id <= 10000; id++) {
  const row = deals[int(1, 8000) - 1];
  notes.push([id, row.id, row.owner_id, pick(NOTES), ago(int(1, 400), int(0, 1439))]);
}

const invoices = [];
const invoice = (id, dealId, status, dueDays, amount = deals[dealId - 1].amount) => {
  const paid = status === "paid";
  invoices.push([id, dealId, `INV-${String(id).padStart(5, "0")}`, amount, status,
    dateAgo(dueDays), paid ? ago(dueDays - 2) : null, paid ? `pi_seed_${id}` : null,
    ago(dueDays + 30)]);
};
const SPECIAL_DEALS = new Set([1100, 3001, 3002, 3102, 3200]);
for (const row of deals) {
  if (invoices.length === 3000) break;
  if (row.status !== "won" || SPECIAL_DEALS.has(row.id)) continue;
  const r = rand();
  if (r < 0.7) invoice(invoices.length + 1, row.id, "paid", int(30, 400));
  else if (r < 0.9) invoice(invoices.length + 1, row.id, "open", pick([int(-60, -3), int(3, 60)]));
  else invoice(invoices.length + 1, row.id, "void", int(10, 300));
}
invoice(4001, 3200, "open", -10, 1250);
invoice(4010, 3001, "open", 10);
invoice(4011, 3001, "open", 20);
invoice(4012, 3002, "open", 45);
invoice(4013, 3002, "open", -5);
invoice(4014, 3001, "open", -30);
invoice(4015, 3001, "paid", 60);
invoice(4016, 3002, "void", 15);
invoice(4020, 1100, "open", -14, 5000);

async function insert(client, table, columns, rows) {
  for (let start = 0; start < rows.length; start += 1000) {
    const chunk = rows.slice(start, start + 1000);
    const values = chunk.map((_, r) =>
      `(${columns.map((_, c) => `$${r * columns.length + c + 1}`).join(", ")})`);
    const sql = `INSERT INTO ${table} (${columns.join(", ")}) VALUES ${values.join(", ")}`;
    await client.query(sql, chunk.flat());
  }
}

const rates = [0, 1, 2, 3, 4].map((days) => ["eur", dateAgo(days), 0.92]);
const meetings = [3, 2, 1].map((days) => [ago(days), ago(days - 1)]);

await transaction(async (client) => {
  await insert(client, "users", ["id", "name", "email", "role", "status", "created_at"], users);
  await insert(client, "api_tokens", ["id", "user_id", "token", "created_at"], tokens);
  await insert(client, "contacts", ["id", "name", "email", "company", "phone", "owner_id",
    "birthday", "created_at", "updated_at"], contacts);
  const dealColumns = ["id", "name", "contact_id", "owner_id", "stage", "status", "amount",
    "expected_close", "created_at", "updated_at", "synced_unix"];
  await insert(client, "deals", dealColumns, deals.map((d) => dealColumns.map((c) => d[c])));
  await insert(client, "activities", ["id", "deal_id", "user_id", "kind", "summary",
    "occurred_at"], activities);
  await insert(client, "notes", ["id", "deal_id", "author_id", "text", "created_at"], notes);
  await insert(client, "invoices", ["id", "deal_id", "number", "amount", "status", "due_date",
    "paid_at", "payment_intent", "created_at"], invoices);
  await insert(client, "exchange_rates", ["currency", "day", "rate"], rates);
  await insert(client, "meetings", ["starts", "ends"], meetings);
  for (const table of ["users", "api_tokens", "contacts", "deals", "activities", "notes",
    "invoices"]) {
    await client.query(`SELECT setval(pg_get_serial_sequence('${table}', 'id'),
      (SELECT max(id) FROM ${table}))`);
  }
});
await pool.end();
const total = users.length + tokens.length + contacts.length + deals.length +
  activities.length + notes.length + invoices.length + rates.length + meetings.length;
console.log(`seeded ${total} rows`);
