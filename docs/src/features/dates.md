# Dates

Seed data has dates: an invoice due last week, a deal updated yesterday. Each reset moves
every date forward by the time since bake, so the data keeps its age. An invoice due a week
before bake is due a week before today.

## What moves

The shift has two parts:

- **Timestamps** move by the whole days since the anchor, rounded down, so their time of day
  stays.
- **Dates** move by the calendar days (UTC) since the anchor's date.

Miroir moves every date and timestamp column of every app database, and leaves alone:

- values from `9000-01-01` on, such as a `9999-12-31` that means "never";
- the columns in `database.time.exclude`, such as birthdays.

Integer columns that hold Unix time don't look like time. List them in `database.time.epoch`
as `table.column` globs, with their unit, `s` or `ms`:

```yaml
apps:
  crm:
    database:
      url: postgres://postgres@127.0.0.1:5432/crm
      time:
        exclude: [contacts.birthday]
        epoch: {deals.synced_unix: s, "events.*_ms": ms}
```

The reset observation's `now` is the time the episode starts, and the adapters tell the agent
today's date.

## The anchor

`time.anchor` is when the seed data is dated. The default, `bake`, is when bake ran. Set an
RFC 3339 instant when the seed data is written for a fixed day:

```yaml
time:
  anchor: "2026-01-15T09:00:00Z"
```

## Dates outside the databases

Miroir moves only database columns. For dates elsewhere, such as files or a search index, set
`time.hook`: a command each reset runs after moving the databases, before the apps start. It
gets the shift as `MIROIR_SHIFT_DAYS` (whole days, for timestamps) and
`MIROIR_SHIFT_DATE_DAYS` (calendar days, for dates). A failing hook fails the reset.

```yaml
time:
  hook: [node, dist/shift-index.js]
```

An env with a hook can't have [clock tasks](clock.md) or captured worlds: those move only the
databases.

## Captured worlds

A world can be a snapshot taken at a known time, such as a production dump. List when it was
taken, and bake moves its dates from then to the anchor:

```yaml
worlds: [default, prod-2026-03]
time:
  captured: {prod-2026-03: "2026-03-02T00:00:00Z"}
```

## Answers that are dates

A task with `answer: date` writes dates as they were at bake. Miroir moves the agent's answer
back by the same shift before storing it, so `SELECT value = '2026-01-20' FROM answer` keeps
working on any day.
