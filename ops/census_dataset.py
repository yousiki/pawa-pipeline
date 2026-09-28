#!/usr/bin/env python3
"""Census of a local dataset directory or a Hugging Face dataset branch. Read-only.

Examples:
    python3 ops/census_dataset.py --local-dir <out>/dataset
    python3 ops/census_dataset.py --hf-repo <owner>/<dataset> --revision <branch>
"""

from __future__ import annotations

import argparse
import json
import math
import re
import statistics
import sys
from collections import Counter, defaultdict
from collections.abc import Iterable
from pathlib import Path
from typing import Any

EPISODE_RE = re.compile(r"-(\d+)p-[^-]+$")
QC_METRIC_PATHS = (
    "degeneracy.idle_frac", "degeneracy.longest_idle_s", "degeneracy.short_run_frac",
    "degeneracy.camera_travel_agreement", "degeneracy.frozen_frac", "degeneracy.longest_frozen_s",
    "degeneracy.stuck_total_s", "degeneracy.dark_frame_frac", "degeneracy.longest_dark_run_s",
    "motion.idle_frame_ratio", "motion.moving_frame_ratio", "motion.move_run_median_ticks",
    "motion.move_runs_under_200ms_frac", "motion.path_length_2d", "motion.xz_span",
    "motion.yaw_coverage_rad", "motion.pitch_coverage_rad",
)


def nested(value: Any, path: str) -> Any:
    for part in path.split("."):
        if not isinstance(value, dict):
            return None
        value = value.get(part)
    return value


def numeric_summary(values: Iterable[float]) -> dict[str, float | int]:
    ordered = sorted(values)
    if not ordered:
        return {"count": 0}
    return {
        "count": len(ordered), "min": ordered[0], "median": statistics.median(ordered),
        "p95": ordered[int((len(ordered) - 1) * 0.95)], "max": ordered[-1],
        "mean": statistics.fmean(ordered),
    }


def pov_count(name: str) -> int:
    match = EPISODE_RE.search(name)
    if not match:
        raise ValueError(f"episode directory has no player count: {name}")
    return int(match.group(1))


def classify_failure(message: str) -> str:
    text = message.lower()
    for needle, category in (
        ("near-black", "darkness"), ("no net displacement", "stuck"),
        ("movement-key", "key_chatter"), ("camera turns", "camera_travel"),
        ("frozen", "frozen"), ("overlap", "overlap"), ("participation", "participation"),
    ):
        if needle in text:
            return category
    return "other"


def local_census(root: Path) -> dict[str, Any]:
    if not root.is_dir():
        raise FileNotFoundError(root)

    episodes = sorted(path for path in root.iterdir() if path.is_dir())
    counters = {name: Counter() for name in (
        "by_players", "by_scenario", "by_family", "schema_versions", "file_counts",
        "qc_verdict", "qc_spread", "anchors", "alignment_status", "failure_categories",
        "projections", "codecs", "pov_status", "pov_kinds", "file_bytes",
    )}
    by_players = counters["by_players"]
    by_scenario = counters["by_scenario"]
    by_family = counters["by_family"]
    schema_versions = counters["schema_versions"]
    file_counts = counters["file_counts"]
    qc_verdict = counters["qc_verdict"]
    qc_spread = counters["qc_spread"]
    anchors = counters["anchors"]
    alignment_status = counters["alignment_status"]
    failure_categories = counters["failure_categories"]
    projections = counters["projections"]
    codecs = counters["codecs"]
    pov_status = counters["pov_status"]
    pov_kinds = counters["pov_kinds"]
    file_bytes = counters["file_bytes"]
    qc_values: dict[str, list[float]] = defaultdict(list)
    actual_durations: list[float] = []
    target_durations: list[float] = []
    episode_bytes: list[int] = []
    missing_metadata: list[str] = []
    incomplete_layout: list[str] = []
    frame_files = event_episodes = frame_rows = event_rows = povs = 0
    assembled_episodes = complete_player_episodes = complete_player_povs = 0
    hard_complete_episodes = 0

    for episode in episodes:
        n = pov_count(episode.name)
        by_players[n] += 1
        povs += n
        size = 0
        for file in episode.rglob("*"):
            if not file.is_file():
                continue
            file_size = file.stat().st_size
            size += file_size
            suffix = file.suffix.lower() or "<none>"
            file_counts[suffix] += 1
            file_bytes[suffix] += file_size
            if file.name.endswith("-frames.jsonl"):
                frame_files += 1
                with file.open(encoding="utf-8") as stream:
                    frame_rows += sum(bool(line.strip()) for line in stream)
            elif file.name == "events.jsonl":
                with file.open(encoding="utf-8") as stream:
                    event_rows += sum(bool(line.strip()) for line in stream)
        episode_bytes.append(size)

        episode_path, qc_path = episode / "episode.json", episode / "qc.json"
        if not episode_path.is_file() or not qc_path.is_file():
            missing_metadata.append(episode.name)
            continue
        episode_data = json.loads(episode_path.read_text(encoding="utf-8"))
        qc = json.loads(qc_path.read_text(encoding="utf-8"))
        assembled_episodes += 1
        if (episode / "events.jsonl").is_file():
            event_episodes += 1
        scenario = episode_data.get("scenario", {})
        scenario_name = scenario.get("name", "<missing>") if isinstance(scenario, dict) else str(scenario)
        by_scenario[scenario_name] += 1
        by_family[scenario.get("family", "<missing>") if isinstance(scenario, dict) else "<missing>"] += 1
        schema_versions[str(episode_data.get("schema_version", "<missing>"))] += 1
        actual_durations.append(float(episode_data.get("actual_duration_ms", 0)))
        target_durations.append(float(episode_data.get("target_duration_ms", 0)))
        qc_verdict[str(qc.get("verdict", "<missing>"))] += 1
        spread = qc.get("pov_anchor_spread_ticks")
        qc_spread[str(spread)] += 1
        for message in qc.get("failures", []):
            failure_categories[classify_failure(str(message))] += 1
        player_povs = [p for p in episode_data.get("povs", []) if p.get("kind") == "player"]
        for pov in episode_data.get("povs", []):
            pov_kinds[str(pov.get("kind", "<missing>"))] += 1
            pov_status[str(pov.get("status", "<missing>"))] += 1
        complete_count = sum(p.get("status") == "complete" for p in player_povs)
        complete_player_povs += complete_count
        all_povs_complete = bool(player_povs) and complete_count == len(player_povs)
        if all_povs_complete:
            complete_player_episodes += 1
        required = ["episode.json", "qc.json", "events.jsonl"]
        required.extend(
            video["path"] for pov in player_povs for video in pov.get("videos", {}).values()
            if isinstance(video, dict) and isinstance(video.get("path"), str)
        )
        required.extend(f"{pov['name']}-frames.jsonl" for pov in player_povs if isinstance(pov.get("name"), str))
        layout_ok = all((episode / path).is_file() for path in required)
        if not layout_ok:
            incomplete_layout.append(episode.name)
        if all_povs_complete and spread == 0 and layout_ok:
            hard_complete_episodes += 1
        for pov in qc.get("povs", {}).values():
            alignment = pov.get("alignment", {})
            anchors[str(alignment.get("anchor", "<missing>"))] += 1
            alignment_status[str(alignment.get("status", "<missing>"))] += 1
            for path in QC_METRIC_PATHS:
                value = nested(pov, path)
                if isinstance(value, (int, float)) and not isinstance(value, bool) and math.isfinite(value):
                    qc_values[path].append(float(value))
        for pov in episode_data.get("povs", []):
            for projection, video in pov.get("videos", {}).items():
                projections[projection] += 1
                codecs[str(video.get("codec", "<missing>"))] += 1

    diagnostics_bytes = sum(
        file.stat().st_size for episode in episodes for file in (episode / "diagnostics").rglob("*") if file.is_file()
    )
    total_bytes = sum(episode_bytes)
    return {
        "source": "local", "root": str(root), "episodes": len(episodes), "povs": povs,
        "average_povs_per_episode": povs / len(episodes) if episodes else 0,
        "by_players": dict(sorted(by_players.items())), "by_scenario": dict(sorted(by_scenario.items())),
        "by_family": dict(sorted(by_family.items())), "schema_versions": dict(sorted(schema_versions.items())),
        "assembled_episodes": assembled_episodes, "complete_player_episodes": complete_player_episodes,
        "complete_player_povs": complete_player_povs, "hard_complete_spread0_episodes": hard_complete_episodes,
        "pov_kinds": dict(sorted(pov_kinds.items())), "pov_status": dict(sorted(pov_status.items())),
        "missing_metadata_episodes": missing_metadata, "incomplete_layout_episodes": incomplete_layout,
        "durations": {"actual_ms": numeric_summary(actual_durations), "target_ms": numeric_summary(target_durations),
                      "actual_hours": sum(actual_durations) / 3_600_000, "target_hours": sum(target_durations) / 3_600_000},
        "files": {"count": sum(file_counts.values()), "bytes": total_bytes, "gib": total_bytes / 2**30,
                  "diagnostics_bytes": diagnostics_bytes, "diagnostics_gib": diagnostics_bytes / 2**30,
                  "published_bytes_without_diagnostics": total_bytes - diagnostics_bytes,
                  "by_suffix": {s: {"count": file_counts[s], "bytes": file_bytes[s]} for s in sorted(file_counts)}},
        "records": {"frame_files": frame_files, "frame_rows": frame_rows, "event_episodes": event_episodes, "event_rows": event_rows},
        "media": {"projections": dict(sorted(projections.items())), "codecs": dict(sorted(codecs.items()))},
        "qc": {"verdict": dict(sorted(qc_verdict.items())), "spread_ticks": dict(sorted(qc_spread.items())),
               "anchors": dict(sorted(anchors.items())), "alignment_status": dict(sorted(alignment_status.items())),
               "failure_categories": dict(sorted(failure_categories.items())),
               "pov_metrics": {path: numeric_summary(values) for path, values in sorted(qc_values.items())}},
        "episode_size_bytes": numeric_summary(episode_bytes),
    }


def hf_census(repo: str, revision: str) -> dict[str, Any]:
    try:
        from huggingface_hub import HfApi
    except ImportError as error:
        raise SystemExit("HF census requires huggingface_hub") from error
    api = HfApi()
    top = list(api.list_repo_tree(repo, path_in_repo="dataset", repo_type="dataset", revision=revision, recursive=False))
    folders = [entry for entry in top if type(entry).__name__ == "RepoFolder"]
    by_players: Counter[int] = Counter()
    povs = 0
    for folder in folders:
        n = pov_count(folder.path.rsplit("/", 1)[-1])
        by_players[n] += 1
        povs += n
    files = list(api.list_repo_tree(repo, path_in_repo="dataset", repo_type="dataset", revision=revision, recursive=True))
    file_counts: Counter[str] = Counter()
    file_bytes: Counter[str] = Counter()
    for entry in files:
        if type(entry).__name__ != "RepoFile":
            continue
        suffix = Path(entry.path).suffix.lower() or "<none>"
        file_counts[suffix] += 1
        file_bytes[suffix] += int(getattr(entry, "size", 0) or 0)
    info = api.repo_info(repo, repo_type="dataset", revision=revision)
    return {"source": "huggingface", "repo": repo, "revision": revision, "episodes": len(folders),
            "povs": povs, "average_povs_per_episode": povs / len(folders) if folders else 0,
            "by_players": dict(sorted(by_players.items())),
            "files": {"count": sum(file_counts.values()), "tree_bytes": sum(file_bytes.values()),
                      "by_suffix": {s: {"count": file_counts[s], "bytes": file_bytes[s]} for s in sorted(file_counts)}},
            "repo_info": {"sha": getattr(info, "sha", None), "last_modified": str(getattr(info, "last_modified", None)),
                          "used_storage_bytes": getattr(info, "used_storage", None)}}


def print_summary(result: dict[str, Any]) -> None:
    if result["source"] == "local":
        print(f"local: {result['episodes']} episodes / {result['povs']} POVs, {result['average_povs_per_episode']:.3f} POVs/episode on average")
        print(f"duration {result['durations']['actual_hours']:.3f} h actual / {result['durations']['target_hours']:.3f} h target; {result['files']['count']} files / {result['files']['gib']:.3f} GiB")
        print(f"frame rows {result['records']['frame_rows']} ({result['records']['frame_files']} files), event rows {result['records']['event_rows']} ({result['records']['event_episodes']} episodes); video projections {result['media']['projections']}")
        print(f"assembled {result['assembled_episodes']}, player POV complete {result['complete_player_povs']}, hard-complete + spread0 {result['hard_complete_spread0_episodes']}")
        print(f"QC verdict {result['qc']['verdict']}; spread {result['qc']['spread_ticks']}; failure categories {result['qc']['failure_categories']}")
        print(f"players per episode {result['by_players']}")
    else:
        print(f"HF {result['repo']}@{result['revision']}: {result['episodes']} directories / {result['povs']} POVs")
        print(f"{result['files']['count']} files / tree bytes {result['files']['tree_bytes']}; players per episode {result['by_players']}")
        print(f"repo sha {result['repo_info']['sha']}, last_modified {result['repo_info']['last_modified']}")


def main() -> int:
    parser = argparse.ArgumentParser(description="Read-only census of a local dataset or a Hugging Face dataset branch")
    parser.add_argument("--local-dir", type=Path); parser.add_argument("--hf-repo"); parser.add_argument("--revision", default="main")
    parser.add_argument("--json-out", type=Path); parser.add_argument("--self-check", action="store_true")
    args = parser.parse_args()
    if bool(args.local_dir) == bool(args.hf_repo):
        parser.error("specify exactly one of --local-dir or --hf-repo")
    result = local_census(args.local_dir) if args.local_dir else hf_census(args.hf_repo, args.revision)
    if args.self_check:
        assert result["episodes"] >= 0 and result["povs"] >= result["episodes"]
        if result["source"] == "local":
            assert result["assembled_episodes"] <= result["episodes"]
            assert result["hard_complete_spread0_episodes"] <= result["assembled_episodes"]
    print_summary(result)
    if args.json_out:
        args.json_out.write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    sys.exit(main())
