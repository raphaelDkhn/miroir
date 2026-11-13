# Concepts

**Env**  
A product's Docker image extended with Miroir, a [manifest](make/manifest.md), and [tasks](make/tasks.md). One container runs one episode at a time.

**App**  
A product interface accessible to the agent or grader: an HTTP API, MCP server, database, or combination.

**Persona**  
The identity the agent acts as. Its credentials and headers are automatically attached to every request.

**World**  
A seeded snapshot of product data. Most envs have one, named `default`.

**Bake**  
Seeds each world at image build time and saves its pristine state for future resets.

**Reset**  
Restores a world to its pristine state, restarts the product, advances dates, and waits until it is ready.

**Task**  
An agent instruction, grading checks, allowed changes, and a golden run.

**Episode**  
One attempt at a task, from reset until submission, step or time limit, or crash.

**Net change**  
A row-level difference between the initial and final state: inserted, updated (including changed columns), or deleted. Changes reverted before the end are ignored.

**Effect**  
An outgoing email or HTTP request captured by a [sink](features/sinks.md).

**Check**  
A SQL query or command that scores an episode from 0 to 1. Weighted checks contribute to reward; gates must score 1.

**Harm**  
An unauthorized state change, failed gate, or broken environment. Harm overrides the normal reward with `harm_reward` (0 by default).

**Golden run**  
A sequence of tool calls that solves a task. [`miroir check`](make/check.md) replays it and requires a reward of 1.

**Probe**  
A sequence of tool calls with an expected reward range, used to verify that incorrect behavior is graded correctly.

**Episode database**  
A SQLite database containing an episode's tool calls, effects, net changes, and answer. Checks can query it. See the [reference](reference/episode-db.md).

**Record**  
The output of a completed episode: tool calls, net changes, effects, grade, timings, and signature.

**Signature**  
A hash of the final state. Episodes with identical final states share a signature, allowing [review](train/review.md) to group them.