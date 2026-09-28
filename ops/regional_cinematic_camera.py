#!/usr/bin/env python3
"""Build a smooth, low regional showcase camera from verified capture evidence.

The keyframes use the renderer camera-path clock, beginning at frame zero and
using the longest source replay duration. This is an editorial recipe for the
seed-4103 regional arena, not a terrain collision solver. Check the real temporal contact sheet before publishing a new map.
No camera pose modifies the recorded game or player trajectories.
"""
import argparse
import json
import math
from pathlib import Path
import statistics
import zipfile


def camera_path(summary, audit, fps, tick_rate=20, duration_ms=None):
    if not audit['pass'] or not summary['diagnostics']['regional_staging']['verified']:
        raise ValueError('camera planning requires a passing regional capture audit')
    ax, az = audit['anchor']
    half = audit['side'] * audit['pitch'] / 2
    terrain = [p['position'] for p in summary['diagnostics']['regional_staging']['placements']]
    centre_y = statistics.median(p[1] for p in terrain)
    start_ms = 0.0
    common_ms = (audit['common_last_world_tick'] - audit['common_first_world_tick']) * 1000 / tick_rate
    duration_ms = common_ms if duration_ms is None else duration_ms
    if start_ms < 0 or duration_ms <= 0:
        raise ValueError('invalid common window on replay clock')
    end_ms = max((summary['recording_stop_world_age_tick'] - summary['recording_start_world_age_tick']) * 1000 / tick_rate + 15000, start_ms + duration_ms + 15000)
    keys = []
    last_yaw = None
    for frame in range(math.ceil(end_ms * fps / 1000) + 1):
        time_ms = round(frame * 1000 / fps)
        u = min(1., max(0., (time_ms - start_ms) / duration_ms))
        # Cosine easing gives zero endpoint velocity, continuous acceleration. The radial
        # push/pull and vertical dip remain smooth through the midpoint (no hard cuts).
        eased = (1 - math.cos(math.pi * u)) / 2
        theta = math.radians(135 - 180 * eased)
        radius = half * (.88 - .43 * math.sin(math.pi * u)**2)
        x, z = ax + radius * math.cos(theta), az + radius * math.sin(theta)
        y = centre_y + 18 - 8 * math.sin(math.pi * u)**2
        # Soft upper bound of known nearby ground heights, with smooth distance weights.
        # Dense staging points are evidence about ground, not proof of empty camera airspace.
        ground_scores = [p[1] - .055 * ((p[0]-x)**2 + (p[2]-z)**2) for p in terrain]
        peak = max(ground_scores)
        ground_upper = peak + math.log(sum(math.exp(g-peak) for g in ground_scores))
        # Smooth maximum avoids a velocity kink when local terrain sets the safety floor.
        floor = ground_upper + 8
        y = max(y, floor) + math.log1p(math.exp(-abs(y-floor)))
        tx, tz = ax - half * .08, az - half * .06
        dx, dz, dy = tx-x, tz-z, centre_y+1.4-y
        yaw = math.degrees(math.atan2(-dx, dz))
        if last_yaw is not None:
            yaw = last_yaw + (yaw-last_yaw+180) % 360 - 180
        last_yaw = yaw
        pitch = math.degrees(math.atan2(-dy, math.hypot(dx, dz)))
        keys.append(dict(time_ms=time_ms, x=x, y=y, z=z, yaw=yaw, pitch=pitch, roll=0.))
    shot = [p for p in keys if start_ms <= p['time_ms'] <= start_ms+duration_ms]
    distance = sum(math.dist([a[k] for k in ('x','y','z')], [b[k] for k in ('x','y','z')]) for a,b in zip(shot,shot[1:]))
    report = dict(shot_start_camera_ms=start_ms, common_duration_ms=common_ms, camera_duration_ms=duration_ms,
                  path_length_blocks=distance, camera_y_range=[min(p['y'] for p in shot),max(p['y'] for p in shot)],
                  centre_ground_y=centre_y, bearing_sweep_deg=180, keyframe_count=len(keys),
                  fps=fps, terrain_collision_validation='requires rendered-pixel review')
    return keys, report


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--capture', type=Path, required=True)
    p.add_argument('--audit', type=Path, required=True)
    p.add_argument('--manifest', type=Path, required=True)
    p.add_argument('--out', type=Path, required=True)
    a = p.parse_args()
    summary = json.loads((a.capture/'capture-summary.json').read_text())
    record = json.loads((a.capture/'capture.json').read_text())
    audit = json.loads(a.audit.read_text())
    manifest = json.loads(a.manifest.read_text())
    if record['world']['seed'] != '4103':
        raise ValueError('this camera recipe is designed for world seed 4103; review a new route for other terrain')
    replay_durations = {}
    for replay in sorted(a.capture.glob('*.mcpr')):
        with zipfile.ZipFile(replay) as archive:
            replay_durations[replay.stem] = json.loads(archive.read('metaData.json'))['duration']
    if not replay_durations or min(replay_durations.values()) <= 0:
        raise ValueError('source replay durations must be available before camera planning')
    keys, report = camera_path(summary, audit, manifest['render']['fps'], record['tick_rate_hz'],
                               duration_ms=max(replay_durations.values()))
    report['source_replay_durations_ms'] = replay_durations
    manifest['render']['render_distance_chunks'] = 24
    manifest['render']['observers'] = [dict(name='obs_regions', mode='keyframes', keyframes=keys)]
    a.out.parent.mkdir(parents=True, exist_ok=True)
    a.out.write_text(json.dumps(manifest, indent=2)+'\n')
    a.out.with_suffix('.camera-report.json').write_text(json.dumps(report, indent=2)+'\n')
    print(json.dumps(report, indent=2))


if __name__ == '__main__':
    main()
