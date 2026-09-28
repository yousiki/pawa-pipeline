# Pinned toolchain via rust-toolchain.toml; everything below is plain cargo plus the pawa CLI.

default: build

# Build the workspace (host toolchain).
build:
    cargo build --release --locked

# Rust tests plus the ops script tests.
test:
    cargo test --workspace --locked
    bash ops/test_hq_worker.sh
    python3 -m unittest discover -s ops -p 'test_*.py'

# Format check, clippy and rustdoc, all warnings as errors (what CI runs).
lint:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets --locked -- -D warnings
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked

fmt:
    cargo fmt --all

# Export every contract's JSON Schema into `out`.
schemas out="out/schema":
    cargo run --release --locked -p pawa -- schema-export --out {{out}}

# Build the runtime image.
image tag="pawa-pipeline:latest":
    docker build -t {{tag}} -f docker/Dockerfile .

# Build pawa + players with the image's toolchain into target/image-bin (mountable into the image).
bins:
    docker build --target bins --output target/image-bin -f docker/Dockerfile .

# End-to-end smoke on one host: CPU capture, GPU render, finalize; results in `out`.
smoke manifest="ops/smoke.json" out="/tmp/pawa-smoke" tag="pawa-pipeline:latest":
    mkdir -p {{out}}
    docker run --rm --gpus all \
      -v "$PWD/ops:/work/ops:ro,z" -v "{{out}}:/mnt/out:Z" \
      -e PAWA_MANIFEST=/work/{{manifest}} -e PAWA_OUT=/mnt/out \
      -e MASTER_ADDR=127.0.0.1 -e RENDER_SLOTS=1 -e CAPTURE_SLOTS=1 \
      -e RENDER_GPU=1 \
      --entrypoint bash {{tag}} /opt/pawa/ops/hq_head.sh

# Same smoke without a GPU: software GL (Mesa llvmpipe), much slower.
smoke-cpu manifest="ops/smoke.json" out="/tmp/pawa-smoke-cpu" tag="pawa-pipeline:latest":
    mkdir -p {{out}}
    docker run --rm \
      -v "$PWD/ops:/work/ops:ro,z" -v "{{out}}:/mnt/out:Z" \
      -e PAWA_MANIFEST=/work/{{manifest}} -e PAWA_OUT=/mnt/out \
      -e MASTER_ADDR=127.0.0.1 -e RENDER_SLOTS=1 -e CAPTURE_SLOTS=1 \
      -e RENDER_GPU=0 \
      --entrypoint bash {{tag}} /opt/pawa/ops/hq_head.sh
