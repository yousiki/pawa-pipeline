//! Observer camera planning: a rendered viewpoint that belongs to no player.
//!
//! A player POV is rendered by attaching ReplayMod's spectator camera to a recorded entity. An
//! observer is the other kind of capture target: a free camera whose pose is decided here and
//! handed to the Java driver as ONE KEYFRAME PER RENDERED FRAME ([`PoseDoc`]). The dense keyframes
//! are the whole trick — every interpolator ReplayMod might apply passes through its own control
//! points, so at the instants we sample, the rendered pose equals the requested pose exactly. That
//! makes the per-frame metadata contract exact by construction instead of by re-deriving
//! ReplayMod's spline in two languages.
//!
//! Tick alignment is inherited, not invented. Frame `i` sits at [`frame_time_ms`]`(i, fps)` and its
//! replay tick is that divided by [`ms_per_tick`], exactly as the player route computes them; the
//! crowd geometry the camera aims at is read from the players' own per-tick positions at
//! `world_age_tick = replay_tick + delta`, with `delta` the metadata join's conclusion
//! ([`crate::join::Join::delta`]). An observer therefore shares the players' clock without needing
//! an action stream of its own.
//!
//! Known limitation, and the reason a human must watch the result: a ReplayMod recording contains
//! only the chunks the recorded player actually received, so a camera placed far outside the source
//! POV's view distance renders fog and void rather than the world. `source_pov` therefore defaults
//! to the most central player, the camera distance is bounded by `max_distance`, and every observer
//! artifact records which replay it was rendered from.
//!
//! Pure logic: no file I/O. The caller loads each POV's tick-stamped positions into a
//! [`PlayerTrack`] and writes the resulting [`PoseDoc`] wherever the driver spec points.

use pawa_schema::driver::{PoseDoc, PoseKeyframe};
use pawa_schema::manifest::{ObserverMode, ObserverSpec};
use pawa_schema::time::WorldAgeTick;
use serde::Serialize;

use crate::frames::{frame_time_ms, ms_per_tick};

/// Poses are quantized to this step on construction, so the keyframes handed to the driver, the
/// rows written to the frame timeline and the poses recorded in the camera-path document are the
/// SAME numbers — a rounded copy and an unrounded copy of a camera path would eventually be
/// compared and disagree. 1e-4 is 0.1 mm of position and 3.6e-4 arcsec of rotation, far below what
/// any renderer resolves.
const QUANTUM: f64 = 1e-4;

/// One POV's tick-stamped positions: `(world_age_tick, x, y, z)`.
///
/// A POV with no usable positions still belongs in the slice — it stays a valid `source_pov`
/// choice, it just cannot contribute to the crowd geometry. Rows need not be sorted (they are
/// sorted here); for a tick recorded twice the LAST row wins, matching the capture loader.
#[derive(Clone, Debug, PartialEq)]
pub struct PlayerTrack {
    pub pov: String,
    pub positions: Vec<(WorldAgeTick, f64, f64, f64)>,
}

/// Crowd geometry at one frame, from the players' own per-tick positions.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct CrowdSample {
    pub frame_idx: u64,
    /// Players with a usable position at this frame's tick.
    pub count: usize,
    pub x: f64,
    pub y: f64,
    pub z: f64,
    /// Largest horizontal distance from the centre to a contributing player.
    pub radius: f64,
}

/// Everything the render stage needs to shoot one observer, plus its provenance.
#[derive(Clone, Debug, PartialEq)]
pub struct ObserverPlan {
    /// Replay whose chunks the camera looks at — the `source_pov` the spec asked for, or the most
    /// central player when it asked for none.
    pub source_pov: String,
    /// The dense camera path: exactly one keyframe per rendered frame, on the CFR grid.
    pub path: PoseDoc,
    /// Horizontal distance from the crowd centre the camera was placed at. For `crowd_orbit` the
    /// per-frame distance tracks the smoothed radius and this reports the widest of them; for
    /// `keyframes` there is no crowd and it is 0.
    pub camera_distance: f64,
    /// Widest crowd radius seen across the episode; 0 when no crowd samples were available.
    pub crowd_radius: f64,
}

/// Why no camera path could be planned. Mirrors [`crate::join::JoinUnavailable`]: a plan either
/// exists or the caller learns, in one sentence, what was missing.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ObserverError {
    pub reason: String,
}

impl std::fmt::Display for ObserverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason)
    }
}

impl std::error::Error for ObserverError {}

fn error<T>(reason: impl Into<String>) -> Result<T, ObserverError> {
    Err(ObserverError {
        reason: reason.into(),
    })
}

/// Minecraft `(yaw, pitch)` in degrees pointing from `eye` at `target`.
///
/// Minecraft's yaw is 0 at +Z and increases toward -X, so the facing vector is
/// `(-sin(yaw), cos(yaw))`; positive pitch looks down. Both conventions match the pose the driver's
/// own static-camera fallback writes.
pub fn look_at(eye: (f64, f64, f64), target: (f64, f64, f64)) -> (f64, f64) {
    let dx = target.0 - eye.0;
    let dy = target.1 - eye.1;
    let dz = target.2 - eye.2;
    let horizontal = dx.hypot(dz);
    // `-0.0` is a distinct float and `atan2(-0.0, -z)` returns -180 where +180 is meant, so a
    // camera due south of its target would report yaw -180. Adding zero collapses the signed zero.
    let yaw = (-dx + 0.0).atan2(dz).to_degrees();
    let pitch = -dy.atan2(horizontal).to_degrees();
    (yaw, pitch)
}

fn quantize(value: f64) -> f64 {
    (value / QUANTUM).round() * QUANTUM
}

fn keyframe(
    time_ms: i64,
    position: (f64, f64, f64),
    yaw: f64,
    pitch: f64,
    roll: f64,
) -> PoseKeyframe {
    PoseKeyframe {
        time_ms,
        x: quantize(position.0),
        y: quantize(position.1),
        z: quantize(position.2),
        yaw: quantize(yaw),
        pitch: quantize(pitch),
        roll: quantize(roll),
    }
}

fn median(values: &[f64]) -> f64 {
    let mut ordered = values.to_vec();
    ordered.sort_by(|a, b| a.total_cmp(b));
    let middle = ordered.len() / 2;
    if ordered.len() % 2 == 1 {
        ordered[middle]
    } else {
        (ordered[middle - 1] + ordered[middle]) / 2.0
    }
}

/// Centred moving average, clamped at both ends. A window of 0 or 1 is the identity.
fn smooth(values: &[f64], window: u32) -> Vec<f64> {
    if window <= 1 || values.len() <= 1 {
        return values.to_vec();
    }
    let half = (window / 2) as usize;
    (0..values.len())
        .map(|index| {
            let low = index.saturating_sub(half);
            let high = (index + half + 1).min(values.len());
            let span = &values[low..high];
            span.iter().sum::<f64>() / span.len() as f64
        })
        .collect()
}

/// `clamp(radius_scale * crowd_radius + margin, min_distance, max_distance)`. The maximum is the
/// void guard described in the module docs, not a framing preference.
fn clamped_distance(spec: &ObserverSpec, radius: f64) -> f64 {
    spec.max_distance.min(
        spec.min_distance
            .max(spec.radius_scale * radius + spec.margin),
    )
}

/// Camera position `distance` from `centre` at compass `bearing_deg` (0 = camera at +Z), raised.
fn polar_position(
    centre: (f64, f64, f64),
    distance: f64,
    bearing_deg: f64,
    height: f64,
) -> (f64, f64, f64) {
    let radians = bearing_deg.to_radians();
    (
        centre.0 - distance * radians.sin(),
        centre.1 + height,
        centre.2 + distance * radians.cos(),
    )
}

/// Per-frame crowd centre and radius from the players' tick-stamped positions.
///
/// Each player's last position at or before the frame's tick is used, so a player whose stream is
/// sparser than one row per tick still contributes. Players whose stream has not started yet do not
/// contribute, which is what makes a staggered-spawn episode's early frames aim at the players who
/// are actually there — those frames yield no sample at all if nobody has appeared.
pub fn crowd_samples(
    tracks: &[PlayerTrack],
    frame_count: u64,
    fps: u32,
    tick_rate_hz: u32,
    delta: i64,
) -> Vec<CrowdSample> {
    let sorted: Vec<Vec<(i64, f64, f64, f64)>> = tracks
        .iter()
        .filter(|track| !track.positions.is_empty())
        .map(|track| {
            let mut rows: Vec<(i64, f64, f64, f64)> = track
                .positions
                .iter()
                .map(|(tick, x, y, z)| (tick.get(), *x, *y, *z))
                .collect();
            // Stable sort: a tick recorded twice keeps input order, so the cursor below lands on
            // the last row for that tick.
            rows.sort_by_key(|row| row.0);
            rows
        })
        .collect();

    let mpt = ms_per_tick(tick_rate_hz);
    let mut cursors = vec![0usize; sorted.len()];
    let mut held: Vec<Option<(f64, f64, f64)>> = vec![None; sorted.len()];
    let mut samples = Vec::new();

    for frame_idx in 0..frame_count {
        let target = frame_time_ms(frame_idx, fps) / mpt + delta;
        for (index, rows) in sorted.iter().enumerate() {
            while cursors[index] < rows.len() && rows[cursors[index]].0 <= target {
                let row = rows[cursors[index]];
                held[index] = Some((row.1, row.2, row.3));
                cursors[index] += 1;
            }
        }
        let present: Vec<(f64, f64, f64)> = held.iter().flatten().copied().collect();
        if present.is_empty() {
            continue;
        }
        let count = present.len() as f64;
        let x = present.iter().map(|p| p.0).sum::<f64>() / count;
        let y = present.iter().map(|p| p.1).sum::<f64>() / count;
        let z = present.iter().map(|p| p.2).sum::<f64>() / count;
        let radius = present
            .iter()
            .map(|p| (p.0 - x).hypot(p.2 - z))
            .fold(0.0f64, f64::max);
        samples.push(CrowdSample {
            frame_idx,
            count: present.len(),
            x,
            y,
            z,
            radius,
        });
    }
    samples
}

/// Pick the recording most likely to contain the chunks the observer looks at.
///
/// ReplayMod can only render what the recorded player received, so the source replay should be the
/// most central player. Centrality is measured between medians — each player's median horizontal
/// position against the crowd's median centre — so one player's excursion to the map edge cannot
/// decide the choice. Ties break on POV name, which keeps the pick deterministic across reruns.
pub fn select_source_pov(
    samples: &[CrowdSample],
    tracks: &[PlayerTrack],
) -> Result<String, ObserverError> {
    let mut named: Vec<&PlayerTrack> = tracks.iter().collect();
    named.sort_by(|a, b| a.pov.cmp(&b.pov));
    let Some(first) = named.first() else {
        return error("no POV tracks were supplied");
    };
    if samples.is_empty() {
        return Ok(first.pov.clone());
    }
    let centre = (
        median(&samples.iter().map(|s| s.x).collect::<Vec<_>>()),
        median(&samples.iter().map(|s| s.z).collect::<Vec<_>>()),
    );
    // Strictly better wins, so the first POV in name order keeps a tie.
    let mut best: Option<(f64, &str)> = None;
    for track in &named {
        if track.positions.is_empty() {
            continue;
        }
        let xs: Vec<f64> = track.positions.iter().map(|row| row.1).collect();
        let zs: Vec<f64> = track.positions.iter().map(|row| row.3).collect();
        let score = (median(&xs) - centre.0).hypot(median(&zs) - centre.1);
        if best.is_none_or(|(previous, _)| score < previous) {
            best = Some((score, track.pov.as_str()));
        }
    }
    Ok(best.map_or_else(|| first.pov.clone(), |(_, pov)| pov.to_string()))
}

/// One fixed pose for the whole episode, aimed at the episode's median crowd centre.
///
/// The most robust mode: nothing about the pose depends on per-frame jitter, and a still camera
/// over a moving crowd is the easiest thing for a human to check against the player POVs.
fn crowd_static_path(
    spec: &ObserverSpec,
    samples: &[CrowdSample],
    frame_count: u64,
    fps: u32,
) -> Vec<PoseKeyframe> {
    let centre = (
        median(&samples.iter().map(|s| s.x).collect::<Vec<_>>()),
        median(&samples.iter().map(|s| s.y).collect::<Vec<_>>()),
        median(&samples.iter().map(|s| s.z).collect::<Vec<_>>()),
    );
    let radius = samples.iter().map(|s| s.radius).fold(0.0f64, f64::max);
    let position = polar_position(
        centre,
        clamped_distance(spec, radius),
        spec.bearing_deg,
        spec.height,
    );
    let (yaw, pitch) = look_at(position, centre);
    (0..frame_count)
        .map(|frame_idx| keyframe(frame_time_ms(frame_idx, fps), position, yaw, pitch, 0.0))
        .collect()
}

/// A bearing sweep around the smoothed per-frame crowd centre, always aimed at that centre.
fn crowd_orbit_path(
    spec: &ObserverSpec,
    samples: &[CrowdSample],
    frame_count: u64,
    fps: u32,
) -> Vec<PoseKeyframe> {
    // Frames before the first sample (nobody had spawned yet) borrow that first sample, so the
    // smoothing window sees a dense series.
    let first = samples[0];
    let mut dense = Vec::with_capacity(frame_count as usize);
    let mut cursor = 0usize;
    for frame_idx in 0..frame_count {
        while cursor + 1 < samples.len() && samples[cursor + 1].frame_idx <= frame_idx {
            cursor += 1;
        }
        dense.push(if samples[cursor].frame_idx == frame_idx {
            samples[cursor]
        } else {
            first
        });
    }
    let xs = smooth(
        &dense.iter().map(|s| s.x).collect::<Vec<_>>(),
        spec.smooth_frames,
    );
    let ys = smooth(
        &dense.iter().map(|s| s.y).collect::<Vec<_>>(),
        spec.smooth_frames,
    );
    let zs = smooth(
        &dense.iter().map(|s| s.z).collect::<Vec<_>>(),
        spec.smooth_frames,
    );
    let radii = smooth(
        &dense.iter().map(|s| s.radius).collect::<Vec<_>>(),
        spec.smooth_frames,
    );

    let span = (frame_count - 1).max(1) as f64;
    (0..frame_count)
        .map(|frame_idx| {
            let index = frame_idx as usize;
            let centre = (xs[index], ys[index], zs[index]);
            let bearing = spec.bearing_deg + spec.sweep_deg * (frame_idx as f64 / span);
            let position = polar_position(
                centre,
                clamped_distance(spec, radii[index]),
                bearing,
                spec.height,
            );
            let (yaw, pitch) = look_at(position, centre);
            keyframe(frame_time_ms(frame_idx, fps), position, yaw, pitch, 0.0)
        })
        .collect()
}

/// `end` rewritten so interpolating from `start` never takes the long way round.
fn shortest_arc(start: f64, end: f64) -> f64 {
    start + (end - start + 180.0).rem_euclid(360.0) - 180.0
}

/// Operator keyframes resampled onto the frame grid: linear position, shortest-arc yaw.
///
/// Times outside the hand-written path clamp to its first and last pose, so a path shorter than
/// the episode holds rather than extrapolates.
fn keyframe_path(spec: &ObserverSpec, frame_count: u64, fps: u32) -> Vec<PoseKeyframe> {
    let mut keys = spec.keyframes.clone();
    keys.sort_by_key(|key| key.time_ms);
    let mut resolved: Vec<PoseKeyframe> = Vec::with_capacity(keys.len());
    for key in keys {
        let yaw = match resolved.last() {
            Some(previous) => shortest_arc(previous.yaw, key.yaw),
            None => key.yaw,
        };
        resolved.push(keyframe(
            key.time_ms,
            (key.x, key.y, key.z),
            yaw,
            key.pitch,
            key.roll,
        ));
    }

    let mut poses = Vec::with_capacity(frame_count as usize);
    let mut cursor = 0usize;
    for frame_idx in 0..frame_count {
        let time_ms = frame_time_ms(frame_idx, fps);
        while cursor + 2 < resolved.len() && resolved[cursor + 1].time_ms <= time_ms {
            cursor += 1;
        }
        let low = &resolved[cursor];
        let high = &resolved[(cursor + 1).min(resolved.len() - 1)];
        let fraction = if high.time_ms <= low.time_ms || time_ms <= low.time_ms {
            0.0
        } else if time_ms >= high.time_ms {
            1.0
        } else {
            (time_ms - low.time_ms) as f64 / (high.time_ms - low.time_ms) as f64
        };
        let lerp = |a: f64, b: f64| a + (b - a) * fraction;
        poses.push(keyframe(
            time_ms,
            (
                lerp(low.x, high.x),
                lerp(low.y, high.y),
                lerp(low.z, high.z),
            ),
            lerp(low.yaw, high.yaw),
            lerp(low.pitch, high.pitch),
            lerp(low.roll, high.roll),
        ));
    }
    poses
}

/// Resolve one observer spec against an episode's player positions into a dense camera path.
///
/// `frame_count` is the caller's estimate of the rendered length and decides the keyframe count
/// exactly; over-generating is free (the driver clamps the path to the replay's real duration)
/// while under-generating would freeze the camera before the last rendered frame, so the caller
/// should pad. `delta` is the metadata join's `world_age_tick - replay_tick`.
pub fn plan_observer(
    spec: &ObserverSpec,
    tracks: &[PlayerTrack],
    frame_count: u64,
    fps: u32,
    tick_rate_hz: u32,
    delta: i64,
) -> Result<ObserverPlan, ObserverError> {
    if frame_count < 2 {
        return error(format!("frame_count must be >= 2; got {frame_count}"));
    }
    if spec.min_distance > spec.max_distance {
        return error(format!(
            "observer {:?}: min_distance {} must be <= max_distance {}",
            spec.name, spec.min_distance, spec.max_distance
        ));
    }
    match spec.mode {
        ObserverMode::Keyframes if spec.keyframes.is_empty() => {
            return error(format!(
                "observer {:?}: mode=keyframes needs keyframes",
                spec.name
            ));
        }
        ObserverMode::Keyframes => {}
        _ if !spec.keyframes.is_empty() => {
            return error(format!(
                "observer {:?}: keyframes are only used by mode=keyframes",
                spec.name
            ));
        }
        _ => {}
    }

    let crowd_pov_count = tracks.iter().filter(|t| !t.positions.is_empty()).count();
    let samples = if crowd_pov_count == 0 {
        Vec::new()
    } else {
        crowd_samples(tracks, frame_count, fps, tick_rate_hz, delta)
    };

    let source_pov = match &spec.source_pov {
        Some(pov) => {
            if !tracks.iter().any(|track| &track.pov == pov) {
                let known: Vec<&str> = tracks.iter().map(|t| t.pov.as_str()).collect();
                return error(format!(
                    "observer {:?}: source_pov {pov:?} is not one of this episode's POVs: {known:?}",
                    spec.name
                ));
            }
            pov.clone()
        }
        None => select_source_pov(&samples, tracks)?,
    };

    let keyframes = match spec.mode {
        ObserverMode::Keyframes => keyframe_path(spec, frame_count, fps),
        _ if samples.is_empty() => {
            return error(format!(
                "observer {:?}: mode={:?} needs crowd samples; no POV position stream was usable",
                spec.name, spec.mode
            ));
        }
        ObserverMode::CrowdOrbit => crowd_orbit_path(spec, &samples, frame_count, fps),
        ObserverMode::CrowdStatic => crowd_static_path(spec, &samples, frame_count, fps),
    };

    let crowd_radius = samples.iter().map(|s| s.radius).fold(0.0f64, f64::max);
    Ok(ObserverPlan {
        source_pov,
        path: PoseDoc {
            // Same provenance string the driver echoes back, so a requested path and a rendered
            // trace are directly comparable documents.
            source: Some("camera_path".into()),
            keyframes,
        },
        camera_distance: if samples.is_empty() {
            0.0
        } else {
            clamped_distance(spec, crowd_radius)
        },
        crowd_radius,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(mode: &str) -> ObserverSpec {
        serde_json::from_str(&format!(r#"{{"name": "obs00", "mode": "{mode}"}}"#))
            .expect("observer spec")
    }

    fn track(pov: &str, x: f64) -> PlayerTrack {
        PlayerTrack {
            pov: pov.into(),
            positions: (0..120)
                .map(|tick| (WorldAgeTick(1000 + tick), x, 64.0, 0.0))
                .collect(),
        }
    }

    /// Two players standing 20 blocks apart: the static camera must sit one clamped distance from
    /// their centre and look at it, and the orbit must sweep its bearing across the episode.
    #[test]
    fn plans_a_dense_path_that_frames_the_crowd() {
        let tracks = [track("p01", -10.0), track("p00", 10.0)];
        let centre = (0.0, 64.0, 0.0);

        let plan =
            plan_observer(&spec("crowd_static"), &tracks, 100, 20, 20, 1000).expect("static plan");
        assert_eq!(plan.source_pov, "p00", "ties break on POV name");
        assert_eq!(plan.path.keyframes.len(), 100);
        assert!((plan.crowd_radius - 10.0).abs() < 1e-9);
        // clamp(1.6 * 10 + 12, 16, 96) == 28
        assert!((plan.camera_distance - 28.0).abs() < 1e-9);

        let first = plan.path.keyframes[0].clone();
        for (index, pose) in plan.path.keyframes.iter().enumerate() {
            assert_eq!(pose.time_ms, frame_time_ms(index as u64, 20));
            assert_eq!(
                (pose.x, pose.y, pose.z, pose.yaw, pose.pitch),
                (first.x, first.y, first.z, first.yaw, first.pitch),
                "crowd_static holds one pose for the whole episode"
            );
        }
        let horizontal = (first.x - centre.0).hypot(first.z - centre.2);
        assert!(
            (16.0..=96.0).contains(&horizontal),
            "distance {horizontal} outside the clamp"
        );
        assert!((horizontal - 28.0).abs() < 1e-3);
        assert!(
            (first.y - (centre.1 + spec("crowd_static").height)).abs() < 1e-3,
            "camera sits `height` above the centre"
        );
        let (yaw, pitch) = look_at((first.x, first.y, first.z), centre);
        assert!((first.yaw - yaw).abs() < 1e-3 && (first.pitch - pitch).abs() < 1e-3);

        let orbit =
            plan_observer(&spec("crowd_orbit"), &tracks, 100, 20, 20, 1000).expect("orbit plan");
        assert_eq!(orbit.path.keyframes.len(), 100);
        for pair in orbit.path.keyframes.windows(2) {
            assert!(pair[1].yaw > pair[0].yaw, "bearing sweep must be monotone");
        }
        // bearing 135 → 225 puts the look-at yaw at -45 → +45, no ±180 wrap in between.
        assert!((orbit.path.keyframes[0].yaw + 45.0).abs() < 1e-3);
        assert!((orbit.path.keyframes[99].yaw - 45.0).abs() < 1e-3);
    }
}
