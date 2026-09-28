//! Capture-stage intermediates: written by the `players` runtime, read by the align stage,
//! archived under `diagnostics/`. Never read by training.
//!
//! Per shard, into the episode's capture directory:
//!
//! ```text
//! <pov>-actions.jsonl      ActionFrame per line — dense per-tick control stream
//! <pov>-events.jsonl       ActionEvent per line — discrete scripted actions
//! <pov>-meta.json          CaptureMeta — per-bot outcome and clock anchors
//! swarm-summary[-shardK].json   SwarmSummary — per-shard QC source
//! ```
//!
//! Versus the v1 shapes: per-row constants (`episode_id`, `player_id`, `runtime_*`,
//! `schema_version`) are gone — the filename carries the POV and `CaptureMeta` carries the rest;
//! derivable per-tick quantities (`vx/vy/vz`, `move_dir`, `look_yaw/look_pitch`, `on_ground`) and
//! the wall clock (`epoch_ms`) are gone per the field audit; and every "gametime" alias is now
//! spelled `world_age_tick`, in its newtype.

use serde::{Deserialize, Serialize};

use crate::JsonMap;
use crate::dataset::{MotionMetrics, MovementKeys, SeparationMetrics};
use crate::time::{ClientTick, DurationMs, PlayerTimeMs, ServerTick, WorldAgeTick};

/// One line of `<pov>-actions.jsonl`: the control state one bot held for one tick.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActionFrame {
    pub client_tick: ClientTick,
    pub server_tick: ServerTick,
    /// World age latched from the most recent `SetTime` packet; `None` before the first one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub world_age_tick: Option<WorldAgeTick>,
    /// Bot-process monotonic clock. Diagnostic only.
    pub player_time_ms: PlayerTimeMs,
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub yaw: f64,
    pub pitch: f64,
    /// Camera yaw change applied this tick, radians.
    pub d_yaw: f64,
    /// Camera pitch change applied this tick, radians.
    pub d_pitch: f64,
    pub keys: MovementKeys,
    /// Edge semantics (one true row per swing), like `use`/`place_block` — solaris parity.
    pub attack: bool,
    #[serde(rename = "use")]
    pub use_item: bool,
    pub place_block: bool,
    /// Sustained (held) semantics, like solaris: true on EVERY tick a block break is in
    /// progress, from mine start to break/abort. Rows from earlier collection
    /// rounds carry the old single-tick pulse instead.
    pub mine: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hotbar_slot: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub held_item: Option<String>,
    /// True when the bot's body touches water this tick (azalea `Physics::is_in_water`).
    /// Defaulted so pre-v3 intermediates (golden fixtures) still parse.
    #[serde(default)]
    pub is_in_water: bool,
    /// Full inventory snapshot this tick, already in the dataset's solaris shape — the assembly
    /// copies it onto the frame row verbatim. `None` only in pre-v3 intermediates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inventory: Option<crate::dataset::InventorySnapshot>,
}

/// One line of `<pov>-events.jsonl`: a discrete scripted action.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActionEvent {
    /// Monotonic per-bot sequence number.
    pub seq: u64,
    pub action_id: String,
    /// Scenario-defined action type (`place_block`, `attack_peer`, ...); open vocabulary.
    pub action_type: String,
    /// Event kind discriminator as the scenario engine emits it.
    #[serde(rename = "type")]
    pub kind: String,
    pub client_tick: ClientTick,
    pub server_tick: ServerTick,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub world_age_tick: Option<WorldAgeTick>,
    pub player_time_ms: PlayerTimeMs,
    /// Set when the action should be visibly identifiable in the rendered video.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visible_event_id: Option<String>,
    pub payload: ActionPayload,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ActionPayload {
    pub description: String,
    #[serde(flatten)]
    pub extra: JsonMap,
}

/// How one bot's episode ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BotStatus {
    Ok,
    Error,
}

/// `<pov>-meta.json` — one bot's outcome and, crucially, its clock anchors. Every anchor states
/// its clock domain in its name and type; the metadata join reads these to place the replay
/// timeline on the world-age clock.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CaptureMeta {
    pub episode_id: String,
    pub player_id: String,
    pub username: String,
    /// Scenario that actually ran (after `auto` resolution).
    pub scenario: String,
    /// Scenario the manifest asked for.
    pub scenario_requested: String,
    pub scenario_seed: u32,
    pub status: BotStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    pub step_errors: Vec<String>,
    /// Runtime provenance, surfaced into `episode.json` by finalize.
    pub runtime_family: String,
    pub azalea_version: String,

    /// Dense action rows recorded.
    pub dense_frames: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_met: Option<bool>,
    pub actual_duration_ms: DurationMs,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_duration_ms: Option<DurationMs>,

    // ---- clock anchors ----
    /// Client tick when the `go` signal fired; the zero of `*_tick_since_go` fields.
    pub go_client_tick_base: ClientTick,
    /// World age when `go` fired — the join's last-resort anchor.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub go_world_age_tick: Option<WorldAgeTick>,
    /// Client ticks from `go` to episode end.
    pub end_tick_since_go: i64,
    /// World age at episode end.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_world_age_tick: Option<WorldAgeTick>,
    /// World age of the last recorded action row.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_frame_world_age_tick: Option<WorldAgeTick>,
    /// Bot-process clock of the last recorded action row. Diagnostic only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_frame_player_time_ms: Option<PlayerTimeMs>,
    /// Client ticks from `go` to the first `place_block` event, when any — a visual anchor for
    /// spot-checking alignment by eye.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub place_tick_since_go: Option<i64>,
    /// World age of that first `place_block` event.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub place_world_age_tick: Option<WorldAgeTick>,

    /// Condensed event list for quick inspection without parsing the events file.
    pub actions: Vec<CondensedAction>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CondensedAction {
    pub action_type: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub server_tick: ServerTick,
}

/// `swarm-summary[-shardK].json` — one shard's QC source, merged across shards by finalize.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SwarmSummary {
    pub episode_id: String,
    pub shard_index: u32,
    pub shard_count: u32,
    /// Bots this shard hosted.
    pub bot_count: u32,
    /// Global index of this shard's first bot.
    pub bot_index_offset: u32,
    /// Bots across all shards of the episode.
    pub total_bot_count: u32,
    pub overall_status: SwarmStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_duration_ms: Option<DurationMs>,
    pub hard_timeout_ms: DurationMs,
    pub runtime_family: String,
    /// Process topology, e.g. `"single_process_multi_player"`.
    pub runtime_topology: String,
    /// Usernames flagged by the low-motion heuristic.
    pub low_motion_players: Vec<String>,
    pub separation_metrics: SeparationMetrics,
    /// Behaviour knobs the runtime actually resolved (defaults + clamps applied) — the echo that
    /// lets a run's effective configuration be read from its artifacts.
    pub behavior: ResolvedBehavior,
    /// Swarm geometry as resolved.
    pub swarm: ResolvedSwarm,
    pub players: Vec<PlayerSummary>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SwarmStatus {
    /// Every bot finished `ok`.
    Complete,
    /// At least one bot did not.
    Partial,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlayerSummary {
    pub player_id: String,
    pub username: String,
    pub status: BotStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    pub dense_frames: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_met: Option<bool>,
    pub actual_duration_ms: DurationMs,
    pub motion_metrics: MotionMetrics,
}

/// Behaviour configuration as resolved by the runtime's `Cfg::resolve`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResolvedBehavior {
    pub action_hold_ms: u64,
    pub allow_parkour: bool,
    pub allow_sprinting: bool,
    pub inter_round_still_ms: u64,
    pub look_settle_ms: u64,
    pub look_sweep_duration_ms: u64,
    pub look_sweep_steps: u64,
    pub look_sweep_yaw_rad: f64,
    pub min_dwell_ticks: u64,
    pub move_no_progress_ms: u64,
    pub move_timeout_ms: u64,
    pub nav_mode: String,
    pub operation_pause_ms: u64,
    pub pitch_rate_deg_s: f64,
    pub yaw_rate_deg_s: f64,
}

/// Swarm geometry as resolved by the runtime.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResolvedSwarm {
    pub formation_radius: f64,
    pub goal_radius: f64,
    pub landmark_count: u64,
    pub landmark_distance: f64,
    pub lane_spacing: f64,
    pub scenario: String,
    pub scenario_requested: String,
    pub scenario_seed: u32,
}

/// `diagnostics/capture.json` — the bridge between the capture unit and everything downstream:
/// clock anchors, the POV inventory, and episode provenance. Written once by the capture unit;
/// read by render units (connected set, anchors) and finalize (everything).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CaptureRecord {
    pub episode_id: String,
    pub run_id: String,
    /// Wall-clock instant capture began.
    pub started_epoch_ms: crate::time::EpochMs,
    pub mc_version: String,
    pub tick_rate_hz: u32,
    pub world: crate::dataset::WorldInfo,
    pub scenario: crate::dataset::ScenarioRef,
    pub runtime: crate::dataset::RuntimeInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_duration_ms: Option<DurationMs>,
    /// Longest per-player actual duration — the episode's effective length.
    pub actual_duration_ms: DurationMs,
    /// World age when `replay start` was issued, shared across the episode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recording_start_world_age_tick: Option<WorldAgeTick>,
    /// Episode-wide stop age; per-player values take precedence for the join.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recording_stop_world_age_tick: Option<WorldAgeTick>,
    /// Per-player stop age, keyed by username — the preferred stop anchor (recordings stop one
    /// player at a time).
    pub recording_stop_world_age_by_player: std::collections::BTreeMap<String, WorldAgeTick>,
    pub povs: Vec<CapturePov>,
    pub shards: Vec<ShardRecord>,
}

/// One planned bot in the capture inventory.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CapturePov {
    /// Canonical `pNN` name.
    pub name: String,
    pub username: String,
    pub player_id: String,
    /// False when the bot never connected — its render is skipped by design.
    pub connected: bool,
}

/// One players-shard process outcome.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ShardRecord {
    pub index: u32,
    pub exit_code: i32,
    pub peak_rss_kib: u64,
}
