import { pool } from "./db.js";

await pool.query(`
CREATE TABLE users (
  id serial PRIMARY KEY,
  name text NOT NULL,
  email text NOT NULL UNIQUE,
  role text NOT NULL CHECK (role IN ('manager', 'rep')),
  status text NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'leaving', 'inactive')),
  created_at timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE api_tokens (
  id serial PRIMARY KEY,
  user_id int NOT NULL REFERENCES users,
  token text NOT NULL UNIQUE,
  last_used_at timestamptz,
  created_at timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE contacts (
  id serial PRIMARY KEY,
  name text NOT NULL,
  email text,
  company text,
  phone text,
  owner_id int REFERENCES users,
  birthday date,
  created_at timestamptz NOT NULL DEFAULT now(),
  updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE deals (
  id serial PRIMARY KEY,
  name text NOT NULL,
  contact_id int REFERENCES contacts,
  owner_id int NOT NULL REFERENCES users,
  stage text NOT NULL DEFAULT 'lead'
    CHECK (stage IN ('lead', 'qualified', 'proposal', 'won', 'lost')),
  status text NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'won', 'lost')),
  amount numeric(12, 2) NOT NULL DEFAULT 0,
  expected_close date,
  created_at timestamptz NOT NULL DEFAULT now(),
  updated_at timestamptz NOT NULL DEFAULT now(),
  synced_unix bigint NOT NULL DEFAULT 0
);
CREATE TABLE notes (
  id serial PRIMARY KEY,
  deal_id int NOT NULL REFERENCES deals ON DELETE CASCADE,
  author_id int NOT NULL REFERENCES users,
  text text NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE activities (
  id serial PRIMARY KEY,
  deal_id int NOT NULL REFERENCES deals ON DELETE CASCADE,
  user_id int REFERENCES users,
  kind text NOT NULL CHECK (kind IN ('call', 'email', 'meeting', 'note')),
  summary text NOT NULL,
  occurred_at timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE invoices (
  id serial PRIMARY KEY,
  deal_id int NOT NULL REFERENCES deals,
  number text NOT NULL UNIQUE,
  amount numeric(12, 2) NOT NULL,
  currency text NOT NULL DEFAULT 'usd',
  status text NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'paid', 'void', 'overdue')),
  due_date date NOT NULL,
  paid_at timestamptz,
  payment_intent text,
  created_at timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE exchange_rates (
  currency text NOT NULL,
  day date NOT NULL,
  rate numeric(10, 4) NOT NULL,
  valid_until date NOT NULL DEFAULT '9999-12-31',
  PRIMARY KEY (currency, day),
  CHECK (day < valid_until)
);
CREATE TABLE meetings (
  id serial PRIMARY KEY,
  starts timestamptz NOT NULL,
  ends timestamptz NOT NULL,
  EXCLUDE USING gist (tstzrange(starts, ends) WITH &&)
);
CREATE INDEX contacts_owner ON contacts (owner_id);
CREATE INDEX deals_owner ON deals (owner_id, status);
CREATE INDEX deals_contact ON deals (contact_id);
CREATE INDEX notes_deal ON notes (deal_id);
CREATE INDEX activities_deal ON activities (deal_id, occurred_at);
CREATE INDEX invoices_deal ON invoices (deal_id);

CREATE FUNCTION touch_updated_at() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  NEW.updated_at = now();
  RETURN NEW;
END $$;
CREATE TRIGGER deals_touch BEFORE UPDATE ON deals
  FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
CREATE TRIGGER contacts_touch BEFORE UPDATE ON contacts
  FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
`);
await pool.end();
console.log("migrated");
