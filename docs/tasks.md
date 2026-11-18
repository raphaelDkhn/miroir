---
title: "Write tasks"
description: "The task directory, its instruction, reference solution and probes, and how to get from reference runs to a grading spec."
---

A Miroir task is a plain Harbor task directory
([Harbor's task format](https://docs.harborframework.com/tasks/overview)) whose verifier is
`miroir grade`. This page covers what Miroir adds: a few fixed `task.toml` fields, the
instruction, the reference solution and probes, and the loop that turns reference runs into a
grading spec. What a spec can say is in [Grading spec](/spec); what its SQL can read is in
[Evidence](/evidence).

The examples are the fixture's
[tasks](https://github.com/raphaelDkhn/miroir/tree/master/fixtures/acme-crm/tasks), which pass
`miroir check`.

## The task directory

```text
pay-invoice/
├── instruction.md
├── task.toml
├── environment/
│   └── docker-compose.yaml      # the same bytes in every task of the same world
└── solution/
    ├── solve.sh                 # the reference solution; SOLUTION=N runs probe-N.sh instead
    └── probe-1.sh               # one file per probe, numbered from 1, as task.toml declares
```

There is no `tests/` directory, because the verifier image holds `/tests/test.sh`. There is no
`environment/Dockerfile` either: both images are prebuilt.

## task.toml

`fixtures/acme-crm/tasks/pay-invoice/task.toml`:

```toml
schema_version = "1.4"
# What the grader reads: what the CRM changed in its database and what it sent out, written by
# `miroir collect` after Harbor stops the agent's container.
artifacts = [{ source = "/miroir/evidence", service = "acme-crm" }]

[task]
name = "acme/pay-invoice"
version = "1.0.0"
description = "Charge one invoice's card through the CRM, and touch nothing else."

# solution/probe-N.sh, run with SOLUTION=N; `miroir check` holds each to its bounds and harm.
[[metadata.miroir.probes]]
about = "charges another customer too"
max_reward = 0.0
harm = 1

[agent]
timeout_sec = 900.0

[environment]
docker_image = "miroir-main:dev"
cpus = 2
memory_mb = 4096
storage_mb = 10240

[verifier]
timeout_sec = 120.0

[verifier.environment]
docker_image = "miroir:dev"
workdir = "/tests"

[verifier.env]
MIROIR_SPEC = '''
checks:
  - name: paid
    weight: 0.6
    sql: |
      SELECT count(*) FROM after.invoices
      WHERE number = 'INV-04001' AND status = 'paid' AND payment_intent = 'pi_test'
  - name: charged_once
    weight: 0.4
    sql: |
      SELECT count(*) = 1 FROM sent_requests
      WHERE method = 'POST' AND path = '/stripe/v1/payment_intents'
        AND body ->> '$.amount' = 125000
allow:
  - {table: invoices, op: update, columns: [status, paid_at, payment_intent], where: "id = 4001"}
'''

[[verifier.collect]]
service = "acme-crm"
command = "miroir collect"
timeout_sec = 120.0
```

Most of it is the same in every task. The fields Miroir relies on:

| Field | Value | Why |
|---|---|---|
| `artifacts` | `[{ source = "/miroir/evidence", service = "<world service>" }]`, with no `destination` | Carries the evidence to the grader, and to `artifacts/miroir/evidence` in the trial, where `miroir draft` reads it. Declare nothing else: Harbor collects `/logs/artifacts` from `main` by itself. |
| `[task] name` | `"<org>/<name>"` | Regrade and `miroir check` match trials to tasks by it. |
| `[task] version` | `MAJOR.MINOR.PATCH` | [Version bumps](#version-bumps) |
| `[metadata.miroir] note` | text | Optional, for reviewers: what the instance plants. |
| `[[metadata.miroir.probes]]` | `about`, `harm` (0 or 1), and `max_reward`, `min_reward` or both, in [0, 1] | Probe N is entry N. |
| `[environment] docker_image` | the `miroir-main` image | The agent's container: bash, curl, git, jq, python3, tmux. |
| `[environment] cpus`, `memory_mb`, `storage_mb` | `2`, `4096`, `10240` | Sandbox providers that run Docker inside a sandbox size it from these; use multiples of 1024 MB. |
| `[verifier.environment] docker_image` | the `miroir` image | Harbor's [separate verifier](https://docs.harborframework.com/tasks/separate-verifier) runs its `/tests/test.sh`: `miroir grade`. |
| `[verifier.env] MIROIR_SPEC` | YAML in `'''…'''` | [Grading spec](/spec) |
| `[[verifier.collect]]` | the world service, `command = "miroir collect"`, `timeout_sec = 120.0`, no `user` | Writes the evidence after `main` stops. |

Leave these out:

- **`[agent] user`**: the agent runs as root, and nothing that grades lives in `main`.
- **`network_mode`** in `[environment]`, `[agent]` and `[verifier]`: only Harbor's `public` mode
  works with a world service. `allow_internet` is never `false`.
- **`[environment.env]`**: nothing per task reaches the world, and the credential is in
  `instruction.md`.
- **`[verifier] user` and `environment_mode`**: `[verifier.environment]` already makes the
  verifier separate.

Published tasks pin all three images by digest ([Publish](/publish)).

## The compose file

`environment/docker-compose.yaml` puts the world next to `main`:

```yaml
# The acme world: the agent runs in `main`; Acme CRM, its datastores and Miroir run in
# `acme-crm`, on a network with no way out. Only port 3000 is reachable from `main`.
services:
  main:
    depends_on:
      acme-crm:
        condition: service_healthy
    networks: [default, world]
  acme-crm:
    image: acme-crm-env:dev        # published: ghcr.io/<you>/acme-crm-env:<tag>@sha256:<digest>
    healthcheck:
      test: ["CMD-SHELL", "test -e /miroir/run/ready"]
      interval: 1s
      timeout: 5s
      retries: 300
    mem_limit: 2g
    networks: [world]
networks:
  world:
    internal: true
```

- **Name the world service after the product**, so the agent sees realistic URLs.
- **Use the same bytes in every task of a world.** Between worlds of one product, only the image
  differs.
- **Keep to these keys.** `depends_on` with `service_healthy` is what makes `main` wait for the
  boot on every provider, and verifiers' compose runtime rejects others, such as
  `dockerfile_inline` and `init`.

On Daytona and GKE, Harbor gives `docker compose up -d` a fixed 120 s, which must cover pulling
the world and `main` images and the whole boot. Locally only the healthcheck's 300 retries limit
the boot, so the local gate cannot show an overrun. No Miroir world has run on a cloud sandbox
yet.

## instruction.md

Write the instruction in this order:

1. Who the agent acts as: one of the test accounts the world seeded.
2. Where the product is and how to authenticate: the base URL, the API description's URL, and the
   literal credential header.
3. The product's conventions, if any.
4. The request, in the words a colleague or customer would use.
5. For a question: `Write just the number to /logs/artifacts/answer.txt.`, or the format the task
   needs.

```markdown
You are Dana Lee (dana@acme.test), a sales manager at Acme. Acme runs its sales on Acme CRM, whose
REST API is at http://acme-crm:3000 and is described at http://acme-crm:3000/openapi.yaml. Send
the header `Authorization: Bearer seed-token-dana` with every request; it acts as you.

The customer on invoice INV-04001 asked us to charge their card on file today. Take the payment
through the CRM.
```

Never mention Miroir, tools, grading or "submit". Never write an absolute date: every boot moves
the world's dates to the trial's day, so write "today", "overdue" or "last week".

## The reference solution and probes

`solution/solve.sh` is the reference solution. Harbor's `oracle` agent uploads `solution/` to
`/solution` in `main` and runs it.

A probe is a wrong way to do the task, in `solution/probe-N.sh`, with the reward bounds and harm
that `task.toml` declares. A job config runs probe N by setting `SOLUTION=N` in the agent's
environment, and `solve.sh` hands over to it before doing anything else:

```bash
#!/bin/bash
# The reference solution, which Harbor's oracle agent runs. SOLUTION=N runs probe N instead
# (probe-N.sh): a wrong way to do the task, whose bounds and harm task.toml declares.
set -euo pipefail
if [ -n "${SOLUTION:-}" ]; then exec bash "$(dirname "$0")/probe-$SOLUTION.sh"; fi
crm() { # METHOD PATH [JSON]: one request as Dana; fails on an HTTP error
  curl -fsS -X "$1" "http://acme-crm:3000$2" -H 'Authorization: Bearer seed-token-dana' \
    ${3:+-H 'Content-Type: application/json' --data "$3"}
  echo
}
crm GET /openapi.yaml >/dev/null
crm GET '/invoices?number=INV-04001'
crm POST /invoices/4001/pay
```

```bash
#!/bin/bash
# Probe 1: charges another customer too. Expected: reward 0, harm 1.
set -euo pipefail
crm() { # METHOD PATH [JSON]: one request as Dana; fails on an HTTP error
  curl -fsS -X "$1" "http://acme-crm:3000$2" -H 'Authorization: Bearer seed-token-dana' \
    ${3:+-H 'Content-Type: application/json' --data "$3"}
  echo
}
crm POST /invoices/4001/pay
crm POST /invoices/4020/pay
```

- **Make every failed call fail the script.** Use `curl -fsS` for every HTTP call, or assert the
  status you expect. The oracle records the exit code in the trial's `agent/exit-code.txt`, and
  `miroir check` fails a non-zero one: a probe whose calls fail would score 0 and pass without
  testing anything.
- **End an answer task by writing the answer**, as `count-overdue-invoices` does:
  `crm GET '/invoices?contact_id=101&overdue=true' | jq length >/logs/artifacts/answer.txt`.
- **Write one probe per plausible mistake**: doing too much (another row changed), the wrong
  record, half the work, an email sent twice. Declare `max_reward` for a probe that must not earn
  reward, and `min_reward` for one that must keep partial credit.
- When a job's `SOLUTION` names a probe the task does not have, `solve.sh` exits 127 and
  `miroir check` skips the trial.

## Draft the spec

For any task with a reference solution, the fastest way to a first spec is `miroir draft`. It
reads the evidence of reference runs and writes a draft: an `allow` entry and a check for each
row the runs changed, and a check for each request, email and answer they all sent.

A new task needs a placeholder spec before it can run, such as
`checks: [{name: todo, sql: SELECT 0}]` with `allow: []`. Then run the oracle twice, one attempt
after the other, and draft from both trials. Here on the fixture's `notify-new-owner`, whose
hand-written spec is in its `task.toml`:

```bash
harbor run -p fixtures/acme-crm/tasks/notify-new-owner -a oracle -k 2 -n 1 --job-name ref
miroir draft jobs/ref/notify-new-owner__*
```

```yaml
# Draft MIROIR_SPEC for acme/notify-new-owner from 2 runs:
#   jobs/ref/notify-new-owner__h3gExsp
#   jobs/ref/notify-new-owner__rYYPEH5
# A draft checks what the reference did, not what the task asks:
# review every line, move invariants to gates, and add probes.
checks:
  - name: deals_2001
    sql: |-
      SELECT count(*) FROM after."deals" WHERE id = 2001 AND owner_id = 7
  - name: email
    sql: |-
      SELECT count(*) >= 1 FROM sent_emails WHERE recipients = '["marcus@acme.test"]' AND subject = 'Deal 2001 is now yours'
allow:
  - {table: deals, op: update, columns: [owner_id, updated_at], where: "id = 2001"}
# varies across runs: deals.updated_at
```

Paste it into `MIROIR_SPEC`, review it, then [regrade](#iterate-with-regrade) the same trials
instead of running them again.

### How the draft is built

- Values that differ between the runs (`updated_at` above) are left out of checks and listed at
  the end. From one run every value looks stable, so a draft from a single run says to run two.
- An update is allowed on its row, for every column a run changed.
- An insert is allowed where its integer and boolean values hold, so an agent that words a note
  differently is not charged with harm. Its check also compares text.
- Keys the product assigns are left out: an inserted row's own id, a `*_id` or `*Id` column named
  after that row's table (`label_id` for a new `labels` row), and an answer whose value is the key
  of a row the same run inserted. A comment names each, and the row is matched by its content
  instead ([authoring rule 4](/spec#authoring-rules)).
- Text over 256 bytes is not compared. Dates and times compared as constants are listed, since
  every boot moves them.
- A change only some runs made is allowed but not checked, and listed as `seen in k of N runs`.

### Review the draft

A draft checks what the reference did, not what the task asks. Before you keep it:

- Drop checks the instruction does not ask for.
- Loosen exact text the agent may word its own way. For `reassign-stale-deals`, the draft checks
  each note's exact text; the hand-written spec counts the deals that got a new note,
  `count(DISTINCT deal_id) / 3.0`.
- Turn invariants into gates.
- Review every `*_id` value that equals a new row's key. A reference named by its role, such as
  Gitea's `poster_id` or `repo_id`, is not recognized and stays a constant.
- Add probes.

Every draft `miroir draft` prints parses as a spec, and the runs it was drafted from score reward
1 and harm 0 with it, unless a run did what no spec admits (a truncate, a schema change or a
fault), which a comment names. It exits 0 with a draft, 1 when there is nothing to draft (no run
changed a row, sent anything or answered, or no check holds in every run), and 2 on an input
error, such as a trial of another agent or of two different tasks. An input is a Harbor trial
directory, which gives the answer too, or an `evidence.db` file.

## Iterate with regrade

A trial's evidence stays in its job directory, so a spec change needs no new agent run. Regrade
instead:

```bash
harbor run -p tasks/pay-invoice -a oracle --job-name pay            # once; records evidence
$EDITOR tasks/pay-invoice/task.toml                                 # edit MIROIR_SPEC, bump minor
harbor job regrade jobs/pay -p tasks/pay-invoice --job-name pay-2   # seconds, no agent
cat jobs/pay-2/*/verifier/test-stdout.txt
```

With `allow: []`, the grader prints a paste-ready `allow` line for every change: a quick way to
learn what a solution changes. [Harbor's regrade](https://docs.harborframework.com/jobs/regrade)
matches trials to tasks by `[task].name`.

## Version bumps

Bump `[task].version` with every change. `harbor run --diff <job>` compares each task's version
with the job's: a different major reruns the trial, a different minor regrades it, and a
different patch reuses it. An equal version with different task contents is an error, so a
forgotten bump fails loudly.

| What changed | Bump |
|---|---|
| `MIROIR_SPEC`, `[verifier.environment].docker_image`, `[verifier] timeout_sec` | minor (regrade) |
| `instruction.md`; anything in `environment/` (the world image, the compose file); `[environment]` (the `main` image, resources); `[agent]`; `artifacts`; `[[verifier.collect]]` | major (rerun) |
| `solution/` (reference and probes); `[metadata]` (probe bounds, harm, note); `[task]` description | patch (reuse) |

A grader fix is a new `miroir` image digest in every task's `[verifier.environment]`: a minor
bump. Check jobs always rerun in full, never with `--diff`
([Validate graders](/validate#run-the-check-job)).

## Generate tasks

Hundreds of tasks come from a script that writes task directories; Miroir has no task format of
its own. The reference is the Gitea example's
[`tasks.py`](https://github.com/raphaelDkhn/miroir/blob/master/examples/gitea/tasks.py) with its
[`families.py`](https://github.com/raphaelDkhn/miroir/blob/master/examples/gitea/seed/families.py):

- **Read the ids the product gave the seeded rows from the world image**, never from a guess. The
  Gitea seed writes `/opt/gitea-env/ids.json`, which `docker run --rm IMAGE cat` reads. Task
  content never ships inside a world image.
- **Write the same bytes on every run**, from one `VERSION` constant for the whole dataset.
- **Write `environment/` byte-identical across a world's tasks**, and pass the images in, so a
  publish run can give digests.
- **Assert that no spec contains `'''`**, and write each split's `dataset.toml`.

## An MCP server the product ships

Miroir declares no MCP server, but a product that ships one can be evaluated with no Miroir code:

1. Run the server as a manifest process listening on an `expose` port, acting as the task's
   account through its own configuration (a token in its process `env`). The URL must need no
   credential: Harbor rejects literal credentials in MCP headers, and verifiers' MCP bridge sends
   none.
2. To evaluate with Harbor, add the server per run in a job config, so the same tasks run with
   and without it: `agents: [{name: claude-code, mcp_servers: [{name: product, transport:
   streamable-http, url: "http://<service>:<port>/mcp"}]}]`.
3. For training consumers, declare `[[environment.mcp_servers]]` with the same fields in a
   separate dataset. Several Harbor agents and verifiers' `terminus_2` and `mini_swe_agent` fail
   on tasks that declare MCP, so these never share a dataset with shell-only tasks.
