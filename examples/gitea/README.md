# Gitea: an env for engineering-operations agents

This example turns [Gitea](https://about.gitea.com) 28.3, the self-hosted Git forge, into a
reinforcement-learning environment. The product is the stock `gitea/gitea:28.3.0` image,
unchanged: the env adds Postgres, Miroir, a manifest, a seed and twelve tasks to it.

The agent acts as Dana Lee, engineering manager at Orbit, a payments company, and does what she
would hand an assistant: triage an issue, clean up the backlog, offboard an engineer, grant a
contractor access, wire up CI, merge what's ready, ship a release, contain a leaked secret,
close an incident. It works through Gitea's REST API. Each episode is graded on what changed in
Gitea's database, and on the emails and webhook calls Gitea sent.

| File | What |
|---|---|
| [`Dockerfile.miroir`](Dockerfile.miroir) | the stock image, plus Postgres, Python for the seed, and `miroir` |
| [`app.ini`](app.ini) | Gitea's config: the env's Postgres, the sinks, nothing that acts on its own |
| [`miroir.yaml`](miroir.yaml) | the processes, the bake, the state, the API, the persona, the sinks |
| [`seed.py`](seed.py) | Orbit's organization, created through Gitea's API, then dated |
| [`tasks/`](tasks) | the twelve tasks |
| [`train.py`](train.py) | GRPO training with TRL |

## Orbit

The seed builds Orbit the way its people would have, through Gitea's API: 13 staff and 3
outsiders, 8 teams, and 6 repositories (`api`, `web`, `mobile`, `infra`, `payments-gateway` and
`docs`) with their files, a CODEOWNERS file, labels and milestones. They hold 106 issues and 12
pull requests, 114 comments, 12 reviews, two releases, and the webhooks of a deploy pipeline.

Every record is dated: some issues have waited on their reporters for two months, and an
incident opened 40 minutes ago. Each reset moves the dates to the day of the episode, so an
issue that is 34 days stale is 34 days stale on any day.

## Tasks

| Task | Split | The agent must | What grades it |
|---|---|---|---|
| `count-urgent-bugs` | train | count the open p0 and p1 bugs nobody owns | an `integer` answer; an open pull request has the same labels |
| `list-stale-prs` | train | find the pull requests quiet for over two weeks | an `ids` answer, on dates that move with each reset |
| `triage-new-issue` | train | label an issue, set its milestone, assign the owner its CODEOWNERS names | four weighted checks; a gate on every other issue |
| `close-stale-needs-info` | train | close four of six issues waiting on their reporters, explaining each | credit per issue; gates on the recent two and on every other issue |
| `offboard-engineer` | train | hand a leaver's open work to a colleague, then take her off every team | the order matters (below); gates on other people's assignments and teams |
| `grant-contractor-access` | train | give a contractor write access to one repository, and no more | the mobile team would also open `api` to him: a gate |
| `add-ci-webhook` | train | add a webhook with chosen events and a secret, then test it | the webhook's row, and the delivery the HTTP sink received |
| `merge-approved-prs` | train | merge what is approved with no change request standing | two to merge among ten: a draft, superseded approvals, QA's request |
| `ship-release` | validation | publish v2.4.0 with its notes, move the open issues, close the milestone | five checks, including the deploy webhook's call; three gates |
| `ask-coworker` | validation | ask Marcus on the issue, then act on his answer | a model plays Marcus, who answers in Gitea |
| `redact-leaked-key` | test | redact a live key, lock the issue, ask security to rotate it | the email Gitea sends Inès; gates on deleting or repeating the key |
| `close-mitigated-incident` | test | close the incident within 30 minutes of the on-call's comment | a clock: the comment comes at minute 50, the check scores at minute 80 |

Each task has a golden run that earns 1, and probes for what a weak agent would do: 30 in all,
each held to a maximum reward (`max_reward`) or, for another right way to do it, a minimum
(`min_reward`). Rewards are partial where the work is: closing two of the four stale issues,
without the comments, earns 0.3. A change the task doesn't allow, or a failed gate, is harm,
and earns `harm_reward`.

## Run it

You need Docker with buildx, and [uv](https://docs.astral.sh/uv/) for the Python part. Run the
commands from the repository's root.

### Build and check

```sh
docker buildx bake gitea-env
docker run --rm --network none gitea-env check
```

The build runs `miroir bake`, which seeds Orbit and keeps the state. `check` replays every
golden run, an empty submit, repeats and every probe, and checks that the env idles, resets
cleanly and writes only to its state. It takes about a quarter of an hour:

```text
miroir check: gitea
static                       pass
users                        pass
idle                         pass
task                         golden null  repeat  reset  writes golden statuses
add-ci-webhook               pass   pass  pass    pass   pass   ok 201 200 204 ok
ask-coworker                 pass   pass  pass    pass   pass   201 201 200 201 201 ok
close-mitigated-incident     pass   pass  pass    pass   pass   200 ok 200 200 201 ok
close-stale-needs-info       pass   pass  pass    pass   pass   200 201 201 201 201 201 201 201 201 ok
count-urgent-bugs            pass   pass  pass    pass   pass   200 200 ok
grant-contractor-access      pass   pass  pass    pass   pass   204 ok
list-stale-prs               pass   pass  pass    pass   pass   200 ok
merge-approved-prs           pass   pass  pass    pass   pass   200 200 200 200 200 200 200 200 200 200 200 200 200 ok
offboard-engineer            pass   pass  pass    pass   pass   200 200 200 201 201 201 201 201 201 200 204 204 204 ok
redact-leaked-key            pass   pass  pass    pass   pass   200 200 204 201 ok
ship-release                 pass   pass  pass    pass   pass   200 200 200 200 201 201 201 201 200 ok
triage-new-issue             pass   pass  pass    pass   pass   200 200 200 201 ok
reset p50 4.4 s, p95 6.77 s; grading p50 1.7 s, max 5.29 s
PASSED
```

### Play an episode

Serve the env, install the client, and try the triage task by hand with the wrong owner:

```sh
docker run --rm -d -p 127.0.0.1:8000:8000 -e MIROIR_TOKEN=secret gitea-env
uv venv && uv pip install ./python
```

```python
from miroir import Session

session = Session.lease(["http://127.0.0.1:8000"], token="secret", wait_busy=False)
print(session.reset("triage-new-issue")["instruction"])

api = lambda method, path, body=None: session.step(
    "api_request", {"method": method, "path": path, "body": body})["observation"]["result"]
print(api("GET", "/repos/orbit/api/raw/CODEOWNERS")["body"])
api("PUT", "/repos/orbit/api/issues/58/labels", {"labels": ["kind/bug", "priority/p1"]})
api("PATCH", "/repos/orbit/api/issues/58", {"milestone": 2, "assignees": ["marcus"]})

outcome = session.finish()
print(outcome["reward"])  # 0.7: CODEOWNERS gives internal/exports/ to Noah
print([(check["name"], check["score"]) for check in outcome["metadata"]["checks"]])
print([effect["data"]["to"] for effect in outcome["record"]["effects"]])  # Gitea emailed Marcus
session.close()
```

### Evaluate a model

Start a pool of containers, one per rollout in flight, then run the
[verifiers](../../docs/src/train/verifiers.md) environment against any OpenAI-compatible model:

```sh
export MIROIR_TOKEN=secret
uv pip install './python[verifiers]'
miroir pool --size 8 --urls urls.txt -- python/tests/docker.sh gitea-env
```

```sh
vf-run miroir-verifiers -m <model> --client.base_url https://api.openai.com/v1 \
  --client.api_key_var OPENAI_API_KEY -r 8 \
  --env.taskset.urls_file urls.txt --env.taskset.token_var MIROIR_TOKEN \
  --env.taskset.split train --env.taskset.traces traces/
```

`-r 8` plays each task eight times. Add `--env.taskset.coworker_model` (and its base URL and key
variable) to run `ask-coworker`, which needs a model to play Marcus.

### Train

[`train.py`](train.py) trains with TRL's GRPO, eight rollouts of a task per step, each in a
container of its own. Harm earns -0.5 there, less than doing nothing, so the policy learns to
stop short of a damaging change. On a GPU machine:

```sh
uv pip install './python[trl]' 'trl[vllm]'
miroir pool --size 20 --urls urls.txt -- python/tests/docker.sh gitea-env
python examples/gitea/train.py --urls urls.txt
```

Each episode's record goes to `traces/`.

### Let the agent write code

With a [sandbox](../../docs/src/features/sandbox.md) beside each container (set
`SANDBOX_IMAGE=miroir-sandbox` for `docker.sh`), the `code` surface gives the agent `run_code` in
place of `api_request`: bash with `curl`, `jq` and Python, reaching Gitea as Dana at
`$API_GITEA`. Pass `surface="code"` to `environment_factory`, or `--env.taskset.surface code` to
`vf-run`.

### Keep the tasks honest

Training needs tasks a model sometimes solves and sometimes fails. With eight rollouts of each
task in `traces/`, class the tasks, then train on the learnable ones:

```sh
miroir calibrate traces/ --model my-model --out calibration.json
python examples/gitea/train.py --urls urls.txt --calibration calibration.json
```

Grades can be wrong in both directions. `miroir review` groups the rollouts by end state, shows
each one with its grade, and turns every grade you correct into a probe, which `check` then
replays from then on:

```sh
miroir review traces/ --tasks examples/gitea/tasks
docker run --rm --network none -v "$PWD/examples/gitea/tasks:/miroir/tasks:ro" gitea-env check
```


## How the env is made

- **The image.** `Dockerfile.miroir` starts from the stock image. That image declares `/data` a
  volume, which keeps nothing a build writes, so Gitea's state goes to `/var/lib/gitea`. Gitea's
  secrets are generated at build, outside `app.ini`, and `app.ini` belongs to root: Gitea would
  otherwise write a generated JWT secret into it at its first start.
- **Gitea's config.** `app.ini` points Gitea at the env's Postgres, its mail at the SMTP sink,
  and its webhooks at the HTTP sink, the only host they may reach. It turns off every cron task
  (several run at start by default), keeps queues and sessions in memory, and searches issues in
  the database rather than in an on-disk index.
- **The manifest.** Two processes, Postgres and Gitea, each as its own user. Bake creates the
  database, runs Gitea's migrations and creates an admin; then, with Gitea up, `seed.py` builds
  Orbit. Gitea refuses to run as root, so the commands that call it go through `su-exec git`. Gitea
  stores times as Unix seconds in `*_unix` columns, which `time.epoch` moves at each reset. The
  changes that follow from others (the activity feed, notifications, webhook deliveries, a token's
  last use) are ignored. `quiet` runs `gitea manager flush-queues`, so grading waits for the mail
  and webhook queues to drain.
- **The seed.** `seed.py` acts as each person through Gitea's API, with the admin's `Sudo`
  header, so Gitea creates its own rows: timeline events, counters, review requests. It stores
  the personas' tokens as Gitea does, salted PBKDF2, and writes the part of Gitea's spec
  `api_docs` serves: the full spec lists 537 operations, more than a listing can show, so it
  keeps the 167 an engineering manager uses. Then it dates every record by its age.

The tasks name issues by number in their instructions, and by database id in their checks: the
seed makes the same ids on every build. Each task file says which id is which.
