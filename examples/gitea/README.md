# Gitea: Miroir on an unmodified product

This example is the evidence that Miroir turns an existing application into an environment for
agentic RL that works. The application is [Gitea](https://about.gitea.com) 28.3, the
self-hosted Git forge, as its maintainers ship it: the stock image `gitea/gitea:28.3.0`, pinned
by digest, with no patch. Each claim below gives the measurement behind it and the command that
repeats it.

The tasks are the everyday work of a forge: triage, cleanup, access, offboarding, webhooks,
merges, releases, incidents, a leaked secret. They don't stand for an agent product built on
Gitea. They are here because Gitea's own behavior decides each one, its permissions and
cascades, its notifications and webhooks, so a policy earns the reward only by working the real
product.

## The claims

| | Claim | Measured by | Result |
|---|---|---|---|
| 1 | The product runs unmodified | what the env adds to the image | config, a manifest and a seed; no line of Gitea changes |
| 2 | Every task is solvable and graded right | `miroir check` on all 144 tasks | 144 of 144 pass: golden runs earn 1, empty submits 0, repeats the same; 360 of 360 probes stay in bounds |
| 3 | Episodes don't leak into each other | `check`'s `reset` and `writes` stages | 576 resets, and in each world every one hashes the same; no file written outside the state |
| 4 | Collateral damage is caught | probes that do the task and damage something else | 144 probes do harm, and every one earns `harm_reward` |
| 5 | It is fast enough to train on | `check`'s timings, a soak of a pool of containers, and what a model reads | reset 3.7 s and grading 1.6 s at the median; eight containers on one laptop ran 65 episodes a minute, none void; rollouts past a 32k context fell from 31% to 12% |
| 6 | Training on it improves held-out work | RL on the train split, scored on held-out worlds and families | not run yet: [the experiment](#6-training-improves-held-out-work) is ready, with its decision rule |

## What the env holds

### Three worlds

| World | Company | Split | People | Issues and pull requests | Labels such as |
|---|---|---|---|---|---|
| `orbit` | Orbit Payments, payments | train, validation | 20 | 283 | `kind/bug`, `priority/p0` |
| `ferry` | Ferrylane Logistics, freight and delivery | train, validation | 20 | 264 | `type/bug`, `priority/critical` |
| `kestrel` | Kestrel Health, clinic scheduling | test | 20 | 274 | `kind/defect`, `severity/s1` |

Each world is one Gitea organization, which the seed builds through Gitea's API at bake:
staff and outsiders, eight or nine teams with their permissions, six repositories with their
files and CODEOWNERS, labels and milestones, and the issues and pull requests, with their
comments, reviews, assignees, releases and webhooks. Every record is dated by its age, and each
reset moves the dates to the day of the episode, so an issue quiet for 40 days is quiet for 40
days on any day. The test world shares no person, repository or label with the training worlds,
so a test score can't come from names, numbers or answers seen in training.

### Twelve task families

Each family plants its situations in every world and asks about them four times, with other
repositories, people, thresholds and answers.

| Family | The agent must | What decides it in Gitea | Trained on |
|---|---|---|---|
| `count-issues` | count the open issues that match labels and an assignment | the issues list holds pull requests too | yes |
| `stale-prs` | list the pull requests quiet for more than N days | activity is the last update, reviews included, not the opening | yes |
| `triage` | label a new issue, set its milestone, assign the owner CODEOWNERS names | patterns are regular expressions; the catch-all owner is the wrong one | yes |
| `close-stale` | close, each with a comment, the issues waiting on their reporter for N days | the recent ones and the other repositories stay open | yes |
| `offboard` | hand a leaver's open work to a colleague, then take them off every team | leaving the last team unassigns them everywhere, so the order matters | yes |
| `grant-access` | give a contractor one repository and nothing more | each team the contractor could join opens more | yes |
| `add-webhook` | add a CI webhook with exactly the events asked, and test it | Gitea stores `pull_request` as eight events; the delivery reaches the HTTP sink | yes |
| `merge-ready` | merge what has an approval and no change request standing | each reviewer's latest verdict stands; a draft can't merge | yes |
| `incident` | close an incident within N minutes of the on-call engineer's comment | a clock: the comment arrives at minute M of the episode | yes |
| `ship-release` | publish a release with its notes, move the open issues, close the milestone | publishing fires the deploy pipeline's webhook | no |
| `redact-secret` | redact a live key in a comment, lock the issue, ask security to rotate it | Gitea keeps the edit history, and emails whom a comment mentions | no |
| `ask-coworker` | ask the colleague who fixed a bug whether the fix shipped, then act on the answer | a model plays the colleague, who answers in Gitea | no |

| Split | Tasks | What it holds |
|---|---|---|
| train | 54 | three tasks of each trained family, in `orbit` and `ferry` |
| validation | 42 | the fourth task of each trained family there, and every task of the other three |
| test | 48 | every task of `kestrel` |

Every task has weighted checks for partial credit, gates for what must not happen, the changes
it may make, a golden run that earns 1, and probes: 360 runs of what a weak agent would do,
each held to a reward bound.

## 1. The product runs unmodified

[`Dockerfile.miroir`](Dockerfile.miroir) starts from the stock image by digest, and adds
Postgres, Python for the seed, and `miroir`. What the env adds:

| File | Lines | What |
|---|---|---|
| [`Dockerfile.miroir`](Dockerfile.miroir) | 30 | the stock image, Postgres, Python, `miroir` |
| [`app.ini`](app.ini) | 112 | Gitea's config: the env's Postgres, mail and webhooks to the sinks, no cron |
| [`miroir.yaml`](miroir.yaml) | 82 | the processes, the bake, the worlds, the state, the API, the personas, the sinks |
| [`seed/`](seed) | 4,100 | the worlds, the twelve families, the seed that builds them in Gitea, and their tests |

What Miroir does that the env doesn't write: it captures row changes on Gitea's 117 tables with
the triggers bake installs; moves the dates in its 118 Unix-time columns at every reset, from
one manifest line (`epoch: {"*.*_unix": s}`); catches the mail Gitea sends and the webhook
deliveries it makes in the sinks; restores the state before each episode; serves Gitea's API
as the agent's tools, with 167 of its operations described in `api_docs`; and grades.

## 2. Every task is solvable and graded right

```sh
docker buildx bake gitea-env
for w in orbit ferry kestrel; do
  docker run --rm --network none gitea-env check --world $w --json > check-$w.json
done
python examples/gitea/evidence.py check-orbit.json check-ferry.json check-kestrel.json
```

`check` replays every task's golden run, an immediate empty submit, two repeats, every probe,
the golden answer hidden among guesses (`hedge`), and the golden run with timestamps a day
behind dates (`drift`). For the 144 tasks that is 1,212 episodes, which took 40 minutes per
world, with the three worlds checked in parallel on one machine:

```text
tasks: 144 of 144 pass
  golden: 144 of 144
  null: 144 of 144
  repeat: 144 of 144
  reset: 144 of 144
  writes: 144 of 144
  probes: 144 of 144
  hedge: 132 of 132
  drift: 144 of 144
probes: 360 of 360 within bounds; 228 wrong ways held to 0, 108 partial ones held below full credit, 24 other right ways held to 1; 144 of them harm
episodes replayed: 1212
orbit: 192 fingerprinted resets; reset p50 3.73 s, p95 4.65 s; grading p50 1.58 s, max 5.54 s; idle pass
ferry: 192 fingerprinted resets; reset p50 3.63 s, p95 4.5 s; grading p50 1.58 s, max 5.52 s; idle pass
kestrel: 192 fingerprinted resets; reset p50 3.68 s, p95 4.85 s; grading p50 1.58 s, max 5.35 s; idle pass
```

`check` earns its place while an env is built. Here its `repeat` stage caught a race in Gitea
itself: once a pull request merges, Gitea re-checks every other open pull request of the
repository in the background, and refuses a merge that comes before the check ends ("Please
try again later"). A golden run that merged two pull requests of one repository in a row earned
0.75, 0.75 and 1 on three identical runs. Rewards that depend on timing are noise to a policy,
so `merge-ready` keeps at most one pull request ready per repository. Its `probes` stage caught
a weak probe of the generator's: one meant to triage a neighboring issue too picked an issue
already assigned to that owner, changed nothing, and earned 1. It now picks an issue the owner
isn't on, and `seed/test_seed.py` holds it there.

## 3. Episodes don't leak into each other

A reset kills every process, restores the state directories, moves the dates and starts Gitea
again. `check` fingerprints the data after each reset of a golden run, an empty submit and the
two repeats, time columns aside, and requires every one of a world to hash the same, whichever
task it was for: the destructive probes that run in between (deleting a comment, merging past
a change request, releasing another repository) must leave nothing behind. In the run above
all 576 pass, 192 in each world over its 48 tasks. The `writes` stage requires that no file
outside the state changes across a task's runs, and the `idle` stage that 30 seconds with no
agent leave no change, no email and no webhook call.

## 4. Collateral damage is caught

A change the task doesn't allow, or a failed gate, makes the episode harm, which earns
`harm_reward` whatever the checks scored. The probes that do the task and damage something
else, such as handing a leaver's closed work over too, triaging a neighbor, or merging past a
change request: 144 of the 360 probes. Each earns `harm_reward`, 0 by default;
[`rl.toml`](rl.toml) sets it to -0.5, below doing nothing, so a policy learns to stop short of
the damage.

## 5. Fast enough to train on

In the check above, a reset took 3.7 s at the median and 4.9 s at p95, and grading 1.6 s at
the median and 5.5 s at most. A soak with [`tools/scale_run.py`](../../tools/scale_run.py) kept
eight containers busy with golden runs on one laptop (an Apple M3 Pro, Docker with 12 CPUs and
8 GB): 787 episodes in 12 minutes, 65 a minute at the median after two minutes of warm-up and
never fewer than 59, none void, all earning 1. A lease waited 0.02 s at p95, and resets slowed
to 5.1 s at the median under that load.

```sh
id=$(docker create gitea-env) && docker cp "$id":/miroir/tasks gitea-tasks && docker rm "$id"
export MIROIR_TOKEN=secret
miroir pool --size 8 --urls urls.txt -- python/tests/docker.sh gitea-env &
uv run tools/scale_run.py --urls urls.txt --tasks gitea-tasks --concurrency 8 --minutes 12 \
  --token-var MIROIR_TOKEN
```

That leaves the time to the model. In the first training run on an earlier version of this env,
a step of 40 rollouts took 469 s at the median, with about 17 model turns per rollout; the env's
part of an episode, a reset and a grading, is about 5 s of that.

### What the model reads

What the model reads costs more than what the env does. Every API answer stays in the context
for the rest of the episode and goes into the sample training learns from, and Gitea's
answers are long: each pull request holds two full copies of its repository, each user their
account settings and avatar link. A list of 50 pull requests was 73 KB, 23,600 tokens, past
the 64 KiB at which Miroir cuts an answer. The manifest's `api.omit` drops what the agent
can't use before it sees it: links to avatars, clones and downloads, the repository copies,
migration fields, label colors, and a user's account details beyond who they are. Each key
names nothing else in Gitea's spec, or is limited to users by the key it sits under. A list
that still doesn't fit keeps its first items as JSON, and says how many it left out.

| | Gitea's answers | With `api.omit` |
|---|---|---|
| observation tokens in the golden runs of orbit's 44 tasks | 463,084 | 158,878 (-66%) |
| answers cut at 64 KiB in those runs | 9 | 0 |
| Qwen3.5-9B on the 54 train tasks: mean reward | 0.45 | 0.57 |
| rollouts past a 32,768-token context, which training cuts | 31% | 12% |
| tokens at the end of a rollout, at the median | 24,800 | 14,400 |

Paired by task, the reward rose by 0.12, with a standard error of 0.05. It rose where whole
lists now fit: `merge-ready` from 0 to 0.64, `stale-prs` from 0.48 to 1. It fell on
`add-webhook`, from 0.60 to 0.33 over nine rollouts, and on `triage`, from 0.80 to 0.67;
`offboard` stays at 0, as the model lists every issue of every repository instead of filtering
by assignee and passes the context each time. Two cuts were tried and put back because they
cost reward: an issue's web link, whose `/pulls/` path is how the model told a pull request
from an issue when counting issues, and a shorter `api_docs` listing, after which it made more
failing calls on `add-webhook`.

The rollouts ran on Prime Inference with thinking off and sampling at temperature 1, as
[`rl.toml`](rl.toml) trains, with up to 4,096 tokens a turn: 205 on Gitea's answers and 122
with `api.omit`, one to three for each task.

## 6. Training improves held-out work

The experiment that tests the last claim: train on the train split only, and score the policy
before its first step and every 20 steps after on four held-out sources.

| Source | Tasks | Held out by |
|---|---|---|
| `validation-trained-families` | 18 | other tasks of the families it trains on, in the worlds it trains on |
| `validation-held-out-families` | 16 | families it never trains on, in the worlds it trains on |
| `test-trained-families` | 36 | a company it never sees, with the families it trains on |
| `test-held-out-families` | 8 | a company it never sees, with families it never trains on |

The `ask-coworker` tasks need a model to play the colleague, which Prime's hosted adapter has
none of, so these sources leave them out. [`rl.toml`](rl.toml) runs it on Prime Intellect's
Hosted Training: Qwen3.5-9B, GRPO with eight rollouts of five tasks a step for 60 steps, harm
earning -0.5, on three RTX PRO 6000 GPUs, for about ten hours. The policy is scored by the same
orchestrator, sampling and containers before and after training, so the first evaluation is the
baseline.

**It has not been run yet.** The rule that decides the claim is fixed before it is: pair each
task's success rate at step 60 with its rate at step 0; the claim holds if the mean gain on
`test-trained-families` is more than twice its standard error over the 36 tasks. That needs
each task's results at both evaluations, not only each source's mean, so keep the evaluation's
rollouts (`prime train rollouts <run-id> --step <n>`) or traces. The other three sources are
read the same way, and say how far a gain reaches: to other instances, to other families, to
both at once.

This design answers what an earlier run of this example could not: trained on five fixed tasks
with nothing held out, its training reward rose from 0.55 to 0.69 in 16 steps, which memorizing
five answers would also give.

## Run it

You need Docker with buildx, and [uv](https://docs.astral.sh/uv/) for the Python part. Run the
commands from the repository's root.

### Build and check

```sh
docker buildx bake gitea-env
docker run --rm --network none gitea-env check --world orbit
```

The build seeds the three worlds and writes their tasks, in about two minutes. A world's check
takes about 40 minutes; check the worlds in parallel, one container each.

### Play an episode

Serve the env, install the client, and try a triage task by hand with the wrong owner:

```sh
docker run --rm -d -p 127.0.0.1:8000:8000 -e MIROIR_TOKEN=secret gitea-env
uv venv && uv pip install ./python
```

```python
from miroir import Session

session = Session.lease(["http://127.0.0.1:8000"], token="secret", wait_busy=False)
print(session.reset("triage.orbit.1")["instruction"])

api = lambda method, path, body=None: session.step(
    "api_request", {"method": method, "path": path, "body": body}
)["observation"]["result"]
print(api("GET", "/repos/orbit/payments-gateway/raw/CODEOWNERS")["body"])
api(
    "PUT", "/repos/orbit/payments-gateway/issues/66/labels", {"labels": ["kind/bug", "priority/p1"]}
)
api("PATCH", "/repos/orbit/payments-gateway/issues/66", {"milestone": 7, "assignees": ["ravi"]})

outcome = session.finish()
print(outcome["reward"])  # 0.7: CODEOWNERS gives src/captures/ to Marcus, not to Ravi
print([(check["name"], check["score"]) for check in outcome["metadata"]["checks"]])
session.close()
```

Each task file starts with a note that says which row is which; read one with
`docker run --rm --entrypoint cat gitea-env /miroir/tasks/triage.orbit.1.yaml`.

### Evaluate a model

Start a pool of containers, one per rollout in flight, then run the
[verifiers](../../docs/src/train/verifiers.md) environment against any OpenAI-compatible model:

```sh
export MIROIR_TOKEN=secret
uv pip install './python[verifiers]'
miroir pool --size 8 --urls urls.txt -- python/tests/docker.sh gitea-env
```

```sh
vf-run miroir-verifiers -m <model> --client.base_url <url> --client.api_key_var <VAR> -r 4 \
  --env.taskset.urls_file urls.txt --env.taskset.token_var MIROIR_TOKEN \
  --env.taskset.split test --env.taskset.traces traces/
```

`-r 4` plays each task four times. Add `--env.taskset.coworker_model` (with its base URL and
key variable) to run the `ask-coworker` tasks.

### Train on Prime Intellect

[`rl.toml`](rl.toml) is the experiment of [section 6](#6-training-improves-held-out-work). Prime
runs the trainer, the inference server and the env's verifiers adapter, and each rollout leases
a container in a pool of [Prime sandboxes](../../docs/src/run/pool.md#on-prime-intellect). With
the `prime` CLI logged in and the `verifiers` extra installed:

```sh
# The env's image, built on Prime for its VMs
prime images push gitea-env:0.2 --context examples/gitea --dockerfile examples/gitea/Dockerfile.miroir
# The adapter, on the Environments Hub as <you>/miroir-verifiers
python python/push_hub.py
# A sandbox per rollout in flight, each at a public URL that only the token guards
export MIROIR_TOKEN=$(openssl rand -hex 24)
miroir pool --size 40 --urls urls.txt -- \
  python examples/gitea/prime_sandbox.py prime/<you>/gitea-env:0.2
```

Once the pool is ready, set `env.taskset.id` in `rl.toml` to your Hub id. Prime passes no
variables to a full-parameter run's env, so [`with_pool.py`](with_pool.py) writes a copy of the
config with the pool's URLs and token in every source:

```sh
python examples/gitea/with_pool.py urls.txt > rl.local.toml
prime train rl.local.toml
prime train metrics <run-id>   # the held-out sources' scores, step by step
```

The run knows the URLs it started with, so keep the pool running until it ends, and stop it
after. The run's config and logs hold the pool's token, which is why each pool gets its own.

`prime images push` builds the Dockerfile on Prime, which takes `miroir` from the
`MIROIR_IMAGE` image. The seed writes the tasks, so that image must hold a `miroir` whose bake
validates the tasks after the seed, as this repository's does; `docker buildx bake gitea-env`
builds it from source.

### Train with TRL

[`train.py`](train.py) trains with TRL's GRPO on a GPU machine, eight rollouts of a task per
step, each in a container of its own, and evaluates on the validation split:

```sh
uv pip install './python[trl]' 'trl[vllm]'
miroir pool --size 20 --urls urls.txt -- python/tests/docker.sh gitea-env
python examples/gitea/train.py --urls urls.txt
```

### Keep the grades honest

`miroir review` groups rollouts by end state and turns each grade you correct into a probe,
which `check` replays from then on. The tasks themselves are in the image, so keep the probes in
`examples/gitea/probes/` and mount only them:

```sh
miroir review traces/ --tasks examples/gitea
docker run --rm --network none -v "$PWD/examples/gitea/probes:/miroir/tasks/probes:ro" \
  gitea-env check --world orbit
```

`miroir calibrate traces/ --model M --out calibration.json` classes the tasks as solved,
learnable or out of reach for a model; `train.py --calibration calibration.json` and
`--env.taskset.calibration` train on the learnable ones.

## How the env is made

- **The image.** The stock image declares `/data` a volume, which keeps nothing a build writes,
  so Gitea's state goes to `/var/lib/gitea`. Gitea's secrets are generated at build, outside
  `app.ini`, which belongs to root: Gitea would otherwise write a generated JWT secret into it.
- **Gitea's config.** [`app.ini`](app.ini) points Gitea at the env's Postgres, its mail at the
  SMTP sink and its webhooks at the HTTP sink, the only host they may reach. It turns off every
  cron task, keeps queues and sessions in memory, and searches issues in the database.
- **The manifest.** Postgres and Gitea, each as its own user; three worlds; the state; Gitea's
  API with the part of its spec `api_docs` serves, and the keys its answers leave out
  (`api.omit`); one persona per world, the company's
  engineering manager; the sinks. The changes that follow from others (the activity feed,
  notifications, webhook deliveries, a token's last use) are ignored, and `quiet` waits for
  Gitea's queues to drain before grading.
- **The worlds.** [`seed/worlds.py`](seed/worlds.py) writes each company down (its people,
  teams, repositories, CODEOWNERS and label names) and samples the rest from the world's seed:
  the background issues, their labels, assignees, comments and ages.
- **The families.** [`seed/families.py`](seed/families.py) gives each family two parts. Before
  the seed builds a world, `plant` adds what the family's tasks are about: the stale issues, the
  leaver's work, the pull requests in every review state. Once Gitea has given the rows their
  ids, `write` makes each task: the instruction, the checks on those ids, the changes allowed,
  the golden run, the probes. Answers come from the whole world, every family's plants
  included.
- **The seed.** [`seed/seed.py`](seed/seed.py) builds the world through Gitea's API, acting as
  each person with the admin's `Sudo` header, so Gitea writes its own rows: timeline events,
  counters, review requests. It dates every record, reads back the ids, and writes the tasks to
  `/miroir/tasks`, which bake then validates. The same world builds the same on every bake.
- **The checks of the generator.** [`seed/test_seed.py`](seed/test_seed.py) builds the three
  worlds without Gitea, in a second: every family asks four ways, the test world shares nothing
  with the others, answers are unambiguous, everyone assigned can be.

## Limits

- One product. The example shows that the approach holds on Gitea; another product's env needs
  its own manifest and seed.
- The worlds are written and sampled, not imported: their structure follows how a forge is
  used, their titles are templates, and each holds under 300 issues and pull requests, fewer
  than a real organization's. [`miroir import`](../../docs/src/production/import.md) is the way
  to a world made from a production dump.
- The tasks take one to about twenty API calls. Long work, over hundreds of calls, isn't here.
- The agent works through Gitea's REST API; Gitea's MCP server and web UI aren't wired in.
- Claim 6 is a design until the experiment runs.
