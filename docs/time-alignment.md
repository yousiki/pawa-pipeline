# Time alignment

This document answers one question: given an episode directory, how do frames from different
views line up in world time. File formats are in [dataset-spec.md](dataset-spec.md); this page is
only about clocks.

Summary: every stream is mapped to the server's world-age ticks and cropped to the common interval
covered by the actions of all retained players, and a three-tick display-phase correction aligns
actions with the rendered frames. Row `i` of every file in an episode is the same world tick.

## The authoritative clock

Every row of every `<pov>-frames.jsonl` carries `worldAgeTicks`, the Minecraft server's world age
(20 ticks per second). All views of an episode are recorded against the same server, so this is one
clock for the whole episode.

The episode frame rate equals the tick rate (`fps` = `tick_rate_hz` = 20), so one row is one video
frame is one tick. Within a file, rows are contiguous and ticks are consecutive:

```
worldAgeTicks(row i) = worldAgeTicks(row 0) + i
```

Finalize crops all files to the same world-tick interval (see
[Common window](dataset-spec.md#common-window)), so row 0 of every file has the same `worldAgeTicks`.
Pairing views is therefore a matter of pairing frame indices. Joining rows on equal `worldAgeTicks`
gives the same result and is the safest generic form.

## How ticks are assigned to frames

The mapping is computed during finalize and baked into the rows; consumers do not need to redo it.
To audit it, or to read the raw action streams under `diagnostics/`, the join is:

```
world_age_tick = replay_tick + display_delta
display_delta  = delta - render_phase_ticks
```

`replay_tick` is the frame index of the untrimmed render (ticks since the replay recording
started). Both parameters are stored per player POV in `qc.json`:

- `povs.<pov>.alignment.delta_world_age_minus_replay_tick`: `delta`, the world-age tick minus the
  replay tick at the start of that player's recording. It is chosen from several anchors, in order
  of preference: the world age at which the player's recording stopped matched against the last
  rendered frame (`stop_anchored_frames`), the world age when recording started
  (`recording_start_world_age`), and the world age when the episode's go signal fired
  (`go_world_age`). The winning anchor is `alignment.anchor`; the rejected candidates are listed only
  in `alignment.candidate_deltas`.
- `povs.<pov>.alignment.render_phase_ticks`: the display-phase correction, 3 ticks.

### Display-phase correction

A rendered frame shows the world slightly after the action row with the same tick: the input goes
from client to server and back into the recording before it becomes visible. The lag was measured
from isolated camera-turn events, comparing the frame where central-region pixels change most with
the tick of the action row. It was never below 3 ticks and occasionally reached 7. The pipeline
subtracts the lower bound, 3 ticks, so the action always precedes its visible effect and the
remaining lag is 0 to 4 frames in most cases. Over-correcting would put effects before their causes,
so the correction is deliberately not larger.

## Pairing frames across views

```python
import json
from pathlib import Path

def rows(path):
    return [json.loads(line) for line in Path(path).read_text().splitlines() if line.strip()]

ep = Path("20260101T000000Z-pair_chase-4p-0a1b2c3d")
povs = {f.name.split("-")[0]: rows(f) for f in sorted(ep.glob("p*-frames.jsonl"))}

# Row i of every POV is the same world tick.
for i in range(min(len(r) for r in povs.values())):
    frames = {pov: r[i] for pov, r in povs.items()}
    assert len({row["worldAgeTicks"] for row in frames.values()}) == 1

# Equivalent join on the world clock.
by_tick = {pov: {row["worldAgeTicks"]: row for row in r} for pov, r in povs.items()}
shared = sorted(set.intersection(*(set(m) for m in by_tick.values())))
for tick in shared:
    frames = {pov: by_tick[pov][tick] for pov in by_tick}
    # frames["p00"]["frame_count"] is the frame index of this tick in p00-default.mp4 and
    # p00-equirectangular.mp4; likewise for every other POV.
```

Observer files (`obsNN-frames.jsonl`) carry `worldAgeTicks` and `frame_count` too and are paired
the same way; their rows have no player-state columns.

Discrete events in `events.jsonl` carry `world_age_tick`; the frame an event lands on is the row of
the acting POV with that `worldAgeTicks`.

## Wall-clock columns

`renderTime` (ms), `epochTime` (s) and `relativeTimeMs` (ms) exist for Solaris compatibility. They
are not sampled system time; they are derived from the frame grid:

```
relativeTimeMs(row i) = round(i * 1000 / fps)        # round half to even; 50 * i at 20 fps
renderTime(row i)     = started_epoch_ms + relativeTimeMs(row i)
epochTime(row i)      = renderTime(row i) / 1000
```

`started_epoch_ms` is stored in `episode.json` and is the same for every file of the episode,
because every file's row 0 is the same world tick. Hence:

- equal `renderTime` means equal frame index means the same world tick, across all views;
- `relativeTimeMs` starts at 0 in every file, as in Solaris;
- aligning players by wall clock, as a Solaris loader does, gives exactly the same pairing as aligning
  by `worldAgeTicks`.

## `pov_anchor_spread_ticks`

`qc.json` records `pov_anchor_spread_ticks`: the maximum minus the minimum of the first shipped
`worldAgeTicks` across player POVs. The tick-space crop makes it 0 by construction. A non-zero
value would mean files start on different ticks, which is what cropping by frame index would
produce: players are recorded by separate processes whose recordings start several ticks apart, so
equal frame indices would not be equal ticks even though each POV's own alignment is correct.

A spread above 2 ticks is recorded as a failure in `qc.json` `failures` and sets `verdict` to
`fail`, but `verdict` is a soft label and such an episode is still written. A consumer that relies
on frame indices being comparable across views should check `pov_anchor_spread_ticks == 0`
directly rather than `verdict == "pass"`.

## Comparison with Solaris timing

| | solaris-engine | this dataset |
|---|---|---|
| Cross-player alignment | host wall clock, approximate | `worldAgeTicks`, exact to the tick |
| Wall-clock columns | sampled system time (jitter) | linear function of the world clock |
| Equal wall clock across players | approximately the same moment | exactly the same tick |
| Equal frame index across players | no guarantee | the same tick |
| Action-to-frame phase | not characterized | 3-tick correction, residual lag 0 to 4 frames |
| Episode-level time-base check | none | `pov_anchor_spread_ticks` |

Differences in behavior distributions (camera rate, key combinations) are listed in
[dataset-spec.md](dataset-spec.md#compatibility-with-the-solaris-format).
