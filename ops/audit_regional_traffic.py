#!/usr/bin/env python3
"""Audit reservations AND measured region occupancy on the common all-player world clock.

Uses only standard Python. A ledger pass cannot hide physical crowding or missing players.
The input is the capture directory (also retained under dataset/diagnostics/capture).
"""
import argparse
from collections import Counter
import json
import math
from pathlib import Path


def audit(root):
    summary = json.loads((root / 'capture-summary.json').read_text())
    record = json.loads((root / 'capture.json').read_text())
    hz = record['tick_rate_hz']
    required_ticks = max(200, math.ceil(record['target_duration_ms'] / 1000 * hz * .9))
    staging = summary['diagnostics']['regional_staging']
    ax, az = summary['anchor']
    side, pitch, cap = staging['side'], staging['pitch'], staging['capacity']
    placements = staging['placements']
    streams = []
    events = []
    for path in sorted(root.glob('p*-actions.jsonl')):
        rows = [json.loads(line) for line in path.read_text().splitlines()]
        streams.append({r['world_age_tick']: r for r in rows if r.get('world_age_tick') is not None and r['server_tick'] >= 0})
    for path in sorted(root.glob('p*-events.jsonl')):
        events.extend(json.loads(line) for line in path.read_text().splitlines())
    regional = [e for e in events if str(e.get('visible_event_id', '')).startswith('region-')]
    common = sorted(set.intersection(*(set(s) for s in streams))) if streams else []
    max_physical = 0
    overfull = []
    outside = 0
    trajectory = []
    seen = [set() for _ in streams]
    for tick in common:
        counts = Counter()
        positions = []
        for i, stream in enumerate(streams):
            r = stream[tick]
            col = math.floor((r['x'] - ax) / pitch + (side - 1) / 2 + .5)
            row = math.floor((r['z'] - az) / pitch + (side - 1) / 2 + .5)
            if not (0 <= col < side and 0 <= row < side):
                outside += 1
                continue
            cell = row * side + col
            counts[cell] += 1
            seen[i].add(cell)
            positions.append([r['x'], r['y'], r['z'], cell])
        max_physical = max(max_physical, max(counts.values(), default=0))
        if any(c > cap for c in counts.values()): overfull.append(tick)
        if (tick - common[0]) % hz == 0:
            trajectory.append({'world_age_tick': tick, 'positions': positions, 'counts': dict(counts)})
    claims = [e['payload']['claims'] for e in regional if 'claims' in e['payload']]
    max_claims = max((max(c, default=0) for c in claims), default=0)
    arrivals = [e for e in regional if e['payload'].get('arrived')]
    result = {
        'players': len(streams), 'expected_players': len(placements),
        'staging_verified': staging['verified'], 'side': side, 'pitch': pitch, 'capacity': cap,
        'common_world_ticks': len(common), 'common_duration_sec': len(common)/hz,
        'common_first_world_tick': common[0] if common else None,
        'common_last_world_tick': common[-1] if common else None,
        'required_common_ticks': required_ticks,
        'max_resident_plus_incoming_claims': max_claims,
        'max_measured_region_occupancy': max_physical,
        'overfull_world_ticks': len(overfull), 'outside_player_frames': outside,
        'reservation_event_count': len(regional), 'completed_moves': len(arrivals),
        'players_visiting_multiple_regions': sum(len(s) > 1 for s in seen),
        'regions_visited_per_player': [len(s) for s in seen],
        'anchor': [ax, az], 'trajectory_1hz': trajectory,
    }
    result['pass'] = bool(staging['verified'] and len(streams) == len(placements)
        and len(common) >= required_ticks and claims and max_claims <= cap and not overfull and not outside
        and (side == 1 or result['players_visiting_multiple_regions'] >= len(streams) * .75))
    return result


if __name__ == '__main__':
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('capture_dir', type=Path)
    p.add_argument('--out', required=True, type=Path)
    a = p.parse_args()
    result = audit(a.capture_dir)
    a.out.write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps({k: v for k, v in result.items() if k not in ['trajectory_1hz', 'regions_visited_per_player']}, indent=2))
    raise SystemExit(0 if result['pass'] else 1)
