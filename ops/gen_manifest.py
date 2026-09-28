#!/usr/bin/env python3
"""Generate a collection manifest from the compiled scenario catalog.

Collection scenarios in `crates/schema/src/catalog.rs` are declared through `s(...)`, `s6(...)` or
`s7(...)`, which share one argument list and differ only in the scenario generation they tag.
This reads those entries, selects scenarios, and emits one episode per
(scenario, player count, repeat). Showcase scenarios declared as plain struct literals
(generation 0, e.g. `regional_capacity_traffic`) are never selected; write their manifests by hand
(see `ops/regional-showcase-*.json`). Seeds are a hash of the run id and the episode coordinates,
so regenerating a manifest is byte-stable.

Evaluation-family scenarios (`eval_*`) are excluded unless `--evaluation` is given, and then
they are the ONLY scenarios emitted: they must never share a manifest, output prefix or dataset
branch with training data.

    python3 ops/gen_manifest.py --run-id demo --players 2 4 --repeats 2 --out demo.json
"""

import argparse
import hashlib
import json
import re
from pathlib import Path
from typing import Any

ENTRY = re.compile(
    r'\bs(?P<gen>[67]?)\(\s*"(?P<name>[^"]+)"\s*,\s*"(?P<family>[^"]+)"\s*,\s*(?P<min>\d+)\s*,\s*'
    r'(?P<single>true|false)\s*,\s*(?P<edits>true|false)\s*,\s*"(?P<scenery>[^"]+)"\s*,\s*'
    r'(?P<far>true|false)\s*,?\s*\)',
    re.S,
)


def catalog(path: Path) -> list[dict[str, Any]]:
    body = path.read_text().split("pub const SCENARIOS", 1)[1].split("\n];", 1)[0]
    return [
        {
            "name": m["name"],
            "family": m["family"],
            "generation": int(m["gen"] or 5),
            "min_players": int(m["min"]),
            "single_process": m["single"] == "true",
            "scenery": m["scenery"],
            "far_players": m["far"] == "true",
        }
        for m in ENTRY.finditer(body)
    ]


def seed(run_id: str, name: str, bots: int, index: int) -> int:
    digest = hashlib.sha256(f"{run_id}:{name}:{bots}:{index}".encode()).digest()
    return int.from_bytes(digest[:4], "big") % (2**31 - 1) + 1


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--run-id")
    p.add_argument("--out", type=Path)
    p.add_argument("--catalog", type=Path, default=Path("crates/schema/src/catalog.rs"))
    p.add_argument("--players", type=int, nargs="+", default=[2], help="player counts N")
    p.add_argument("--repeats", type=int, default=1, help="episodes per (scenario, N)")
    p.add_argument("--duration", type=float, default=120.0, help="episode length in seconds")
    p.add_argument("--generations", type=int, nargs="*", help="scenario generations (default: all)")
    p.add_argument("--scenarios", nargs="*", help="explicit scenario names (default: all selected)")
    p.add_argument("--evaluation", action="store_true", help="emit ONLY evaluation-family scenarios")
    p.add_argument("--spread-range", type=float, help="per-episode swarm.spread_range override")
    p.add_argument("--list", action="store_true", help="print the selected scenarios and exit")
    a = p.parse_args()

    rows = catalog(a.catalog)
    rows = [r for r in rows if (r["family"] == "evaluation") == a.evaluation]
    if a.generations:
        rows = [r for r in rows if r["generation"] in a.generations]
    if a.scenarios:
        known = {r["name"] for r in rows}
        unknown = sorted(set(a.scenarios) - known)
        if unknown:
            raise SystemExit(f"not in the selected catalog: {unknown}")
        rows = [r for r in rows if r["name"] in a.scenarios]
    if a.list:
        for r in rows:
            print(f'{r["name"]:40} gen={r["generation"]} family={r["family"]} min_players={r["min_players"]}')
        print(f"{len(rows)} {'evaluation' if a.evaluation else 'collection'} scenarios selected")
        return
    if not (a.run_id and a.out):
        p.error("--run-id and --out are required unless --list")

    episodes = []
    for r in sorted(rows, key=lambda r: r["name"]):
        for bots in a.players:
            if r["min_players"] > bots:
                continue
            for i in range(a.repeats):
                scenario = {"name": r["name"], "seed": seed(a.run_id, r["name"], bots, i), "scenery": r["scenery"]}
                if r["far_players"]:
                    scenario["far_players"] = 1
                ep = {
                    "episode_id": f'{r["name"]}-n{bots:02d}-{a.run_id}-{i + 1:02d}',
                    "player_count": bots,
                    "shards": 1 if r["single_process"] else 0,
                    "episode_duration_sec": a.duration,
                    "scenario": scenario,
                }
                if a.spread_range is not None:
                    ep["swarm"] = {"spread_range": a.spread_range}
                episodes.append(ep)
    if not episodes:
        raise SystemExit("no episodes selected")

    manifest = {
        "run_id": a.run_id,
        "capture_cpus": 16,
        "render": {"fps": 20, "threads_per_render": 8, "hud": False, "observers": []},
        "episodes": episodes,
    }
    a.out.write_text(json.dumps(manifest, indent=2) + "\n")
    print(f"{a.out}: {len(episodes)} episodes from {len(rows)} scenarios")


if __name__ == "__main__":
    main()
