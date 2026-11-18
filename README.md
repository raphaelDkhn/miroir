# Miroir

Miroir turns a real product's container image into [Harbor](https://docs.harborframework.com)
environments graded from what the agent actually did to the product: the database rows it
changed, the emails and HTTP calls the product sent, and the answer it gave. Collateral damage is
reported as `harm` next to `reward`.

Miroir adds only what Harbor cannot do: run a multi-process product with a seeded world in one
image, capture its changes and side effects below the API, and grade that evidence in Harbor's
separate verifier. Harbor runs everything else: tasks, datasets, agents, sandboxes, jobs,
regrades and publishing.

## Quick start

Requirements: Docker with buildx, [uv](https://docs.astral.sh/uv/) and Harbor 0.26.0. From a
clone of this repository, build the test fixture (Acme CRM, a small CRM with 7 tasks), run its
grader self-test through Harbor, and read the result:

```bash
uv tool install harbor==0.26.0
docker buildx bake
harbor run -c fixtures/acme-crm/check.yaml --job-name quick-start
alias miroir='docker run --rm -v "$PWD:$PWD" -w "$PWD" miroir:dev miroir'
miroir check --tasks fixtures/acme-crm/tasks jobs/quick-start
```

```text
validation
  acme/close-won-deal            golden ok  nop ok  probe 1 ok  probe 2 ok
  acme/count-overdue-invoices    golden ok  nop ok
  acme/log-call-activities       golden ok  nop ok  probe 1 ok
  acme/merge-duplicate-contacts  golden ok  nop ok  probe 1 ok
  acme/notify-new-owner          golden ok  nop ok  probe 1 ok
  acme/pay-invoice               golden ok  nop ok  probe 1 ok
  acme/reassign-stale-deals      golden ok  nop ok  probe 1 ok
  7 tasks, 21 trials, 7 skipped probe slots, 0 violations, 0 voids
```

Every task's reference solution scored reward 1 and harm 0, doing nothing scored 0 and 0, and
every probe, a deliberately wrong solution, got the reward and harm its task declares. The first
build compiles Miroir and took 6 min 44 s on a 12-CPU Docker Desktop host; the job's 28 trials
took 57 s. The [quick start](https://raphaeldkhn.github.io/miroir/quick-start.html) goes on to
read a trial and run an agent.

## How it works

```text
 Harbor trial = one fresh compose project (network_mode public)
 ┌────────────────────────────────┐   HTTP :3000   ┌───────────────────────────────────────┐
 │ main   miroir-main             │ ─────────────▶ │ acme-crm   world image                │
 │  agent harness, bash, curl, jq │  network       │  tini → miroir run (supervisor)       │
 │  answer → /logs/artifacts/     │  "world"       │  web :3000 (0.0.0.0, in expose)       │
 │           answer.txt           │  (internal)    │  postgres, redis, sinks: 127.0.0.1    │
 └────────────────────────────────┘                │  triggers → miroir.log                │
                                                   └───────────────────────────────────────┘
 1. up: main waits for the world's healthcheck (miroir run wrote /miroir/run/ready)
 2. the agent runs in main
 3. Harbor downloads main's /logs/artifacts and stops main
 4. Harbor runs `miroir collect` in the world service → /miroir/evidence/evidence.db
 5. Harbor downloads /miroir/evidence and tears the environment down
 6. Harbor starts the verifier from the miroir image, restores both artifacts, and runs
    /tests/test.sh (`miroir grade`) with MIROIR_SPEC → /logs/verifier/reward.json {reward, harm}
```

- The **world image** is the product plus Postgres, `tini` and the `miroir` binary, with a world
  seeded at `docker build` by `miroir bake`. `miroir run` boots it in each trial, moves its dates
  to the trial's day, and refuses to boot if any TCP listener but the product's API is off loopback.
- The **evidence** is one SQLite file: the rows the episode changed, with their before and after
  images, the emails and requests the product sent, and faults.
- The **spec**, `MIROIR_SPEC` in each task's `task.toml`, holds SQL checks that score `reward`,
  gates, and the changes in scope. Any other change is `harm`.
- A **probe** is a wrong solution with the reward and harm it must get. `miroir check` holds the
  reference solution, doing nothing and every probe to their expectations, and summarizes agent
  runs per task (pass@1, pass^k, and whether a task is learnable).
- `miroir draft` writes a first spec from the evidence of reference runs.

Tasks are plain Harbor task directories, so they run wherever Harbor runs, and Miroir never wraps
`harbor run`.

## Documentation

- [Introduction](https://raphaeldkhn.github.io/miroir/): who it is for, and who does what between
  Harbor and Miroir
- [Quick start](https://raphaeldkhn.github.io/miroir/quick-start.html)
- [Wrap a product](https://raphaeldkhn.github.io/miroir/environment.html): the world image and its
  manifest
- [Write tasks](https://raphaeldkhn.github.io/miroir/tasks.html): the task directory, the grading
  spec and the evidence
- [Validate and publish](https://raphaeldkhn.github.io/miroir/validate.html)
- [Train](https://raphaeldkhn.github.io/miroir/train.html)
- [Gitea example](examples/gitea): stock Gitea, three worlds and a generator of 120 tasks

Serve the docs locally with `mdbook serve docs`.

## Coding agents

The [Miroir skill](skills/miroir/SKILL.md) teaches a coding agent to wrap a product, write and
validate tasks, and draft specs. In Claude Code:

```bash
claude plugin marketplace add raphaelDkhn/miroir
claude plugin install miroir@miroir
```

Other agents that read [Agent Skills](https://agentskills.io) can load `skills/miroir` directly.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Report security issues privately, as
[SECURITY.md](SECURITY.md) describes.

## License

[Apache-2.0](LICENSE)
