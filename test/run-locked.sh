#!/usr/bin/env bash
# Serialize harness runs around a lock directory.
#
# A run rebuilds the `dx` web bundle when the tree is stale and then serves it
# from disk. Two concurrent runs on the same tree therefore compile into the
# same target dir and can serve each other half-written files, which shows up
# as a flaky scenario failure rather than an obvious build error. Authoring can
# be parallel; running cannot.
#
# Usage: test/run-locked.sh --scenario S10 --surfaces web [--evidence-dir DIR]
# Waits up to LOCK_WAIT seconds (default 900) for the lock, then fails.
#
# The lock records its owner (`owner` file: host, pid, timestamp) so a wait
# exhaustion names who holds it instead of inviting deletion of someone
# else's lock. A SIGKILLed run (power loss) leaves the metadata behind; the
# holder's own trap removes the lock on clean exit.
set -euo pipefail

# Resolve the repo root from this script's own location, so the wrapper works
# from any cwd.
ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

LOCK="${TMPDIR:-/tmp}/po-e2e.lock"
WAIT="${LOCK_WAIT:-900}"
WAITED=0

while ! mkdir "$LOCK" 2>/dev/null; do
    if [ "$WAITED" -ge "$WAIT" ]; then
        echo "run-locked: gave up after ${WAIT}s waiting for $LOCK" >&2
        if [ -f "$LOCK/owner" ]; then
            echo "run-locked: held by: $(cat "$LOCK/owner")" >&2
            echo "run-locked: if that run is dead (crashed host), its owner can remove it: rmdir $LOCK" >&2
        else
            echo "run-locked: held by an older run (no owner record)" >&2
        fi
        exit 1
    fi
    sleep 5
    WAITED=$((WAITED + 5))
done

printf '%s pid=%s started=%s\n' "$(hostname)" "$$" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" > "$LOCK/owner"

cleanup() { rm -rf "$LOCK" 2>/dev/null || true; }
trap cleanup EXIT INT TERM

node test/run.ts "$@"
