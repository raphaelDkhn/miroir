# Calibrate tasks

A task teaches a model something only if its rollouts sometimes succeed and sometimes fail.
`miroir calibrate` sorts tasks by how a model does on them, so training can skip the tasks it
always solves and the tasks it never does.

Run several rollouts of each task, such as eight, with traces saved. Then:

```sh
miroir calibrate traces/ --model my-model-v3 --out calibration.json
```

```text
task                      n  void   pass@1   pass@8   pass^8   reward      var     harm  class
count-overdue-invoices    8     0     1.00     1.00     1.00     1.00     0.00     0.00  solved
notify-new-owner          8     0     0.38     1.00     0.00     0.69     0.06     0.00  learnable
pay-invoice               8     0     0.00     0.00     0.00     0.00     0.00     0.25  out_of_reach
```

## Classes

| Class | When |
|---|---|
| `learnable` | the rewards vary |
| `solved` | every rollout succeeds |
| `out_of_reach` | every rollout earns the same, short of success |

A task with fewer than two graded rollouts gets no class. `pass@k` is the chance that at
least one of `k` rollouts succeeds; `pass^k`, that all `k` do. `-k` sets `k`; by default it
is the most rollouts any task has.

## Use it

Give the file to an adapter to train on the learnable tasks only:

```python
train_dataset = dataset(url, calibration="calibration.json")  # keep=("learnable",)
```

```sh
vf-run miroir-verifiers -m <model> --env.taskset.urls_file urls.txt \
  --env.taskset.calibration calibration.json
```

The file records each task's `task_hash`. When a task file changes, the adapters refuse the
calibration until you calibrate that task again.
