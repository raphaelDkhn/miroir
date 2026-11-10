# Import and scrub dumps

Seed data made from production is realistic, but it holds people's names, emails and secrets.
`miroir import` turns production dumps into seed data: every such value becomes a keyed fake
of the same shape, and the import fails if any original is left.

It runs inside the env image, which has the datastores:

```sh
mkdir -p in out
head -c 32 /dev/urandom > in/key
docker run --rm -v "$PWD/in:/in:ro" -v "$PWD/out:/out" my-env import \
  --dump crm=/in/crm.sql --scrub /in/scrub.yaml --key /in/key --out /out
```

| Option | What |
|---|---|
| `--dump APP=FILE` | a plain SQL dump of an app's database; one per app |
| `--scrub FILE` | the scrub spec |
| `--key FILE` | the key, at least 32 bytes |
| `--traces FILE` | production agent traces to scrub too; see [Tasks from traces](traces.md) |
| `--out DIR` | where to write the results |

Dump one database per file: `pg_dump` without `--create`, or `mysqldump` without
`--databases`. Mount the inputs and the output outside the env's state directories and
`/tmp`, as `/in` and `/out` above.

## The scrub spec

The spec gives each text column a kind, by app and table:

```yaml
apps:
  crm:
    contacts: {name: name, email: email, phone: phone, notes: text}
    users: {name: name, email: email, password_hash: secret}
    deals: {title: keep, stage: keep}
```

| Kind | Becomes |
|---|---|
| `name` | each word replaced by a fake word, keeping its case |
| `email` | a fake address, such as `3f9a1c2e4b7d8a90@1c2e4b7d.example` |
| `phone` | fake digits in the same format |
| `secret` | a fake secret |
| `text` | free text with its emails, and the known names, phones and secrets, replaced |
| `blank` | NULL |
| `keep` | unchanged |

List every column that is not a number, a truth value, a timestamp or a UUID; the import
names the ones missing. `name`, `email`, `phone` and `secret` take text columns; `text` also
takes JSON and arrays.

## Keyed fakes

Under one key, an original always gets the same fake, and no two originals share one. Rows
that matched in production still match, and dumps scrubbed apart stay consistent. Keep the
key secret and out of the image: with it, anyone could test guesses of the originals.

## Output

| File | What |
|---|---|
| `<app>.sql` | the scrubbed dump of each app |
| `traces.jsonl` | the scrubbed traces, with `--traces` |
| `report.json` | rows scrubbed per column, fakes drawn, traces kept and left out |

After scrubbing, the import scans every column for originals. If it finds any, it writes
nothing and names the columns, so you can give them the right kind.

## Load it at bake

Copy the scrubbed dump into the image, and load it in a `bake` command:

```yaml
bake:
  - [createdb, -h, 127.0.0.1, -U, postgres, crm]
  - [psql, -h, 127.0.0.1, -U, postgres, -d, crm, -v, ON_ERROR_STOP=1, -f, /seed/crm.sql]
```

A dump is a snapshot from a known day. Declare it in `time.captured`, and bake moves its dates
to the anchor. See [Captured worlds](../features/dates.md#captured-worlds).
