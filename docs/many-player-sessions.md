# Many-player sessions

Capture runs without graphics clients, so a session's population is bounded by the server and the
player runtime, not by rendering. This document describes the scenario and camera recipe used for
64- and 128-player sessions. Example manifests: `ops/regional-showcase-n{4,64,128}.json`.

## `regional_capacity_traffic`

A showcase scenario for many players spread over a shared area. It is catalog generation 0, so
`ops/gen_manifest.py` never selects it; request it explicitly by manifest. It requires
`scenery: none` (natural terrain).

- **Grid.** A square grid of 18-block cells grows with the population, initially four players per
  populated cell, minimum 2 x 2. N=64 uses 4 x 4 cells; N=128 uses 6 x 6, including four initially
  empty cells. The grid is anchored to the located biome.
- **Capacity.** Each cell admits at most six resident or incoming claimants. An episode-local mutex
  makes the capacity check and slot reservation atomic; no lock is held across navigation. Each
  region has nine distinct addresses for at most six claimants, and every player holds a distinct
  address.
- **Transfers.** Players move to adjacent cells, so traffic flows across the grid without a central
  rendezvous or long crossings. Local visits and randomized durations avoid synchronized mass
  departures. A traveller keeps both its source and destination claims while moving; measured
  entry into the destination releases the source, staying in the source cancels the destination,
  and an ambiguous position keeps both. A timer never releases a live reservation.
- **Routing.** Pathfinding is restricted to the source/destination rectangle plus a safety margin.
  After two stationary failures, a short inward recovery picks a heading with four blocks of
  clearance and stops after 2.5 blocks or 0.8 s, with both claims still held.
- **Staging.** Initial placement resolves the surface height and verifies solid natural ground
  (dirt, stone, sand, gravel), retrying inside the same cell rather than leaving a player on a tree
  canopy, and keeps four blocks between players. All staging happens before recording: gameplay
  contains no scripted teleport and no terrain replacement.

The reservation ledger bounds addressed occupants, but collisions, knockback and pathfinder detours
can still move bodies out of their regions, so occupancy is measured from telemetry after capture.

## Recording distant terrain

A replay contains only the chunks its player received, so distant terrain must be requested at
capture time. `ops/regional-wide-capture.json` (pass it via `PAWA_CAPTURE_CONFIG`, together with a
larger `SERVER_XMX` such as `16G`) sets a 24-chunk view distance on the server and forwards it in
each player's initial client-information packet; raising only the server setting leaves clients at
the default 8, and raising only the renderer setting cannot recover chunks that were never
recorded. Simulation distance stays 8. The preset also adds setup margin to the player watchdog,
since a 128-player, 24-chunk join plus staging otherwise consumes most of it.

## Free camera

`ops/regional_cinematic_camera.py` authors an observer (free camera) path from a finished capture:
it pushes in, trucks around the players and pulls out over a 180-degree sweep, with a height floor
derived from the measured staging positions.

```sh
python3 ops/audit_regional_traffic.py CAPTURE_DIR --out regions.json
python3 ops/regional_cinematic_camera.py --capture CAPTURE_DIR --audit regions.json \
  --manifest ops/regional-showcase-n64.json --out cinematic-n64.json
```

The output manifest carries dense keyframes and sets `render.render_distance_chunks` to 24; render
the same capture with it (`--stage render`). The camera path is map-specific and its report gives
planned length and height only, so rendered frames must still be inspected. The renderer pins the
chunk radius in each lane's options (Minecraft 26.2 applies its graphics preset after loading
options, so an explicit radius also pins `graphicsPreset` to `custom`), and a render whose saved
radius differs from the requested one fails.

Geometry and timeline checks:

```sh
python3 -m unittest discover -s ops -p test_regional_cinematic_camera.py
```

## Validation

1. Capture (CPU only), one large session at a time.
2. `pawa qc-actions --capture-dir <capture>` for every player, with default thresholds.
3. `ops/audit_regional_traffic.py` on the common world clock: claims, measured occupancy, grid
   departures and cross-region movement; it also exports 1 Hz trajectories.
4. Render the observer and sampled player views on GPUs and review time-distributed frames with a
   VLM for dispersal, visible motion, terrain interaction, clustering and rendering defects;
   `ops/regional_review_sheet.py` builds timestamped contact sheets from finalized videos for this.
   A good first frame is not evidence for the rest of the clip.

Results on world seed 4103 and scenario seed 2141:

| Players | Action QC | Common telemetry | Players that crossed regions | Max claims / measured occupants per cell | Overfull ticks / outside-grid frames |
|---|---|---|---|---|---|
| 4 | 4/4 pass | 121.85 s | 4/4 | 4 / 4 | 0 / 0 |
| 64 | 64/64 pass | 61.50 s | 64/64 | 6 / 6 | 0 / 0 |
| 128 | 128/128 pass | 61.55 s | 128/128 | 6 / 6 | 0 / 0 |

With the 24-chunk capture and the free camera, a 64-player session rendered 91.5 s of aligned
observer video and a 128-player session 85.4 s, both with finalize QC passing and zero POV anchor
spread. These establish the listed examples, not success on every seed or terrain.
