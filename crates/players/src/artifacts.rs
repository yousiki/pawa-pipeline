use crate::*;

use pawa_schema::capture::{
    ActionFrame, BotStatus, CaptureMeta, CondensedAction, PlayerSummary, ResolvedBehavior,
    ResolvedSwarm, SwarmStatus, SwarmSummary,
};
use pawa_schema::catalog::pov_name;
use pawa_schema::dataset::{MotionMetrics, MovementKeys, SeparationMetrics};
use pawa_schema::time::DurationMs;
use serde::Serialize;

fn write_jsonl<T: Serialize>(path: &PathBuf, rows: &[T]) {
    let mut out = String::new();
    for row in rows {
        if let Ok(serialized) = serde_json::to_string(row) {
            out.push_str(&serialized);
            out.push('\n');
        }
    }
    let _ = fs::write(path, out);
}

fn bot_status(status: &str) -> BotStatus {
    if status == "ok" {
        BotStatus::Ok
    } else {
        BotStatus::Error
    }
}

pub(crate) fn write_bot_artifacts(inner: &Arc<BotInner>, shared: &Shared) {
    let cfg = &shared.inner.cfg;
    let engine = &shared.inner.engine;
    let _ = fs::create_dir_all(&cfg.out_dir);
    let pov = pov_name(cfg.index_offset + inner.index, cfg.total_bot_count);

    let events = inner.events.lock();
    let frames = inner.frames.lock();
    let end_tick = inner.offset_tick();
    let actual = inner.episode_actual_duration_ms.load(Ordering::SeqCst);
    let last_frame = frames.last();

    write_jsonl(&cfg.out_dir.join(format!("{pov}-events.jsonl")), &events);
    write_jsonl(&cfg.out_dir.join(format!("{pov}-actions.jsonl")), &frames);

    let place = events
        .iter()
        .find(|event| event.action_type == "place_block");
    let meta = CaptureMeta {
        episode_id: cfg.episode_id.clone(),
        player_id: inner.player_id.clone(),
        username: inner.username.clone(),
        scenario: engine.name.clone(),
        scenario_requested: engine.requested.clone(),
        scenario_seed: engine.seed,
        status: bot_status(&inner.status.lock()),
        error_code: inner.error_code.lock().clone(),
        step_errors: inner.step_errors.lock().clone(),
        runtime_family: RUNTIME_FAMILY.to_string(),
        azalea_version: AZALEA_VERSION.to_string(),
        dense_frames: frames.len() as u64,
        duration_met: *inner.duration_met.lock(),
        actual_duration_ms: DurationMs(actual),
        target_duration_ms: (cfg.episode_duration_ms > 0)
            .then_some(DurationMs(cfg.episode_duration_ms)),
        go_client_tick_base: ClientTick(inner.go_tick_base.load(Ordering::SeqCst)),
        go_world_age_tick: (*inner.go_world_age.lock()).map(WorldAgeTick),
        end_tick_since_go: end_tick,
        end_world_age_tick: inner.world_age_tick().map(WorldAgeTick),
        last_frame_world_age_tick: last_frame.and_then(|frame| frame.world_age_tick),
        last_frame_player_time_ms: last_frame.map(|frame| frame.player_time_ms),
        place_tick_since_go: place.map(|event| event.server_tick.get()),
        place_world_age_tick: place.and_then(|event| event.world_age_tick),
        actions: events
            .iter()
            .map(|event| CondensedAction {
                action_type: event.action_type.clone(),
                kind: event.kind.clone(),
                server_tick: event.server_tick,
            })
            .collect(),
    };
    let _ = fs::write(
        cfg.out_dir.join(format!("{pov}-meta.json")),
        serde_json::to_string_pretty(&meta).unwrap_or_default(),
    );
}

fn quantile(sorted: &[f64], q: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let pos = (sorted.len() as f64 - 1.0) * q;
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    let value = if lo == hi {
        sorted[lo]
    } else {
        sorted[lo] + (sorted[hi] - sorted[lo]) * (pos - lo as f64)
    };
    Some(r3(value))
}

fn separation_metrics(states: &[Arc<BotInner>]) -> SeparationMetrics {
    let mut by_tick: HashMap<i64, Vec<(f64, f64)>> = HashMap::new();
    for state in states {
        for (tick, x, z) in state.sep_samples.lock().iter() {
            by_tick.entry(*tick).or_default().push((*x, *z));
        }
    }
    let mut nearest = Vec::new();
    let mut evaluated_player_frames = 0u64;
    let mut overlap_frame_count = 0u64;
    let mut crowd_frame_count = 0u64;
    for positions in by_tick.values() {
        if positions.len() < 2 {
            continue;
        }
        for i in 0..positions.len() {
            let mut best = f64::INFINITY;
            for j in 0..positions.len() {
                if i == j {
                    continue;
                }
                let dx = positions[i].0 - positions[j].0;
                let dz = positions[i].1 - positions[j].1;
                best = best.min((dx * dx + dz * dz).sqrt());
            }
            if best.is_finite() {
                evaluated_player_frames += 1;
                nearest.push(best);
                if best < 0.8 {
                    overlap_frame_count += 1;
                }
                if best < 2.0 {
                    crowd_frame_count += 1;
                }
            }
        }
    }
    nearest.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    SeparationMetrics {
        crowd_frame_count,
        crowd_frame_ratio: (evaluated_player_frames > 0)
            .then(|| r_ratio(crowd_frame_count as f64 / evaluated_player_frames as f64)),
        evaluated_player_frames,
        median_nearest_neighbor_2d: quantile(&nearest, 0.50),
        min_nearest_neighbor_2d: nearest.first().map(|value| r3(*value)),
        overlap_frame_count,
        overlap_frame_ratio: (evaluated_player_frames > 0)
            .then(|| r_ratio(overlap_frame_count as f64 / evaluated_player_frames as f64)),
        p05_nearest_neighbor_2d: quantile(&nearest, 0.05),
        p95_nearest_neighbor_2d: quantile(&nearest, 0.95),
    }
}

/// The 8-way direction label a frame's keys encode; `move_churn` counts runs of these. Derived
/// here because the frame itself no longer stores a label the keys already determine.
pub(crate) fn key_dir(keys: &MovementKeys) -> &'static str {
    match (keys.forward, keys.back, keys.left, keys.right) {
        (true, false, false, false) => "forward",
        (true, false, true, false) => "forward_left",
        (true, false, false, true) => "forward_right",
        (false, true, false, false) => "back",
        (false, true, true, false) => "back_left",
        (false, true, false, true) => "back_right",
        (false, false, true, false) => "left",
        (false, false, false, true) => "right",
        _ => "none",
    }
}

fn motion_metrics(frames: &[ActionFrame]) -> MotionMetrics {
    if frames.is_empty() {
        return MotionMetrics {
            dense_frames: 0,
            idle_frame_ratio: 1.0,
            low_motion: true,
            move_run_median_ticks: None,
            move_runs_under_200ms_frac: None,
            moving_frame_ratio: 0.0,
            path_length_2d: 0.0,
            pitch_coverage_rad: 0.0,
            xz_span: 0.0,
            yaw_coverage_rad: 0.0,
        };
    }
    let mut min_x = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut min_z = f64::INFINITY;
    let mut max_z = f64::NEG_INFINITY;
    let mut path = 0.0;
    let mut moving = 0;
    let mut prev: Option<(f64, f64)> = None;
    let mut min_yaw = f64::INFINITY;
    let mut max_yaw = f64::NEG_INFINITY;
    let mut min_pitch = f64::INFINITY;
    let mut max_pitch = f64::NEG_INFINITY;
    for frame in frames {
        min_x = min_x.min(frame.x);
        max_x = max_x.max(frame.x);
        min_z = min_z.min(frame.z);
        max_z = max_z.max(frame.z);
        if let Some((px, pz)) = prev {
            let step = ((frame.x - px).powi(2) + (frame.z - pz).powi(2)).sqrt();
            path += step;
            if step > 0.05 {
                moving += 1;
            }
        }
        prev = Some((frame.x, frame.z));
        min_yaw = min_yaw.min(frame.yaw);
        max_yaw = max_yaw.max(frame.yaw);
        min_pitch = min_pitch.min(frame.pitch);
        max_pitch = max_pitch.max(frame.pitch);
    }
    let xz_span = ((max_x - min_x).powi(2) + (max_z - min_z).powi(2)).sqrt();
    let moving_ratio = if frames.len() > 1 {
        moving as f64 / (frames.len() - 1) as f64
    } else {
        0.0
    };
    let yaw_coverage = max_yaw - min_yaw;
    let pitch_coverage = max_pitch - min_pitch;
    let low_motion = (xz_span < 8.0 && path < 20.0) || (moving_ratio < 0.05 && yaw_coverage < 1.0);
    let move_dirs = frames
        .iter()
        .map(|frame| key_dir(&frame.keys))
        .collect::<Vec<_>>();
    let (move_run_median_ticks, move_runs_under_200ms_frac) = move_churn(&move_dirs);
    MotionMetrics {
        dense_frames: frames.len() as u64,
        idle_frame_ratio: r_ratio(1.0 - moving_ratio),
        low_motion,
        move_run_median_ticks,
        move_runs_under_200ms_frac: move_runs_under_200ms_frac.map(r_ratio),
        moving_frame_ratio: r_ratio(moving_ratio),
        path_length_2d: r3(path),
        pitch_coverage_rad: r4(pitch_coverage),
        xz_span: r3(xz_span),
        yaw_coverage_rad: r4(yaw_coverage),
    }
}

pub(crate) fn move_churn(move_dirs: &[&str]) -> (Option<f64>, Option<f64>) {
    let mut runs = Vec::new();
    let mut current = None;
    for &direction in move_dirs {
        match current {
            Some((label, count)) if label == direction => current = Some((label, count + 1)),
            _ => {
                if let Some((label, count)) = current.take()
                    && label != "none"
                {
                    runs.push(count);
                }
                current = Some((direction, 1));
            }
        }
    }
    if let Some((label, count)) = current
        && label != "none"
    {
        runs.push(count);
    }
    if runs.is_empty() {
        return (None, None);
    }
    runs.sort_unstable();
    let mid = runs.len() / 2;
    let median = if runs.len() % 2 == 1 {
        runs[mid] as f64
    } else {
        (runs[mid - 1] + runs[mid]) as f64 / 2.0
    };
    let under = runs.iter().filter(|&&count| count < 4).count() as f64 / runs.len() as f64;
    (Some(median), Some(under))
}

pub(crate) fn resolved_swarm(cfg: &Cfg, engine: &ScenarioEngine) -> ResolvedSwarm {
    ResolvedSwarm {
        formation_radius: cfg.formation_radius,
        goal_radius: cfg.goal_radius,
        landmark_count: cfg.landmarks.len() as u64,
        landmark_distance: cfg.landmark_distance,
        lane_spacing: cfg.lane_spacing,
        scenario: engine.name.clone(),
        scenario_requested: engine.requested.clone(),
        scenario_seed: engine.seed,
    }
}

pub(crate) fn resolved_behavior(cfg: &Cfg) -> ResolvedBehavior {
    ResolvedBehavior {
        action_hold_ms: cfg.action_hold_ms,
        allow_parkour: cfg.allow_parkour,
        allow_sprinting: cfg.allow_sprinting,
        inter_round_still_ms: cfg.inter_round_still_ms,
        look_settle_ms: cfg.look_settle_ms,
        look_sweep_duration_ms: cfg.look_sweep_duration_ms,
        look_sweep_steps: cfg.look_sweep_steps,
        look_sweep_yaw_rad: cfg.look_sweep_yaw_rad,
        min_dwell_ticks: cfg.min_dwell_ticks as u64,
        move_no_progress_ms: cfg.move_no_progress_ms,
        move_timeout_ms: cfg.move_timeout_ms,
        nav_mode: cfg.nav_mode.clone(),
        operation_pause_ms: cfg.operation_pause_ms,
        pitch_rate_deg_s: cfg.pitch_rate_deg_s,
        yaw_rate_deg_s: cfg.yaw_rate_deg_s,
    }
}

pub(crate) fn write_swarm_summary(shared: &Shared, ok: usize) {
    let cfg = &shared.inner.cfg;
    let engine = &shared.inner.engine;
    let mut low_motion_players = Vec::new();
    let players = shared
        .inner
        .states
        .iter()
        .map(|state| {
            let frames = state.frames.lock();
            let motion_metrics = motion_metrics(&frames);
            if motion_metrics.low_motion {
                low_motion_players.push(state.username.clone());
            }
            PlayerSummary {
                player_id: state.player_id.clone(),
                username: state.username.clone(),
                status: bot_status(&state.status.lock()),
                error_code: state.error_code.lock().clone(),
                dense_frames: frames.len() as u64,
                duration_met: *state.duration_met.lock(),
                actual_duration_ms: DurationMs(
                    state.episode_actual_duration_ms.load(Ordering::SeqCst),
                ),
                motion_metrics,
            }
        })
        .collect();
    let summary = SwarmSummary {
        episode_id: cfg.episode_id.clone(),
        shard_index: cfg.shard_index as u32,
        shard_count: cfg.shard_count as u32,
        bot_count: cfg.bot_count as u32,
        bot_index_offset: cfg.index_offset as u32,
        total_bot_count: cfg.total_bot_count as u32,
        overall_status: if ok == cfg.bot_count {
            SwarmStatus::Complete
        } else {
            SwarmStatus::Partial
        },
        target_duration_ms: (cfg.episode_duration_ms > 0)
            .then_some(DurationMs(cfg.episode_duration_ms)),
        hard_timeout_ms: DurationMs(cfg.effective_hard_timeout_ms as i64),
        runtime_family: RUNTIME_FAMILY.to_string(),
        runtime_topology: cfg.runtime_topology.clone(),
        low_motion_players,
        separation_metrics: separation_metrics(&shared.inner.states),
        behavior: resolved_behavior(cfg),
        swarm: resolved_swarm(cfg, engine),
        players,
    };
    let _ = fs::create_dir_all(&cfg.out_dir);
    let filename = if cfg.shard_count > 1 {
        format!("swarm-summary-shard{}.json", cfg.shard_index)
    } else {
        "swarm-summary.json".to_string()
    };
    let _ = fs::write(
        cfg.out_dir.join(filename),
        serde_json::to_string_pretty(&summary).unwrap_or_default(),
    );
}
