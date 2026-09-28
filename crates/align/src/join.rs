//! The metadata join: placing a rendered replay timeline on the world-age clock.
//!
//! Re-homed from the pyo3 bridge (`crates/core`, deleted) without semantic change. One delta —
//! `world_age_tick - replay_tick` — is chosen from the available anchors, best first:
//!
//! 1. `stop_anchored_frames`: the per-player recording-stop world age minus the rendered frame
//!    count. Preferred because it anchors at the end the recording actually has.
//! 2. `recording_start_world_age`: world age at `replay start`, shared across the episode.
//! 3. `go_world_age`: world age latched at the `go` signal. Least preferred — the recording
//!    preroll before `go` is unverified; `qc.json`'s `anchor` records which one was used.
//!
//! The chosen delta is the CONCLUSION and the only number a consumer may read; rejected candidates
//! stay inside `candidate_deltas` and only there. (An earlier top-level mirror of the rejected
//! start anchor — off by up to 134 ticks where the conclusion was off by at most 4 — was read by
//! the HUD by name. The mirror is gone; do not reintroduce one.)

use std::collections::{BTreeMap, HashMap};

use pawa_schema::capture::ActionEvent;
use pawa_schema::dataset::{AlignmentQc, AlignmentStatus, AnchorMode};
use pawa_schema::time::{ReplayTick, WorldAgeTick};
use serde::Serialize;

use crate::frames::{frame_time_ms, ms_per_tick};

/// The world-age anchors one POV's capture produced.
#[derive(Clone, Copy, Debug, Default)]
pub struct Anchors {
    pub go_world_age_tick: Option<WorldAgeTick>,
    pub recording_start_world_age_tick: Option<WorldAgeTick>,
    /// Per-player stop age when available, else the episode-wide one.
    pub recording_stop_world_age_tick: Option<WorldAgeTick>,
}

/// One join failure. The finalize unit surfaces `message` verbatim in the episode's error
/// text; the structured counts live in [`AlignmentQc`].
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Issue {
    pub code: &'static str,
    pub subject: String,
    pub message: String,
}

/// Per-event outcome, archived under `diagnostics/` — training reads frames, not this.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct EventAlignment {
    pub action_id: String,
    pub action_type: String,
    pub world_age_tick: Option<WorldAgeTick>,
    pub replay_tick: Option<ReplayTick>,
    pub matched_frame_idx: Option<u64>,
    pub status: EventMatch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventMatch {
    /// The event's replay tick is exactly a rendered frame's tick.
    Exact,
    /// Fell within `tick_rate * 10` ticks past the last frame: the recording stopped first.
    DroppedTrailing,
    /// Outside the rendered range entirely.
    OutOfRange,
    /// A grid hole (only possible when fps < tick rate).
    MissingReplayTick,
    /// The event carries no world-age tick.
    Unavailable,
}

/// Render display phase, in ticks: the rendered picture lags the action record by the
/// client→server→recording round trip. Conservative PROVEN LOWER BOUND — see the measurement
/// history on [`pawa_schema::dataset::AlignmentQc::render_phase_ticks`]. Applied to the
/// tick→frame DISPLAY mapping only; the recording-start delta keeps its meaning.
pub const RENDER_PHASE_TICKS: i64 = 3;

/// The join's full outcome for one POV.
#[derive(Clone, Debug)]
pub struct Join {
    /// `world_age_tick - replay_tick` at the recording start.
    pub delta: i64,
    /// The delta for mapping world-age ticks onto DISPLAYED frames: `delta - render phase`.
    /// Frames assembly and event→frame mapping use this one.
    pub display_delta: i64,
    pub qc: AlignmentQc,
    pub events: Vec<EventAlignment>,
    pub failures: Vec<Issue>,
}

/// Why no join was possible at all (distinct from a join that ran and failed validation).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct JoinUnavailable {
    pub reason: String,
}

pub fn metadata_join(
    events: &[ActionEvent],
    frame_count: u64,
    fps: u32,
    tick_rate_hz: u32,
    max_error_ticks: u32,
    anchors: Anchors,
) -> Result<Join, JoinUnavailable> {
    metadata_join_with_phase(
        events,
        frame_count,
        fps,
        tick_rate_hz,
        max_error_ticks,
        anchors,
        RENDER_PHASE_TICKS,
    )
}

/// [`metadata_join`] with an explicit render phase — 0 reproduces the uncompensated v1 mapping
/// (the golden regression uses that).
#[allow(clippy::too_many_arguments)]
pub fn metadata_join_with_phase(
    events: &[ActionEvent],
    frame_count: u64,
    fps: u32,
    tick_rate_hz: u32,
    max_error_ticks: u32,
    anchors: Anchors,
    render_phase_ticks: i64,
) -> Result<Join, JoinUnavailable> {
    if events.is_empty() {
        return Err(JoinUnavailable {
            reason: "action event stream is empty".into(),
        });
    }
    if frame_count == 0 {
        return Err(JoinUnavailable {
            reason: "rendered video has no frames".into(),
        });
    }

    // Anchor selection, best first. The stop anchor places replay tick 0 such that the LAST frame
    // lands on the stop age — measured in TICKS, so the rendered frame count converts through
    // `tick_rate / fps` (they cancel at fps == tick rate, which is every run so far; a 30 fps
    // render of a 20 Hz world spans 2/3 as many ticks as frames).
    let stop_anchored = anchors
        .recording_stop_world_age_tick
        .map(|stop| stop.get() - (frame_count * u64::from(tick_rate_hz) / u64::from(fps)) as i64);
    let candidates = BTreeMap::from([
        (
            "go_world_age".to_string(),
            anchors.go_world_age_tick.map(WorldAgeTick::get),
        ),
        (
            "recording_start_world_age".to_string(),
            anchors
                .recording_start_world_age_tick
                .map(WorldAgeTick::get),
        ),
        ("stop_anchored_frames".to_string(), stop_anchored),
    ]);
    let (anchor, delta) = if let Some(delta) = stop_anchored {
        (AnchorMode::StopAnchoredFrames, delta)
    } else if let Some(start) = anchors.recording_start_world_age_tick {
        (AnchorMode::RecordingStartWorldAge, start.get())
    } else if let Some(go) = anchors.go_world_age_tick {
        (AnchorMode::GoWorldAge, go.get())
    } else {
        return Err(JoinUnavailable {
            reason: "no recording anchor is available".into(),
        });
    };

    // The rendered frame grid, as replay ticks. Exact map — no nearest-frame guessing.
    let mut frame_by_tick: HashMap<i64, u64> = HashMap::with_capacity(frame_count as usize);
    let mut first_tick = i64::MAX;
    let mut last_tick = i64::MIN;
    for frame_idx in 0..frame_count {
        let tick = frame_time_ms(frame_idx, fps) / ms_per_tick(tick_rate_hz);
        frame_by_tick.insert(tick, frame_idx);
        first_tick = first_tick.min(tick);
        last_tick = last_tick.max(tick);
    }
    let trailing_window = i64::from(tick_rate_hz) * 10;

    let mut aligned = Vec::with_capacity(events.len());
    let mut failures = Vec::new();
    let mut matched = 0u64;
    let mut unavailable = 0u64;
    let mut dropped_trailing = 0u64;

    let display_delta = delta - render_phase_ticks;
    for event in events {
        let subject = event.action_id.clone();
        let outcome = match event.world_age_tick {
            None => {
                unavailable += 1;
                failures.push(Issue {
                    code: "world_age_tick_unavailable",
                    subject: subject.clone(),
                    message: format!(
                        "{}: missing world_age_tick for strict metadata join",
                        event.action_type
                    ),
                });
                EventAlignment {
                    action_id: event.action_id.clone(),
                    action_type: event.action_type.clone(),
                    world_age_tick: None,
                    replay_tick: None,
                    matched_frame_idx: None,
                    status: EventMatch::Unavailable,
                }
            }
            Some(age) => {
                let replay_tick = age.get() - display_delta;
                let (status, frame) = if let Some(frame) = frame_by_tick.get(&replay_tick) {
                    matched += 1;
                    (EventMatch::Exact, Some(*frame))
                } else if replay_tick > last_tick && replay_tick <= last_tick + trailing_window {
                    dropped_trailing += 1;
                    (EventMatch::DroppedTrailing, None)
                } else {
                    unavailable += 1;
                    let status = if replay_tick < first_tick || replay_tick > last_tick {
                        EventMatch::OutOfRange
                    } else {
                        EventMatch::MissingReplayTick
                    };
                    failures.push(Issue {
                        code: "frame_grid_unmatched",
                        subject: subject.clone(),
                        message: format!(
                            "{}: replay_tick {replay_tick} could not be mapped exactly ({status:?})",
                            event.action_type
                        ),
                    });
                    (status, None)
                };
                EventAlignment {
                    action_id: event.action_id.clone(),
                    action_type: event.action_type.clone(),
                    world_age_tick: Some(age),
                    replay_tick: Some(ReplayTick(replay_tick)),
                    matched_frame_idx: frame,
                    status,
                }
            }
        };
        aligned.push(outcome);
    }

    let qc = AlignmentQc {
        status: if failures.is_empty() {
            AlignmentStatus::Applied
        } else {
            AlignmentStatus::Failed
        },
        anchor,
        delta_world_age_minus_replay_tick: delta,
        render_phase_ticks,
        candidate_deltas: candidates,
        actions_matched: matched,
        actions_unavailable: unavailable,
        dropped_trailing_count: dropped_trailing,
        validation_tolerance_frames: i64::from(max_error_ticks.max(1)) * i64::from(fps)
            / i64::from(tick_rate_hz.max(1)),
    };
    Ok(Join {
        delta,
        display_delta,
        qc,
        events: aligned,
        failures,
    })
}
