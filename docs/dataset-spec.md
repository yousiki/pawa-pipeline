# Dataset specification

This document describes what one finalized episode contains: the directory layout and every file
and field a consumer reads. It covers dataset schema version 3 (`schema_version` in
`episode.json`).

The type definitions in `crates/schema` are authoritative (`dataset.rs` for everything shipped,
`time.rs` for the clock newtypes). Every field carries a doc comment there, and the same
definitions are exported as JSON Schema:

```sh
just schemas   # cargo run --release -p pawa -- schema-export --out out/schema
```

For how the clocks relate and how to pair frames across players, see
[time-alignment.md](time-alignment.md).

## Output tree

A run writes everything under one output root (`--out`):

```
<out>/
  work/<episode_id>/capture/          raw capture artifacts (per-player actions, events, .mcpr replays)
  work/<episode_id>/render/<pov>/     one render's products and its render record
  dataset/<episode dir>/              the finalized episode (below)
  status/<episode_id>/*.json          one status file per unit (capture, <pov>.render, finalize, hud)
  runs/<run_id>.json                  run report
  logs/                               task stdout/stderr
```

`work/` is staging. Finalize moves the dataset files out of it and parks everything left over under
the episode's `diagnostics/` directory. `dataset/` is the deliverable.

## Episode directory

The directory name is `<UTC start %Y%m%dT%H%M%SZ>-<scenario>-<N>p-<hash8>`, where `N` is the player
count and `hash8` is the first 8 hex digits of the sha256 of the episode's `capture.json`. The
capture record is written once, so a finalize retry reproduces the same name. The run-unique string
episode id is kept in `episode.json`.

```
<ts>-<scenario>-<N>p-<hash8>/
  episode.json                       episode identity, constants and POV inventory
  qc.json                            alignment evidence and quality metrics
  events.jsonl                       discrete scripted actions, all players, on the world-age clock
  p00-default.mp4                    rectilinear POV video
  p00-equirectangular.mp4            equirectangular panorama from the same camera origin
  p00-frames.jsonl                   one row per video frame, alignment pre-joined
  p00-episode_info.json              Solaris-style per-player episode record
  p00-default-depth/                 optional depth sequence (render.depth)
  p00-equirect-depth/                optional depth sequence (render.depth)
  p00-hud.mp4                        optional action-HUD review video (render.hud)
  p01-...
  obs00-default.mp4                  observer (free camera) views, same file grammar
  obs00-equirectangular.mp4
  obs00-frames.jsonl
  obs00-episode_info.json
  diagnostics/                       everything else; never needed for training
```

Every per-view file is `<pov>-<suffix>`. Player POVs are named `p00`, `p01`, ... (three digits,
`p000`, `p001`, ..., when the episode has more than 100 players). Observers take the names declared in the run
manifest (`render.observers[].name`, e.g. `obs00`). Videos are `<pov>-<projection>.mp4`, where the
manifest projection `equirect` is written out as `equirectangular`; other projection names declared
in the manifest appear as is. Depth directories are `<pov>-<projection>-depth/` and use the
manifest projection name unchanged.

Default media: the `default` projection is 640x360 H.264, the `equirect` projection 1024x512 H.264
with spherical metadata, both at 20 fps. The frame rate equals the server tick rate (20 Hz), so one
frame is one tick.

### Common window

All files of an episode cover the same interval of world time. Finalize takes, for each player POV,
the first and last frame whose row has a matched action, intersects those spans in `worldAgeTicks`
space across all player POVs, and converts the common tick span back into each POV's own frame
window. Videos are re-encoded frame-exactly with the run's encoder settings, depth sequences are
cut to the same frames, and rows are renumbered from 0.

Consequences:

- Frame `i` of every file in the episode shows the same world tick. Frame indices are directly
  comparable across POVs.
- Every shipped player row carries the full player-state payload. A missing action row inside the
  window is a capture defect and fails the episode instead of shipping a partial row.
- The first row of each file has `action.camera = [0, 0]` and no `hotbar.N` edge, because the
  previous row it would be differenced against was trimmed away.

Observers are cut to the same tick span. An observer that does not cover it is marked failed; that
never fails the episode.

## `episode.json`

| Field | Type | Meaning |
|---|---|---|
| `schema_version` | int | Dataset schema version, currently `3`. |
| `episode_id` | string | Run-unique episode id from the run manifest. |
| `run_id` | string | Run (batch) the episode was captured in. |
| `scenario` | object | `name` (catalog scenario), `family` (behavior family, e.g. `group_motion`), `seed`, `scenery` (scenery generator, `none` for untouched terrain). |
| `mc_version` | string | Minecraft version of the server and render client, e.g. `"26.2"`. |
| `runtime` | object | `family` (e.g. `"azalea"`), `topology` (process layout, e.g. `"single_process_multi_player"`), `azalea_version`. |
| `tick_rate_hz` | int | Server tick rate all tick clocks count at. |
| `fps` | int | Frame rate of every video and `frames.jsonl` in the episode. |
| `started_epoch_ms` | int | Wall-clock base of the trimmed episode; equals `renderTime` of row 0 in every complete file. |
| `target_duration_ms` | int | Duration requested by the manifest. |
| `actual_duration_ms` | int | Duration of the shipped common window. |
| `world` | object | `seed` (string, as given to the server) and `level_type` (e.g. `"minecraft:normal"`). |
| `povs` | array | Every planned view, players and observers, including failed ones. |

Each `povs[]` entry:

| Field | Meaning |
|---|---|
| `name` | File prefix (`p00`, `obs00`, ...). |
| `kind` | `player` or `observer`. |
| `status` | `complete`; `skipped_no_connect` (the player never joined the server, nothing was recorded, the rest of the episode is intact); or `failed` (details in `diagnostics/`). |
| `username` | In-game name. Players only. |
| `source_pov` | Observers only: the player whose replay the observer was rendered from. It bounds what the observer can see, since a replay only contains chunks that player received. |
| `videos` | Map from projection name (`default`, `equirect`, ...) to media info. Empty unless `complete`. |
| `depth` | Map from projection name to depth info. Empty unless depth export was enabled. |

Media info: `path` (relative to the episode directory), `sha256`, `container`, `codec`, `width`,
`height`, `fps`, `frame_count` (total frames), `duration_ms`, `spherical` (true when the file
carries equirectangular projection metadata).

Depth info: `path` (directory relative to the episode directory), `width`, `height`, `frame_count`,
`format` (`"float32_bgra_png"`). The directory holds `0.depth.png`, `1.depth.png`, ..., one per video
frame. Each PNG stores ReplayMod's float32 depth value in its four BGRA bytes; it is not a
viewable grayscale image and must be decoded before use.

A failed player POV fails the episode, so in a shipped episode every player POV is `complete` or
`skipped_no_connect`. Only observers can appear as `failed`.

## `<pov>-frames.jsonl`

One JSON object per line, one line per video frame: line `i` describes frame `i` of every video of
that POV. The columns fall into two groups: the Solaris row vocabulary, kept with identical names,
units and conventions, and superset columns that Solaris does not have.

### Solaris columns

| Field | Type | Meaning |
|---|---|---|
| `x`, `y`, `z` | float | Feet position in world blocks, full precision (not rounded). |
| `yaw`, `pitch` | float | Body orientation in **radians, mineflayer convention** (see below). Pitch is positive looking up. |
| `action` | object | Control state at this frame's tick (below). |
| `inventory` | object | Full inventory snapshot (below). |
| `renderTime` | int | Synthetic wall clock, ms since the Unix epoch: `started_epoch_ms + relativeTimeMs`. |
| `epochTime` | float | `renderTime` in seconds. |
| `relativeTimeMs` | int | Milliseconds since the first frame on the constant-rate grid: `round(i * 1000 / fps)`. |
| `physicsTime` | int | Player-runtime monotonic clock (ms) of the matched action row. |
| `worldAgeTicks` | int | The frame's world-age tick. The authoritative clock shared by all POVs. |
| `isInWater` | bool | Whether the player is in water. |
| `frame_count` | int | 0-based frame index of this row (not a total). |
| `episode_id` | int | Integer sequence number of the episode within its run manifest. The string id is in `episode.json`. |
| `extra_info` | object | `{"seed": <int>}`: the world seed as an integer (`0` if the seed is not numeric; the exact string is `episode.json` `world.seed`). |

`action` object:

| Key | Semantics |
|---|---|
| `forward`, `back`, `left`, `right`, `jump`, `sprint`, `sneak` | Held keys (continuous state). |
| `camera` | `[d_yaw, d_pitch]`: orientation change since the previous row, radians, mineflayer convention. `[0, 0]` on the first row. |
| `attack`, `use`, `place_block` | One-shot edge events. |
| `mine` | Continuous state: true on every tick a block is being broken. |
| `hotbar.1` ... `hotbar.9` | Hotbar selection edges: `hotbar.N` is true only on the row where the selected slot changed to `N-1`. The current slot is `inventory.quickBarSlot`. |
| `mount`, `dismount`, `place_entity` | Always false. No scenario rides or places entities; the keys exist for format compatibility. |

`inventory` object, grouped like the mineflayer player window:

| Key | Meaning |
|---|---|
| `quickBarSlot` | Selected hotbar slot, 0-8. |
| `mainInventory` | Non-empty stacks in window slots 9-35. |
| `hotbar` | Non-empty stacks in window slots 36-44. |
| `heldItem` | Stack in slot `quickBarSlot + 36`, or `null`. |
| `offHandItem` | Stack in slot 45, or `null`. |

Each item stack has the prismarine-item serialization shape: `type` (protocol registry id),
`count`, `metadata` (always 0), `nbt` (`null`), `stackId` (`null`), `name` (registry name without
`minecraft:`), `displayName`, `stackSize`, `slot` (window slot index).

### Superset columns

| Field | Meaning |
|---|---|
| `camera_pose` | `{x, y, z, yaw, pitch, roll, fov_deg}`: the absolute pose of the camera this frame was actually rendered from, taken from the render driver's pose trace. Angles in **degrees, Notchian (vanilla Minecraft) convention**. For a player POV this is the spectator camera at the player's eyes; for an observer it is the planned free camera. `roll` is 0 except for banked observer paths. `fov_deg` is the Minecraft FOV setting of the render client, pinned to 70. Minecraft applies it vertically for landscape frames, which gives about 102.5 degrees horizontally at 640x360. Absent only where the pose trace has a gap. |
| `server_tick` | Server tick of the matched action row, as the client observed it. |
| `held_item` | Item name in the selected hotbar slot, if any (flat copy of `inventory.heldItem.name`). |

### Angle conventions

Two conventions are shipped on purpose. `yaw`, `pitch` and `action.camera` follow mineflayer
(radians); `camera_pose` follows vanilla Minecraft (degrees). The conversion changes both unit and
convention (mineflayer `lib/conversions.js`):

```
yaw_mineflayer   = (pi - radians(yaw_notchian)) mod 2*pi
pitch_mineflayer = -radians(pitch_notchian)
```

Notchian yaw 0 (facing +Z, south) is mineflayer `pi`; Notchian 180 (north) is mineflayer `0`. The
signs of yaw and pitch deltas flip: turning right is a positive Notchian delta and a negative
`action.camera[0]`. Converting only the unit gives wrong values that still look plausible.

### Observer rows

Observer files use the same row type without player state: `x`, `y`, `z`, `yaw`, `pitch`,
`action`, `inventory`, `physicsTime`, `isInWater`, `server_tick` and `held_item` are absent. The
clock columns, `frame_count`, `episode_id`, `extra_info` and `camera_pose` are present. A loader
that iterates `episode.json` `povs` can treat both kinds uniformly.

## `events.jsonl`

Discrete scripted actions of all players, ordered by world tick (ties by POV, then event id). One
object per line:

| Field | Meaning |
|---|---|
| `seq` | 0-based sequence number, a total order over the episode. |
| `event_id` | Event id assigned by the scenario. |
| `pov` | Acting player's POV name. |
| `action_type` | Scenario-defined type (`place_block`, `attack_peer`, `observe`, ...). Open vocabulary. |
| `world_age_tick` | Event time on the world-age clock. Find the frame by matching `worldAgeTicks` in the acting POV's `frames.jsonl`. |
| `server_tick` | Server tick of the event. |
| `description` | Human-readable summary. |
| other keys | Scenario-specific payload, passed through unchanged. |

Events are not trimmed to the common window, so some may fall before the first or after the last
shipped frame. Events without a world-age tick cannot be placed on the shared clock; their count is
written to `diagnostics/unplaced-events.json`.

## `<pov>-episode_info.json`

One per complete POV, in the shape of the Solaris per-bot episode record:

| Field | Value |
|---|---|
| `timestamp` | Start of the shipped window, RFC 3339 UTC. |
| `bot_name` | Player username; the POV name for an observer. |
| `episode_number` | Same integer as the rows' `episode_id`. |
| `episode_type` | Scenario name. |
| `instance_id` | String episode id. |
| `encountered_error` | Always `false` (a POV that failed gets no files). |
| `peer_encountered_error` | `true` when another POV of the episode is `failed` (in practice an observer). |
| `bot_died` | Always `false`. |
| `episode_recording_started` | Always `true`. |
| `eval_metadata` | `{}`. |

## `qc.json`

Quality evidence for humans and corpus sweeps; training loaders do not need it.

| Field | Meaning |
|---|---|
| `verdict` | `pass` or `fail`. |
| `failures` | Descriptions of every failed check. Empty when `verdict` is `pass`. |
| `dead_keys` | Movement keys no player pressed in this episode. Reported only. |
| `pair_agreement_active` | Mean action-tuple agreement between player pairs over frames where either is active (1.0 = clones). |
| `camera_travel_agreement` | Pooled share of turning-while-moving frames that agree on whether the camera turned toward the travel direction (predictability of the camera from the path). |
| `pov_anchor_spread_ticks` | Max minus min of the first shipped `worldAgeTicks` across player POVs. 0 when all files start on the same tick. |
| `separation` | Crowd-spacing statistics over the swarm (crowded/overlapping frame counts and ratios, nearest-neighbor distance min/median/p05/p95). |
| `povs` | Per player POV: `alignment`, `degeneracy`, `motion`. Observers are not listed. |

`verdict` is a soft label. Finalize ships the episode either way and records the reason in
`failures`. What fails an episode outright (no `dataset/` directory is written) is a failed player
render, an unusable or invalid clock alignment, an empty common window, or an incomplete row inside
it. The checks behind `verdict` are described in
[scenario-authoring.md](scenario-authoring.md#behavioral-qc).

`alignment` (how the POV was joined to the world-age clock):

| Field | Meaning |
|---|---|
| `status` | `applied` or `failed`. |
| `anchor` | Winning anchor: `stop_anchored_frames` (preferred), `recording_start_world_age`, or `go_world_age`. |
| `delta_world_age_minus_replay_tick` | Applied delta, `world_age_tick - replay_tick` at the recording start. |
| `render_phase_ticks` | Display-phase correction subtracted when mapping ticks onto frames (3). |
| `candidate_deltas` | Every anchor candidate considered, `null` when unavailable. Diagnostic only; the rows already use the winning one. |
| `actions_matched`, `actions_unavailable`, `dropped_trailing_count` | Join statistics. |
| `validation_tolerance_frames` | Tolerance allowed between expected and observed events during validation. |

`degeneracy` holds the behavioral QC measurements (`frames`, `idle_frac`, `longest_idle_s`,
`moving`, `short_run_frac`, `forward_runs_sustained`, `forward_runs_sustained_sprint`,
`camera_frames`, `camera_travel_agreement`, `frozen_frac`, `longest_frozen_s`, `stuck_total_s`,
`dark_frame_frac`, `longest_dark_run_s`). `motion` holds movement coverage computed by the capture
runtime (`dense_frames`, `idle_frame_ratio`, `moving_frame_ratio`, `low_motion`,
`move_run_median_ticks`, `move_runs_under_200ms_frac`, `path_length_2d`, `xz_span`,
`yaw_coverage_rad`, `pitch_coverage_rad`).

## `diagnostics/`

Everything else the pipeline produced for the episode: raw capture streams, replay files, render
logs, per-POV alignment event reports. It is kept in full for debugging and can be deleted as a
whole without affecting any file described above.

## Dataset census

`ops/census_dataset.py` summarizes a finalized dataset read-only: episodes and POVs, player-count
and scenario distributions, durations, frame and event rows, file sizes, projections and QC
metrics. It reads a local output tree or a Hugging Face dataset repo without downloading media:

```sh
python3 ops/census_dataset.py --local-dir <out>/dataset --json-out census.json
python3 ops/census_dataset.py --hf-repo <owner>/<dataset> --revision main
```

## Compatibility with the Solaris format

The dataset is a drop-in superset of the dataset produced by
[solaris-engine](https://github.com/solaris-wm/solaris-engine). Every Solaris row field is present
with the same name, unit, convention and value semantics; the additions are the panoramic
(equirectangular) video, the optional depth and observer views, and the extra row fields
`camera_pose`, `server_tick` and `held_item`, whose names do not collide with Solaris keys.

Directory layout. Solaris writes a flat directory for a fixed pair of bots:
`<episode:06d>_<Bot>_instance_<iii>.mp4`, a JSON array of rows next to it, and an
`..._episode_info.json` per bot, and its loader pairs files by name. That layout cannot hold N
players or extra videos per player. This dataset uses one directory per episode with `pNN-`
prefixed files. A Solaris loader only needs its file discovery changed:

- glob `p*-default.mp4` inside each episode directory instead of the flat file pattern;
- read `pNN-frames.jsonl` line by line instead of one JSON array;
- read `pNN-episode_info.json` as before.

Row fields map one to one:

| Solaris | This dataset |
|---|---|
| `x, y, z`, `yaw, pitch` (radians, mineflayer) | identical |
| `action.*` incl. `camera`, `hotbar.N` edges, continuous `mine` | identical; `mount`, `dismount`, `place_entity` constant false |
| `inventory` | identical shape |
| `renderTime`, `epochTime`, `relativeTimeMs` | identical units; derived from the world clock rather than sampled (see [time-alignment.md](time-alignment.md)) |
| `physicsTime` | player-runtime monotonic clock |
| `worldAgeTicks`, `isInWater`, `frame_count`, `episode_id` | identical |
| `extra_info.seed` | the real world seed (Solaris writes a constant) |
| none | `camera_pose`, `server_tick`, `held_item` |

Known distribution differences. Field semantics match, but the players are driven differently
(Azalea players with a quantized controller instead of mineflayer), so some value distributions
differ:

- **Action-to-frame phase.** A three-tick display-phase correction is applied; the remaining lag
  between an action row and its visible effect is 0 to 4 frames, always with the action first.
  Solaris data is typically within about one frame.
- **Camera rate.** The controller turns at most 3.6 degrees (0.0628 rad) per tick, so
  `action.camera` never contains the large per-frame turns (up to about 20 degrees) found in
  Solaris data.
- **Key combinations.** Rows record the control input sent on every tick, so diagonal combinations
  such as `forward` + `left` are common; mineflayer pathfinding mostly produces plain `forward` with
  turns.
- **Alignment.** Solaris aligns players by host wall clock. Here every frame is placed on the server's
  world-age tick, and frame `i` is the same tick in every file of an episode.
