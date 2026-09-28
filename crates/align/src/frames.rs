//! `frames.jsonl` assembly: one [`FrameRow`] per encoded video frame, pre-joined.
//!
//! The row count is the encoded video's (ffprobe), so the timeline is 1:1 with the frames by
//! construction. Three inputs meet here:
//!
//! * the CFR grid — pure arithmetic over `fps` ([`frame_time_ms`] is THE definition; the old
//!   pipeline had three copies whose roundings disagreed);
//! * the driver's pose trace, matched by TIME within [`MATCH_TOLERANCE_MS`] (never by index — the
//!   driver rounds its sample instants with Java's half-up, this side rounds half-even, and
//!   matching by nearest time keeps a 1 ms disagreement from becoming a whole-frame error);
//! * the dense action stream, matched by WORLD AGE via the join's delta.
//!
//! A frame past the last pose sample holds that pose (what ReplayMod itself renders past the final
//! keyframe). A frame in an interior GAP gets no pose: filling it from a neighbour would be a
//! silent approximation of a required input, and a loader can skip an honest hole but cannot see a
//! wrong pose.
//!
//! v3 rows speak solaris (docs/dataset-spec.md): body angles are converted here from
//! the capture intermediates' notchian radians to mineflayer-convention radians, `action.camera`
//! and the `hotbar.N` edge events are DIFFED between consecutive matched rows, and the wall-clock
//! triple (`renderTime`/`epochTime`/`relativeTimeMs`) is synthesized linearly from the episode's
//! `started_epoch_ms` — the capture side records no wall clock.

use std::collections::HashMap;
use std::f64::consts::{PI, TAU};

use pawa_schema::capture::ActionFrame;
use pawa_schema::dataset::{CameraPose, ExtraInfo, FrameRow, SolarisAction};
use pawa_schema::driver::PoseDoc;
use pawa_schema::time::{EpochMs, ReplayTimeMs, WorldAgeTick};
use serde::Serialize;

/// How far a pose sample may sit from a frame's grid time and still be that frame's pose.
/// Absorbs ROUNDING between Java and Rust, nothing else — deliberately far below one frame
/// interval.
pub const MATCH_TOLERANCE_MS: i64 = 2;

/// Replay time of a rendered frame: `round_half_even(i * 1000 / fps)`. THE definition of the CFR
/// frame grid — do not re-derive it. Ties round to even, and every consumer of the grid must
/// reproduce that exactly; at fps 20 the division is exact and rounding never fires.
pub fn frame_time_ms(frame_idx: u64, fps: u32) -> i64 {
    let fps = i64::from(fps.max(1));
    let numerator = frame_idx as i64 * 1000;
    let quotient = numerator / fps;
    let remainder = numerator % fps;
    match (remainder * 2).cmp(&fps) {
        std::cmp::Ordering::Less => quotient,
        std::cmp::Ordering::Greater => quotient + 1,
        // Exactly halfway: round to even.
        std::cmp::Ordering::Equal => quotient + (quotient % 2),
    }
}

/// Milliseconds per server tick.
pub fn ms_per_tick(tick_rate_hz: u32) -> i64 {
    1000 / i64::from(tick_rate_hz.max(1))
}

/// Episode-level constants the v3 row vocabulary repeats per row (solaris's shape, not ours to
/// change): the synthetic wall-clock base and the row-level identity columns.
#[derive(Clone, Copy, Debug)]
pub struct EpisodeRowCtx {
    pub fps: u32,
    pub tick_rate_hz: u32,
    /// Wall-clock instant capture began — the base of the synthetic linear wall clock.
    pub started_epoch_ms: EpochMs,
    /// Row-level integer `episode_id` (position in the run manifest).
    pub episode_seq: i64,
    /// World seed for `extra_info.seed`; 0 when the seed string is non-numeric.
    pub seed: i64,
}

/// Pose coverage of an assembled timeline — the QC evidence for camera holes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct PoseCoverage {
    pub posed_rows: u64,
    /// Frames before the first pose sample.
    pub unposed_head: u64,
    /// Frames past the last sample whose pose was HELD from it (normal encoder overshoot).
    pub held_tail: u64,
    /// Holes between samples — should be 0; anything else is a driver trace defect.
    pub unposed_interior: u64,
}

pub struct FramesOutcome {
    pub rows: Vec<FrameRow>,
    pub coverage: PoseCoverage,
    /// Frames whose world-age tick found a dense action row.
    pub action_rows_matched: u64,
}

/// Shortest signed arc between two mineflayer yaws (both in `[0, 2π)`), in `(-π, π]`.
fn wrap_arc(delta: f64) -> f64 {
    let wrapped = (delta + PI).rem_euclid(TAU) - PI;
    if wrapped == -PI { PI } else { wrapped }
}

/// Assemble the rows for one POV.
///
/// `actions` is empty for an observer (its rows then carry no player-state columns).
/// `delta` is the join's conclusion; `fov_deg` comes from the render client's pinned options.
pub fn assemble_frames(
    frame_count: u64,
    ctx: &EpisodeRowCtx,
    poses: &PoseDoc,
    actions: &[ActionFrame],
    delta: i64,
    fov_deg: f64,
) -> FramesOutcome {
    let mut keyframes = poses.keyframes.clone();
    keyframes.sort_by_key(|pose| pose.time_ms);
    let last_pose_time = keyframes.last().map(|pose| pose.time_ms);

    // First dense action row per world age. Duplicates (a tick recorded twice) keep the first.
    let mut action_by_age: HashMap<i64, &ActionFrame> = HashMap::with_capacity(actions.len());
    for action in actions {
        if let Some(age) = action.world_age_tick {
            action_by_age.entry(age.get()).or_insert(action);
        }
    }

    let mpt = ms_per_tick(ctx.tick_rate_hz);
    let mut rows = Vec::with_capacity(frame_count as usize);
    let mut cursor = 0usize;
    let mut coverage = PoseCoverage::default();
    let mut first_posed: Option<u64> = None;
    let mut last_posed: Option<u64> = None;
    let mut action_rows_matched = 0u64;
    // Previous MATCHED row's mineflayer orientation and hotbar slot: the diff bases for
    // `action.camera` and the `hotbar.N` edges. None before the first match, so neither fires on
    // the first row (blueprint: edges are events, not state).
    let mut prev_orientation: Option<(f64, f64)> = None;
    let mut prev_hotbar: Option<u8> = None;

    for frame_idx in 0..frame_count {
        let time_ms = frame_time_ms(frame_idx, ctx.fps);
        // Nearest sample, scanning forward (both sequences ascend).
        let camera_pose = if keyframes.is_empty() {
            None
        } else {
            while cursor + 1 < keyframes.len()
                && (keyframes[cursor + 1].time_ms - time_ms).abs()
                    <= (keyframes[cursor].time_ms - time_ms).abs()
            {
                cursor += 1;
            }
            let held = last_pose_time.is_some_and(|last| time_ms > last);
            if held || (keyframes[cursor].time_ms - time_ms).abs() <= MATCH_TOLERANCE_MS {
                let pose = &keyframes[cursor];
                coverage.posed_rows += 1;
                if held {
                    coverage.held_tail += 1;
                }
                first_posed.get_or_insert(frame_idx);
                last_posed = Some(frame_idx);
                Some(CameraPose {
                    x: pose.x,
                    y: pose.y,
                    z: pose.z,
                    yaw: pose.yaw,
                    pitch: pose.pitch,
                    roll: pose.roll,
                    fov_deg,
                })
            } else {
                None
            }
        };

        let replay_tick = time_ms / mpt;
        let world_age = WorldAgeTick(replay_tick + delta);
        let mut row = FrameRow {
            x: None,
            y: None,
            z: None,
            yaw: None,
            pitch: None,
            action: None,
            inventory: None,
            render_time: EpochMs(ctx.started_epoch_ms.get() + time_ms),
            epoch_time: (ctx.started_epoch_ms.get() + time_ms) as f64 / 1000.0,
            relative_time_ms: ReplayTimeMs(time_ms),
            physics_time: None,
            world_age_ticks: world_age,
            is_in_water: None,
            frame_count: frame_idx,
            episode_id: ctx.episode_seq,
            extra_info: ExtraInfo { seed: ctx.seed },
            camera_pose,
            server_tick: None,
            held_item: None,
        };

        if let Some(sample) = action_by_age.get(&world_age.get()) {
            action_rows_matched += 1;
            // Capture intermediates hold notchian RADIANS; convert without a deg round trip —
            // the v3 bar is bit parity with the captured doubles (1-ULP drift fails review).
            let yaw_mf = pawa_schema::dataset::mineflayer_yaw_from_notchian_rad(sample.yaw);
            let pitch_mf = pawa_schema::dataset::mineflayer_pitch_from_notchian_rad(sample.pitch);
            let camera = match prev_orientation {
                Some((prev_yaw, prev_pitch)) => {
                    [wrap_arc(yaw_mf - prev_yaw), pitch_mf - prev_pitch]
                }
                None => [0.0, 0.0],
            };
            let mut action = SolarisAction {
                forward: sample.keys.forward,
                back: sample.keys.back,
                left: sample.keys.left,
                right: sample.keys.right,
                jump: sample.keys.jump,
                sprint: sample.keys.sprint,
                sneak: sample.keys.sneak,
                camera,
                attack: sample.attack,
                use_item: sample.use_item,
                place_block: sample.place_block,
                mine: sample.mine,
                ..SolarisAction::default()
            };
            if let (Some(slot), true) = (
                sample.hotbar_slot,
                prev_hotbar.is_some() && prev_hotbar != sample.hotbar_slot,
            ) {
                action.set_hotbar_edge(slot);
            }
            prev_orientation = Some((yaw_mf, pitch_mf));
            if sample.hotbar_slot.is_some() {
                prev_hotbar = sample.hotbar_slot;
            }

            row.x = Some(sample.x);
            row.y = Some(sample.y);
            row.z = Some(sample.z);
            row.yaw = Some(yaw_mf);
            row.pitch = Some(pitch_mf);
            row.action = Some(action);
            row.inventory = sample.inventory.clone();
            row.physics_time = Some(sample.player_time_ms);
            row.is_in_water = Some(sample.is_in_water);
            row.server_tick = Some(sample.server_tick);
            row.held_item = sample.held_item.clone();
        }
        rows.push(row);
    }

    match (first_posed, last_posed) {
        (Some(first), Some(last)) => {
            coverage.unposed_head = first;
            coverage.unposed_interior = (last - first + 1) - coverage.posed_rows;
        }
        _ => coverage.unposed_head = frame_count,
    }
    FramesOutcome {
        rows,
        coverage,
        action_rows_matched,
    }
}

/// Trim assembled rows to the inclusive frame window `[start, end]` and renumber them as the
/// dataset's own timeline.
///
/// The v3 contract is that EVERY shipped player row carries the full solaris payload (a consumer
/// indexes `row["action"]` unconditionally), so finalize cuts all POVs — rows and videos alike —
/// to one common WORLD-TICK span, converted by the caller to each POV's own frame window. After
/// the cut, `frame_count` and `relativeTimeMs` restart at zero and the synthetic wall clock is
/// rebased onto `new_started` — the same episode base for every POV, which is honest because
/// every file's frame 0 sits on the same world tick: the shipped wall clocks are one linear
/// function of `worldAgeTicks` across files. The
/// first surviving row's diff-derived fields (`action.camera`, `hotbar.N` edges) are reset:
/// their basis row was trimmed away, and the first row of a file fires nothing.
pub fn trim_rows(rows: &mut Vec<FrameRow>, start: u64, end: u64, fps: u32, new_started: EpochMs) {
    rows.truncate(end as usize + 1);
    rows.drain(..(start as usize).min(rows.len()));
    for (idx, row) in rows.iter_mut().enumerate() {
        let time_ms = frame_time_ms(idx as u64, fps);
        row.frame_count = idx as u64;
        row.relative_time_ms = ReplayTimeMs(time_ms);
        row.render_time = EpochMs(new_started.get() + time_ms);
        row.epoch_time = (new_started.get() + time_ms) as f64 / 1000.0;
    }
    if let Some(action) = rows.first_mut().and_then(|row| row.action.as_mut()) {
        action.camera = [0.0, 0.0];
        action.clear_hotbar_edges();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ties round to even; the grid definition must reproduce that exactly.
    #[test]
    fn grid_rounding_is_half_even() {
        // fps 20: exact, no rounding.
        assert_eq!(frame_time_ms(0, 20), 0);
        assert_eq!(frame_time_ms(7, 20), 350);
        // fps 16: frame 1 is 62.5 ms — half-even gives 62 (even), Java's `Math.round` would say 63.
        assert_eq!(frame_time_ms(1, 16), 62);
        assert_eq!(frame_time_ms(3, 16), 188); // 187.5 → 188 (even)
        // fps 3: 333.33 → 333, 666.67 → 667.
        assert_eq!(frame_time_ms(1, 3), 333);
        assert_eq!(frame_time_ms(2, 3), 667);
    }

    /// Wrap behaviour of the camera diff: crossing the 0/2π seam must yield the short way round.
    #[test]
    fn camera_diff_wraps_the_seam() {
        assert!((wrap_arc(0.1) - 0.1).abs() < 1e-12);
        assert!((wrap_arc(TAU - 0.1) - (-0.1)).abs() < 1e-12);
        assert!((wrap_arc(-TAU + 0.1) - 0.1).abs() < 1e-12);
        assert_eq!(wrap_arc(PI), PI);
    }

    /// The v3 value-parity bar is BIT-level: captured doubles must reach the dataset row with
    /// identical bit patterns. Guards the two ULP leaks a code review caught — a
    /// rad→deg→rad round trip in the angle conversion, and serde_json re-reads without the
    /// `float_roundtrip` feature.
    #[test]
    fn captured_doubles_survive_to_the_row_bit_for_bit() {
        use pawa_schema::dataset::{
            mineflayer_pitch_from_notchian_rad, mineflayer_yaw_from_notchian_rad,
        };
        // Awkward doubles: no short decimal representation, not on any decimal grid.
        for &v in &[
            std::f64::consts::PI / 7.0,
            1.0 / 3.0,
            -58.479_977_617_735_116,
            0.123_456_789_012_345_67,
        ] {
            // serde_json text round trip (float_roundtrip feature) is bit-exact.
            let text = serde_json::to_string(&v).unwrap();
            let back: f64 = serde_json::from_str(&text).unwrap();
            assert_eq!(back.to_bits(), v.to_bits(), "serde_json round trip of {v}");
            // The radian-domain angle conversion adds no unit round trip: pitch is a pure
            // negation (exact), and yaw's PI-subtraction result survives its own text trip.
            assert_eq!(
                mineflayer_pitch_from_notchian_rad(v).to_bits(),
                (-v).to_bits()
            );
            let yaw = mineflayer_yaw_from_notchian_rad(v);
            let yaw_text: f64 =
                serde_json::from_str(&serde_json::to_string(&yaw).unwrap()).unwrap();
            assert_eq!(yaw_text.to_bits(), yaw.to_bits());
        }
    }

    /// The edge/diff machinery over a synthetic 4-frame timeline with a coverage gap: the first
    /// row fires nothing, a yaw turn lands as a sign-flipped mineflayer camera delta, a hotbar
    /// switch fires exactly one `hotbar.N` edge, and a row after a gap diffs against the last
    /// MATCHED row rather than inventing a delta.
    #[test]
    fn assembly_diffs_camera_and_hotbar_edges() {
        use pawa_schema::capture::ActionFrame;
        use pawa_schema::dataset::MovementKeys;
        use pawa_schema::time::{ClientTick, PlayerTimeMs, ServerTick};

        let frame = |age: i64, yaw_notchian_deg: f64, hotbar: u8| ActionFrame {
            client_tick: ClientTick(age),
            server_tick: ServerTick(age),
            world_age_tick: Some(WorldAgeTick(age)),
            player_time_ms: PlayerTimeMs(age * 50),
            x: 0.0,
            y: 64.0,
            z: 0.0,
            yaw: yaw_notchian_deg.to_radians(),
            pitch: 0.0,
            d_yaw: 0.0,
            d_pitch: 0.0,
            keys: MovementKeys::default(),
            attack: false,
            use_item: false,
            place_block: false,
            mine: false,
            hotbar_slot: Some(hotbar),
            held_item: None,
            is_in_water: true,
            inventory: None,
        };
        // Ages 0, 1, 3 — age 2 is a coverage hole.
        let actions = vec![frame(0, 0.0, 0), frame(1, 10.0, 2), frame(3, 10.0, 2)];
        let ctx = EpisodeRowCtx {
            fps: 20,
            tick_rate_hz: 20,
            started_epoch_ms: EpochMs(1_000_000),
            episode_seq: 3,
            seed: 2026,
        };
        let poses = PoseDoc {
            source: None,
            keyframes: Vec::new(),
        };
        let rows = assemble_frames(4, &ctx, &poses, &actions, 0, 70.0).rows;

        // Row 0: matched, but nothing to diff against — no edge, zero camera.
        let a0 = rows[0].action.as_ref().unwrap();
        assert_eq!(a0.camera, [0.0, 0.0]);
        assert!(!a0.hotbar_1 && !a0.hotbar_3);
        assert_eq!(rows[0].is_in_water, Some(true));
        assert_eq!(rows[0].render_time.get(), 1_000_000);
        assert_eq!(rows[0].episode_id, 3);

        // Row 1: +10° notchian turn → −10° in mineflayer radians; slot 0→2 fires hotbar.3 only.
        let a1 = rows[1].action.as_ref().unwrap();
        assert!((a1.camera[0] - (-10.0f64.to_radians())).abs() < 1e-9);
        assert!(a1.hotbar_3);
        assert!(!a1.hotbar_1 && !a1.hotbar_2);
        let expected_yaw =
            pawa_schema::dataset::mineflayer_yaw_from_notchian_rad(10.0f64.to_radians());
        assert!((rows[1].yaw.unwrap() - expected_yaw).abs() < 1e-9);
        assert_eq!(rows[1].render_time.get(), 1_000_050);

        // Row 2: the hole — no player-state columns at all.
        assert!(rows[2].action.is_none() && rows[2].x.is_none() && rows[2].is_in_water.is_none());

        // Row 3: same orientation and slot as the last MATCHED row — no delta, no edge.
        let a3 = rows[3].action.as_ref().unwrap();
        assert_eq!(a3.camera, [0.0, 0.0]);
        assert!(!a3.hotbar_3);
    }

    /// Trimming renumbers the surviving rows, rebases the synthetic wall clock, and resets the
    /// first survivor's diff-derived fields (its basis row is gone, so it fires nothing).
    #[test]
    fn trim_renumbers_rebases_and_resets_first_row() {
        use pawa_schema::capture::ActionFrame;
        use pawa_schema::dataset::MovementKeys;
        use pawa_schema::time::{ClientTick, PlayerTimeMs, ServerTick};

        let frame = |age: i64, yaw_notchian_deg: f64, hotbar: u8| ActionFrame {
            client_tick: ClientTick(age),
            server_tick: ServerTick(age),
            world_age_tick: Some(WorldAgeTick(age)),
            player_time_ms: PlayerTimeMs(age * 50),
            x: 0.0,
            y: 64.0,
            z: 0.0,
            yaw: yaw_notchian_deg.to_radians(),
            pitch: 0.0,
            d_yaw: 0.0,
            d_pitch: 0.0,
            keys: MovementKeys::default(),
            attack: false,
            use_item: false,
            place_block: false,
            mine: false,
            hotbar_slot: Some(hotbar),
            held_item: None,
            is_in_water: false,
            inventory: None,
        };
        // Row 1 turns and switches slots, so pre-trim it carries a camera delta and an edge.
        let actions = vec![frame(0, 0.0, 0), frame(1, 10.0, 2), frame(2, 10.0, 2)];
        let ctx = EpisodeRowCtx {
            fps: 20,
            tick_rate_hz: 20,
            started_epoch_ms: EpochMs(1_000_000),
            episode_seq: 0,
            seed: 1,
        };
        let poses = PoseDoc {
            source: None,
            keyframes: Vec::new(),
        };
        let mut rows = assemble_frames(3, &ctx, &poses, &actions, 0, 70.0).rows;
        assert!(rows[1].action.as_ref().unwrap().hotbar_3);

        // Keep rows 1..=2; the new base moves forward by one frame (50 ms at fps 20).
        trim_rows(&mut rows, 1, 2, 20, EpochMs(1_000_050));
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].frame_count, 0);
        assert_eq!(rows[0].relative_time_ms.get(), 0);
        assert_eq!(rows[0].render_time.get(), 1_000_050);
        assert_eq!(rows[1].frame_count, 1);
        assert_eq!(rows[1].relative_time_ms.get(), 50);
        assert_eq!(rows[1].render_time.get(), 1_000_100);
        // world_age_ticks stays absolute — it is the shared clock, not a file-local index.
        assert_eq!(rows[0].world_age_ticks.get(), 1);
        // The first survivor's diff basis is gone: no camera delta, no hotbar edge.
        let a0 = rows[0].action.as_ref().unwrap();
        assert_eq!(a0.camera, [0.0, 0.0]);
        assert!(!a0.hotbar_3);
        // Interior diffs are untouched (row 2 had no delta against row 1 anyway).
        assert_eq!(rows[1].action.as_ref().unwrap().camera, [0.0, 0.0]);
    }
}
