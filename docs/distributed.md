# Running at scale

The pipeline splits every episode into independent units and schedules them with
[HyperQueue](https://github.com/It4innovations/hyperqueue). Capture runs headless on CPUs; rendering
replays the saved recordings offline and can run on different hosts, with GPUs, at a different
time. All units communicate only through files under one output directory, so any set of hosts that
can see the same output tree (a shared filesystem, or a tree synced between phases) can run a
collection.

## Units

`pawa plan` turns a run manifest into a static task graph. Per episode:

| Unit | Command | Depends on | Resources requested |
|---|---|---|---|
| capture | `pawa unit capture` | nothing | `capture_cpus` CPUs (manifest, default 16) and one `captureslot` |
| render-pov | `pawa unit render-pov --pov pNN` | the episode's capture | `render.threads_per_render` CPUs (default 8) and one `renderslot` |
| render-observer | `pawa unit render-observer --observer <name>` | capture and every render-pov of the episode | same as render-pov |
| finalize | `pawa unit finalize` | every render of the episode | 2 CPUs and one `cpustage` |
| hud | `pawa unit hud` (only when `render.hud`) | finalize | 4 CPUs and one `cpustage` |

Every unit also takes `--manifest`, `--episode-id` and `--out`. One render-pov task renders all
projections of one player view (plus depth passes when `render.depth` is set), so views are
distributed across render workers independently. `render.pov_sample` renders only a seeded,
deterministic subset of the players; capture always records the whole swarm.

Episodes never wait on each other. Tasks are prioritized depth-first by episode order, so a worker
with free capacity finishes the render, finalize and HUD of started episodes before opening new
captures, and finished episodes appear continuously.

### Exit codes and status

Each unit writes its status file under `<out>/status/<episode_id>/` atomically (`capture.json`,
`<pov>.render.json`, `finalize.json`, ...) and exits with:

- `0`: complete, or skipped by design (a player that never connected is recorded as
  `skipped_no_connect` and the graph keeps its shape);
- `2`: contract failure (bad input); never retried;
- `3`: stage failure; retryable.

`pawa plan --resume` reads the status tree and emits only units that have not succeeded: failed
units and their never-run dependents. Units that completed, were skipped by design, or failed with
a contract error are left out, and dependencies on them are treated as satisfied. `pawa run-report`
rebuilds `<out>/runs/<run_id>.json` from the status tree and exits 3 while any unit is failed.

### Stages

`--stage` restricts the graph to one phase:

- `capture`: only capture units.
- `render`: render, finalize and HUD units, for episodes whose `status/<ep>/capture.json` records a
  completed capture. Dependencies on capture are satisfied by those files instead of by tasks.
- `all` (default): the whole graph.

`--stage render` can be re-run while captures are still arriving: each invocation picks up the
episodes whose capture has completed since, and `--resume` skips what is already rendered.

## Launchers

The image ships four scripts under `/opt/pawa/ops/`:

| Script | Role |
|---|---|
| `hq_head.sh` | Starts the HyperQueue server, a local worker, and the submit loop: `pawa plan --resume`, submit, wait, then up to `PAWA_RETRY_ROUNDS` resubmission rounds for stage failures, and a final `pawa run-report`. Exits with the report's status. |
| `hq_worker.sh` | Starts one worker that connects to the head through the access file under `$PAWA_OUT/hq/access/`, and stops when the server stops. Waits up to 15 minutes for the head to appear. |
| `hq_entry.sh` | Multi-node entry point: `RANK=0` runs `hq_head.sh`, any other rank runs `hq_worker.sh`. Fits launchers that start the same command on every node with a rank variable. |
| `hq_install.sh` | Puts `hq` on `PATH` (sourced by the other two). The image carries a HyperQueue build patched for a server crash when a task with dependents fails; the release-download fallback outside the image does not have that fix and is only suitable for small local runs. |

Head environment:

| Variable | Default | Meaning |
|---|---|---|
| `PAWA_MANIFEST` | required | Run manifest path, as seen inside the container. |
| `PAWA_OUT` | `/mnt/out` | Output root, shared by all hosts. |
| `MASTER_ADDR` | required | Address workers use to reach the head. Ports 9788 (workers) and 9789 (clients). |
| `PAWA_PLAN_STAGE` | `all` | `capture`, `render` or `all`, passed to `pawa plan --stage`. |
| `PAWA_RETRY_ROUNDS` | `2` | Resubmission rounds for exit-3 failures. |

Worker environment (the head's local worker reads the same variables):

| Variable | Default | Meaning |
|---|---|---|
| `PAWA_CPUS` | `16` | CPUs the worker offers. |
| `CAPTURE_SLOTS` | `1` | Concurrent captures, 0 to 10. Each slot index derives its own server and RCON ports and world directory. `0` makes a render-only worker. |
| `RENDER_SLOTS` | `1` | Render lanes on this host (X displays and game-state directories); must be at least 1. |
| `HQ_RENDER_SLOTS` | `RENDER_SLOTS` | Render slots advertised to the scheduler: `RENDER_SLOTS`, or `0` for a capture-only worker. |
| `CPU_STAGE_SLOTS` | `PAWA_CPUS` | Concurrent finalize/HUD units. |

Only one head may run per output tree. A starting head deletes the previous access file and
journal, so a restarted run always begins with a fresh server; resuming across restarts is done by
`pawa plan --resume` against the status tree.

Minimal single-host run (this is what `just smoke` does):

```sh
docker run --rm -v "$PWD/ops:/work/ops:ro,z" -v /data/run1:/mnt/out:Z \
  -e PAWA_MANIFEST=/work/ops/smoke.json -e PAWA_OUT=/mnt/out \
  -e MASTER_ADDR=127.0.0.1 -e RENDER_SLOTS=1 -e CAPTURE_SLOTS=1 --gpus all \
  --entrypoint bash pawa-pipeline:latest /opt/pawa/ops/hq_head.sh
```

To add hosts, start the same image with `hq_worker.sh` (or `hq_entry.sh` with `RANK` > 0) and the
same output tree mounted at `PAWA_OUT`. When several containers on one host mount the same
directory under SELinux, use the shared label `:z`, not `:Z`.

## Splitting capture and rendering

Capture needs no graphics and scales with CPU hosts; rendering needs GL and is much faster on GPUs.
A collection can run the two phases on different pools over the same output tree:

1. **Capture on CPU hosts.** Head with `PAWA_PLAN_STAGE=capture`; workers with `CAPTURE_SLOTS`
   sized to the host (each capture reserves `capture_cpus` CPUs) and `HQ_RENDER_SLOTS=0`. This
   fills `work/<ep>/capture/` and `status/<ep>/capture.json`.
2. **Make the tree visible to the render pool.** Either both pools mount the same filesystem, or
   copy `work/` and `status/` to the render side.
3. **Render on GPU hosts.** Head with `PAWA_PLAN_STAGE=render`; workers with `CAPTURE_SLOTS=0`,
   `RENDER_GPU=1` and `RENDER_SLOTS` set to the number of concurrent renders. Finalize and HUD run
   in this phase too. The head's own local worker uses the same settings, so run the head on a
   render host (or give it `HQ_RENDER_SLOTS=0`).

Because rendering reads only the saved replays and capture streams, a captured session can be
rendered again later with different projections, depth export or observer cameras, without
replaying the gameplay. Finalize keeps the capture artifacts under the episode's
`diagnostics/capture/`; to re-render, place them back at `work/<ep>/capture/` in a fresh output
root together with the episode's `status/<ep>/capture.json`, change the `render` block of the
manifest, and run the render stage there.

## GPU rendering

Renders run the Minecraft client under Xvfb with ReplayMod. On a host that exposes NVIDIA
devices (`/dev/nvidiaN`), the client runs under VirtualGL's EGL back end (`vglrun -d eglN`) and
rasterizes on the GPU; without one it falls back to Mesa llvmpipe (software) with a warning. For
GPU rendering the container needs the GPU devices and the NVIDIA graphics driver libraries (`docker run --gpus all` with the NVIDIA container toolkit; the
image sets `NVIDIA_DRIVER_CAPABILITIES=all`).

| Variable | Meaning |
|---|---|
| `RENDER_GPU` | Unset: auto-detect (GPU if `/dev/nvidiaN` exists, else llvmpipe). `1`: require the GPU; lane `i` renders on EGL device `i mod RENDER_GPUS`. `eglN`: every lane on device N. `0`: force llvmpipe. |
| `RENDER_GPUS` | Number of GPUs to spread lanes over (default: the number of `/dev/nvidiaN` devices). |
| `RENDER_SLOTS` | Lanes per host. Each lane gets its own X display and game-state directory. |
| `RENDER_DISPLAY` | Base X display (default `:99`). |
| `RENDER_STATE_DIR` | Writable root for per-lane game state. |
| `RENDER_ENCODER` | Override the manifest encoder: an ffmpeg encoder name, or `auto` to use `h264_nvenc` when a real NVENC encode succeeds and the manifest encoder otherwise. |

A render that uses the GPU (requested or detected) but gets a software renderer fails instead of
silently falling back.

### Preflight

`pawa doctor` checks a host before it takes work: Java 25 or newer; `Xvfb`, `glxinfo`, `vglrun`,
`ffmpeg`, `ffprobe` and `hq` on `PATH`; working `libx264` and `libsvtav1` encoders (and the
`RENDER_ENCODER` override, if set); a GL probe that must report a hardware renderer whenever the GPU is
in use (llvmpipe otherwise); the player binary (`PAWA_PLAYER_BIN`) and server pack
(`PAWA_SERVER_PACK`); and a write, fsync and read-back round trip on `PAWA_OUT`.

## Capture settings

Capture defaults can be overlaid with a JSON file named by `PAWA_CAPTURE_CONFIG` (fields of
`CaptureConfig` in `crates/capture/src/lib.rs`: server heap, view and simulation distance, join
stagger, timeouts, minimum capture rate, and so on). `SERVER_XMX` sets the server's maximum heap
directly. A capture whose mean per-player recording rate falls below the configured minimum is
rejected, since a server that falls behind produces sparse action rows no alignment can repair.

Players are split into runtime processes (shards) that connect to one server: `shards: 0` (the
default) uses one shard per 10 players; scenarios whose roles resolve against peer indices are
`single_process` and require `shards: 1`.

## Large sessions and observers

Example manifests in `ops/`:

| Manifest | Contents |
|---|---|
| `smoke.json` | Two 2-player, 60 s episodes. End-to-end check. |
| `many-player-smoke.json` | Two 4-player, 60 s episodes (a rendezvous with one far player, and a crowded interaction). |
| `many-player-acceptance.json` | Five scenarios at 4, 8 and 16 players, 300 s each. |
| `regional-showcase-n{4,64,128}.json` | `regional_capacity_traffic` at 4, 64 and 128 simultaneous players; see [`many-player-sessions.md`](many-player-sessions.md). |
| `observer-smoke.json` | Two 8-player episodes with two observer cameras each: `crowd_static` and `crowd_orbit`. |

Without graphics clients, the cost of a large session during capture is the server and the player
runtime; rendering cost scales with the number of views rendered, which `render.pov_sample` bounds.
Player POV names use three digits once an episode has more than 100 players (`p000` ... `p127`
at 128 players).

Observers are free cameras declared under `render.observers`. Their paths are computed from the
players' recorded positions (`crowd_static`: fixed camera facing the crowd center; `crowd_orbit`:
orbit around the smoothed crowd center; `keyframes`: a hand-written path), one keyframe per frame
on the same tick grid as the player views. An observer is rendered from one player's replay
(`source_pov`, by default the most central player), and a replay contains only the chunks that
player received, so a camera far outside that player's view distance renders fog;
`max_distance` bounds how far the camera pulls back. An observer failure never fails the episode.
