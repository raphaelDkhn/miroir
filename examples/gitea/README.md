# Gitea

Stock [Gitea](https://about.gitea.com) 28.3 as a Miroir world, and a generator that writes 120
Harbor tasks against it. Gitea runs unmodified from its official image, pinned by digest; the
example only adds configuration, a seed that builds each world through Gitea's own API, and the
task generator.

Each world is a company's engineering organization in Gitea: people, teams, repositories,
CODEOWNERS, labels, milestones, issues, pull requests, reviews and releases, with ages that move
with the calendar.

| World     | Company             | Splits             |
| --------- | ------------------- | ------------------ |
| `orbit`   | Orbit Payments      | train, validation  |
| `ferry`   | Ferrylane Logistics | train, validation  |
| `kestrel` | Kestrel Health      | test               |

The test world shares no person, repository or label with the other two, so a test score
measures what an agent knows about Gitea, not about the data it trained on.

Ten task families, each asked four ways in every world:

| Family          | The request                                                             |
| --------------- | ----------------------------------------------------------------------- |
| `stale-prs`     | Name the open pull requests with no activity for N days (an answer)     |
| `offboard`      | Hand a leaver's open work to a colleague, then take them off every team |
| `count-issues`  | Count the open issues that match labels and assignees (an answer)       |
| `triage`        | Label a new issue, set its milestone, assign its CODEOWNERS owner       |
| `close-stale`   | Close the issues that waited N days on their reporter, with a comment   |
| `grant-access`  | Give a contractor one repository, and nothing more                      |
| `add-webhook`   | Add a CI webhook with exactly the events asked, then test it            |
| `merge-ready`   | Merge the approved pull requests that no change request holds back      |
| `ship-release`  | Publish a release from a milestone, then roll the milestone over        |
| `redact-secret` | Redact a key pasted in a comment, lock the issue, have the key rotated  |

The first eight are trained on. `ship-release` and `redact-secret` are held out: their tasks
are only in the validation split of `orbit` and `ferry`, and in the test world. That makes 48
train, 32 validation and 40 test tasks, with 336 probes: wrong ways to do a task, each with the
reward and harm it must get. Grading reads what Gitea changed in its database, the webhook
deliveries and emails it sent, and the answer file.

## Files

| Path                | What it is                                                                |
| ------------------- | ------------------------------------------------------------------------- |
| `Dockerfile.miroir` | The world image: Gitea, Postgres, Python for the seed, tini and miroir    |
| `miroir.yaml`       | How Miroir runs and bakes the world                                       |
| `app.ini`           | Gitea's configuration                                                     |
| `seed/worlds.py`    | The three companies                                                       |
| `seed/plants.py`    | What each family plants in a world before it is built                     |
| `seed/seed.py`      | Builds a world through Gitea's API at `docker build`, and records its ids |
| `seed/families.py`  | Writes each family's tasks: instruction, grading spec, solution, probes   |
| `tasks.py`          | Renders the tasks as Harbor task directories                              |
| `check.yaml`        | The Harbor job that validates every task's grader                         |

Only `worlds.py`, `plants.py` and `seed.py` go into the image. A change to them changes the
worlds; a change to `families.py` or `tasks.py` changes only the generated tasks.

## Build

Requirements: Docker with buildx, [uv](https://docs.astral.sh/uv/) and
[Harbor](https://harborframework.com) (`uv tool install harbor==0.26.0`). From the repository
root:

```bash
docker buildx bake miroir main gitea
```

That builds `miroir:dev` (the grader), `miroir-main:dev` (the agent's container) and one image
per world: `gitea-env:orbit`, `gitea-env:ferry` and `gitea-env:kestrel`. Each world is built
inside `docker build` by `miroir bake`, which runs the seed with the `WORLD` build argument.
`docker run --rm -p 3000:3000 gitea-env:orbit` boots one in the foreground.

## Generate the tasks

```bash
uv run examples/gitea/tasks.py --out examples/gitea/datasets \
  --world orbit=gitea-env:orbit --world ferry=gitea-env:ferry --world kestrel=gitea-env:kestrel \
  --main miroir-main:dev --grader miroir:dev
```

For each world, the generator reads the ids Gitea gave its rows from the image
(`docker run --rm gitea-env:orbit cat /opt/gitea-env/ids.json`), plants the world again, and
stops unless the plants match the image's: the seed records their SHA-256 in `ids.json`. If they
differ, rebuild the worlds from this checkout, or run the generator with the image's Python
(`uv run --python 3.12 examples/gitea/tasks.py …`).

It writes `examples/gitea/datasets/{train,validation,test}/<task>/` and a `dataset.toml` for
each split. Tasks are named `raphaeldkhn/gitea.<family>.<world>.<n>`; `--org` changes the first
part. The output is the same bytes on every run, and the directory is replaced whole, so edit
`seed/families.py` and regenerate instead of editing it. To publish, pass the images by digest
(`ghcr.io/…@sha256:…`) so every task pins all three.

Each task's agent acts as the world's engineering manager, through Gitea's REST API at
`http://gitea:3000/api/v1`, with the token written in its instruction.

## Validate the grader

```bash
alias miroir='docker run --rm -v "$PWD:$PWD" -w "$PWD" miroir:dev miroir'
miroir check --tasks examples/gitea/datasets                   # static checks
docker compose config --help | grep -q -- --no-env-resolution  # lint needs Compose 2.35+
harbor lint examples/gitea/datasets/*/*/
harbor run -c examples/gitea/check.yaml -n 8 --job-name gitea-gate
miroir check --tasks examples/gitea/datasets jobs/gitea-gate
```

`check.yaml` runs every task with its reference solution, with an agent that does nothing, and
with each of its probes. `miroir check` must then exit 0:
[Validate graders](https://miroir.mintlify.site/validate#trial-checks) lists
what it holds each trial to.

## Run an agent

```bash
harbor run -p examples/gitea/datasets/test -a claude-code -m anthropic/claude-sonnet-5-5 \
  -k 4 -n 8 --job-name gitea-eval
miroir check jobs/gitea-eval
```

Any Harbor agent with a shell works: the instruction holds the URL and the token.

## Calibrate

Before training, run the agent you mean to train on the train split, then train on the tasks
whose class is `learnable` ([Calibrate](https://miroir.mintlify.site/train#calibrate)):

```bash
harbor run -p examples/gitea/datasets/train -a <agent> -m <model> -k 4 --job-name gitea-calibrate
miroir check --json jobs/gitea-calibrate
```

## Fix a check and regrade

```bash
$EDITOR examples/gitea/seed/families.py   # fix the check, then bump VERSION's minor in tasks.py
uv run examples/gitea/tasks.py …           # the command above
harbor job regrade jobs/gitea-eval -p examples/gitea/datasets/test --job-name gitea-eval-2
```

Regrading reuses the trials' evidence, so it runs no agent. `VERSION` in `tasks.py` is every
task's version; [Version bumps](https://miroir.mintlify.site/tasks#version-bumps)
says which part to bump.

## Train

Not yet run: the commands below follow TRL's `examples/async_grpo_harbor` (AsyncGRPO through
OpenEnv's `harbor_env`, which runs Harbor's own trials), and the TRL commit and OpenEnv release
they run with will be pinned here after the first run. On a host with two GPUs and Docker, from
that example's directory:

```bash
openenv harbor serve --dataset examples/gitea/datasets/train --port 8200 --capture-port 8300
CUDA_VISIBLE_DEVICES=0 VLLM_SERVER_DEV_MODE=1 vllm serve Qwen/Qwen3.5-2B …  # TRL's flags
CUDA_VISIBLE_DEVICES=1 python async_grpo_harbor.py --server http://localhost:8200 \
  --vllm-url http://localhost:8000 --model Qwen/Qwen3.5-2B --split <served dataset> \
  --harness mini-swe-agent --sandbox docker --reward-key reward --agent-step-limit 40 --max-steps 5
```

[Train](https://miroir.mintlify.site/train#the-committed-recipe) says which of these
flags matter to Miroir, and how harm and voids reach the trainer.

## Results

Measured on 2026-11-18 with the images built from this repository and the tasks at version 2.0.0,
on a Mac with Docker Desktop 4.37 (Engine 27.4; 12 CPUs and 8 GB of memory for its VM) and
Harbor 0.26.0. Another project's Harbor trials used the same Docker VM on and off: during the
one-at-a-time trials, and possibly during the gate and determinism jobs. The boots had it to
themselves.

| Claim                                                         | Evidence                        |
| ------------------------------------------------------------- | ------------------------------- |
| Grader validation passes: 120 reference, 120 idle, 336 probes | all pass, no voids or errors    |
| The same run scores the same every time                       | 3 reference runs per task agree |
| A world boots in seconds                                      | 3.5 to 4.7 s locally            |
| World image size                                              | 380 MB, 114 MB compressed       |
| A world boots within a cloud sandbox's 120 s                  | not yet run                     |
| A frontier agent on the test split: reward, harm rate, pass^4 | not yet run                     |
| Training on the train split improves held-out work            | open: not yet run               |

- **The grader.** `harbor run -c examples/gitea/check.yaml -n 8` ran 720 trials in 38 min 1 s:
  every task with its reference solution, with `nop`, and with `SOLUTION=1` to `4`; 144 of those
  probe slots are past a task's probe count, and `miroir check` skips them. The reference
  solution scored reward 1 and harm 0 on 120 of 120 tasks, doing nothing 0 and 0 on 120 of 120,
  and 336 of 336 probes stayed within their bounds with the harm they declare. `miroir check`
  found 0 violations, 0 voids and 0 check errors. The Docker VM had at most 4.2 GiB of memory in
  use. With Compose 2.38.2, `harbor lint` found nothing in the 120 tasks.
- **Determinism.** The oracle alone with `-k 2` ran 240 trials in 12 min 1 s, with at most
  4.9 GiB of memory in use. All 240 scored reward 1 and harm 0, and `miroir check` over them and
  the gate job held each task's three reference runs to one reward and harm, with 0 violations.
- **Boot.** `docker run gitea-env:<world>`, twice per world: `booted in` 4.7 and 4.1 s (orbit),
  4.2 and 3.5 s (ferry), 3.7 and 4.0 s (kestrel). Starting Gitea took 1.6 to 2.0 s of it, and
  waiting for its queues 1.5 to 2.4 s.
- **Images.** Each world image is 380 MB, and 113 to 114 MB as `docker save | gzip`. The three
  share every layer but their baked world, 65 MB. Baking a world (`RUN miroir bake`) took 57 to
  60 s, with the three baking at once. `miroir-main` is 251 MB (80 MB compressed) and `miroir`
  23 MB (9 MB).
- **Time per trial** (mean, then maximum). One at a time, the reference runs of ten tasks, one
  per family, took 14.5 s (17.1 s): environment setup 7.4 s (9.1 s), agent 0.9 s (1.6 s), from
  the agent's end to the verifier's start, which stops `main` and collects the evidence, 3.2 s
  (4.0 s), and verifier 3.0 s (3.3 s). Over the gate job's 720 trials, 8 at a time, a trial took
  25.2 s (52.7 s): environment setup 14.1 s (33.1 s), agent 1.3 s (5.9 s), collection 4.8 s
  (9.3 s), verifier 5.1 s (10.1 s).

## Limitations

- The worlds are generated, not taken from production data;
  [From production](https://miroir.mintlify.site/environment#from-production) shows
  how to build one from a snapshot.
- The tasks are short workflows, not projects: their reference solutions make 2 to 16 requests
  to Gitea, the API description included.
- Validation runs each grader on its reference solution, on doing nothing and on its probes. A
  wrong way that no probe tries is graded only as far as its task's checks and scope reach.
- Checks are SQL over what changed and what was sent. The quality of a comment is approximated:
  `close-stale` looks for the word "reopen".
- Git refs and files are not graded: Miroir captures database rows, emails and webhook calls.
