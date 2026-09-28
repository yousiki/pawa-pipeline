# Scenario authoring

A scenario is a Rust script that every player of an episode runs. It decides where each player walks,
where it looks, and what it builds, breaks or attacks. This page covers how to write and register
one, and the validation loop a scenario goes through before it is used for collection.

## The authoring loop

Scenarios start as natural-language descriptions of player behavior and the expected outcome, for
example "two to four players build a shelter together; each one lays a wall while the others watch
it rise". A coding agent (or a person) turns the description into a script by composing the
reusable primitives described below. Scripts can compile and run and still not produce the intended
interaction, so each one is validated on behavior and on appearance:

1. **Write** `crates/players/src/scenarios/<name>.rs` and register it.
2. **Capture without rendering.** Run the episode on the server with headless players only. This takes
   minutes and needs no GPU.
3. **Behavioral QC.** `pawa qc-actions` reads the recorded actions and positions and reports
   inactivity, ineffective movement, abrupt control changes and other degenerate behavior. A
   failure goes back to step 1 with the printed diagnostics.
4. **Render** the passing capture (POV and panorama).
5. **Visual review.** A vision-language model (or a person) reviews sampled videos against the
   described outcome: did the players actually meet, did the wall go up, is the partner in view
   when the description says so. Failures go back to step 1 with the reviewer's notes.

The VLM itself is not part of this repository; any model that accepts images and the scenario
description works. The pipeline provides the pieces around it: the capture, the QC gate, the
renders, timestamped contact sheets of finalized videos for the reviewer
(`ops/regional_review_sheet.py`), and an action-HUD video for checking by eye that recorded
actions match the picture.

## Writing a scenario

### The script

Create `crates/players/src/scenarios/<name>.rs`:

```rust
use super::*;

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let me = global_index(ctx);
    while !ctx.target_met() {
        // ... move, look, build, record ...
    }
}
```

Every player runs the same function; behavior is differentiated by the player's index. `ctx.target_met()`
turns true when the episode duration is reached, and the script must keep producing activity until
then.

### Registration

1. In `crates/players/src/scenarios/mod.rs`, add `mod <name>;` and a match arm
   `"<name>" => <name>::run(ctx).await,` in `run_scenario`.
2. In `crates/schema/src/catalog.rs`, add an entry to `SCENARIOS`. The constructors `s(...)`,
   `s6(...)` and `s7(...)` take the same arguments and differ only in the generation they tag:

   ```rust
   s6(
       "<name>",       // name, as used in manifests
       "construction", // family
       2,              // min_players: fewest players the script is meaningful with
       false,          // single_process: roles resolve against peer indices; forbid sharding
       true,           // world_edits: the scenario places or breaks blocks
       "none",         // recommended_scenery: one of KNOWN_SCENERY
       false,          // needs_far_players: some players start far away (rendezvous scenarios)
   ),
   ```

   Keep the entry in this literal shape: `ops/gen_manifest.py` reads the catalog by pattern-matching
   these calls. Generation 6 scenarios get an automatic opening look at the shared anchor before the
   script runs; generation 7 names are dispatched through `scenarios/v7.rs` instead of the match in
   `run_scenario`, so a standalone new script should use `s` or `s6` and its own match arm.
3. Update the scenario counts asserted in `catalog_is_well_formed` (same file).

The catalog is the single source of truth: manifest validation rejects unknown scenario names,
`player_count < min_players`, sharded single-process scenarios, and a `far_players` value that does not
match `needs_far_players`. The QC thresholds are also tiered by catalog metadata (see below).

### Primitives

`crates/players/src/ctx.rs` (`Ctx`) exposes the player:

| Group | Methods |
|---|---|
| Identity and state | `index`, `bot_count`, `cfg`, `anchor`, `position`, `on_ground`, `elapsed_ms`, `target_met`, `rng_next`, `inventory` |
| Peers and entities | `peer_position`, `peer_username`, `group_center`, `nearest_hostile`, `entity_position`, `distance_to_entity` |
| Movement | `move_to`, `move_default`, `move_to_watching_peer`, `move_to_watching_point`, `hold_visual`, `walk_until_airborne`, `set_jump`, `still`, `pause` |
| Camera | `look`, `look_default`, `look_until_facing`, `sweep`, `sweep_default`, `drag_view`, `wait_camera` |
| World edits | `equip`, `place_block`, `build_blocks`, `build_tower_underneath`, `dig`, `place_ground_torch`, `block_id_at`, `find_visible_ores`, `target_in_reach` |
| Combat | `attack_peer`, `attack_entity` |
| Annotation | `record(action_type, event_id, description, payload)` writes a row to `events.jsonl` |

`crates/players/src/scenarios/common.rs` holds higher-level helpers shared by scripts, including
`settle_on_ground`, `global_index`, `travel_to`, `hold_facing`, `look_at_peer`,
`glance_at_nearest_peer`, `witness_peer_edits`, `raise_landmark`, `shake_loose`, `station_ring`,
`ring_radius`, `walkable_xz_near`, `pattern_blocks`, `point_at_yaw`, `head_of` and
`is_far_traveler`. Read existing scenarios in the same family before writing a new one; most new
scripts are a recombination of these.

Closed vocabularies in the catalog: `KNOWN_SCENERY` lists the scenery generators and
`EQUIP_ITEMS` the hotbar kit every player receives. `equip` of an item not in the kit silently equips
nothing.

## Design guidelines

- **Support general N.** A scenario must be meaningful for any player count at or above
  `min_players`, not only for two players. Derive roles from the player index (`global_index`, pairs or
  slots computed from it) rather than hard-coding two players, and do not put a player count in the
  scenario name. `ctx.index()` and `ctx.bot_count()` are local to the player's process shard;
  `global_index(ctx)` is the index across the whole swarm. If roles depend on peer indices, set
  `single_process` so the swarm is not sharded.
- **Avoid near-clones.** Players in one episode should not emit the same action sequence. Offset
  phases, headings and targets per index; QC flags pairs that agree on more than 80% of active
  frames. With four or more players, give each one its own work area instead of duplicating the
  same work.
- **Few live mobs.** Keep zombies, animals and villagers rare, and keep some scenarios entirely
  free of them. Vary appearance (block types, items) rather than adding mechanics.
- **Stay in the light.** Scripts should keep players out of unlit caves; long dark stretches fail the
  video darkness check at finalize.
- **Real terrain for memory scenarios.** A scenario that tests whether a view can be reproduced
  after the camera turns away and back must run on normal terrain (`minecraft:normal`, the
  default), not Superflat. On a uniform plane every direction looks the same, so a model can
  produce a plausible frame without remembering anything.
- **Per-episode overrides, not new defaults.** Orchestration knobs such as `swarm.spread_range`
  are per-episode manifest fields (`SwarmSpec` is all optional and falls back to global defaults).
  Change them in the manifest of the episodes that need them; changing a global default silently
  changes every existing scenario.
- **Evaluation scenarios stay out of training.** Scenarios with family `evaluation` (names
  `eval_*`) are for evaluation only. `ops/gen_manifest.py` excludes them by default and emits only
  them with `--evaluation`; run them under a separate run id and output directory, never mixed into
  a training manifest. See [evaluation-scenarios.md](evaluation-scenarios.md).

## Capture without rendering

Build the image once (`just image`, tag `pawa-pipeline:latest`). Write a one-episode manifest. Use
`N = max(min_players, 4)` so both small and larger crowds are exercised, the scenario's
`recommended_scenery`, and `shards: 1` for `single_process` scenarios:

```json
{
  "run_id": "dev",
  "capture_cpus": 8,
  "episodes": [
    {
      "episode_id": "dev-<name>",
      "player_count": 4,
      "shards": 1,
      "episode_duration_sec": 120,
      "scenario": {"name": "<name>", "seed": 2026, "scenery": "none"}
    }
  ],
  "render": {
    "projections": [{"name": "default", "method": "DEFAULT", "width": 640, "height": 360}],
    "fps": 20
  }
}
```

Scenarios with `needs_far_players` also need `"far_players": 1` in `scenario`. Validate it with
`cargo run --release -p pawa -- manifest --manifest dev.json`.

Run only the capture unit. The image's entrypoint is `pawa`; to test a changed script without
rebuilding the whole image, rebuild just the binaries with the image's own toolchain (`just bins`,
which writes `target/image-bin/{pawa,players}`; a host `cargo build` may link a newer glibc than
the image has) and mount the player runtime over the baked one (`PAWA_PLAYER_BIN`, default
`/opt/pawa/bin/players`):

```sh
just bins
mkdir -p /tmp/dev
cp dev.json /tmp/dev/
docker run --rm -v /tmp/dev:/data:z \
  -v "$PWD/target/image-bin/players:/opt/pawa/bin/players:ro,z" \
  pawa-pipeline:latest \
  unit capture --manifest /data/dev.json --episode-id dev-<name> --out /data/out
```

The capture lands in `/tmp/dev/out/work/dev-<name>/capture/`. Exit code 0 means the unit
completed; the status is in `out/status/dev-<name>/capture.json`.

For several episodes at once, run the HyperQueue head in capture-only mode
(`PAWA_PLAN_STAGE=capture`, see [distributed.md](distributed.md)); captures land in the same
`work/` layout.

## Behavioral QC

```sh
docker run --rm -v /tmp/dev:/data:z pawa-pipeline:latest \
  qc-actions --capture-dir /data/out/work/dev-<name>/capture
```

(`cargo run --release -p pawa -- qc-actions --capture-dir ...` on the host works the same.) The scenario
name and tick rate are read from `capture.json`; `--scenario` overrides the name, which selects
the threshold tier. The command prints per-player metrics and every violation, then `PASS` (exit 0)
or `FAIL` (exit 3).

The checks live in `crates/align/src/qc.rs` (`check_episode`, `Thresholds`) and read only the
per-tick action rows: key bits, camera deltas and positions. Defaults:

| Category | Check | Fails when |
|---|---|---|
| Inactivity | moving frames | fewer than 200 frames with a movement key held |
| | idle fraction | more than 75% of frames with no movement key and no camera motion |
| | longest idle stretch | more than 20 s |
| | frozen | more than 20 s in one stretch with under 0.5 blocks of net 3D displacement over the trailing second and no discrete action (placing, mining, attacking, using). Vertical displacement uses the rolling floor, so climbing counts and jumping in place does not. |
| Ineffective movement | stuck | more than 5 s total of movement keys held through frozen stretches of 5 s or longer (walking into a wall or tree) |
| Abrupt control changes | key chatter | more than 35% of movement-key presses shorter than 10 ticks (0.5 s), once a player has at least 20 presses |
| Predictable controls | sprint | more than 95% of sustained (at least 1 s) forward runs agree on sprint, once there are at least 20 |
| | camera/travel | more than 95% of turning-while-moving frames agree on turning toward (or away from) the travel direction, once there are at least 100 |
| Near-clones | pair agreement | player pairs agree on more than 80% of action tuples over frames where either is active |

Keys that no player pressed are reported as `dead_keys` but do not fail by default.

Thresholds are tiered by catalog metadata. `world_edits` scenarios relax the three key-based
inactivity bounds (50 moving frames, 90% idle, 90 s idle stretch) because building in place is
legitimate; `pursuit` disables the camera/travel check (watching a target while moving is the
point); `combat` allows 65% short presses. The displacement-based limits (frozen, stuck) are never
relaxed.

Finalize runs the same `check_episode` with the same thresholds, plus two checks that
`qc-actions` cannot run because they need rendered or trimmed output:

- **Darkness.** The default-projection video is sampled at 2 frames/s; a frame is near-black when
  its mean luma is under 24/255. Fails when more than 25% of samples are dark or a dark stretch
  exceeds 20 s.
- **Time base.** `pov_anchor_spread_ticks` above 2 (see [time-alignment.md](time-alignment.md)).

`ops/surface_audit.py --local-dir <out>/dataset` adds one more detector over finalized episodes:
players stuck underground or facing a wall, from the height signal in `frames.jsonl` and
(with `--video`) the contrast of the POV video.

A `qc-actions` PASS therefore does not guarantee `verdict: pass` in `qc.json`. The finalize verdict
is a soft label: the episode ships either way and the failures are listed in `qc.json`.

## Render and review

Render a passing capture by running the full pipeline on the manifest (for example
`just smoke ops/<file>.json`, or the HyperQueue launchers in
[distributed.md](distributed.md)); episodes whose capture already completed can be rendered with
`PAWA_PLAN_STAGE=render` over the same output directory. Add both projections to the manifest when
reviewing the panorama.

Set `"hud": true` under `render` to also produce `<pov>-hud.mp4`, the POV video with the recorded
keys and camera deltas drawn on top; `pawa hud --video <pov>-default.mp4 --frames
<pov>-frames.jsonl --out hud.mp4` does the same for a single finalized POV. Then review sampled
videos against the scenario description and iterate.
