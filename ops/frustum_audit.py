#!/usr/bin/env python3
"""Window-level view-predictability audit: within one training window, how much of the view
was already seen by the context frame(s).

Read-only. The source can be a local dataset directory or a Hugging Face branch (streamed, never
written to disk).

    python3 ops/frustum_audit.py --self-check
    python3 ops/frustum_audit.py --hf-repo <owner>/<dataset> \
        --revision <branch> --sample-per-scenario 2 --json-out /tmp/frustum.json

Four criteria are reported side by side because they are not equivalent (the first two are easy
to conflate):

- `sweep_over_fov`   the yaw **range swept** within the window exceeds the horizontal FOV. This is
                     NOT "zero overlap between first and last frame": turn 51 deg out and back and
                     the range exceeds the FOV while first and last frames overlap completely.
- `zero_overlap`     the **shortest angular distance** between first and last frame >= horizontal
                     FOV. This is true zero overlap. Shortest distance rather than cumulative angle,
                     because a 350 deg turn is really a 10 deg offset and the views still overlap.
- `infov_axis`       legacy inFOV metric: fraction of frames whose optical axis lies inside the
                     first frame's frustum. Systematically optimistic: with the axis only 10 deg
                     off, the current frame's edge already extends 10 deg past the first frame's
                     boundary, so new content is already on screen.
- `overlap_mean`     mean fraction of frustum angular overlap across the window, including pitch,
                     i.e. "on average, how much of the view the first frame has seen". This is the
                     quantity to use as a QC gate for new scenarios.

`peer_first_frame` / `peer_intrudes` / `peer_absent` quantify a second leak in multi-player
windows: is a teammate already visible in the first frame, or do they walk in mid-window (the
latter asks the model to generate a player that is absent from every context frame)? The test
uses only angle and distance and **ignores occlusion**, so the true intrusion rate can only be
higher. It is also the check for changes to `swarm.spread_range`: run before and after and compare.

**Known approximations**: overlap is computed as a rectangle intersection on the direction sphere,
ignoring parallax and disocclusion from translation as well as spherical distortion. Computing the
translation term properly needs the depth channel, and `render.depth` is off by default, so this
script only reports the displacement distribution and makes no claim about its contribution. The
horizontal FOV is derived from the vertical `fov_deg` and the projection aspect ratio (Minecraft's
FOV setting is the vertical FOV when aspect >= 1).
"""

from __future__ import annotations

import argparse
import json
import math
import random
import statistics
import sys
from collections import defaultdict
from collections.abc import Iterator
from pathlib import Path
from typing import Any

DEFAULT_WINDOW = 93  # 93 frames @20fps = 4.65s training window with a single context frame
PEER_MAX_DISTANCE = 128.0  # view_distance=8 chunks; teammates farther away are not rendered at all
PEER_STRIDE = 3  # check teammate visibility every 3 frames: an intrusion lasts several frames, so per-frame checks cost 3x for no information


def wrap180(degrees: float) -> float:
    """Wrap an angle difference into [-180, 180), i.e. the shortest angular distance."""
    return (degrees + 180.0) % 360.0 - 180.0


def horizontal_fov(vertical_fov_deg: float, width: int, height: int) -> float:
    aspect = width / height
    return 2.0 * math.degrees(math.atan(math.tan(math.radians(vertical_fov_deg) / 2.0) * aspect))


def unwrap(series: list[float]) -> list[float]:
    """Accumulate shortest angular steps into a continuous yaw series (used for the swept range)."""
    out = [series[0]]
    for previous, current in zip(series, series[1:]):
        out.append(out[-1] + wrap180(current - previous))
    return out


def peer_visible(viewer: dict[str, float], peer: dict[str, float], hfov: float, vfov: float,
                 max_distance: float) -> bool:
    """Whether the peer lies in the viewer's frustum for this frame. Angle and distance only, no occlusion."""
    d_x, d_y, d_z = peer["x"] - viewer["x"], peer["y"] - viewer["y"], peer["z"] - viewer["z"]
    ground = math.hypot(d_x, d_z)
    if ground > max_distance or ground < 1e-6:
        return False
    bearing = math.degrees(math.pi - math.atan2(-d_x, -d_z))
    elevation = -math.degrees(math.atan2(d_y, ground))
    return (abs(wrap180(bearing - viewer["yaw"])) <= hfov / 2.0
            and abs(elevation - viewer["pitch"]) <= vfov / 2.0)


def window_metrics(poses: list[dict[str, float]], window: int, hfov: float, vfov: float,
                   peers: list[list[dict[str, float]]] | None = None
                   ) -> Iterator[dict[str, float]]:
    """Split one POV trajectory into non-overlapping windows and yield one metrics row per window.

    `peers` are the trajectories of the other POVs in the same episode, frame-aligned with this one
    (guaranteed by the dataset's common-window trimming).
    """
    peers = peers or []
    for start in range(0, len(poses) - window + 1, window):
        chunk = poses[start:start + window]
        first = chunk[0]
        yaws = unwrap([pose["yaw"] for pose in chunk])
        sweep_range = max(yaws) - min(yaws)
        sweep_total = sum(abs(b - a) for a, b in zip(yaws, yaws[1:]))
        net_shortest = abs(wrap180(chunk[-1]["yaw"] - first["yaw"]))

        overlaps, axis_inside = [], 0
        for pose in chunk:
            d_yaw = abs(wrap180(pose["yaw"] - first["yaw"]))
            d_pitch = abs(pose["pitch"] - first["pitch"])
            overlaps.append(max(0.0, hfov - d_yaw) / hfov * max(0.0, vfov - d_pitch) / vfov)
            if d_yaw <= hfov / 2.0 and d_pitch <= vfov / 2.0:
                axis_inside += 1

        displacement = math.dist((first["x"], first["z"]), (chunk[-1]["x"], chunk[-1]["z"]))
        path = sum(math.dist((a["x"], a["z"]), (b["x"], b["z"])) for a, b in zip(chunk, chunk[1:]))

        peer_metrics: dict[str, float] = {}
        if peers:
            in_first = any(
                peer_visible(first, peer[start], hfov, vfov, PEER_MAX_DISTANCE)
                for peer in peers if len(peer) > start
            )
            ever = in_first or any(
                peer_visible(chunk[offset], peer[start + offset], hfov, vfov, PEER_MAX_DISTANCE)
                for peer in peers
                for offset in range(0, len(chunk), PEER_STRIDE)
                if len(peer) > start + offset
            )
            peer_metrics = {
                "peer_first_frame": float(in_first),
                "peer_intrudes": float(ever and not in_first),
                "peer_absent": float(not ever),
            }
        yield {**peer_metrics,
            "sweep_range": sweep_range, "sweep_total": sweep_total, "net_shortest": net_shortest,
            "net_over_sweep": net_shortest / sweep_range if sweep_range > 1e-9 else 0.0,
            "sweep_over_fov": float(sweep_range > hfov), "zero_overlap": float(net_shortest >= hfov),
            "infov_axis": axis_inside / len(chunk), "overlap_mean": statistics.fmean(overlaps),
            "displacement": displacement, "path_length": path,
        }


def iter_poses_local(path: Path) -> Iterator[dict[str, float]]:
    with path.open() as handle:
        for line in handle:
            yield json.loads(line)["camera_pose"]


def iter_poses_hf(session: Any, url: str, token: str | None) -> Iterator[dict[str, float]]:
    headers = {"Authorization": f"Bearer {token}"} if token else {}
    response = session.get(url, headers=headers, stream=True, timeout=180)
    response.raise_for_status()
    try:
        for line in response.iter_lines(decode_unicode=True):
            if line:
                yield json.loads(line)["camera_pose"]
    finally:
        response.close()


def read_poses(poses_iter: Iterator[dict[str, float]]) -> list[dict[str, float]]:
    return [{"yaw": p["yaw"], "pitch": p["pitch"], "x": p["x"], "y": p["y"], "z": p["z"],
             "fov": p["fov_deg"]} for p in poses_iter]


def episode_windows(traces: list[list[dict[str, float]]], window: int,
                    projection: tuple[int, int]) -> list[dict[str, float]]:
    """Process all player POVs of one episode together: the peer criteria need the other trajectories.

    Frame indices are directly comparable across POVs: finalize trims each episode to the world-tick
    interval common to all POVs, so frame 0 of every file is the same world instant
    (see docs/time-alignment.md).
    """
    rows: list[dict[str, float]] = []
    for index, poses in enumerate(traces):
        if len(poses) < window:
            continue
        vfov = poses[0]["fov"]
        hfov = horizontal_fov(vfov, *projection)
        peers = [other for position, other in enumerate(traces) if position != index]
        rows += window_metrics(poses, window, hfov, vfov, peers)
    return rows


def percentiles(values: list[float]) -> dict[str, float]:
    ordered = sorted(values)
    if not ordered:
        return {}
    def at(fraction: float) -> float:
        return ordered[min(len(ordered) - 1, int(len(ordered) * fraction))]
    return {"p10": at(0.10), "median": statistics.median(ordered), "p90": at(0.90),
            "p99": at(0.99), "mean": statistics.fmean(ordered)}


def summarise(rows: list[dict[str, float]]) -> dict[str, Any]:
    if not rows:
        return {"windows": 0}
    out: dict[str, Any] = {"windows": len(rows)}
    for key in ("sweep_range", "sweep_total", "net_shortest", "overlap_mean", "infov_axis",
                "displacement", "path_length", "net_over_sweep"):
        out[key] = percentiles([row[key] for row in rows])
    for key in ("sweep_over_fov", "zero_overlap"):
        out[key] = statistics.fmean(row[key] for row in rows)
    multi = [row for row in rows if "peer_first_frame" in row]
    if multi:
        out["multi_player_windows"] = len(multi)
        for key in ("peer_first_frame", "peer_intrudes", "peer_absent"):
            out[key] = statistics.fmean(row[key] for row in multi)
    return out


def scenario_of(episode_dir: str) -> str:
    parts = episode_dir.split("-")
    return "-".join(parts[1:-2]) if len(parts) >= 4 else episode_dir


def audit_local(root: Path, window: int, sample: int, only: str | None) -> dict[str, Any]:
    by_scenario: dict[str, list[dict[str, float]]] = defaultdict(list)
    seen: dict[str, int] = defaultdict(int)
    for episode in sorted(path for path in root.iterdir() if path.is_dir()):
        scenario = scenario_of(episode.name)
        if only and scenario != only:
            continue
        if sample and seen[scenario] >= sample:
            continue
        manifest = json.loads((episode / "episode.json").read_text())
        projection = projection_size(manifest)
        traces = [read_poses(iter_poses_local(trace))
                  for trace in sorted(episode.glob("p*-frames.jsonl"))]
        by_scenario[scenario] += episode_windows(traces, window, projection)
        seen[scenario] += 1
    return assemble(by_scenario, {"source": "local", "dir": str(root)}, window)


def projection_size(manifest: dict[str, Any]) -> tuple[int, int]:
    """Width/height of the POV's default projection from episode.json; falls back to the schema default 640x360."""
    for pov in manifest.get("povs", []):
        for projection in pov.get("projections", []) or []:
            if projection.get("method") == "DEFAULT" or projection.get("name") == "default":
                return int(projection["width"]), int(projection["height"])
    return 640, 360


def audit_hf(repo: str, revision: str, window: int, sample: int, only: str | None) -> dict[str, Any]:
    import requests
    from huggingface_hub import HfApi, get_token

    api, token, session = HfApi(), get_token(), requests.Session()
    tree = api.list_repo_tree(repo, path_in_repo="dataset", repo_type="dataset",
                              revision=revision, recursive=False)
    episodes = sorted(item.path.split("/")[-1] for item in tree
                      if item.__class__.__name__ == "RepoFolder")

    grouped: dict[str, list[str]] = defaultdict(list)
    for episode in episodes:
        scenario = scenario_of(episode)
        if only and scenario != only:
            continue
        grouped[scenario].append(episode)
    # Random sample with a fixed seed rather than the first N: directory names sort by time, so the first N would bias toward early episodes and small N.
    rng = random.Random(41)
    chosen = {name: (eps if not sample or len(eps) <= sample else rng.sample(eps, sample))
              for name, eps in grouped.items()}

    base = f"https://huggingface.co/datasets/{repo}/resolve/{revision}/dataset"
    by_scenario: dict[str, list[dict[str, float]]] = defaultdict(list)
    total = sum(len(v) for v in chosen.values())
    done = 0
    for scenario, picked in sorted(chosen.items()):
        for episode in picked:
            done += 1
            manifest = session.get(f"{base}/{episode}/episode.json",
                                   headers={"Authorization": f"Bearer {token}"} if token else {},
                                   timeout=60)
            manifest.raise_for_status()
            data = manifest.json()
            projection = projection_size(data)
            povs = [pov["name"] for pov in data.get("povs", [])
                    if pov.get("kind", "player") == "player"]
            print(f"[{done}/{total}] {episode} ({len(povs)} POV)", file=sys.stderr, flush=True)
            traces = []
            for pov in povs:
                url = f"{base}/{episode}/{pov}-frames.jsonl"
                try:
                    traces.append(read_poses(iter_poses_hf(session, url, token)))
                except Exception as error:  # one unreadable POV should not abort the whole audit
                    print(f"  skip {pov}: {error}", file=sys.stderr)
            by_scenario[scenario] += episode_windows(traces, window, projection)
    return assemble(by_scenario, {"source": "huggingface", "repo": repo, "revision": revision},
                    window)


def assemble(by_scenario: dict[str, list[dict[str, float]]], provenance: dict[str, Any],
             window: int) -> dict[str, Any]:
    every = [row for rows in by_scenario.values() for row in rows]
    return {**provenance, "window_frames": window, "overall": summarise(every),
            "by_scenario": {name: summarise(rows) for name, rows in sorted(by_scenario.items())}}


def report(result: dict[str, Any]) -> None:
    overall = result["overall"]
    if not overall.get("windows"):
        print("no windows to summarise (trajectories shorter than the window?)")
        return
    print(f"source: {result.get('repo') or result.get('dir')} | window {result['window_frames']} frames "
          f"| {overall['windows']} non-overlapping windows\n")
    print("criteria side by side (the first two are easy to conflate):")
    print(f"  sweep_over_fov  swept range > horizontal FOV  : {overall['sweep_over_fov']:6.1%}")
    print(f"  zero_overlap    first/last shortest dist >= FOV: {overall['zero_overlap']:6.1%}   <- true zero overlap")
    print(f"  infov_axis      legacy (axis inside frustum)   : median {overall['infov_axis']['median']:.2f}")
    print(f"  overlap_mean    frustum overlap fraction       : median {overall['overlap_mean']['median']:.2f}\n")
    for key, unit in (("sweep_range", "deg"), ("net_shortest", "deg"), ("displacement", "blocks"),
                      ("path_length", "blocks")):
        stats = overall[key]
        print(f"  {key:14s} p10 {stats['p10']:7.2f} median {stats['median']:7.2f} "
              f"p90 {stats['p90']:7.2f} p99 {stats['p99']:7.2f} {unit}")
    if overall.get("multi_player_windows"):
        print(f"\nmulti-player windows ({overall['multi_player_windows']}; peer test ignores occlusion, so these are lower bounds):")
        print(f"  peer_first_frame visible in first frame : {overall['peer_first_frame']:6.1%}")
        print(f"  peer_intrudes    walks in mid-window    : {overall['peer_intrudes']:6.1%}")
        print(f"  peer_absent      never visible          : {overall['peer_absent']:6.1%}")
    print("\nper scenario (ascending overlap_mean, first 15 and last 8):")
    ranked = sorted(((name, s) for name, s in result["by_scenario"].items() if s.get("windows")),
                    key=lambda item: item[1]["overlap_mean"]["median"])
    print(f"  {'scenario':34s} {'overlap':>7s} {'axis':>6s} {'sweep°':>7s} {'disp':>6s} {'win':>5s}")
    shown = ranked[:15] + ([("...", None)] if len(ranked) > 23 else []) + ranked[-8:]
    if len(ranked) <= 23:
        shown = ranked
    for name, stats in shown:
        if stats is None:
            print("  ...")
            continue
        print(f"  {name:34s} {stats['overlap_mean']['median']:7.2f} "
              f"{stats['infov_axis']['median']:6.2f} {stats['sweep_range']['median']:7.1f} "
              f"{stats['displacement']['median']:6.2f} {stats['windows']:5d}")


def self_check() -> None:
    """Synthetic-trajectory asserts: the criteria must tell "turn away" apart from "turn away and back"."""
    hfov, vfov, window = 102.5, 70.0, 93

    def track(yaws: list[float]) -> list[dict[str, float]]:
        return [{"yaw": y, "pitch": 0.0, "x": 0.0, "y": 64.0, "z": 0.0} for y in yaws]

    still = next(iter(window_metrics(track([0.0] * window), window, hfov, vfov)))
    assert still["overlap_mean"] == 1.0 and still["zero_overlap"] == 0.0
    assert still["sweep_range"] == 0.0

    # Turn 180 deg one way: last-frame shortest distance 180 >= FOV, true zero overlap.
    away = next(iter(window_metrics(
        track([i * 180.0 / (window - 1) for i in range(window)]), window, hfov, vfov)))
    assert away["zero_overlap"] == 1.0, away
    assert away["sweep_over_fov"] == 1.0

    # Swing 60 deg each way (range 120 > FOV 102.5) and return: first and last frames overlap fully.
    # This is exactly why sweep_over_fov must not be read as zero overlap.
    back = next(iter(window_metrics(
        track([60.0 * math.sin(2 * math.pi * i / (window - 1)) for i in range(window)]),
        window, hfov, vfov)))
    assert back["sweep_over_fov"] == 1.0, back
    assert back["zero_overlap"] == 0.0, back
    assert back["net_shortest"] < 1e-6, back

    # Legacy metric is optimistic: an axis 40 deg off still counts as "inside the frustum" (40 < 102.5/2), while the real overlap is only ~61%.
    tilted = next(iter(window_metrics(track([0.0] + [40.0] * (window - 1)), window, hfov, vfov)))
    assert tilted["infov_axis"] == 1.0, tilted
    assert 0.55 < tilted["overlap_mean"] < 0.65, tilted

    # Turn 350 deg: large cumulative angle but only 10 deg shortest distance, views still overlap; cumulative angle cannot decide zero overlap.
    around = next(iter(window_metrics(
        track([i * 350.0 / (window - 1) for i in range(window)]), window, hfov, vfov)))
    assert around["net_shortest"] < 10.001, around
    assert around["zero_overlap"] == 0.0, around

    assert abs(horizontal_fov(70.0, 640, 360) - 102.5) < 0.1

    print("self-check passed: 5 synthetic trajectories + FOV conversion")
    print("  key point: a window that swings 60 deg each way and returns has sweep_over_fov=1 but zero_overlap=0")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--local-dir", type=Path, help="local dataset directory")
    parser.add_argument("--hf-repo")
    parser.add_argument("--revision", default="main")
    parser.add_argument("--window", type=int, default=DEFAULT_WINDOW)
    parser.add_argument("--sample-per-scenario", type=int, default=2, help="0 = all episodes")
    parser.add_argument("--scenario", help="audit only this scenario")
    parser.add_argument("--json-out", type=Path)
    parser.add_argument("--self-check", action="store_true")
    args = parser.parse_args()

    if args.self_check:
        self_check()
        return 0
    if not args.local_dir and not args.hf_repo:
        parser.error("need --local-dir or --hf-repo (or --self-check)")

    result = (audit_local(args.local_dir, args.window, args.sample_per_scenario, args.scenario)
              if args.local_dir else
              audit_hf(args.hf_repo, args.revision, args.window, args.sample_per_scenario,
                       args.scenario))
    report(result)
    if args.json_out:
        args.json_out.write_text(json.dumps(result, indent=1))
        print(f"\nJSON → {args.json_out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
