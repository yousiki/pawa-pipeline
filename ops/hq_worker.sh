#!/usr/bin/env bash
# Rank N>0 of a PAWA run: one HyperQueue worker. Connects to the head's server via the
# access file on the shared mount and exits when the server does.
set -euo pipefail

OUT="${PAWA_OUT:-/mnt/out}"
RENDER_SLOTS="${RENDER_SLOTS:-1}"
HQ_RENDER_SLOTS="${HQ_RENDER_SLOTS:-$RENDER_SLOTS}"
CAPTURE_SLOTS="${CAPTURE_SLOTS:-1}"
CPUS="${PAWA_CPUS:-16}"
CPU_STAGE_SLOTS="${CPU_STAGE_SLOTS:-$CPUS}"

decimal() { [[ "$1" == 0 || "$1" =~ ^[1-9][0-9]*$ ]]; }

decimal "$RENDER_SLOTS" || {
  echo "RENDER_SLOTS must be a positive decimal integer" >&2
  exit 2
}
decimal "$HQ_RENDER_SLOTS" || {
  echo "HQ_RENDER_SLOTS must be a non-negative decimal integer" >&2
  exit 2
}
decimal "$CAPTURE_SLOTS" || { echo "CAPTURE_SLOTS must be a non-negative integer" >&2; exit 2; }
decimal "$CPUS" || {
  echo "PAWA_CPUS must be a positive decimal integer" >&2
  exit 2
}
decimal "$CPU_STAGE_SLOTS" || {
  echo "CPU_STAGE_SLOTS must be a non-negative decimal integer" >&2
  exit 2
}
RENDER_SLOTS_N=$RENDER_SLOTS
HQ_RENDER_SLOTS_N=$HQ_RENDER_SLOTS
CAPTURE_SLOTS_N=$CAPTURE_SLOTS
CPUS_N=$CPUS
CPU_STAGE_SLOTS_N=$CPU_STAGE_SLOTS
((RENDER_SLOTS_N > 0)) || { echo "RENDER_SLOTS must be a positive decimal integer" >&2; exit 2; }
((HQ_RENDER_SLOTS_N == 0 || HQ_RENDER_SLOTS_N == RENDER_SLOTS_N)) || {
  echo "HQ_RENDER_SLOTS must be 0 or equal RENDER_SLOTS" >&2
  exit 2
}
((CAPTURE_SLOTS_N <= 10)) || { echo "CAPTURE_SLOTS must be 0..10 (slots are port offsets between 25565 and 25575)" >&2; exit 2; }
((CPUS_N > 0)) || { echo "PAWA_CPUS must be a positive decimal integer" >&2; exit 2; }

source "$(dirname "$0")/hq_install.sh"

ACCESS_DIR="$OUT/hq/access"
export HQ_SERVER_DIR="${HQ_SERVER_DIR:-$ACCESS_DIR}"

worker_args=(worker start --cpus "$CPUS_N")
# Indexed, not a plain count: a capture owns two ports and a world directory it wipes each
# episode, all derived from the granted index (see derive_capture_slot). A sum resource would hand
# out N permits with no way to tell the holders apart, and they would collide.
((CAPTURE_SLOTS_N > 0)) && worker_args+=(--resource "captureslot=range(0-$((CAPTURE_SLOTS_N - 1)))")
((HQ_RENDER_SLOTS_N > 0)) && worker_args+=(--resource "renderslot=range(0-$((HQ_RENDER_SLOTS_N - 1)))")
((CPU_STAGE_SLOTS_N > 0)) && worker_args+=(--resource "cpustage=sum($CPU_STAGE_SLOTS_N)")
worker_args+=(--on-server-lost stop)

for _ in $(seq 1 180); do
  if [[ ! -f "$HQ_SERVER_DIR/access.json" ]]; then
    sleep 5
    continue
  fi
  log=$(mktemp)
  if hq "${worker_args[@]}" > >(tee "$log") 2> >(tee -a "$log" >&2); then
    rm -f "$log"
    exit 0
  fi
  if grep -q 'Server connection closed' "$log"; then
    rm -f "$log"
    exit 0
  fi
  rm -f "$log"
  # A stale access file from an earlier head may exist while the replacement head is queued.
  # Keep trying for the full 15-minute startup window so workers see the fresh file when it lands.
  sleep 5
done
echo "worker never connected" >&2
exit 1
