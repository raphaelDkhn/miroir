---
title: "Evidence"
description: "The tables a check can read, how their values look, and recipes for common checks."
---

The evidence is one SQLite file, `/miroir/evidence/evidence.db`. `miroir collect` writes it once,
after the agent stops, and the grader reads a copy.

## What checks read

Read the tables the grader adds in memory, not the JSON in `changes`. It builds a `before` and an
`after` table for every captured table, plus `answer`:

| Name | Rows |
|---|---|
| `after."<t>"`, for every captured table | The rows the episode inserted or updated, as it left them: the table's columns, then `_op` (`insert` or `update`) and `_change` (`changes.id`). Empty when untouched. |
| `before."<t>"`, for every captured table | The rows it updated or deleted, as it found them, with `_op` (`update` or `delete`) and `_change`. |
| `answer(value, number, ids)` | Always one row. `value`: the answer file's text, trimmed (first 64 KiB of a regular file), or NULL. `number`: `value` as a 64-bit integer, else NULL. `ids`: `value` split on commas and whitespace, a leading `#` dropped from each token, sorted (numerically when all are numbers), deduplicated and joined with `,`; NULL when a token holds a character outside `A-Z a-z 0-9 - _ . : / @`. |

Rows absent from `changes` are as baked. Ignored tables, ignored columns, tables created after the
bake, files and git refs are not in the evidence, so no check can read them.

### Values

Values keep their JSON types:

- booleans read as 1 or 0, and JSON objects and arrays as JSON text;
- `timestamptz` reads as `2026-11-17T10:00:00+00:00`, and `timestamp` as `2026-11-17T10:00:00`
  (UTC);
- dates read as `YYYY-MM-DD`, and Unix times as integers;
- `numeric` reads as REAL;
- a value nested past 126 levels, or a number past a double's range, reads as its JSON text.

A row counts as changed when the text Postgres wrote for it changed. So rewriting a numeric to a
new scale (`1.5` to `1.50`) is an update, even though both images read the same REAL.

## The file

```sql
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
-- exactly two rows: format = '1' and miroir = '<the collecting binary's version>'

CREATE TABLE tables (name TEXT PRIMARY KEY, columns TEXT NOT NULL, pk TEXT NOT NULL);
-- every table captured at boot: JSON arrays of column names (table order, ignored columns
-- left out) and primary-key columns ([] for a keyless table)

CREATE TABLE changes (
  id      INTEGER PRIMARY KEY,
  tbl     TEXT NOT NULL,   -- table; schema.table outside public
  op      TEXT NOT NULL,   -- insert | update | delete | truncate | schema
  pk      TEXT NOT NULL,   -- JSON object of the primary key; {} for keyless rows, truncate, schema
  before  TEXT,            -- JSON row as the episode found it; NULL for insert
  after   TEXT,            -- JSON row as the episode left it; NULL for delete
  columns TEXT             -- JSON array of changed columns (update, schema); NULL otherwise
);
-- one row per net-changed row (keyless tables: one per net instance), or per table for
-- truncate and schema; rows absent from changes are as baked
-- id order: truncates, then each table's keyed rows in the order the change log first touched
-- them, then the rows of keyless tables, then schema changes
-- pk, before, after: a value nested deeper than 126 levels, or a number outside a double's
-- range, is a JSON string of its text

CREATE TABLE sent_emails (
  seq INTEGER PRIMARY KEY,   -- position in the effects journal (shared with sent_requests)
  sender TEXT, recipients TEXT NOT NULL, cc TEXT NOT NULL, bcc TEXT NOT NULL,
  subject TEXT, text TEXT, html TEXT
);
-- recipients, cc, bcc: JSON arrays of addresses; bcc = envelope recipients missing from headers

CREATE TABLE sent_requests (
  seq INTEGER PRIMARY KEY,
  method TEXT NOT NULL, path TEXT NOT NULL,
  query TEXT NOT NULL,     -- JSON object of query parameters
  headers TEXT NOT NULL,   -- JSON object, lower-case names, authorization dropped
  body TEXT NOT NULL       -- always valid JSON: the parsed body, a JSON string of the text, or null
);

CREATE TABLE faults (seq INTEGER PRIMARY KEY, kind TEXT NOT NULL, detail TEXT NOT NULL);
-- kind: crash | sink | quiet_timeout | database | log_budget | evidence_budget
```

## Check recipes

These patterns cover the fixture's checks and the Gitea example's.

One row reaches a value (`close-won-deal`):

```sql
SELECT count(*) FROM after.deals WHERE id = 1100 AND status = 'won' AND stage = 'won'
```

A share of target rows reaches a value, for partial credit (`reassign-stale-deals`):

```sql
SELECT count(*) / 3.0 FROM after.deals WHERE id IN (1042, 1043, 1057) AND owner_id = 7
```

Rows the agent inserts, matched by content (`log-call-activities`):

```sql
SELECT count(DISTINCT deal_id) / 4.0 FROM after.activities
WHERE _op = 'insert' AND kind = 'call' AND summary = 'Quarterly check-in call'
  AND deal_id IN (1200, 1201, 1202, 1203)
```

A row deleted (`merge-duplicate-contacts`):

```sql
SELECT count(*) FROM before.contacts WHERE id = 2046 AND _op = 'delete'
```

Baked rows that must go, counted with the identity of [rule 2](/spec#authoring-rules) (Gitea's
`offboard`, where the generator knows how many teams the leaver is on):

```sql
SELECT ((SELECT count(*) FROM before.team_user WHERE uid = <leaver>)
      - (SELECT count(*) FROM after.team_user WHERE uid = <leaver>)) / <teams>.0
```

Text that must appear nowhere else, as a gate over every row the episode wrote (Gitea's
`redact-secret`, whose world holds the leaked key in one comment only):

```sql
SELECT count(*) = 0 FROM (SELECT content FROM after.comment WHERE id <> <comment>
                          UNION ALL SELECT content FROM after.issue)
WHERE instr(content, '<key>') > 0
```

A value the product sent (`notify-new-owner`, `pay-invoice`):

```sql
SELECT count(*) = 1 FROM sent_emails
WHERE json_array_length(recipients) = 1 AND recipients ->> '$[0]' = 'marcus@acme.test'
  AND subject = 'Deal 2001 is now yours'
```

```sql
SELECT count(*) = 1 FROM sent_requests
WHERE method = 'POST' AND path = '/stripe/v1/payment_intents' AND body ->> '$.amount' = 125000
```

Nothing sent, or nothing sent elsewhere, as a gate (`reassign-stale-deals`; Gitea's
`ship-release`):

```sql
SELECT count(*) = 0 FROM sent_emails
```

```sql
SELECT count(*) = 0 FROM sent_requests WHERE path <> '/deploy/<repo>'
```

Answers: `SELECT number = 3 FROM answer` for a count, `SELECT ids = '41,57,63' FROM answer` for a
list of ids in any order or format, and `SELECT value = '<text>' FROM answer` for an exact text.
