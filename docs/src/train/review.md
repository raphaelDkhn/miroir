# Review grades

Checks can be wrong: too strict, or too easy to fool. `miroir review` shows you how rollouts
actually ended, asks whether each grade is right, and turns each wrong grade into a probe, so
`miroir check` keeps it fixed.

```sh
miroir review traces/ --tasks fixtures/acme-crm/tasks
```

`traces/` holds the records the adapters save with `traces=` (TRL) or
`--env.taskset.traces` (verifiers): files, or directories of them.

## How it goes

Rollouts that ended in the same state share a [signature](../concepts.md), so review groups
them: one end state per task and signature, however many rollouts reached it. For each, it
shows the rewards, the net changes and effects, the check scores and the calls of one example,
then asks:

```text
label: [c]orrect, should [f]ail, should [p]ass, [s]kip, [q]uit?
```

| Label | Means | Writes |
|---|---|---|
| `correct` | the grade is right | the label |
| `should_fail` | it earned too much | the label, and a probe with `max_reward` set to the task's `null_max` |
| `should_pass` | it earned too little | the label, and a probe with `min_reward: 1` |

The probe replays the example rollout's own calls. It goes in `tasks/probes/<task-id>.yaml`,
and the labels in `tasks/probes/labels.jsonl`, so labeled end states are not asked again. At
the end, review prints how many grades you agreed with.

Fix the checks, then run [`miroir check`](../make/check.md): the new probes fail until the
grade is right.

## Options

| Option | Does |
|---|---|
| `--label TASK@SIGNATURE=LABEL` | label an end state without asking; repeat for more |
| `--json` | print the queue of unlabeled end states as JSON, and ask nothing |

Rollouts that broke the env, or that have no grade, have no end state to label. Review counts
them per task instead.
