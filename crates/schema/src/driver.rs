//! The render driver's contracts — the only JSON that crosses the Rust↔Java boundary.
//!
//! The Java mod (`assets/renderer/replaymod-driver-mod`) reads exactly one
//! environment variable, [`DRIVER_SPEC_ENV`], naming a [`DriverSpec`] file; its presence activates
//! the mod. The mod writes back a status file ([`DriverStatus`]) that the render unit polls, and a
//! pose trace ([`PoseDoc`]) recording where the camera actually was. Java parses with Gson and has
//! **no defaults on the reading side by design** — every field here is required there, so adding a
//! field means updating the mod in the same change.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The one environment variable the render driver reads: the path of its [`DriverSpec`].
pub const DRIVER_SPEC_ENV: &str = "RM_RENDER_SPEC";

/// Filename of the pose trace the driver writes next to its rendered media.
pub const POSE_TRACE_NAME: &str = "poses.json";

/// Everything one render client launch needs, in one archived JSON document.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DriverSpec {
    /// Status file the driver writes and the render unit polls.
    pub status_path: String,
    pub fps: u32,
    pub encoding: RenderEncoding,
    /// Also render ReplayMod's float depth buffer as per-frame PNGs.
    pub depth: bool,
    pub projections: Vec<DriverProjection>,
    pub camera: CameraArgs,
    /// Entity id to follow, overriding the POV lookup. -1 = resolve from the job's `player_id`.
    pub spectate_entity_id: i64,
    /// Ticks to wait after reaching the menu before opening the replay.
    pub open_delay_ticks: u32,
    /// Ticks to wait for the replay to become ready before failing the job.
    pub ready_timeout_ticks: u32,
    /// Milliseconds to let the world load before sampling begins.
    pub world_load_ms: u32,
    /// The material kit every capture bot carries, in hotbar slot order (see
    /// `catalog::hotbar_kit`). The protocol never syncs a spectated remote player's container to
    /// the replay client, so the driver paints this kit into the HUD's hotbar and derives the
    /// selected slot from the (synced) mainhand equipment — the nine items are distinct, so the
    /// mapping is exact.
    pub hud_hotbar: Vec<HudHotbarSlot>,
    /// The replays this client session renders, in order — all jobs share one Minecraft boot.
    pub jobs: Vec<DriverJob>,
}

/// One hotbar slot of the capture kit, as the render driver's HUD reconstruction needs it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HudHotbarSlot {
    /// Registry id without the `minecraft:` prefix, e.g. `oak_planks`.
    pub item: String,
    pub count: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum RenderEncoding {
    /// Packed video via ReplayMod's custom-ffmpeg path.
    Mp4,
    /// Raw PNG frames, benchmark-only; never a dataset render.
    Png,
}

/// One projection as the driver reads it: codec choices already resolved into a verbatim ffmpeg
/// fragment. Deliberately narrower than the manifest-side projection spec.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DriverProjection {
    /// Projection name, `[a-z0-9_]+`; becomes the output filename stem.
    pub name: String,
    pub method: RenderMethod,
    pub width: u32,
    pub height: u32,
    /// A codec-and-options fragment passed through to ffmpeg verbatim, e.g.
    /// `"libx264 -preset veryfast -crf 20"` — not a bare codec name.
    pub encoder: String,
    /// ReplayMod anti-aliasing constant, e.g. `"NONE"`.
    pub anti_aliasing: String,
    /// Horizontal spherical field of view, degrees. Meaningful for spherical methods only.
    pub spherical_fov_x: u32,
    /// Vertical spherical field of view, degrees.
    pub spherical_fov_y: u32,
}

/// ReplayMod render method constants this pipeline uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RenderMethod {
    /// Flat perspective render.
    Default,
    /// 360° equirectangular projection; gets spherical metadata injected after encode.
    Equirectangular,
}

/// Camera behaviour for a client launch.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CameraArgs {
    pub mode: CameraMode,
    /// Permit the static fallback when the spectator camera cannot attach. Off by default: a video
    /// whose camera never follows the player is not acceptable POV output — the job must fail as
    /// `camera_attach_failed` instead.
    pub allow_static: bool,
    /// Free-camera pose used when nothing is followed and no anchor is available.
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub yaw: f64,
    pub pitch: f64,
    /// Offset of an anchored static camera from its anchor, so a replay with no followable player
    /// still frames recorded content rather than empty sky.
    pub dy: f64,
    pub dz: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum CameraMode {
    /// Attach the spectator camera to the job's POV owner (the normal player-POV path).
    Auto,
    /// Explicit synonym of `auto`, kept because the Java side accepts both.
    Spectator,
    /// Free camera at the configured pose; only valid for renders that want no player.
    Static,
}

/// One replay the driver opens, and where its output goes. Exactly one of `player_id` (spectator
/// POV) or `camera_path` (observer) is set.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DriverJob {
    pub replay_path: String,
    pub out_path: String,
    /// POV owner — the ONLY entity the spectator camera may attach to. Absent for an observer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub player_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub episode_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub render_id: Option<String>,
    /// The POV's first recorded position — the driver's second-choice camera anchor when the
    /// recording carries no spawn packet for the POV owner's own UUID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_pos: Option<[f64; 3]>,
    /// Path of an observer's dense camera-path document ([`PoseDoc`] shape, one keyframe per
    /// rendered frame so interpolation is a no-op). Mutually exclusive with `player_id`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub camera_path: Option<String>,
}

// ---- what the driver writes back ----

/// The driver's status file, rewritten as the session progresses. The render unit polls it and
/// decides success from the per-job statuses, not from the client's exit code.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DriverStatus {
    pub phase: DriverPhase,
    /// Client ticks elapsed since the driver started.
    pub tick: u64,
    pub job_count: u32,
    /// Index of the job currently being processed.
    pub job_index: u32,
    /// Session-level failure taxonomy entry, when the whole session failed.
    pub failure_class: Option<String>,
    pub failure_message: Option<String>,
    /// Session-level detail mirror of the last-processed job. Read per-job detail from `jobs`.
    #[serde(default)]
    pub detail: BTreeMap<String, serde_json::Value>,
    pub jobs: Vec<DriverJobStatus>,
}

/// The driver's session phase machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DriverPhase {
    WaitMenu,
    Opening,
    WaitReady,
    Rendering,
    Done,
    Failed,
}

/// Per-job outcome inside [`DriverStatus`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DriverJobStatus {
    pub index: u32,
    pub player_id: Option<String>,
    pub replay_path: String,
    pub out_path: String,
    /// Job state as the Java side names it (`pending`/`rendering`/`done`/`failed`). Kept a string:
    /// the vocabulary is owned by the mod.
    pub status: String,
    pub failure_class: Option<String>,
    pub failure_message: Option<String>,
    /// Per-job detail: `camera_mode`, `entity_id_used`, `entity_id_source`, ...
    #[serde(default)]
    pub detail: BTreeMap<String, serde_json::Value>,
}

/// A camera pose document: an observer's INPUT camera path and a POV render's OUTPUT pose trace
/// share this one shape, so the requested path and the rendered truth are directly comparable.
///
/// Poses are keyed by `time_ms`, never by frame index — Java and the consumer round frame times
/// independently, and matching by nearest time keeps a 1 ms rounding disagreement from becoming a
/// whole-frame error.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PoseDoc {
    /// Where the poses came from: `"camera_path"` (observer echo) or `"entity_tracker"` (POV).
    /// Absent on hand-written input paths.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub keyframes: Vec<PoseKeyframe>,
}

/// One pose sample on the replay timeline.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PoseKeyframe {
    /// Instant on the replay timeline, milliseconds.
    pub time_ms: i64,
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub yaw: f64,
    pub pitch: f64,
    /// Camera roll; entity poses have none, so it defaults to 0.
    #[serde(default)]
    pub roll: f64,
}
