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
train, 32 validation and 40 test tasks, with 300 probes: wrong ways to do a task, each with the
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
docker compose config --help | grep -q -- --no-env-resolution  # else lint skips the images
harbor lint examples/gitea/datasets/*/*/
harbor run -c examples/gitea/check.yaml -n 8 --job-name gitea-gate
miroir check --tasks examples/gitea/datasets jobs/gitea-gate
```

`check.yaml` runs every task with its reference solution, with an agent that does nothing, and
with each of its probes. The grader passes when the reference solution scores reward 1 and
harm 0 on all 120 tasks, doing nothing scores 0 and 0, every probe stays within its bounds with
the harm it declares, and no trial voids or records a check error. Rerun the job in full after
any change; Harbor's mean reward for it means nothing, `miroir check` is the verdict.

## Run an agent

```bash
harbor run -p examples/gitea/datasets/test -a claude-code -m anthropic/claude-sonnet-4-5 \
  -k 4 -n 8 --job-name gitea-eval
miroir check jobs/gitea-eval
```

Any Harbor agent with a shell works: the instruction holds the URL and the token. `miroir check`
prints, per task and per agent, the mean reward, the harm rate, pass@1, pass^4 and a class.

## Calibrate

Run the agent you mean to train on the train split with at least two attempts per task, then
read the classes:

```bash
harbor run -p examples/gitea/datasets/train -a <agent> -m <model> -k 4 --job-name gitea-calibrate
miroir check --json jobs/gitea-calibrate
```

A task is `solved` when every attempt scores 1, `unsolved` when every attempt scores the same
value below 1, and `learnable` when the rewards differ. Only the learnable tasks give a GRPO
group any signal.

## Fix a check and regrade

```bash
$EDITOR examples/gitea/seed/families.py   # fix the check, then bump VERSION's minor in tasks.py
uv run examples/gitea/tasks.py …           # the command above
harbor job regrade jobs/gitea-eval -p examples/gitea/datasets/test --job-name gitea-eval-2
```

Regrading reuses the trials' evidence, so it takes seconds and runs no agent. `VERSION` in
`tasks.py` is every task's version: bump the minor for a grading change, which Harbor regrades;
the major for an instruction or world change, which it reruns; the patch for solutions, probes
and notes, which it reuses. `harbor run --diff jobs/gitea-eval …` then does the least work.

## Train

`reward` is the training signal; `harm` is a metric. Not yet run: the commands below follow
TRL's `examples/async_grpo_harbor` (AsyncGRPO through OpenEnv's `harbor_env`, which runs
Harbor's own trials), and the TRL commit and OpenEnv release they run with will be pinned here
after the first run. On a host with two GPUs and Docker, from that example's directory:

```bash
openenv harbor serve --dataset examples/gitea/datasets/train --port 8200 --capture-port 8300
CUDA_VISIBLE_DEVICES=0 VLLM_SERVER_DEV_MODE=1 vllm serve Qwen/Qwen3.5-2B …  # TRL's flags
CUDA_VISIBLE_DEVICES=1 python async_grpo_harbor.py --server http://localhost:8200 \
  --vllm-url http://localhost:8000 --model Qwen/Qwen3.5-2B --split <served dataset> \
  --harness mini-swe-agent --sandbox docker --reward-key reward --agent-step-limit 40 --max-steps 5
```

What matters for Miroir: the tasks are read from local directories, `--sandbox docker` runs
their compose services (the default, e2b, does not), the harness is a shell (no MCP), and
`--reward-key reward` picks the training signal out of `{reward, harm}`. A trial the grader
voided comes back with no reward and stays out of its group's baseline. To penalize harm,
subtract it in the trainer (`reward - λ·harm`); the tasks never do.

## Results

| Claim                                                               | Evidence          |
| ------------------------------------------------------------------- | ----------------- |
| The grader is right: 120 reference runs, 120 idle runs, 300 probes  | not yet run       |
| A world boots fast enough for a cloud sandbox (`booted in …`)       | not yet measured  |
| World image size                                                    | not yet measured  |
| A frontier agent on the test split: reward, harm rate, pass^4       | not yet run       |
| Training on the train split improves held-out work                  | open: not yet run |

## Limitations

- The worlds are generated, not taken from production data. Miroir's docs show how to build a
  world from a production snapshot.
- The tasks are short workflows of 1 to 20 API calls, not projects.
- Checks are SQL over what changed and what was sent. The quality of a comment is approximated:
  `close-stale` looks for the word "reopen".
- Git refs and files are not graded: Miroir captures database rows, emails and webhook calls.
