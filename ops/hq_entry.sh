#!/usr/bin/env bash
# Multi-node entrypoint (e.g. a PyTorch-style distributed job): RANK 0 runs the head (server +
# submit loop + worker, reachable at MASTER_ADDR), every other rank runs a worker.
set -euo pipefail
cd "$(dirname "$0")"
case "${RANK:-0}" in
  0) exec bash ./hq_head.sh ;;
  *) exec bash ./hq_worker.sh ;;
esac
