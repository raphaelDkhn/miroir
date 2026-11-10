# Concepts

Env
: A product's Docker image with Miroir, a [manifest](make/manifest.md) and
  [tasks](make/tasks.md) added. One container runs one episode at a time.

App
: A part of the product that the agent or the grader reaches: an HTTP API, an MCP server, a
  Postgres or MySQL database, or several of these.

Persona
: The user the agent acts as. Its headers, such as a bearer token, are pinned on every request
  the agent sends.

World
: A seed snapshot of the product's data. Most envs have one, named `default`.

Bake
: Seeding the data once, at image build, and keeping the state as the pristine copy of each
  world.

Reset
: Restoring a world before an episode: kill every process, copy the state back, move the dates,
  start the product, wait until it is quiet.

Task
: An instruction for the agent, the checks that grade it, the changes it may make, and a golden
  run.

Episode
: One attempt at a task, from reset to its end: a submit, the step limit, the time limit or a
  crash.

Net change
: How a row differs between the reset and the end of the episode: inserted, updated (with the
  columns that changed) or deleted. A row changed and changed back has no net change.

Effect
: An email or HTTP call the product sent, which a [sink](features/sinks.md) caught.

Check
: A SQL query or a command scored from 0 to 1. A weighted check adds to the reward; a gate must
  score 1.

Harm
: A net change the task does not allow, a failed gate, or an episode that broke the env. Harm
  earns `harm_reward`, 0 by default.

Golden run
: A list of tool calls that solves the task. [`miroir check`](make/check.md) replays it and
  requires a reward of 1.

Probe
: A list of tool calls whose reward must stay within bounds, such as a wrong solution that must
  earn 0.

Episode database
: A SQLite file with the episode's tool calls, effects, net changes and answer, which checks
  can query. See the [reference](reference/episode-db.md).

Record
: What a finished episode returns: its calls, net changes, effects, grade, timings and a
  signature.

Signature
: A hash of the end state. Rollouts that end in the same state share it, which
  [review](train/review.md) uses to group them.
