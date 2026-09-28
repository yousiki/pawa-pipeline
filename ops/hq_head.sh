#!/usr/bin/env bash
# Rank 0 of a PAWA run: HyperQueue server + a local worker + the submit/retry loop.
#
# Workers follow the server (--on-server-lost stop), the server stops when this script is done,
# and a crashed head resumes from the status tree (pawa plan --resume).
set -euo pipefail

: "${PAWA_MANIFEST:?PAWA_MANIFEST is required}"
OUT="${PAWA_OUT:-/mnt/out}"
RETRY_ROUNDS="${PAWA_RETRY_ROUNDS:-2}"
PLAN_STAGE="${PAWA_PLAN_STAGE:-all}"

source "$(dirname "$0")/hq_install.sh"

ACCESS_DIR="$OUT/hq/access"
export HQ_SERVER_DIR="$ACCESS_DIR"
mkdir -p "$ACCESS_DIR" "$OUT/hq" "$OUT/logs"

# A resubmitted run must start a FRESH server: generate-access refuses to overwrite a stale
# access file, and replaying the previous head's journal would resurrect its half-finished job
# alongside the one this head is about to submit (double execution on the same work tree).
# Cross-job resume is the status tree's job (`pawa plan --resume`), not the journal's.
rm -f "$ACCESS_DIR/access.json" "$OUT/hq/journal"

hq server generate-access "$ACCESS_DIR/access.json" \
  --worker-host "${MASTER_ADDR:?MASTER_ADDR is required}" --client-host "$MASTER_ADDR" \
  --worker-port 9788 --client-port 9789
hq server start --server-dir /tmp/hq-server \
  --access-file "$ACCESS_DIR/access.json" --journal "$OUT/hq/journal" &
for _ in $(seq 1 60); do
  if hq job list >/dev/null 2>&1; then break; fi
  sleep 2
done

"$(dirname "$0")/hq_worker.sh" &

submit_and_wait() {
  local jdf="$1"
  local job_id
  job_id=$(hq job submit-file "$jdf" | grep -oE '[0-9]+' | head -1)
  # `hq job wait` exits non-zero when the job contains failed tasks. That code carries no
  # decision here — task failures are judged from the status tree by `pawa run-report`,
  # and under `set -e` a meaningful-looking non-zero would kill the whole head script.
  hq job wait "$job_id" || true
}

rc=0
jdf=$(pawa plan --manifest "$PAWA_MANIFEST" --out "$OUT" --pawa-bin "$(command -v pawa)" --stage "$PLAN_STAGE" --resume | awk 'NR == 1 {first = $0} END {print first}')
submit_and_wait "$jdf"

# Application-level retries: HQ redeploys on worker LOSS natively, but an exit-3 stage failure
# needs an explicit resubmission round. Exit-2 contract failures are never resubmitted.
for _ in $(seq 1 "$RETRY_ROUNDS"); do
  if pawa run-report --manifest "$PAWA_MANIFEST" --out "$OUT" --stage "$PLAN_STAGE"; then
    rc=0
    break
  fi
  rc=3
  pawa plan --manifest "$PAWA_MANIFEST" --out "$OUT" --pawa-bin "$(command -v pawa)" --stage "$PLAN_STAGE" --resume
  if ! grep -q '\[\[task\]\]' "$jdf"; then
    break # nothing retryable remains
  fi
  submit_and_wait "$jdf"
done
pawa run-report --manifest "$PAWA_MANIFEST" --out "$OUT" --stage "$PLAN_STAGE" || rc=3
echo "hq_head: final rc=$rc"

hq server stop || true
exit "$rc"
