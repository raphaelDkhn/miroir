#!/bin/bash
# The Harbor end-to-end gate (§9.3): build the images, check the fixture's tasks, run its
# check.yaml through Harbor and validate the job with `miroir check`, then regrade the job with a
# changed spec, and void a trial whose evidence is gone. Needs Docker with buildx, Harbor 0.26.0
# and jq. Its work directories stay under jobs/ for inspection.
set -euo pipefail
cd "$(dirname "$0")/.."

fail() {
  echo "e2e: $*" >&2
  exit 1
}

# The miroir just built, which sees only the repository: every path it gets lies under it.
miroir() {
  docker run --rm -v "$PWD:$PWD" -w "$PWD" miroir:dev miroir "$@"
}

for tool in docker harbor jq; do
  command -v "$tool" >/dev/null || fail "$tool is not on PATH"
done
[ "$(harbor --version)" = 0.26.0 ] || fail "needs Harbor 0.26.0: uv tool install harbor==0.26.0"

TASKS=fixtures/acme-crm/tasks
WORK=$(mkdir -p jobs && mktemp -d "$PWD/jobs/e2e.XXXXXX")
JOBS="$WORK/jobs"
TMP="$WORK/tasks"
echo "e2e: working in $WORK"

echo "== build"
docker buildx bake miroir main acme-crm-env

echo "== static checks"
miroir check --tasks "$TASKS"
compose=$(docker compose config --help)
old_compose="this Docker Compose lacks 'config --no-env-resolution', so harbor lint cannot check"
old_compose+=" the compose image; update Docker Compose to 2.35 or later"
if grep -q -- --no-env-resolution <<<"$compose"; then
  harbor lint "$TASKS"/*/
elif [ -n "${CI:-}" ]; then
  fail "$old_compose"
else
  echo "skip harbor lint: $old_compose to run it"
fi

echo "== gate: golden, nop and probe runs of every task"
harbor run -c fixtures/acme-crm/check.yaml -y --job-name gate -o "$JOBS"
miroir check --tasks "$TASKS" "$JOBS/gate"

echo "== regrade: pay-invoice's paid check (weight 0.6) scores 0 from version 1.1.0"
cp -R "$TASKS" "$TMP"
paid="      SELECT count(*) FROM after.invoices"$'\n'
paid+="      WHERE number = 'INV-04001' AND status = 'paid' AND payment_intent = 'pi_test'"
toml=$(<"$TMP/pay-invoice/task.toml")
[[ $toml == *"$paid"* && $toml == *'version = "1.0.0"'* ]] ||
  fail "pay-invoice's task.toml no longer holds the paid check and version this step edits"
toml=${toml/"$paid"/"      SELECT 0"}
toml=${toml/'version = "1.0.0"'/'version = "1.1.0"'}
printf '%s\n' "$toml" >"$TMP/pay-invoice/task.toml"
harbor job regrade "$JOBS/gate" -p "$TMP" --job-name regraded -o "$JOBS"
# Each regraded trial against the source trial it names: only pay-invoice's golden run changes.
unexpected=$(jq -nr --slurpfile before <(cat "$JOBS"/gate/*/result.json) \
  --slurpfile after <(cat "$JOBS"/regraded/*/result.json) '
  ($before | map({(.id): .verifier_result.rewards}) | add) as $was
  | if ($after | length) != ($before | length) then
      "\($after | length) trials regraded of \($before | length)"
    else empty end,
    ($after[]
     | (if .task_name == "acme/pay-invoice" and .config.agent.name == "oracle"
           and (.config.agent.env.SOLUTION // "") == ""
        then {reward: 0.4, harm: 0} else $was[.config.source_trial.trial_id] end) as $expected
     | select(.verifier_result.rewards != $expected)
     | "\(.trial_name): \(.verifier_result.rewards), expected \($expected)")')
[ -z "$unexpected" ] || fail "regrade: $unexpected"

echo "== void: a golden trial whose evidence is gone"
cp -R "$JOBS/gate" "$WORK/gate-without-evidence"
read -r trial id < <(jq -r 'select(.task_name == "acme/pay-invoice"
  and .config.agent.name == "oracle" and (.config.agent.env.SOLUTION // "") == "")
  | "\(.trial_name) \(.id)"' "$WORK"/gate-without-evidence/*/result.json) ||
  fail "the gate job has no golden pay-invoice trial"
find "$WORK/gate-without-evidence/$trial/artifacts" -name evidence.db -delete
harbor job regrade "$WORK/gate-without-evidence" -p "$TASKS" --job-name voided -o "$JOBS"
voided=$(jq -r --arg id "$id" 'select(.config.source_trial.trial_id == $id) | .trial_name' \
  "$JOBS"/voided/*/result.json)
jq -e '.reward_valid == false' "$JOBS/voided/$voided/result.json" >/dev/null ||
  fail "$voided: a trial without evidence still has a valid reward"
grep -q '^void: no evidence' "$JOBS/voided/$voided/verifier/test-stdout.txt" ||
  fail "$voided: its verifier/test-stdout.txt gives no 'void: no evidence' line"
status=0
miroir check --tasks "$TASKS" "$JOBS/voided" >"$WORK/voided.txt" || status=$?
if [ "$status" != 1 ] || ! grep -q 'golden VOID no evidence' "$WORK/voided.txt"; then
  fail "miroir check exited $status on a job with a void: $(cat "$WORK/voided.txt")"
fi

echo "e2e: passed"
