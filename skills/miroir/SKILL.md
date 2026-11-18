---
name: miroir
description: Wrap a product's container image as a Miroir world and write Harbor tasks graded from what the agent changed in the product's Postgres database, the emails and HTTP calls the product sent, and the agent's answer. Use to write a Dockerfile.miroir or a miroir.yaml manifest; to fix a failing `miroir bake` or boot; to write or fix task directories (instruction.md, solution/solve.sh, probes, task.toml with a MIROIR_SPEC of checks, gates and allow entries); to draft a spec with `miroir draft`; to validate graders with a check.yaml job and `miroir check`; to regrade after a spec fix; to generate tasks at scale; or to publish or train on Miroir datasets. Also use when a repository has a miroir.yaml, a Dockerfile.miroir or task.toml files holding MIROIR_SPEC, or the user mentions Miroir.
license: Apache-2.0
compatibility: Needs Docker with buildx and Harbor 0.26.0 (uv tool install harbor==0.26.0). The miroir binary is Linux-only; on other hosts run it from its image.
---

# Miroir

Miroir turns a product's container image into Harbor environments graded from what the agent
actually did to the product. Harbor runs everything else: tasks, agents, sandboxes, jobs,
regrades, publishing. Miroir never wraps `harbor run`.

## Mental model

1. A **world image** is the product plus Postgres, `tini`, the `miroir` binary and
   `/miroir/miroir.yaml`, seeded at `docker build` by `RUN miroir bake`; `miroir run` boots it.
2. A **task** is a plain Harbor task directory. The world is a compose service next to `main`,
   where the agent calls the product's own API with a test credential from `instruction.md`.
3. When the agent stops, Harbor's collect hook runs `miroir collect` in the world, which writes
   the **evidence**: net row changes, sent emails and requests, faults.
4. Harbor's separate verifier runs `miroir grade` with the task's **spec** (`MIROIR_SPEC`):
   checks score `reward`; a change no `allow` entry admits, a failed gate or a fault is `harm`,
   which zeroes the reward.
5. A grader is valid when the reference solution scores reward 1 and harm 0, doing nothing scores
   0 and 0, and every **probe** (a wrong solution) stays within its declared bounds and harm.

## Run miroir on the host

`miroir check` and `miroir draft` run on the host, from the Miroir image. The alias sees only the
current directory, so keep every path under it (Harbor's `jobs/` is):

```bash
alias miroir='docker run --rm -v "$PWD:$PWD" -w "$PWD" miroir:dev miroir'
```

`miroir:dev` is what `docker buildx bake` builds in the Miroir repository; releases publish
`ghcr.io/raphaeldkhn/miroir:<major>.<minor>`. `bake`, `run` and `collect` run inside the world
image, and `grade` inside the grader's; none of them runs on the host.

## Choose the job

| The user wants to | Then read |
|---|---|
| turn a product into a world | `references/environment.md`, then `references/manifest.md` |
| fix a bake, a boot or a collect | `references/lifecycle.md`, "Failure messages" |
| write or fix a task or its probes | `references/tasks.md` |
| write or fix a spec | `references/spec.md` and `references/evidence.md` |
| generate many tasks | `references/tasks.md`, "Generate tasks" |
| validate graders, read `miroir check` output | [Validate graders](https://miroir.mintlify.site/validate) |
| publish a dataset | [Publish](https://miroir.mintlify.site/publish), then Harbor's `publish` skill |
| train, or pick tasks to train on | [Train](https://miroir.mintlify.site/train) |
| turn a production snapshot or a ticket into a world or a task | `references/environment.md`, "From production" |

The references are pages of the docs at https://miroir.mintlify.site. A link in them such as
`/validate#trial-checks` is a path on that site; `/environment`, `/manifest`, `/lifecycle`,
`/tasks`, `/spec` and `/evidence` are the references themselves.

Look before you write. Find `miroir.yaml`, `Dockerfile.miroir`, task directories whose
`task.toml` holds `MIROIR_SPEC`, and `check.yaml`; then the product's own Dockerfile or compose
file, migrations, seed scripts, API description and authentication. The Miroir repository's
[`fixtures/acme-crm`](https://github.com/raphaelDkhn/miroir/tree/master/fixtures/acme-crm) (a CRM,
7 tasks) and [`examples/gitea`](https://github.com/raphaelDkhn/miroir/tree/master/examples/gitea)
(stock Gitea, three worlds, a task generator) are working examples to copy.

## Wrap a product

1. Map every process and datastore (command, port, user, environment), what the product sends
   out, and what runs in the background.
2. Write `Dockerfile.miroir` from the fixture's, and `miroir.yaml`: datastores first, a ready
   probe per server, `env` pointing mail and outbound URLs at the sinks, `bake` and `seed`
   commands, `database`, `sinks`, `quiet`.
3. Build: `docker build -f Dockerfile.miroir -t my-env:dev .`. The bake runs inside the build;
   a failure names the step, the command or field, and the fix.
4. Boot: `docker run --rm -p 3000:3000 my-env:dev` until it logs `booted in …`. A boot failure
   names the step and the fix, such as a port that listens off loopback.

## The authoring loop

1. Copy a fixture task directory. Set the world image and service in
   `environment/docker-compose.yaml`, and the service in `artifacts` and `[[verifier.collect]]`.
2. Write `instruction.md` (who the agent acts as, the API's URL and credential, the request) and
   `solution/solve.sh` (every call `curl -fsS`).
3. Give it a placeholder spec, `checks: [{name: todo, sql: SELECT 0}]` with `allow: []`, and run
   `miroir check --tasks tasks` for the static checks.
4. Run the reference twice, then draft the spec from both runs:

   ```bash
   harbor run -p tasks/<task> -a oracle -k 2 -n 1 --job-name ref
   miroir draft jobs/ref/<task>__*
   ```

5. Paste the draft into `MIROIR_SPEC` and review it: drop checks the instruction does not ask
   for, loosen exact text the agent may word its own way, move invariants to gates.
6. Regrade instead of rerunning. Bump the minor version, then:

   ```bash
   harbor job regrade jobs/ref -p tasks/<task> --job-name ref-2
   cat jobs/ref-2/*/verifier/test-stdout.txt
   ```

   The reference must read `reward 1.00  harm 0`. Each out-of-scope change is printed with a
   paste-ready `allow` line.
7. Add probes: `solution/probe-N.sh` plus a `[[metadata.miroir.probes]]` entry with `harm` and a
   reward bound. Run the check job until `miroir check` exits 0:

   ```bash
   harbor run -c check.yaml --job-name gate
   miroir check --tasks tasks jobs/gate
   ```

A probe that earns too much means the spec is too weak: fix the spec. A probe whose calls fail
proves nothing: `miroir check` fails a solution that exits non-zero.

## Rules

1. Only `expose` ports listen off loopback. Bind datastores and everything else to 127.0.0.1; the
   boot refuses any other TCP listener.
2. Read `after."<t>"`, `before."<t>"` and `answer`, not the JSON in `changes`.
3. Let `allow` do row scope. What it forbids needs no gate.
4. A check that doing nothing satisfies is a gate: the null run must score 0.
5. Probes declare `harm` and a reward bound, and every HTTP call in a solution or probe uses
   `curl -fsS`, or asserts the status it expects.
6. Never weaken a probe or the reference solution to make validation pass.
7. `environment/` is the same bytes in every task of a world; nothing per task reaches the world.
8. Bump `[task].version`: minor for the spec or the grader image, major for the instruction, the
   world or the environment, patch for solutions, probes and metadata.
9. Pin all three images by digest when publishing: the world, `miroir-main` and `miroir`.
10. Answers go to `/logs/artifacts/answer.txt`, as the instruction says.

## Harbor mechanics

Harbor's own [`create-task`](https://github.com/harbor-framework/harbor/tree/main/skills/create-task)
and [`publish`](https://github.com/harbor-framework/harbor/tree/main/skills/publish) skills and
its [docs](https://docs.harborframework.com) cover task directories, jobs, agents, regrades and
the Hub. Miroir's tasks follow them, with a separate verifier from the `miroir` image and a
collect hook in the world service.
