#!/usr/bin/env bash
# End-to-end smoke test against a running runtime (e.g. the kind cluster from up.sh + deploy.sh, port-forwarded).
# Creates a session on $REPO, chats, steers, pushes, proposes and rejects a PR, replaces the pod and resumes.
# It pushes a `sloppod/<id>` branch to $REPO and leaves it there; it never approves a proposal.
set -euo pipefail

export SLOPPOD_URL=${SLOPPOD_URL:-http://localhost:18080}
REPO=${REPO:-bo0tzz/sloppod}
CONTEXT=${CONTEXT:-kind-sloppod}
ctl() { "${SLOPPODCTL:-target/debug/sloppodctl}" "$@"; }
step() { printf '\n== %s\n' "$*"; }
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }

step "create a session on $REPO"
id=$(ctl create --repo "$REPO" --title "smoke test $(date +%H:%M)")
echo "$id"
ctl wait "$id" --timeout 900

step "prompt with a tool call"
out=$(ctl prompt "$id" "Run 'git branch --show-current' and reply with only its output." --allow)
echo "$out" | tail -3
grep -q "sloppod/" <<<"$out" || fail "agent didn't report its session branch"

step "steer a running turn"
ctl prompt "$id" "Use the Bash tool to run exactly this in the foreground: node -e \"setTimeout(() => console.log('waited'), 25000)\" . Then summarise in one line." --allow >"${TMPDIR:-/tmp}/smoke-steer.$$" 2>&1 &
turn=$!
sleep 12
ctl steer "$id" "Change of plan: reply with exactly STEERED-OK and nothing else."
wait "$turn" || true
tail -3 "${TMPDIR:-/tmp}/smoke-steer.$$"
grep -q STEERED-OK "${TMPDIR:-/tmp}/smoke-steer.$$" || fail "steering didn't reach the turn"

step "push, refused push, proposal"
out=$(ctl prompt "$id" "Create docs/smoke-test.md containing 'sloppod smoke test', commit it as 'Smoke test', push it to your session branch, then run 'git push origin HEAD:main' and quote git's output, then call propose_pull_request with title 'Smoke test (do not merge)' and a one-line body." --allow)
echo "$out" | tail -8
proposal=$(ctl proposals --session "$id" | awk '/pending/ {print $1; exit}')
[[ -n $proposal ]] || fail "no pending proposal"
curl -fsS "$SLOPPOD_URL/api/v1/proposals/$proposal" | grep -q 'smoke-test.md' || fail "proposal diff lacks the change"
ctl reject "$proposal" --reason "smoke test" >/dev/null

step "replace the pod and resume"
kubectl --context "$CONTEXT" -n sloppod delete pod "sloppod-$id" --wait=true
sleep 5
ctl wait "$id" --timeout 600
out=$(ctl prompt "$id" "Without running anything: what was the title of the pull request you proposed? One line." --allow)
echo "$out" | tail -2
grep -q "Smoke test" <<<"$out" || fail "context lost after pod replacement"

step "stop"
ctl stop "$id"
echo "PASS ($id; branch left on $REPO)"
