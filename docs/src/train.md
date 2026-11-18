# Train

Miroir adds no training code. Every trainer below runs Harbor task directories itself; what
Miroir gives it is the reward contract, voids it can recognize, and calibration with
`miroir check`.

## The reward contract

The grader writes `/logs/verifier/reward.json` with two keys, always both, finite numbers only:

```json
{"harm": 1, "reward": 0.0}
```

- `reward`, in [0, 1], is the training signal: the weighted mean of the task's check scores, or
  0 when there is harm.
- `harm`, 0 or 1, is a metric: a change no `allow` entry admits, a failed gate, or a fault. Its
  mean over a job is the harm rate.
- A void writes no reward file and exits 75. Harbor records an invalid trial, and a trainer must
  leave it out of the batch, not count it as 0. A trial voids only for Miroir's own failures,
  such as a broken spec or missing evidence ([Outcomes](validate.md#outcomes)).
- `reward-details.json`, next to it, holds every check's score and the out-of-scope changes.

Harm zeroes `reward` but never makes it negative. A trainer that wants a penalty below 0 composes
`reward − λ·harm` where the consumer allows it (verifiers and OpenEnv do).

## Calibrate

Before training, run the agent and model to be trained on the train split with at least two
attempts per task, then keep the tasks whose rewards differ between attempts:

```bash
miroir check --json jobs/<job> | jq -r '.agents[].tasks[] | select(.class == "learnable") | .task'
```

A task is `solved` when every attempt scores 1, `unsolved` when every attempt scores the same
value below 1, and `learnable` when the rewards differ. A task that scores 0.6 every time is
`unsolved`: its rewards have no variance, so a GRPO group gives it no advantage. Harbor runs a
subset with `-i <task name>` (repeatable) or a job config's `datasets[].task_names`
([job configs](https://docs.harborframework.com/jobs/configs)).

## The committed recipe

The recipe is TRL's own example, `examples/async_grpo_harbor/async_grpo_harbor.py`: AsyncGRPO
through OpenEnv's `harbor_env`, which runs Harbor's own trials, on a host with two GPUs and
Docker, over the Gitea example's train split. It is not yet run: its exact commands, pinned to
the TRL commit and OpenEnv release they ran with, go in the Gitea example's
[Train section](https://github.com/raphaelDkhn/miroir/blob/master/examples/gitea/README.md#train)
after the first run. What matters to Miroir in it:

- the tasks are read from local task directories, with no publishing;
- `--sandbox docker` runs their compose services (the default, e2b, does not);
- the harness is a shell, with no MCP;
- `--reward-key reward` picks the training signal out of `{reward, harm}`;
- a rollout the verifier did not score comes back as `None`, which keeps voids out of the
  group's baseline. The example adds its own shaping on top: up to 0.3 for few tool calls on a
  fully solved task.

## Consumers

| Consumer | Status | Notes |
|---|---|---|
| Harbor (`harbor run`, local Docker) | verified | The fixture's gate, `tests/e2e.sh`: grader validation, regrade, a void. |
| OpenEnv `harbor_env` → TRL `AsyncGRPOTrainer` | expected, not yet run (the committed recipe) | A void comes back as `reward=None`, which TRL leaves out of the group's statistics. |
| SkyRL `HarborGenerator` / `harbor_skycap` | expected, not yet run | Trains on `rewards["reward"]`. A void has no verifier result: it is retried, then masks its prompt group. |
| Tinker or Miles through `harbor serve` and Harbor's inference relay | expected, not yet run | Works at the trial level. |
| verifiers `HarborTaskset` → prime-rl, local Docker runtime | expected, not yet run | Loads Harbor Hub packages only, so publish the split first (private works with Hub credentials). Set `agent.deployment.trust_compose = true`, use the `bash` harness, and use a Docker Compose with `config --no-env-resolution` (2.35.0 or later). `reward_key` defaults to `reward`, and `harm` becomes a metric. Collect hooks are strict, and `miroir collect` exits 0 whenever it writes evidence. The artifact budget is 32 MiB per trial, shared with `main`'s `/logs/artifacts`; evidence is capped at 16 MiB. |
| verifiers Prime native compose (one VM per service) | unsupported | The VM's own listeners fail the boot's listener audit. |
| Prime Hosted Training | unsupported | Its environment servers have no Docker, so compose tasks cannot run. |
| TRL `trl.experimental.harbor` (white box) | unsupported | It verifies in the agent's container and never runs collect hooks. |
| NeMo Gym `harbor` server | unsupported | It rejects `internal: true` networks outside OpenSandbox. |
| Tinker cookbook `harbor_rl` | unsupported | It runs Dockerfile-only tasks. |

"Unsupported" comes from reading each consumer's source; none of those was run. Whether training
on the Gitea train split improves held-out work is the Gitea example's open claim.
