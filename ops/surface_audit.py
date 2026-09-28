#!/usr/bin/env python3
"""Find POVs whose camera spent the episode with nothing reconstructible in view.

The defect this measures was found by human review of a 2-player collection run: a bot walks
through a cave mouth or down a ravine and spends most of the episode 8-20 blocks underground,
pressed against dirt. 21 of 109 scenarios showed it, and the existing QC missed all but one of
them, because `max_dark_frame_frac` measures LUMA and a dirt wall in daylight is not dark —
those POVs measured `dark_frame_frac = 0.0`.

Two independent signals, cheap one first:

* `below_frac` — fraction of frames at least `DEPTH` blocks under the surface the episode started
  on, read from `frames.jsonl`. No video decode, so it is safe to run beside a render.
* `view_std` — median per-frame luma standard deviation of the rendered video. A wall filling the
  frame is a LOW-VARIANCE signal, which is what distinguishes it from a dim-but-open scene.
  Needs ffmpeg per POV, so it is opt-in (`--video`): measured at 6 parallel decodes it starved a
  concurrent render from 5 POV/min down to 0.2.

Neither alone convicts. Descending is legitimate in a scenario about banks or shorelines
(`terrain_bank_height` measured 85% below-surface and was judged GOOD by review; so was
`terrain_shore_flank_walk` at 48%). The defect is descending AND losing the view. Calibrated
against the 109 human labels, `view_std < 23.8` caught 18/21 BAD with 1/53 false positives on
GOOD; the three it misses sit at 26-29, so treat that band as uncertain rather than clean.

    ops/surface_audit.py --local-dir <out>/dataset
    ops/surface_audit.py --local-dir <out>/dataset --video --jobs 4
    ops/surface_audit.py --self-check
"""

import argparse
import collections
import glob
import json
import os
import re
import statistics
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor

#: Blocks below the starting surface before a frame counts as "under it". Three clears a stair
#: step and a shallow bank while still catching a cave mouth.
DEPTH = 3.0
#: Fraction of an episode's frames under the surface before the POV is called descended.
BELOW_FRAC = 0.30
#: Median luma stddev below which the view holds no reconstructible content. See the calibration
#: in the module docstring — this is the boundary between the GOOD floor (23.9) and the BAD
#: ceiling (23.7) on the labelled set, and it is deliberately not padded outward.
VIEW_STD = 23.8
#: Frames sampled per second for the video pass. The defect is measured in multi-second stretches.
VIDEO_FPS = 2
#: Luma is downscaled this small first: a wall filling the frame is low-variance at any resolution,
#: and decoding less is what keeps this from competing with a render.
VIDEO_W, VIDEO_H = 64, 36


def below_frac(frames_path):
    """Fraction of frames at least `DEPTH` under the surface this POV started on, or None."""
    ys = []
    with open(frames_path) as fh:
        for line in fh:
            try:
                ys.append(json.loads(line)["y"])
            except (json.JSONDecodeError, KeyError):
                continue
    if len(ys) < 50:
        return None
    # Median of the opening ~2 s, not the first frame: a bot can still be settling onto the ground.
    surface = statistics.median(ys[:40])
    return sum(1 for y in ys if y < surface - DEPTH) / len(ys)


def view_std(video_path):
    """Median per-frame luma stddev of a rendered POV, or None when it cannot be decoded."""
    proc = subprocess.run(
        ["ffmpeg", "-v", "error", "-i", video_path,
         "-vf", f"fps={VIDEO_FPS},scale={VIDEO_W}:{VIDEO_H}",
         "-pix_fmt", "gray", "-f", "rawvideo", "-"],
        capture_output=True,
    )
    n = VIDEO_W * VIDEO_H
    stds = [statistics.pstdev(proc.stdout[i:i + n])
            for i in range(0, len(proc.stdout) - n + 1, n)]
    return statistics.median(stds) if stds else None


def scenario_of(episode_dir):
    """`20260901T155654Z-circuit_bearing_ladder-2p-d9b204df` -> `circuit_bearing_ladder`."""
    name = re.sub(r"^\d+T\d+Z-", "", os.path.basename(episode_dir.rstrip("/")))
    return re.sub(r"-\d+p-[0-9a-f]+$", "", name)


def audit(root, with_video, jobs):
    """One row per POV: (scenario, episode, pov, below_frac, view_std or None)."""
    povs = []
    for ep in sorted(glob.glob(os.path.join(root, "*/"))):
        for frames in sorted(glob.glob(os.path.join(ep, "p*-frames.jsonl"))):
            pov = os.path.basename(frames).split("-")[0]
            povs.append((scenario_of(ep), os.path.basename(ep.rstrip("/")), pov, ep, frames))

    def one(row):
        scen, ep_name, pov, ep, frames = row
        below = below_frac(frames)
        if below is None:
            return None
        std = None
        if with_video:
            video = os.path.join(ep, f"{pov}-default.mp4")
            std = view_std(video) if os.path.isfile(video) else None
        return scen, ep_name, pov, below, std

    with ThreadPoolExecutor(max_workers=jobs) as pool:
        return [r for r in pool.map(one, povs) if r]


def report(rows, with_video):
    descended = [r for r in rows if r[3] > BELOW_FRAC]
    blind = [r for r in rows if r[4] is not None and r[4] < VIEW_STD]
    both = [r for r in rows if r[3] > BELOW_FRAC and r[4] is not None and r[4] < VIEW_STD]
    episodes = {r[1] for r in rows}

    print(f"POVs {len(rows)}   episodes {len(episodes)}")
    print(f"  descended (below_frac > {BELOW_FRAC:.0%})        : "
          f"{len(descended):4d} POVs  {len(descended) / len(rows):5.1%}")
    if with_video:
        print(f"  blind view (view_std < {VIEW_STD})         : "
              f"{len(blind):4d} POVs  {len(blind) / len(rows):5.1%}")
        print(f"  both = confirmed defect                    : "
              f"{len(both):4d} POVs  {len(both) / len(rows):5.1%}")
        bad_eps = {r[1] for r in both}
        print(f"  episodes with a confirmed-defect POV       : "
              f"{len(bad_eps):4d} eps   {len(bad_eps) / len(episodes):5.1%}")
        key, flagged = "confirmed", both
    else:
        print("  (no --video: descent is the only signal, and descent alone can be legitimate terrain)")
        key, flagged = "descended", descended

    per = collections.defaultdict(lambda: [0, 0])
    for r in rows:
        per[r[0]][1] += 1
    for r in flagged:
        per[r[0]][0] += 1
    worst = sorted(((a / t, s, a, t) for s, (a, t) in per.items() if t), reverse=True)
    hit = [w for w in worst if w[0] > 0]
    print(f"\naffected scenarios {len(hit)}/{len(worst)} (by {key} POV fraction):")
    for frac, scen, a, t in hit[:25]:
        print(f"  {scen:38s} {a:3d}/{t:3d} POV  {frac:5.0%}")
    return flagged


def self_check():
    """Pin the two thresholds and the id parsing against the labelled calibration measurements."""
    assert scenario_of("20260901T155654Z-circuit_bearing_ladder-2p-d9b204df/") == \
        "circuit_bearing_ladder"
    assert scenario_of("20260901T153138Z-watch_lead_swap-4p-99d0ffcf") == "watch_lead_swap"
    # The labelled boundary: every BAD sat at or under 23.7, the lowest GOOD at 23.9. A threshold
    # that lets either side across silently would undo the calibration.
    assert 23.7 < VIEW_STD < 23.9, VIEW_STD
    # Measured mins of the reviewer-flagged POVs; each must land on the defect side.
    for measured in (14.5, 16.7, 20.9, 21.2, 23.5, 23.7):
        assert measured < VIEW_STD, measured
    # Measured GOOD floor and a legitimate descent that must NOT be convicted by view_std.
    for measured in (23.9, 37.7, 45.4):
        assert measured >= VIEW_STD, measured
    # terrain_bank_height descended in 85% of frames and was judged GOOD: descent alone is not
    # the defect, so the below-surface gate must stay a signal rather than a verdict.
    assert BELOW_FRAC < 0.85
    print("self-check ok")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--local-dir", help="a run's dataset/ directory")
    ap.add_argument("--video", action="store_true",
                    help="also decode each POV for view_std (CPU-heavy; do not run beside a render)")
    ap.add_argument("--jobs", type=int, default=4)
    ap.add_argument("--out", help="write one TSV row per POV here")
    ap.add_argument("--self-check", action="store_true")
    args = ap.parse_args()

    if args.self_check:
        self_check()
        return 0
    if not args.local_dir:
        ap.error("--local-dir is required unless --self-check")

    rows = audit(args.local_dir, args.video, args.jobs)
    if not rows:
        print(f"no POV frames under {args.local_dir}")
        return 2
    report(rows, args.video)
    if args.out:
        with open(args.out, "w") as fh:
            fh.write("scenario\tepisode\tpov\tbelow_frac\tview_std\n")
            for scen, ep, pov, below, std in rows:
                fh.write(f"{scen}\t{ep}\t{pov}\t{below:.4f}\t"
                         f"{'' if std is None else f'{std:.2f}'}\n")
        print(f"\nper-POV metrics written to {args.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
