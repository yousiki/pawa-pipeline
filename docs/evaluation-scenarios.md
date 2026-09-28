# Evaluation scenarios

Most training scenarios turn the camera away from what it has seen and rarely turn back. For
training that is useful signal, but it makes pixel metrics hard to interpret: when the ground truth
shows a region that was never in the context frames, a model is scored against content it had no
way to know. The evaluation family isolates the opposite case. Its scripts point the camera at
something, turn away past the field of view, and turn back, so the frames being scored show regions
that were observed before.

These scenarios have catalog family `evaluation` and names starting with `eval_`. They are for
evaluation only and must never be mixed into a training manifest (see [Isolation](#isolation)).

## The scenarios

| Scenario | `min_players` | Script | What it evaluates |
|---|---|---|---|
| `eval_grounding_turn_back` | 1 | Build a landmark, face it, turn away by 120 or 180 degrees (alternating magnitude and direction), turn back, repeat. | Reconstruction of an observed region. The frames after the return show the landmark view again, so a pixel comparison with the ground truth is meaningful. |
| `eval_memory_dwell` | 2 | Face a shared landmark, turn away by 150 degrees, stay away for 1.5, 3, 6 or 12 s (assigned per player), turn back. | Memory decay: the dwell time is the independent variable, and one episode yields several points of the curve. |
| `eval_shared_unobserved` | 2 | All players open on a shared subject, then turn toward the same distant, previously unseen region (48 blocks away) from different positions. | Cross-view consistency without ground truth: what appears is unpredictable, but the generated views of different players must agree with each other. |

`eval_memory_dwell` and `eval_shared_unobserved` are `single_process` (not sharded). All three
have `world_edits` set because they build their landmark.

## Design constraints

- **Turns exceed the horizontal field of view.** The POV camera has a 70 degree vertical FOV, about
  102.5 degrees horizontally at 640x360. Turning further than that leaves no pixel overlap between
  the away view and the landmark view; a smaller turn would leave a partial overlap that blurs the
  contrast.
- **Displacement stays small.** While holding a view the player walks short (4-block) shuttle legs
  instead of standing still, so camera motion dominates while translation stays bounded. Standing
  still is not an option because the behavioral QC fails players that do not move (see
  [scenario-authoring.md](scenario-authoring.md#behavioral-qc)); legs of this length also stay
  above the key-chatter threshold.
- **Players do not move in lockstep.** Each player enters its cycle with its own phase or delay, so
  QC does not flag them as near-clones and the views are not duplicates.
- **Shared subject.** In the multi-player scenarios every player adds a pillar to one cluster at
  the episode anchor, so every POV opens on the same subject. While one player is turned away the
  others may keep editing the cluster; `witness_peer_edits` records those edits in `events.jsonl`,
  so a subject that genuinely changed can be told apart from a failed reconstruction.
- **The first view is confirmed.** Turns use `look_until_facing`, and each return is recorded as an
  `observe` event with whether the camera actually arrived, so a turn that timed out is not
  mistaken for a completed return.

### Real terrain, not Superflat

These scenarios run on normal terrain (`minecraft:normal`, the manifest default). On a Superflat
world there is no occlusion, no landmark and every direction looks the same, so a model can produce
a plausible frame after a return without remembering anything. That removes exactly the problem the
family is meant to measure. What makes a return scorable is that the region was observed, not that
it lacks detail. The scripts still build a small stone-brick landmark as an anchor for the return.

## Isolation

Three independent gates keep these scenarios out of training data:

1. The catalog entry in `crates/schema/src/catalog.rs` has family `evaluation`.
2. `ops/gen_manifest.py` excludes the family by default. With `--evaluation` it emits only
   evaluation scenarios, never a mix.
3. Evaluation manifests use their own run id and output directory, never shared with a training
   run.

Orchestration differences, such as a tighter spawn spread, are set per episode in the evaluation
manifest (`swarm.spread_range`, e.g. `ops/gen_manifest.py --evaluation --spread-range 8`), never by
changing global defaults, which would change the behavior of every other scenario.

## Frustum audit

`ops/frustum_audit.py` measures, over fixed-length windows of a finalized dataset, how much of what
the camera shows was already visible in the window's first frame. It is read-only and reads only
`episode.json` and the player `frames.jsonl` files.

```sh
python3 ops/frustum_audit.py --self-check
python3 ops/frustum_audit.py --local-dir <out>/dataset --json-out frustum.json
```

Options: `--window` (frames, default 93), `--sample-per-scenario` (episodes per scenario, 0 for all,
default 2), `--scenario` (audit one scenario). `--hf-repo <repo> [--revision <rev>]` streams the
same files from a Hugging Face dataset repository instead of a local directory.

Per window it reports:

| Metric | Definition |
|---|---|
| `sweep_over_fov` | The yaw range swept within the window exceeds the horizontal FOV. |
| `zero_overlap` | The shortest angular distance between the first and last frame's yaw is at least the horizontal FOV. |
| `infov_axis` | Share of frames whose optical axis lies inside the first frame's frustum. |
| `overlap_mean` | Mean angular overlap between each frame's frustum and the first frame's frustum, including pitch. |
| `peer_first_frame` / `peer_intrudes` / `peer_absent` | Whether another player is in view in the first frame, enters the view later, or is never in view. |

It also reports yaw range and displacement distributions. Frustum overlap is computed as a
rectangle intersection on the sphere of directions; parallax from translation, disocclusion and
occlusion are ignored, so the peer figures are a lower bound on intrusion.

`--self-check` runs synthetic trajectories that pin three corrections to naive definitions:

- A large sweep does not imply zero overlap: swinging 60 degrees left and right (a 120 degree
  range) and returning ends with full overlap.
- Net rotation must use the shortest angular distance: a 350 degree turn is a 10 degree offset and
  still overlaps.
- The optical-axis criterion overestimates coverage: an axis 40 degrees off center is still "in
  view", while the actual frustum overlap is only about 61%.
