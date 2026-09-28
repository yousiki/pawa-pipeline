//! Shared JSON contracts — the one authority for every document that crosses a process or
//! language boundary in this pipeline.
//!
//! | Module | Contracts |
//! |---|---|
//! | [`dataset`] | what training reads: `episode.json`, `frames.jsonl`, `events.jsonl`, `qc.json` |
//! | [`capture`] | capture intermediates: action/event streams, metas, swarm summaries, the [`capture::CaptureRecord`] bridge |
//! | [`driver`] | the Java render driver's spec, status and pose documents |
//! | [`manifest`] | the human-written run manifest |
//! | [`status`] | unit status files and the run report |
//! | [`catalog`] | the scenario catalog and its closed vocabularies |
//! | [`time`] | the clock-domain newtypes every time field uses |
//!
//! [`PlayerRunConfig`] lives at the root: it predates the v2 split, is consumed verbatim by
//! `crates/players`, and is already exactly what it needs to be.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod capture;
pub mod catalog;
pub mod dataset;
pub mod driver;
pub mod manifest;
pub mod regions;
pub mod status;
pub mod time;

pub type JsonMap = BTreeMap<String, Value>;

/// The capture runtime's configuration AS WRITTEN — one JSON document, produced by the capture
/// stage (`crates/capture`) and consumed by `crates/players`.
///
/// Every field is `Option`, and DEFAULTS DO NOT LIVE HERE. They live in `Cfg::resolve` in
/// `crates/players`, beside their clamps — `shard_index` bounded by `shard_count`, `place_reach` to
/// a reachable distance, `sprint_bout_prob` to 0..=1. Splitting the default from its clamp would let
/// a JSON document specify a value the runtime then silently changed. Absent means "use the
/// runtime's default"; present means "this exact value", and if it is out of range the clamp is
/// visible in one place.
///
/// `deny_unknown_fields` is the point of the exercise: a misspelled key fails the run at startup
/// naming the field, instead of being dropped on the floor while the bot quietly uses a default.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlayerRunConfig {
    // ---- swarm shape and identity ----
    pub bot_count: Option<i64>,
    pub bot_index_offset: Option<i64>,
    pub episode_id: Option<String>,
    pub far_players: Option<i64>,
    pub shard_count: Option<i64>,
    pub shard_index: Option<i64>,
    pub shard_spread: Option<f64>,
    pub total_bot_count: Option<i64>,

    // ---- connection and filesystem ----
    pub host: Option<String>,
    pub out_dir: Option<String>,
    pub port: Option<i64>,
    /// Requested client chunk radius. Capture forwards its server view distance here.
    pub view_distance: Option<i64>,
    pub signal_dir: Option<String>,
    pub tokio_threads: Option<i64>,

    // ---- timing ----
    pub episode_duration_ms: Option<i64>,
    pub episode_timeout_margin_ms: Option<i64>,
    pub go_timeout_ms: Option<i64>,
    pub hard_timeout_ms: Option<i64>,
    pub stagger_ms: Option<i64>,

    // ---- scenario ----
    pub scenario: Option<String>,
    /// Kept a string because the runtime treats the seed as an opaque token it parses itself; a
    /// JSON number would have to be re-stringified to preserve that, so the writer sends text.
    pub scenario_seed: Option<String>,

    // ---- swarm geometry (echoed back as SwarmConfig) ----
    pub formation_radius: Option<f64>,
    pub goal_radius: Option<f64>,
    pub landmark_distance: Option<f64>,
    pub lane_spacing: Option<f64>,

    // ---- bot behaviour (echoed back as BehaviorConfig) ----
    pub action_hold_ms: Option<i64>,
    pub allow_parkour: Option<bool>,
    pub allow_sprinting: Option<bool>,
    pub inter_round_still_ms: Option<i64>,
    pub look_settle_ms: Option<i64>,
    pub look_sweep_duration_ms: Option<i64>,
    pub look_sweep_steps: Option<i64>,
    pub look_sweep_yaw_rad: Option<f64>,
    pub min_dwell_ticks: Option<i64>,
    pub move_no_progress_ms: Option<i64>,
    pub move_timeout_ms: Option<i64>,
    pub nav_mode: Option<String>,
    pub operation_pause_ms: Option<i64>,
    pub pitch_rate_deg_s: Option<f64>,
    pub yaw_rate_deg_s: Option<f64>,

    // ---- ambient behaviour, digging and placement ----
    pub ambient_idle_ticks: Option<i64>,
    pub ambient_leash: Option<f64>,
    pub ambient_switch_ticks: Option<i64>,
    pub ambient_when_finished: Option<bool>,
    pub dig_timeout_ms: Option<i64>,
    pub place_reach: Option<f64>,
    pub sprint_bout_prob: Option<f64>,
}

/// The one environment variable the capture runtime reads: the path of its [`PlayerRunConfig`].
///
/// The capture stage writes the file and sets this variable, the runtime reads it, and a rename
/// that only lands on one side is a run that cannot start.
pub const PLAYER_CONFIG_ENV: &str = "RM_CONFIG";
