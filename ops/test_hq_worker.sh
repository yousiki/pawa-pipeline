#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
bash -n "$ROOT/ops/hq_worker.sh" "$ROOT/ops/hq_head.sh"

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
mkdir -p "$TMP/bin" "$TMP/out/hq/access"
touch "$TMP/out/hq/access/access.json"
cat >"$TMP/bin/hq" <<'EOF'
#!/usr/bin/env bash
if [[ "$1" == --version ]]; then exit 0; fi
printf '%s\n' "$@" >"$HQ_TEST_ARGV"
if [[ "${HQ_TEST_MODE:-}" == closed ]]; then
  echo "Server connection closed" >&2
  exit 1
fi
EOF
chmod +x "$TMP/bin/hq"

run_worker() {
  : >"$TMP/argv"
  PATH="$TMP/bin:$PATH" HQ_TEST_ARGV="$TMP/argv" PAWA_OUT="$TMP/out" "$@" \
    "$ROOT/ops/hq_worker.sh"
}

run_worker env RENDER_SLOTS=1 PAWA_CPUS=16
expected=$'worker\nstart\n--cpus\n16\n--resource\ncaptureslot=range(0-0)\n--resource\nrenderslot=range(0-0)\n--resource\ncpustage=sum(16)\n--on-server-lost\nstop'
[[ "$(<"$TMP/argv")" == "$expected" ]]

run_worker env RENDER_SLOTS=3 HQ_RENDER_SLOTS=3 CAPTURE_SLOTS=0 CPU_STAGE_SLOTS=0 PAWA_CPUS=24
expected=$'worker\nstart\n--cpus\n24\n--resource\nrenderslot=range(0-2)\n--on-server-lost\nstop'
[[ "$(<"$TMP/argv")" == "$expected" ]]

run_worker env RENDER_SLOTS=1 HQ_RENDER_SLOTS=0 CAPTURE_SLOTS=1 CPU_STAGE_SLOTS=16 PAWA_CPUS=16
expected=$'worker\nstart\n--cpus\n16\n--resource\ncaptureslot=range(0-0)\n--resource\ncpustage=sum(16)\n--on-server-lost\nstop'
[[ "$(<"$TMP/argv")" == "$expected" ]]

run_worker env RENDER_SLOTS=3 HQ_RENDER_SLOTS=0 CAPTURE_SLOTS=0 CPU_STAGE_SLOTS=0 PAWA_CPUS=8
expected=$'worker\nstart\n--cpus\n8\n--on-server-lost\nstop'
[[ "$(<"$TMP/argv")" == "$expected" ]]

run_worker env HQ_TEST_MODE=closed RENDER_SLOTS=1 PAWA_CPUS=16

if run_worker env RENDER_SLOTS=0; then
  echo "RENDER_SLOTS=0 should fail" >&2
  exit 1
fi
if run_worker env RENDER_SLOTS=01; then
  echo "RENDER_SLOTS=01 should fail strict decimal validation" >&2
  exit 1
fi
if run_worker env RENDER_SLOTS=2 HQ_RENDER_SLOTS=1; then
  echo "mismatched HQ_RENDER_SLOTS should fail" >&2
  exit 1
fi
run_worker env RENDER_SLOTS=2 CAPTURE_SLOTS=2 CPU_STAGE_SLOTS=32 PAWA_CPUS=32
expected=$'worker\nstart\n--cpus\n32\n--resource\ncaptureslot=range(0-1)\n--resource\nrenderslot=range(0-1)\n--resource\ncpustage=sum(32)\n--on-server-lost\nstop'
[[ "$(<"$TMP/argv")" == "$expected" ]]

# 11 slots would walk the server port onto the rcon port; derive_capture_slot rejects it too.
if run_worker env CAPTURE_SLOTS=11; then
  echo "CAPTURE_SLOTS=11 should fail" >&2
  exit 1
fi
