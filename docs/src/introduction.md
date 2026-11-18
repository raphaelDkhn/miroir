# Introduction

Miroir turns a real product's container image into [Harbor](https://docs.harborframework.com)
environments graded from what the agent actually did to the product: the database rows it
changed, the emails and HTTP calls the product sent, and the answer it gave. Collateral damage is
reported as `harm` next to `reward`.

Miroir adds only what Harbor cannot do: run a multi-process product with a seeded world in one
image, capture its changes and side effects below the API, and grade that evidence in Harbor's
separate verifier. Harbor runs everything else, and Miroir never wraps `harbor run`: every
command you run against a trial is a Harbor command.

## Who it is for

- **Product and platform teams** that evaluate or train their own agent on their own product.
  They get their real product with realistic data, their agent unchanged, a harm number next to
  the success number, and a CI gate.
- **Publishers** shipping the official environment of their product: a world image plus a Harbor
  dataset that runs wherever Harbor runs, with a release gate.
- **Labs and RL teams** consuming those datasets. For them Miroir is nearly invisible: they run
  Harbor task directories on their own stack, and use `miroir check` to calibrate before
  training.

## Four nouns

- The **world image** is the product, Miroir and a seeded world, baked at `docker build`.
- The **evidence** is what one episode changed and sent: one SQLite file per trial.
- The **spec** is how to grade that evidence: SQL checks, gates and the changes in scope, in the
  task's `[verifier.env] MIROIR_SPEC`.
- A **probe** is a wrong solution with the reward bounds and the harm it must get.

Everything else is Harbor.

## One trial

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

The agent reaches the product only on the ports the world's manifest exposes, and it never
shares a container with the database, the change log or the grader.

## Who does what

| Concern | Owner | How |
|---|---|---|
| Task format, datasets, versions, lint, publishing | Harbor | [Task directories](https://docs.harborframework.com/tasks/overview), `[task].version`, `harbor lint`, [`harbor publish`](https://docs.harborframework.com/harbor-hub/publish) |
| Fresh state per episode, isolation between trials | Harbor | One compose project per trial, removed with its volumes |
| Concurrency, retries, valid rewards | Harbor | `-n`, retry policy, `reward_valid` |
| Sandbox providers | Harbor | [docker, daytona, modal, gke and others](https://docs.harborframework.com/sandboxes/pre-integrated-sandboxes) |
| Agents, harnesses, the agent's tools | Harbor and the agent | [Installed agents](https://docs.harborframework.com/agents/pre-integrated-agents) run in `main` |
| The agent's container | Harbor; Miroir ships a default image | `[environment].docker_image = miroir-main` |
| The world's readiness | Compose, with Miroir's signal | `depends_on: service_healthy`; healthcheck `test -e /miroir/run/ready` |
| The product cut off from the internet | Compose | The world service sits only on an `internal: true` network |
| Several processes in one image: order, readiness, crashes, quiet | Miroir | `miroir run` supervises them |
| A seeded world in the image | Miroir | `miroir bake` at `docker build`, one image per world |
| Seeded ages that track today's date | Miroir | A date shift at boot |
| Outbound email and HTTP | Miroir | Loopback SMTP and HTTP sinks, with canned responses |
| Database change capture | Miroir | Postgres triggers, folded into net changes |
| When evidence is taken | Harbor | A [collect hook](https://docs.harborframework.com/jobs/artifact-collection) in the world service, after `main` stops |
| What the evidence is | Miroir | `miroir collect` writes `/miroir/evidence/evidence.db` |
| Moving evidence and the answer to the grader | Harbor | [Artifacts](https://docs.harborframework.com/tasks/artifacts): `/miroir/evidence` from the world service, `/logs/artifacts` from `main` |
| Isolated grading | Harbor | A [separate verifier](https://docs.harborframework.com/tasks/separate-verifier) from the `miroir` image |
| Judgment: checks, scope, harm, reward | Miroir | `/tests/test.sh` runs `miroir grade` |
| Golden run, null run, repeats | Harbor | The `oracle` agent with `solution/solve.sh`, the `nop` agent, `-k` |
| Probes | Harbor runs them, Miroir asserts | `SOLUTION=N` and `miroir check` |
| Re-scoring after a spec fix | Harbor | [`harbor job regrade`](https://docs.harborframework.com/jobs/regrade), `harbor run --diff` |
| Per-task pass rates, pass^k, classes, void counts | Miroir | `miroir check` over job directories |
| Trajectories and the viewer | Harbor | [`harbor view`](https://docs.harborframework.com/results/view-job-results) |
| Writing hundreds of tasks | The publisher | Any script that writes task directories, such as the [Gitea generator](https://github.com/raphaelDkhn/miroir/blob/master/examples/gitea/tasks.py) |
| Training | The trainers | Consumers that run Harbor task directories ([Train](train.md)) |

## What Miroir is not

- **Not a layer between the agent and the product.** The agent calls the product's own API at
  `http://<service>:<port>` with a test credential written in its instruction. Miroir ships no
  gateway and no tools; a product's own MCP server can still be evaluated
  ([Write tasks](tasks.md#a-products-own-mcp-server)).
- **Not a task format.** Tasks are plain Harbor task directories, written by hand or by a
  generator.
- **Not a judge of prose.** Checks are deterministic SQL over the evidence; the quality of a
  written comment is approximated at best.
- **Not a scrubber.** A production snapshot is scrubbed before it leaves its network; Miroir
  restores the scrubbed dump ([From production](environment.md#from-production)).
- **Not multi-step.** One instruction per task, graded once at the end.
- **Postgres only**, one product per image, and one world per image.

The code is at [github.com/raphaelDkhn/miroir](https://github.com/raphaelDkhn/miroir).
