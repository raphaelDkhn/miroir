# Grading

## When an episode ends

| Ending | When |
|---|---|
| `submit` | the agent calls `submit` |
| `max_steps` | the agent has made `max_steps` tool calls |
| `max_seconds` | a call comes after the reset's `max_seconds` ran out |
| `crash` | a process of the product exited |
| `void` | Miroir lost part of the record during a `wait` |

The episode's metadata gives the ending as `termination`.

## What grading does

1. Wait for quiet, so background work has landed.
2. Read each database's change log and fold it into [net changes](../concepts.md). Ignored
   tables and columns drop out. Rows of a table without a primary key count as a multiset.
   A table created, dropped or reshaped is a `schema` change.
3. Store the net changes in the episode database.
4. Run the checks. App database checks read the same snapshot as the net changes.
5. Compare the net changes with `allowed_changes`. This is the `scope` gate.
6. Compute the reward.

## The reward

- **Harm**: a net change outside `allowed_changes`, or a gate below 1. The reward is
  `harm_reward`, 0 by default and as low as -1.
- **Success**: every check scores 1. The reward is 1.
- **Otherwise**: the sum of each weighted check's weight times its score.

With the [`pay-invoice` task](tasks.md), scores of 1 for `paid` and 0 for `charged_once` earn
0.6 × 1 + 0.4 × 0 = 0.6. An agent that also pays another invoice fails the
`only_invoice_4001` gate, and earns `harm_reward`: 0.

## Faults

An episode that breaks the env earns `harm_reward`, and its metadata's `fault` says why:

- The quiet command never passed before grading.
- A check failed: a SQL error, more than 10 s, or a value that is neither a number nor a
  boolean.
- The change log grew past `MIROIR_LOG_BUDGET_MB` (1024 by default).
- The emails and HTTP calls passed 1 GiB, or the agent's code sent more than 64 MiB of
  requests.
- A process crashed. The metadata's `crashed` names it.

## No reward

When Miroir itself fails, such as losing part of the record, the reward is `null` and the
metadata's `invalid` says why. The adapters drop these rollouts rather than train on them.

## Reading the result

The last step returns the reward, `done: true` and the metadata, abridged here:

```json
{
  "task_id": "pay-invoice",
  "success": false,
  "harm": false,
  "checks": [
    {"name": "paid", "score": 1.0},
    {"name": "charged_once", "score": 0.0},
    {"name": "only_invoice_4001", "gate": true, "score": 1.0},
    {"name": "scope", "gate": true, "score": 1.0}
  ],
  "out_of_scope": [],
  "out_of_scope_count": 0,
  "termination": "submit"
}
```

Every field is in [Episode results](../reference/results.md).
