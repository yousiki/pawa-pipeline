# Sourced by hq_head.sh / hq_worker.sh: put a pinned HyperQueue on PATH.
# Prefers a baked /opt/hq/hq (the image provides a PATCHED build — the release binary crashes
# when a task with downstream dependents fails, see docker/hq-taskqueue-panic.patch); the
# release-download fallback needs egress, is meant for local smoke runs, and carries that bug.
HQ_VERSION="${HQ_VERSION:-0.26.2}"
if command -v hq >/dev/null; then
  :
elif [[ -x /opt/hq/hq ]]; then
  export PATH=/opt/hq:$PATH
else
  mkdir -p /tmp/hqbin
  curl -fsSL "https://github.com/It4innovations/hyperqueue/releases/download/v${HQ_VERSION}/hq-v${HQ_VERSION}-linux-x64.tar.gz" \
    | tar -xz -C /tmp/hqbin
  export PATH=/tmp/hqbin:$PATH
fi
hq --version >&2
