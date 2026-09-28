# syntax=docker/dockerfile:1
#
# The all-Rust PAWA pipeline image. Build acceleration, same resulting image:
#   * BuildKit cache mounts keep the apt archives, the cargo registry and the Rust target dir
#     across builds, so a rebuild recompiles/redownloads only what changed.
#   * The pinned Rust toolchain installs in a layer keyed only on rust-toolchain.toml.
#   * The renderer game dir and the Fabric server pack are sibling stages; BuildKit runs both
#     warmups concurrently with each other.

# Toolchain only — no project source. Both the workspace build and the HyperQueue build branch
# off this stage, so a source edit can never invalidate the other branch's layers.
FROM debian:trixie-slim AS rust-toolchain

ENV DEBIAN_FRONTEND=noninteractive CARGO_HOME=/usr/local/cargo RUSTUP_HOME=/usr/local/rustup \
    PATH=/usr/local/cargo/bin:/usr/local/bin:/usr/bin:/bin
RUN --mount=type=cache,id=apt-builder-cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,id=apt-builder-lists,target=/var/lib/apt/lists,sharing=locked \
    rm -f /etc/apt/apt.conf.d/docker-clean && \
    apt-get update && apt-get install -y --no-install-recommends \
    curl ca-certificates build-essential pkg-config libssl-dev cmake clang patch
RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain none
WORKDIR /build
# The rustup shims install whatever rust-toolchain.toml pins, so this layer holds the toolchain
# download and survives every source edit.
COPY --link rust-toolchain.toml ./
RUN cargo --version

FROM rust-toolchain AS rust-build
COPY --link Cargo.toml Cargo.lock ./
COPY --link crates crates
RUN --mount=type=cache,id=cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=cargo-target,target=/build/target,sharing=locked \
    cargo build --release --locked -p players -p pawa && \
    install -D target/release/players /out/bin/players && \
    install -D target/release/pawa /out/bin/pawa

# `just bins`: export the two binaries, built against the runtime's libc, to the host.
FROM scratch AS bins
COPY --from=rust-build /out/bin /

# Patched HyperQueue. v0.26.2 (and upstream main at the time of writing) crashes the WHOLE server
# whenever a task that has downstream dependents fails: tako's Core::remove_task dequeues tasks
# that were never enqueued, tripping a taskqueue assertion. Our DAG (capture → render → finalize)
# plus real capture failures hits it constantly, so the binary is built from source with the
# 3-line fix (docker/hq-taskqueue-panic.patch) instead of downloaded from releases.
FROM rust-toolchain AS hq-build
COPY --link docker/hq-taskqueue-panic.patch /tmp/hq-taskqueue-panic.patch
RUN --mount=type=cache,id=cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=hq-target,target=/opt/hq-target,sharing=locked \
    export CARGO_TARGET_DIR=/opt/hq-target && \
    curl -fsSL https://codeload.github.com/It4innovations/hyperqueue/tar.gz/680857bdfa45f92c50535cdef525f5a26bbcf55c \
    | tar -xz -C /opt && \
    cd /opt/hyperqueue-680857bdfa45f92c50535cdef525f5a26bbcf55c && \
    patch -p1 < /tmp/hq-taskqueue-panic.patch && \
    rustup toolchain install stable --profile minimal && \
    cargo +stable build --release --locked -p hyperqueue && \
    install -D /opt/hq-target/release/hq /out/hq/hq && \
    /out/hq/hq --version

# Everything the runtime image needs from the OS. Both warmup stages branch off this one so their
# layers are shared, not rebuilt.
FROM debian:trixie-slim AS runtime-base
ENV DEBIAN_FRONTEND=noninteractive \
    MC_VERSION=26.2 FABRIC_LOADER=0.19.3 HEADLESSMC_VERSION=2.10.0 GSON_VERSION=2.14.0 \
    GAME_DIR=/opt/renderer/gamedir RENDER_STATE_DIR=/tmp/pawa-render \
    RENDER_SLOTS=1 RENDER_DISPLAY=:99 \
    LIBGL_ALWAYS_SOFTWARE=1 GALLIUM_DRIVER=llvmpipe \
    VIRTUALGL_VERSION=3.1.5 \
    # Ask the NVIDIA container runtime for the graphics (EGL) driver libs, not just compute.
    # Inert off-GPU; device allocation stays with the orchestrator (NVIDIA_VISIBLE_DEVICES).
    NVIDIA_DRIVER_CAPABILITIES=all \
    PAWA_PLAYER_BIN=/opt/pawa/bin/players \
    PAWA_SERVER_PACK=/opt/pawa/server-pack.tar.gz \
    PAWA_SERVER_WORKDIR=/tmp/pawa-server \
    PATH=/opt/pawa/bin:/opt/hq:/usr/local/bin:/usr/bin:/bin
RUN --mount=type=cache,id=apt-runtime-cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,id=apt-runtime-lists,target=/var/lib/apt/lists,sharing=locked \
    rm -f /etc/apt/apt.conf.d/docker-clean && \
    apt-get update && apt-get install -y --no-install-recommends \
    openjdk-25-jdk-headless xvfb xauth libgl1-mesa-dri libglx-mesa0 libgl1 libegl1 libegl-mesa0 \
    libx11-6 libxcursor1 libxinerama1 libxext6 libxcb1 libxrandr2 libxi6 libxtst6 \
    libxxf86vm1 libxrender1 libglfw3 libopenal1 ffmpeg mesa-utils fonts-dejavu-core procps \
    curl ca-certificates unzip && \
    # VirtualGL: `vglrun -d eglN` redirects the client's GLX onto a GPU's EGL device when
    # RENDER_GPU=1. Not in Debian's repos, so the upstream deb.
    curl -fsSL -o /tmp/virtualgl.deb \
      "https://github.com/VirtualGL/virtualgl/releases/download/${VIRTUALGL_VERSION}/virtualgl_${VIRTUALGL_VERSION}_amd64.deb" && \
    apt-get install -y --no-install-recommends /tmp/virtualgl.deb && rm /tmp/virtualgl.deb && \
    command -v vglrun && \
    # glvnd EGL ICD for the NVIDIA vendor lib. Docker's nvidia runtime provides this file itself,
    # but Apptainer --nv binds only the libraries — without the ICD, glvnd never finds
    # them and VirtualGL falls back to llvmpipe. Harmless where the lib is absent: glvnd skips it.
    mkdir -p /usr/share/glvnd/egl_vendor.d && \
    printf '{"file_format_version":"1.0.0","ICD":{"library_path":"libEGL_nvidia.so.0"}}\n' \
      > /usr/share/glvnd/egl_vendor.d/10_nvidia.json
COPY --link --from=hq-build /out/hq/hq /opt/hq/hq
RUN hq --version

# Renderer game dir: HeadlessMC, the Fabric client mods, the driver mod, and the pre-fetched
# Minecraft client + asset objects.
FROM runtime-base AS renderer
WORKDIR /opt/renderer
# The Modrinth URLs carry an opaque per-version id, so those three cannot be derived from the ENV
# pins above and are spelled out; the rest interpolate.
RUN mkdir -p "$GAME_DIR/mods" && curl -fsSL --parallel --parallel-immediate \
    -o headlessmc-launcher-wrapper.jar \
      "https://github.com/headlesshq/headlessmc/releases/download/$HEADLESSMC_VERSION/headlessmc-launcher-wrapper-$HEADLESSMC_VERSION.jar" \
    -o "$GAME_DIR/mods/fabric-api.jar" \
      https://cdn.modrinth.com/data/P7dR8mSH/versions/vmQp7ixA/fabric-api-0.157.0%2B26.2.jar \
    -o "$GAME_DIR/mods/fabric-language-kotlin.jar" \
      https://cdn.modrinth.com/data/Ha28R6CL/versions/bdhiINYC/fabric-language-kotlin-1.13.13%2Bkotlin.2.4.10.jar \
    -o "$GAME_DIR/mods/replaymod.jar" \
      https://cdn.modrinth.com/data/Nv2fQJo5/versions/5YS1OgDP/replaymod-26.2-2.6.27.jar \
    -o /tmp/fabric-loader.jar \
      "https://maven.fabricmc.net/net/fabricmc/fabric-loader/$FABRIC_LOADER/fabric-loader-$FABRIC_LOADER.jar" \
    -o /tmp/gson.jar \
      "https://repo1.maven.org/maven2/com/google/code/gson/gson/$GSON_VERSION/gson-$GSON_VERSION.jar" \
    -o /tmp/sponge-mixin.jar \
      "https://maven.fabricmc.net/net/fabricmc/sponge-mixin/0.17.3%2Bmixin.0.8.7/sponge-mixin-0.17.3%2Bmixin.0.8.7.jar"
# ReplayMod hides the player's HUD in a render, and two of the three ways it does that are cancels
# no flag can reach: Mixin_SkipHudDuringRender cancels Hud.extractRenderState outright whenever a
# VideoRenderer is active, and MixinInGameHud cancels Hud.extractItemHotbar because CameraEntity
# answers false to RenderHotbarCallback unconditionally. Both cancel at @At("HEAD"), so an injector
# of ours cannot un-cancel them, and injecting into the handlers they add is not possible either:
# mixin validates our injection points before another config's methods are merged, whatever the
# priority. Dropping the two entries is what is left. The greps are the point — if a future ReplayMod
# renames either mixin the build fails here rather than silently shipping HUD-less video.
RUN cd /tmp && unzip -q "$GAME_DIR/mods/replaymod.jar" \
      mixins.render.replaymod.json mixins.replay.replaymod.json && \
    grep -q '"Mixin_SkipHudDuringRender"' mixins.render.replaymod.json && \
    grep -q '"MixinInGameHud"' mixins.replay.replaymod.json && \
    sed -i '/"Mixin_SkipHudDuringRender",/d' mixins.render.replaymod.json && \
    sed -i '/"MixinInGameHud",/d' mixins.replay.replaymod.json && \
    jar --update --file "$GAME_DIR/mods/replaymod.jar" \
      mixins.render.replaymod.json mixins.replay.replaymod.json && \
    rm -f mixins.render.replaymod.json mixins.replay.replaymod.json
# Warmup BEFORE the driver mod: it only downloads the client and prefetches assets (it never
# starts the game, so it reads nothing from mods/), and it is the most expensive layer here —
# ordering it first means a driver/mixin iteration only re-runs the cheap javac layer below.
RUN --mount=type=bind,from=rust-build,source=/out/bin,target=/opt/pawa/bin \
    pawa warmup-render
COPY --link assets/renderer/replaymod-driver-mod /opt/renderer/replaymod-driver-mod
# Gson is a COMPILE-time classpath entry only, never bundled into rmdriver.jar: Minecraft declares
# com.google.code.gson:gson as one of its own libraries (see versions/$MC_VERSION/*.json), so the
# runtime already has it. GSON_VERSION must match the version $MC_VERSION resolves (26.2 → 2.14.0).
RUN cd /opt/renderer/replaymod-driver-mod && mkdir -p build/classes build/jar && \
    javac --release 21 -cp /tmp/fabric-loader.jar:/tmp/gson.jar:/tmp/sponge-mixin.jar -d build/classes \
      src/main/java/moe/siki/pawa/rmdriver/*.java src/main/java/moe/siki/pawa/rmdriver/mixin/*.java && \
    java -ea -cp build/classes:/tmp/gson.jar:/tmp/fabric-loader.jar moe.siki.pawa.rmdriver.SelfCheck && \
    cp -r build/classes/* build/jar/ && cp src/main/resources/fabric.mod.json build/jar/ && \
    cp src/main/resources/mixins.rmdriver.json build/jar/ && \
    rm -f build/jar/moe/siki/pawa/rmdriver/SelfCheck*.class && \
    (cd build/jar && jar --create --file "$GAME_DIR/mods/rmdriver.jar" .)

# Offline Fabric+ServerReplay server pack: download the pinned versions, boot once to fetch vanilla
# libraries and generate the ServerReplay config, archive the clean tree.
FROM runtime-base AS server-pack
RUN --mount=type=bind,from=rust-build,source=/out/bin,target=/opt/pawa/bin \
    pawa warmup-server --pack-out /opt/pawa/server-pack.tar.gz

FROM runtime-base AS runtime
COPY --link --from=renderer /opt/renderer /opt/renderer
COPY --link --from=server-pack /opt/pawa/server-pack.tar.gz /opt/pawa/server-pack.tar.gz
COPY --link --from=rust-build /out/bin /opt/pawa/bin
# Launchers only. The image is the runtime (pawa, MC/Fabric, the driver mod, HQ) and
# nothing here may be bound to a dataset version or a single run: manifests and the batch
# policy that generates them are configuration, and baking them in made every episode-list
# edit or rebalance need a rebuild. Manifests are provided at run time (mounted or passed in by
# the job), and the manifest generators are local tools no pod invokes.
COPY --link ops/hq_entry.sh ops/hq_head.sh ops/hq_worker.sh ops/hq_install.sh \
    /opt/pawa/ops/
WORKDIR /work
ENTRYPOINT ["pawa"]
CMD ["--help"]
