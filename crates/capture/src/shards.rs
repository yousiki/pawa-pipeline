//! The player shards: one `players` process per shard, and what is made of their output.
//!
//! A shard is configured by ONE JSON document and told where it is through ONE environment
//! variable. That replaced 49 `RM_*` variables passed through an inherited environment, an
//! arrangement in which any stray `RM_*` on the host leaked into the run and nine live knobs were
//! unreachable because nothing ever set them. The document is
//! [`pawa_schema::PlayerRunConfig`], read with `deny_unknown_fields`, so a key spelled wrong
//! here fails the shard at startup naming the field instead of being dropped while the bot runs on
//! a default.
//!
//! Defaults are NOT written here. Anything this side has no opinion about is omitted, and the
//! runtime's own `Cfg::resolve` supplies it beside the clamp that was always there — copying a
//! default across the boundary would create a second place to change it.

use std::collections::BTreeMap;
use std::fs::File;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::Result;
use pawa_schema::capture::{
    CaptureMeta, CapturePov, CaptureRecord, PlayerSummary, ShardRecord, SwarmStatus, SwarmSummary,
};
use pawa_schema::dataset::{RuntimeInfo, ScenarioRef, SeparationMetrics, WorldInfo};
use pawa_schema::time::DurationMs;
use pawa_schema::{PLAYER_CONFIG_ENV, PlayerRunConfig, catalog};
use serde::{Deserialize, Serialize};

use crate::proc::Child;
use crate::{CaptureOutcome, Episode, split_bots, write_json};

/// The POV artifacts an episode is complete without exception.
///
/// v2 dropped `-timeline.json`: the dense per-tick timeline the renderer used to emit is now
/// assembled by the align stage from the pose trace, so a capture that produced one would be
/// producing a file nothing reads.
const REQUIRED_POV_SUFFIXES: [&str; 4] = [".mcpr", "-actions.jsonl", "-events.jsonl", "-meta.json"];

/// Minecraft's server tick rate. Fixed by the game, not by configuration.
const TICK_RATE_HZ: u32 = 20;

/// How one shard process ended, and how much memory it took to get there.
///
/// `killed_by_us` distinguishes our own grace-deadline SIGKILL from one the kernel sent: "peaked at
/// 7.6 GiB" and "peaked at 7.6 GiB and was then killed for it" are different facts, and both arrive
/// as exit `-9`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ShardUsage {
    pub shard: u32,
    pub exit_code: Option<i32>,
    pub peak_rss_kib: Option<u64>,
    pub killed_by_us: bool,
}

/// One running shard.
pub(crate) struct Shard {
    index: u32,
    child: Child,
}

/// Launch every shard, paced by the join stagger.
pub(crate) fn spawn(episode: &Episode<'_>) -> Result<Vec<Shard>> {
    let config = episode.config;
    let plan = split_bots(episode.spec().player_count, episode.shards());
    let mut running = Vec::new();
    for (index, (offset, count)) in plan.iter().copied().enumerate() {
        if index > 0 {
            // Pace the launches at the rate the previous shard joins its bots at, so the server sees
            // one continuous trickle of logins rather than P bursts.
            let previous_bots = plan[index - 1].1 as u64;
            std::thread::sleep(Duration::from_millis(previous_bots * config.stagger_ms));
        }
        let document = shard_config(episode, index as u32, offset, count);
        // Kept as a run artifact on purpose: it is the exact input the bots ran on, so a run can be
        // reproduced or diffed without reconstructing an environment from a log.
        let config_path = episode
            .episode_dir()
            .join(format!("players-shard{index}-config.json"));
        write_json(&config_path, &document)?;

        let log = File::create(
            episode
                .episode_dir()
                .join(format!("players-shard{index}-stdout.log")),
        )?;
        let mut command = Command::new(&config.player_bin);
        // The parent environment passes through; exactly one variable is added, naming the document
        // above. Nothing else is injected.
        command
            .env(PLAYER_CONFIG_ENV, &config_path)
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log));
        let child = Child::spawn(&mut command)?;
        tracing::info!(
            shard = index,
            pid = child.pid(),
            bots = count,
            offset,
            "shard started"
        );
        running.push(Shard {
            index: index as u32,
            child,
        });
    }
    Ok(running)
}

/// One shard's `PlayerRunConfig`.
fn shard_config(
    episode: &Episode<'_>,
    shard_index: u32,
    bot_index_offset: u32,
    bot_count: u32,
) -> PlayerRunConfig {
    let config = episode.config;
    let spec = episode.spec();
    // The manifest's behaviour block IS a `PlayerRunConfig`, so it is the base document and this
    // side overwrites only what it decides. Absent stays absent.
    let mut document = spec.player_behavior.clone();

    let margin_ms = (config.episode_timeout_margin_sec * 1000.0) as i64;
    let duration_ms = if spec.episode_duration_sec > 0.0 {
        (spec.episode_duration_sec * 1000.0).round() as i64
    } else {
        0
    };
    // A multi-bot swarm gets the longer floor: more bots means more of the episode is spent joining
    // and pathing before the scenario proper starts.
    let base_hard_timeout_ms = if spec.player_count > 1 {
        240_000
    } else {
        180_000
    };
    document.hard_timeout_ms = Some(if duration_ms > 0 {
        base_hard_timeout_ms.max(duration_ms + margin_ms)
    } else {
        base_hard_timeout_ms
    });
    if duration_ms > 0 {
        document.episode_duration_ms = Some(duration_ms);
        document.episode_timeout_margin_ms = Some(margin_ms);
    }

    document.host = Some("127.0.0.1".into());
    document.port = Some(config.server_port as i64);
    document.view_distance = Some(i64::from(config.view_distance));
    document.episode_id = Some(spec.episode_id.clone());
    document.out_dir = Some(episode.episode_dir().display().to_string());
    document.signal_dir = Some(episode.signal_dir.display().to_string());
    document.go_timeout_ms = Some(120_000);
    if spec.scenario.name == pawa_schema::regions::SCENARIO {
        // Regional staging probes and verifies every exterior slot before replay begins.
        // Its linear setup time must not consume the players' ordinary ready-to-go deadline.
        document.go_timeout_ms = Some(120_000 + i64::from(spec.player_count) * 500);
    }

    document.lane_spacing = spec.swarm.lane_spacing.or(document.lane_spacing);
    document.landmark_distance = spec.swarm.landmark_distance.or(document.landmark_distance);
    document.formation_radius = spec.swarm.formation_radius.or(document.formation_radius);
    document.goal_radius = spec.swarm.goal_radius.or(document.goal_radius);

    document.scenario = Some(spec.scenario.name.clone());
    // A string, because the runtime resolves the seed from an opaque token (a number is used
    // directly, anything else is hashed with the episode id).
    document.scenario_seed = spec
        .scenario
        .seed
        .map(|seed| seed.to_string())
        .or(document.scenario_seed);
    document.far_players = Some(spec.scenario.far_players as i64);

    document.bot_count = Some(bot_count as i64);
    document.shard_index = Some(shard_index as i64);
    document.shard_count = Some(episode.shards() as i64);
    document.bot_index_offset = Some(bot_index_offset as i64);
    document.total_bot_count = Some(spec.player_count as i64);
    // The same value this side paces launches with.
    document.stagger_ms = Some(config.stagger_ms as i64);

    document
}

/// Wait for every shard, then take its exit code and peak RSS.
///
/// Peak, never final, and taken from `wait4` rather than `/proc`: a spike between two samples is
/// invisible to sampling, and a dead child has no `/proc` entry at all — which is exactly what made
/// an earlier OOM series unreadable from its own tail.
pub(crate) fn reap(episode: &Episode<'_>, shards: &mut Vec<Shard>, outcome: &mut CaptureOutcome) {
    let deadline = Instant::now() + Duration::from_secs_f64(episode.config.shard_grace_sec);
    for shard in shards.iter_mut() {
        let remaining = deadline
            .saturating_duration_since(Instant::now())
            .max(Duration::from_millis(500));
        let mut reaped = shard.child.wait(remaining);
        let killed_by_us = reaped.exit_code.is_none();
        if killed_by_us {
            tracing::warn!(
                shard = shard.index,
                "shard outlived its grace period; killing it"
            );
            reaped = shard.child.kill_group();
        }
        tracing::info!(
            shard = shard.index,
            exit = ?reaped.exit_code,
            peak_rss_kib = ?reaped.peak_rss_kib,
            killed_by_us,
            "shard reaped"
        );
        outcome.shard_usage.push(ShardUsage {
            shard: shard.index,
            exit_code: reaped.exit_code,
            peak_rss_kib: reaped.peak_rss_kib,
            killed_by_us,
        });
    }
    shards.clear();
    outcome.peak_rss_kib = outcome
        .shard_usage
        .iter()
        .filter_map(|usage| usage.peak_rss_kib)
        .max();
}

/// Merge the shard summaries, validate the episode, and write `capture.json`.
pub(crate) fn finish(episode: &Episode<'_>, outcome: &mut CaptureOutcome) -> Result<()> {
    let swarm = merge_swarm_summaries(episode)?;
    if let Some(summary) = swarm.as_ref() {
        outcome.connected_players = summary
            .players
            .iter()
            .filter(|player| player.status == pawa_schema::capture::BotStatus::Ok)
            .map(|player| player.username.clone())
            .collect();
        outcome.missing_players = episode
            .usernames
            .iter()
            .filter(|user| !outcome.connected_players.contains(user))
            .cloned()
            .collect();
        let rates = capture_rates(summary, &episode.usernames);
        if !rates.is_empty() {
            let mean = rates.iter().sum::<f64>() / rates.len() as f64;
            outcome.capture_tick_hz_mean = Some(round2(mean));
            outcome.capture_tick_hz_min =
                Some(round2(rates.iter().copied().fold(f64::INFINITY, f64::min)));
        }
    }
    outcome.failure_class = validate(episode, outcome, swarm.as_ref());
    outcome.ok = outcome.failure_class.is_none();
    write_json(
        &episode.episode_dir().join("capture.json"),
        &capture_record(episode, outcome, swarm.as_ref()),
    )?;
    Ok(())
}

/// Every expected player's capture rate, frames per second of episode.
fn capture_rates(summary: &SwarmSummary, expected: &[String]) -> Vec<f64> {
    summary
        .players
        .iter()
        .filter(|player| expected.contains(&player.username))
        .filter(|player| player.actual_duration_ms.get() > 0)
        .map(|player| {
            player.dense_frames as f64 / (player.actual_duration_ms.get() as f64 / 1000.0)
        })
        .collect()
}

/// The first thing wrong with this episode, or `None` when it is complete.
fn validate(
    episode: &Episode<'_>,
    outcome: &CaptureOutcome,
    swarm: Option<&SwarmSummary>,
) -> Option<String> {
    if let Some(failure) = outcome.failures.first() {
        return Some(failure.code.clone());
    }
    if outcome.shard_usage.len() != episode.shards() as usize
        || outcome
            .shard_usage
            .iter()
            .any(|usage| usage.killed_by_us || usage.exit_code != Some(0))
    {
        return Some("bot_process_failed".into());
    }
    let Some(summary) = swarm else {
        return Some("swarm_summary_missing".into());
    };
    let mut ok_players: Vec<&String> = summary
        .players
        .iter()
        .filter(|player| player.status == pawa_schema::capture::BotStatus::Ok)
        .map(|player| &player.username)
        .collect();
    ok_players.sort();
    let mut expected: Vec<&String> = episode.usernames.iter().collect();
    expected.sort();
    if ok_players != expected {
        return Some("swarm_incomplete".into());
    }
    for pov in outcome.pov_map.keys() {
        let missing: Vec<&str> = REQUIRED_POV_SUFFIXES
            .into_iter()
            .filter(|suffix| {
                let path = episode.episode_dir().join(format!("{pov}{suffix}"));
                !path
                    .metadata()
                    .is_ok_and(|meta| meta.is_file() && meta.len() > 0)
            })
            .collect();
        if !missing.is_empty() {
            tracing::error!(%pov, ?missing, "pov artifacts missing");
            return Some("pov_artifacts_missing".into());
        }
    }
    // A server that fell behind produces sparse action rows; no alignment recovers ticks that were
    // never recorded, so the episode is rejected rather than shipped thin.
    let min_hz = episode.config.min_capture_tick_hz;
    if min_hz > 0.0
        && outcome
            .capture_tick_hz_mean
            .is_some_and(|mean| mean < min_hz)
    {
        return Some("capture_tick_rate_too_low".into());
    }
    None
}

/// `capture.json` — the bridge to every downstream unit: clock anchors, the POV inventory,
/// provenance.
fn capture_record(
    episode: &Episode<'_>,
    outcome: &CaptureOutcome,
    swarm: Option<&SwarmSummary>,
) -> CaptureRecord {
    let spec = episode.spec();
    let scenario_name = spec.scenario.name.clone();
    let family = scenario_family(&scenario_name);
    let actual_duration_ms = swarm
        .map(|summary| {
            summary
                .players
                .iter()
                .map(|player| player.actual_duration_ms.get())
                .max()
                .unwrap_or(0)
        })
        .unwrap_or(0);
    CaptureRecord {
        episode_id: spec.episode_id.clone(),
        run_id: outcome.run_id.clone(),
        started_epoch_ms: outcome.started_epoch_ms,
        mc_version: episode.config.minecraft_version.clone(),
        tick_rate_hz: TICK_RATE_HZ,
        world: WorldInfo {
            seed: outcome.world.level_seed.clone(),
            level_type: outcome.world.level_type.clone(),
        },
        scenario: ScenarioRef {
            name: scenario_name,
            family,
            seed: swarm
                .map(|summary| summary.swarm.scenario_seed)
                .or(spec.scenario.seed)
                .unwrap_or(0),
            scenery: spec.scenario.scenery.clone(),
        },
        runtime: RuntimeInfo {
            family: swarm
                .map(|summary| summary.runtime_family.clone())
                .unwrap_or_else(|| "azalea".into()),
            topology: swarm
                .map(|summary| summary.runtime_topology.clone())
                .unwrap_or_else(|| "single_process_multi_player".into()),
            azalea_version: azalea_version(episode).unwrap_or_else(|| "unknown".into()),
        },
        target_duration_ms: swarm
            .and_then(|summary| summary.target_duration_ms)
            .or_else(|| {
                (spec.episode_duration_sec > 0.0)
                    .then(|| DurationMs((spec.episode_duration_sec * 1000.0).round() as i64))
            }),
        actual_duration_ms: DurationMs(actual_duration_ms),
        recording_start_world_age_tick: outcome.recording_start_world_age_tick,
        recording_stop_world_age_tick: outcome.recording_stop_world_age_tick,
        recording_stop_world_age_by_player: outcome.stop_world_age_tick_by_player.clone(),
        povs: outcome
            .pov_map
            .iter()
            .map(|(pov, username)| CapturePov {
                name: pov.clone(),
                username: username.clone(),
                player_id: player_id(swarm, username, &outcome.pov_map, pov),
                // A POV with no `.mcpr` has nothing to render, and render units skip it by design.
                connected: outcome.recordings.contains_key(pov),
            })
            .collect(),
        shards: outcome
            .shard_usage
            .iter()
            .map(|usage| ShardRecord {
                index: usage.shard,
                // The record's types are not optional: an unreaped shard reports -1, which is not a
                // wait status any process can produce, and its peak is 0 rather than a guess.
                exit_code: usage.exit_code.unwrap_or(-1),
                peak_rss_kib: usage.peak_rss_kib.unwrap_or(0),
            })
            .collect(),
    }
}

/// The behaviour family of a catalog scenario name.
fn scenario_family(name: &str) -> String {
    catalog::scenario(name)
        .map(|info| info.family.to_string())
        .unwrap_or_else(|| "unknown".into())
}

/// The runtime's own player id for a username, as the shard reported it.
fn player_id(
    swarm: Option<&SwarmSummary>,
    username: &str,
    pov_map: &BTreeMap<String, String>,
    pov: &str,
) -> String {
    swarm
        .and_then(|summary| {
            summary
                .players
                .iter()
                .find(|player| player.username == username)
                .map(|player: &PlayerSummary| player.player_id.clone())
        })
        .unwrap_or_else(|| {
            // A bot that never reported still gets its planned id, derived the way the runtime
            // derives it: `player<global index>`.
            let index = pov_map.keys().position(|name| name == pov).unwrap_or(0);
            format!("player{index}")
        })
}

/// The protocol library version, read from whichever POV metadata exists.
fn azalea_version(episode: &Episode<'_>) -> Option<String> {
    for pov in episode.pov_map().keys() {
        let path = episode.episode_dir().join(format!("{pov}-meta.json"));
        if let Ok(text) = std::fs::read_to_string(&path)
            && let Ok(meta) = serde_json::from_str::<CaptureMeta>(&text)
        {
            return Some(meta.azalea_version);
        }
    }
    None
}

/// Merge the per-shard summaries into the canonical flat `swarm-summary.json`.
///
/// A single-shard swarm already wrote the canonical file; it is only read back. For P > 1 the parts
/// are concatenated in shard order.
///
/// One field cannot be merged as it stands: `separation_metrics` is computed per shard, over that
/// shard's own bots, and nesting a `{"per_shard": {...}}` map where a `SeparationMetrics` belongs
/// would break the typed contract. The counts here are therefore summed and their ratios recomputed over the summed denominator, while
/// the distance percentiles are dropped: a median of medians is not a median, and a fabricated one
/// would be read as a real cross-swarm separation figure. Nearest-neighbour MINIMUM survives,
/// because the minimum of per-shard minima IS the minimum over the shards.
fn merge_swarm_summaries(episode: &Episode<'_>) -> Result<Option<SwarmSummary>> {
    let canonical = episode.episode_dir().join("swarm-summary.json");
    if episode.shards() <= 1 {
        return Ok(read_summary(&canonical));
    }
    let mut parts: Vec<SwarmSummary> = (0..episode.shards())
        .filter_map(|index| {
            read_summary(
                &episode
                    .episode_dir()
                    .join(format!("swarm-summary-shard{index}.json")),
            )
        })
        .collect();
    if parts.is_empty() {
        return Ok(None);
    }
    parts.sort_by_key(|part| part.shard_index);

    let total = if parts[0].total_bot_count > 0 {
        parts[0].total_bot_count
    } else {
        parts.iter().map(|part| part.bot_count).sum()
    };
    let players: Vec<PlayerSummary> = parts.iter().flat_map(|part| part.players.clone()).collect();
    let ok_count = players
        .iter()
        .filter(|player| player.status == pawa_schema::capture::BotStatus::Ok)
        .count() as u32;
    let complete = ok_count == total && parts.len() as u32 == episode.shards();

    let mut merged = parts[0].clone();
    // The merged document describes the whole swarm, so the per-shard coordinates are neutral.
    merged.shard_index = 0;
    merged.bot_index_offset = 0;
    merged.shard_count = parts.len() as u32;
    merged.bot_count = total;
    merged.total_bot_count = total;
    merged.overall_status = if complete {
        SwarmStatus::Complete
    } else {
        SwarmStatus::Partial
    };
    merged.low_motion_players = parts
        .iter()
        .flat_map(|part| part.low_motion_players.clone())
        .collect();
    merged.separation_metrics = merge_separation(&parts);
    merged.players = players;

    write_json(&canonical, &merged)?;
    Ok(Some(merged))
}

fn merge_separation(parts: &[SwarmSummary]) -> SeparationMetrics {
    let sum = |get: fn(&SeparationMetrics) -> u64| -> u64 {
        parts.iter().map(|part| get(&part.separation_metrics)).sum()
    };
    let evaluated = sum(|metrics| metrics.evaluated_player_frames);
    let crowd = sum(|metrics| metrics.crowd_frame_count);
    let overlap = sum(|metrics| metrics.overlap_frame_count);
    let ratio = |count: u64| (evaluated > 0).then(|| count as f64 / evaluated as f64);
    SeparationMetrics {
        crowd_frame_count: crowd,
        crowd_frame_ratio: ratio(crowd),
        evaluated_player_frames: evaluated,
        median_nearest_neighbor_2d: None,
        min_nearest_neighbor_2d: parts
            .iter()
            .filter_map(|part| part.separation_metrics.min_nearest_neighbor_2d)
            .min_by(f64::total_cmp),
        overlap_frame_count: overlap,
        overlap_frame_ratio: ratio(overlap),
        p05_nearest_neighbor_2d: None,
        p95_nearest_neighbor_2d: None,
    }
}

fn read_summary(path: &std::path::Path) -> Option<SwarmSummary> {
    let text = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str(&text) {
        Ok(summary) => Some(summary),
        Err(error) => {
            tracing::error!(path = %path.display(), %error, "swarm summary unparseable");
            None
        }
    }
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use pawa_schema::capture::{BotStatus, ResolvedBehavior, ResolvedSwarm};
    use pawa_schema::dataset::MotionMetrics;
    use pawa_schema::manifest::EpisodeSpec;

    fn spec(bots: u32, shards: u32) -> EpisodeSpec {
        serde_json::from_value(serde_json::json!({
            "episode_id": "ep-1",
            "player_count": bots,
            "shards": shards,
            "episode_duration_sec": 300.0,
            "scenario": {"name": "landmark_tour"},
        }))
        .expect("spec")
    }

    fn episode<'a>(
        spec: &'a EpisodeSpec,
        config: &'a crate::CaptureConfig,
        request: &'a crate::EpisodeRequest<'a>,
    ) -> Episode<'a> {
        Episode {
            config,
            request,
            signal_dir: request.episode_dir.join(".signals"),
            world: crate::WorldConfig::from_spec(&spec.world).expect("world"),
            usernames: crate::expected_usernames(spec.player_count),
        }
    }

    fn player(username: &str, status: BotStatus, frames: u64, duration_ms: i64) -> PlayerSummary {
        PlayerSummary {
            player_id: format!("player-{username}"),
            username: username.into(),
            status,
            error_code: None,
            dense_frames: frames,
            duration_met: Some(true),
            actual_duration_ms: DurationMs(duration_ms),
            motion_metrics: MotionMetrics {
                dense_frames: frames,
                idle_frame_ratio: 0.1,
                low_motion: false,
                move_run_median_ticks: None,
                move_runs_under_200ms_frac: None,
                moving_frame_ratio: 0.9,
                path_length_2d: 100.0,
                pitch_coverage_rad: 1.0,
                xz_span: 50.0,
                yaw_coverage_rad: 2.0,
            },
        }
    }

    fn summary(shard_index: u32, players: Vec<PlayerSummary>, total: u32) -> SwarmSummary {
        SwarmSummary {
            episode_id: "ep-1".into(),
            shard_index,
            shard_count: 2,
            bot_count: players.len() as u32,
            bot_index_offset: shard_index * players.len() as u32,
            total_bot_count: total,
            overall_status: SwarmStatus::Complete,
            target_duration_ms: Some(DurationMs(300_000)),
            hard_timeout_ms: DurationMs(420_000),
            runtime_family: "azalea".into(),
            runtime_topology: "single_process_multi_player".into(),
            low_motion_players: vec![format!("CapBot{shard_index}")],
            separation_metrics: SeparationMetrics {
                crowd_frame_count: 10,
                crowd_frame_ratio: Some(0.1),
                evaluated_player_frames: 100,
                median_nearest_neighbor_2d: Some(4.0 + shard_index as f64),
                min_nearest_neighbor_2d: Some(1.0 + shard_index as f64),
                overlap_frame_count: 2,
                overlap_frame_ratio: Some(0.02),
                p05_nearest_neighbor_2d: Some(2.0),
                p95_nearest_neighbor_2d: Some(9.0),
            },
            behavior: ResolvedBehavior {
                action_hold_ms: 1200,
                allow_parkour: false,
                allow_sprinting: true,
                inter_round_still_ms: 1500,
                look_settle_ms: 500,
                look_sweep_duration_ms: 1800,
                look_sweep_steps: 24,
                look_sweep_yaw_rad: 0.45,
                min_dwell_ticks: 4,
                move_no_progress_ms: 8000,
                move_timeout_ms: 30000,
                nav_mode: "planned".into(),
                operation_pause_ms: 700,
                pitch_rate_deg_s: 45.0,
                yaw_rate_deg_s: 72.0,
            },
            swarm: ResolvedSwarm {
                formation_radius: 8.0,
                goal_radius: 2.0,
                landmark_count: 3,
                landmark_distance: 9.0,
                lane_spacing: 8.0,
                scenario: "landmark_tour".into(),
                scenario_requested: "landmark_tour".into(),
                scenario_seed: 7,
            },
            players,
        }
    }

    #[test]
    fn shard_documents_carry_only_what_this_side_decides() {
        let spec = spec(20, 2);
        let config = crate::CaptureConfig {
            view_distance: 24,
            ..Default::default()
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let request = crate::EpisodeRequest {
            spec: &spec,
            run_id: "run-1",
            episode_dir: dir.path(),
        };
        let episode = episode(&spec, &config, &request);
        let document = shard_config(&episode, 1, 10, 10);

        assert_eq!(document.bot_count, Some(10));
        assert_eq!(document.bot_index_offset, Some(10));
        assert_eq!(document.total_bot_count, Some(20));
        assert_eq!(document.shard_index, Some(1));
        assert_eq!(document.shard_count, Some(2));
        assert_eq!(document.stagger_ms, Some(300));
        assert_eq!(
            document.view_distance,
            Some(i64::from(config.view_distance))
        );
        // 300s + 120s margin clears the 240s multi-bot floor.
        assert_eq!(document.hard_timeout_ms, Some(420_000));
        assert_eq!(document.episode_duration_ms, Some(300_000));
        // Untouched behaviour knobs stay absent so the runtime's own default applies.
        assert_eq!(document.nav_mode, None);
        assert_eq!(document.place_reach, None);
        // The document round-trips through the runtime's own `deny_unknown_fields` reader.
        let text = serde_json::to_string(&document).expect("serialize");
        let parsed: PlayerRunConfig = serde_json::from_str(&text).expect("deny_unknown_fields");
        assert_eq!(parsed, document);
    }

    #[test]
    fn a_single_bot_swarm_gets_the_shorter_floor() {
        let spec: EpisodeSpec = serde_json::from_value(serde_json::json!({
            "episode_id": "ep-1",
            "player_count": 1,
            "shards": 1,
            "episode_duration_sec": 0.0,
            "scenario": {"name": "landmark_tour"},
        }))
        .expect("spec");
        let config = crate::CaptureConfig::default();
        let dir = tempfile::tempdir().expect("tempdir");
        let request = crate::EpisodeRequest {
            spec: &spec,
            run_id: "run-1",
            episode_dir: dir.path(),
        };
        let document = shard_config(&episode(&spec, &config, &request), 0, 0, 1);
        assert_eq!(document.hard_timeout_ms, Some(180_000));
        assert_eq!(document.episode_duration_ms, None);
    }

    #[test]
    fn merging_shards_concatenates_players_and_sums_separation() {
        let spec = spec(4, 2);
        let config = crate::CaptureConfig::default();
        let dir = tempfile::tempdir().expect("tempdir");
        let request = crate::EpisodeRequest {
            spec: &spec,
            run_id: "run-1",
            episode_dir: dir.path(),
        };
        let episode = episode(&spec, &config, &request);
        write_json(
            &dir.path().join("swarm-summary-shard0.json"),
            &summary(
                0,
                vec![
                    player("CapBot0", BotStatus::Ok, 6000, 300_000),
                    player("CapBot1", BotStatus::Ok, 6000, 300_000),
                ],
                4,
            ),
        )
        .expect("write");
        write_json(
            &dir.path().join("swarm-summary-shard1.json"),
            &summary(
                1,
                vec![
                    player("CapBot2", BotStatus::Ok, 6000, 300_000),
                    player("CapBot3", BotStatus::Error, 10, 300_000),
                ],
                4,
            ),
        )
        .expect("write");

        let merged = merge_swarm_summaries(&episode)
            .expect("merge")
            .expect("summary");
        assert_eq!(merged.players.len(), 4);
        assert_eq!(merged.bot_count, 4);
        assert_eq!(merged.total_bot_count, 4);
        assert_eq!(merged.shard_index, 0);
        assert_eq!(merged.bot_index_offset, 0);
        // One bot errored, so the swarm is partial.
        assert_eq!(merged.overall_status, SwarmStatus::Partial);
        assert_eq!(merged.low_motion_players, ["CapBot0", "CapBot1"]);
        // Counts sum, ratios are recomputed, percentiles are dropped rather than averaged.
        assert_eq!(merged.separation_metrics.evaluated_player_frames, 200);
        assert_eq!(merged.separation_metrics.crowd_frame_count, 20);
        assert_eq!(merged.separation_metrics.crowd_frame_ratio, Some(0.1));
        assert_eq!(merged.separation_metrics.min_nearest_neighbor_2d, Some(1.0));
        assert_eq!(merged.separation_metrics.median_nearest_neighbor_2d, None);
        // The merged document is what the canonical file now holds.
        let reread = read_summary(&dir.path().join("swarm-summary.json")).expect("canonical");
        assert_eq!(reread, merged);
        // Capture rate: 6000 dense frames over 300s is 20 Hz.
        let rates = capture_rates(&merged, &episode.usernames);
        assert_eq!(rates.len(), 4);
        assert_eq!(rates[0], 20.0);
    }
}
