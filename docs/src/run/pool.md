# Pools

A trainer runs many rollouts at once, and each needs a container of its own. `miroir pool`
keeps a number of containers healthy and lists their URLs in a file, which the adapters read
to lease a free container for each rollout.

```sh
export MIROIR_TOKEN=secret
miroir pool --size 8 --urls urls.txt -- python/tests/docker.sh my-env
```

This is the Python CLI, on the host. Size the pool to the rollouts in flight, plus a few
spare. It prints `ready: 8 of 8; start the trainer` once every container is healthy.

## The command

The command after `--` starts or stops one container:

- `COMMAND start` prints `ID URL` as its last line, such as
  `3f2a… http://127.0.0.1:49153`.
- `COMMAND stop ID` removes that container.

It runs with `MIROIR_POOL` set to the pool's name, so it can label what it starts.
[`python/tests/docker.sh`](https://github.com/raphaelDkhn/miroir/blob/master/python/tests/docker.sh)
is a full example: it starts the env alone, or beside a sandbox when `SANDBOX_IMAGE` is set,
and passes `MIROIR_TOKEN` through.

## What the pool does

- Checks each container's `/health` every 10 seconds.
- Replaces a container that is not healthy 15 minutes after its start, that stops answering
  for a minute, or that stays unhealthy for 10 minutes.
- Rewrites the URL file as containers come and go, one URL per line, with the container's id
  after a `#`.
- On SIGINT or SIGTERM, stops every container and removes the file.
- On start, stops the containers an earlier pool on the same file left behind.

It gives up if no container ever becomes healthy, or if the first three starts all fail.

## On Prime Intellect

[`examples/gitea/prime_sandbox.py`](https://github.com/raphaelDkhn/miroir/blob/master/examples/gitea/prime_sandbox.py)
runs each container as a [Prime sandbox](https://docs.primeintellect.ai/sandboxes/overview),
from an image that `prime images push` built:

```sh
export MIROIR_TOKEN=$(openssl rand -hex 24)
miroir pool --size 32 --urls urls.txt -- \
  python examples/gitea/prime_sandbox.py prime/<you>/my-env:<tag>
```

Each sandbox has no egress and publishes port 8000 at
`https://8000-<sandbox-id>.sandbox-v2-prod.pinfra.io`. Prime does not authenticate that URL,
so `MIROIR_TOKEN` is all that guards the env, and the script refuses to start without it.

A sandbox is a VM, not a container, which changes two things:

- Each reset kills every process but PID 1 and miroir, so `miroir run` needs a PID namespace
  of its own: the script starts it with `unshare --pid --fork --mount-proc tini -- miroir run`.
- `/run` is mounted empty at boot, so a datastore can't use a directory the image made there.
  Give it a socket elsewhere, or none, as the Gitea example does for Postgres.

## Using the file

```python
from miroir.trl import environment_factory

factory = environment_factory(urls_file="urls.txt", token="secret")
```

```sh
vf-run miroir-verifiers -m <model> --env.taskset.urls_file urls.txt --env.taskset.token_var MIROIR_TOKEN
```

The adapters reread the file at each lease, so replaced containers drop out on their own.
