//! The dataset an episode ships: what a video-world-model training pipeline reads.
//!
//! Layout per episode (everything else the pipeline produces goes to `diagnostics/` and is not a
//! contract). The directory name is the HF layout's:
//! `<UTC start %Y%m%dT%H%M%SZ>-<scenario>-<N>p-<hash8 of capture.json>`, and every per-POV file
//! sits flat under it with the POV name as prefix — the shape a solaris-engine loader globs:
//!
//! ```text
//! dataset/<ts>-<scenario>-<Np>-<hash8>/
//!   episode.json               EpisodeManifest — identity, constants, POV inventory
//!   qc.json                    QcReport — alignment + degeneracy verdicts, human-facing
//!   events.jsonl               EventRow per line — discrete action events on the world-age clock
//!   <pov>-default.mp4          one file per projection: <pov>-<projection>.mp4
//!   <pov>-equirectangular.mp4  (the `equirect` projection spelled out, HF/solaris style)
//!   <pov>-frames.jsonl         FrameRow per line — one row PER VIDEO FRAME, alignment pre-joined
//!   <pov>-episode_info.json    SolarisEpisodeInfo — the solaris per-bot manifest projection
//!   diagnostics/               free-form, wholesale-deletable, never read by training
//! ```
//!
//! Observers are first-class POVs: `obs00-*` files differ from `p00-*` only in that their rows
//! carry no player-state columns (a free camera has no body, action or inventory). A loader that
//! iterates [`EpisodeManifest::povs`] handles both without a special case.
//!
//! Field selection rule: a field is here because the
//! multi-player multi-view video-generation task consumes it, not because some stage happened to
//! produce it. Derivable quantities (velocity, movement-direction labels) and wall-clock stamps are
//! deliberately absent.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::time::{DurationMs, EpochMs, PlayerTimeMs, ReplayTimeMs, ServerTick, WorldAgeTick};

/// Version of the dataset contract in this module. Bumped on any breaking change; old corpora are
/// re-captured rather than migrated. v3 = solaris-engine row compatibility
/// (docs/dataset-spec.md): frames.jsonl rows carry the solaris field vocabulary
/// (mineflayer-convention radians, edge-event hotbar keys, full inventory snapshots, synthetic
/// wall clock) plus our superset (`camera_pose`, `server_tick`, `held_item`).
pub const DATASET_SCHEMA_VERSION: u32 = 3;

// ---- angle conventions ----
//
// The dataset ships TWO angle conventions on purpose (docs/dataset-spec.md):
// solaris row fields (`yaw`/`pitch`/`action.camera`) are radians in the MINEFLAYER convention;
// our superset `camera_pose` is degrees in the NOTCHIAN convention. The conversion is a double
// difference (unit AND convention), from mineflayer's own `lib/conversions.js`:
//
//   fromNotchianYaw(yaw)     = euclideanMod(PI - toRadians(yaw), 2*PI)
//   fromNotchianPitch(pitch) = -toRadians(pitch)
//
// so notchian yaw 0 (south, +Z) is mineflayer PI, notchian 180 (north) is mineflayer 0, and a
// positive notchian delta becomes a negative mineflayer delta.

/// Notchian radians → mineflayer radians, yaw. The capture intermediates already hold radians,
/// and a rad→deg→rad round trip costs ULPs the v3 bit-parity bar does not allow.
pub fn mineflayer_yaw_from_notchian_rad(notchian_yaw_rad: f64) -> f64 {
    (std::f64::consts::PI - notchian_yaw_rad).rem_euclid(std::f64::consts::TAU)
}

/// Notchian radians → mineflayer radians, pitch. Negation is exact; no unit round trip.
pub fn mineflayer_pitch_from_notchian_rad(notchian_pitch_rad: f64) -> f64 {
    -notchian_pitch_rad
}

/// `episode.json` — the one place episode-level constants live. Nothing in here is repeated
/// per-frame or per-event; the old format stamped `runtime_family`/`schema_version` on every row of
/// every file, which taught downstream readers to look for constants in the wrong place.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EpisodeManifest {
    /// [`DATASET_SCHEMA_VERSION`] at write time.
    pub schema_version: u32,
    pub episode_id: String,
    /// The run (batch) this episode was captured in.
    pub run_id: String,
    pub scenario: ScenarioRef,
    /// Minecraft version of both the capture server and the render client, e.g. `"26.2"`.
    pub mc_version: String,
    /// What produced the action streams (bot runtime family and process topology). Constant per
    /// episode — hoisted here from the old per-frame stamps.
    pub runtime: RuntimeInfo,
    /// Server simulation rate all tick clocks count at, ticks per second.
    pub tick_rate_hz: u32,
    /// Video frame rate every video and `frames.jsonl` in this episode uses.
    pub fps: u32,
    /// Wall-clock base of the trimmed dataset: every complete POV's `renderTime[0]`. The
    /// finalize trim aligns all files to the same first world tick, so one base serves the
    /// whole episode.
    pub started_epoch_ms: EpochMs,
    /// Length the manifest asked for.
    pub target_duration_ms: DurationMs,
    /// Length actually captured (go → done), before rendering.
    pub actual_duration_ms: DurationMs,
    /// World-generation parameters, kept so an episode's terrain is reproducible.
    pub world: WorldInfo,
    /// Every viewpoint of this episode, players and observers alike, including ones that failed —
    /// a POV missing from this list was never planned, one listed with a non-`complete` status was
    /// planned and didn't make it.
    pub povs: Vec<PovEntry>,
}

/// Which scenario drove the bots, and with what seed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScenarioRef {
    /// Catalog scenario name.
    pub name: String,
    /// Behaviour family (`group_motion`, `construction`, `pursuit`, ...).
    pub family: String,
    pub seed: u32,
    /// Scenery generator the coordinator built before the run (`none` when untouched terrain).
    pub scenery: String,
}

/// Provenance of the action streams.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RuntimeInfo {
    /// Bot runtime family, e.g. `"azalea"`.
    pub family: String,
    /// Process topology, e.g. `"single_process_multi_player"`.
    pub topology: String,
    /// Version of the bot runtime's protocol library.
    pub azalea_version: String,
}

/// World-generation parameters.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorldInfo {
    /// World seed as given to the server. A string because Minecraft accepts non-numeric seeds.
    pub seed: String,
    /// Level type identifier, e.g. `"minecraft:normal"`.
    pub level_type: String,
}

/// One viewpoint in [`EpisodeManifest::povs`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PovEntry {
    /// POV name, the per-file prefix in the episode directory: `p00`, `p01`, ... for players;
    /// `obs00`, ... for observers.
    pub name: String,
    pub kind: PovKind,
    pub status: PovStatus,
    /// In-game username. Players only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// Observers only: the player recording this camera was rendered inside, and therefore the
    /// bound on what it can see — ReplayMod renders only chunks that player received.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_pov: Option<String>,
    /// Rendered media by projection name (`default`, `equirect`, ...). Empty when `status` is not
    /// `complete`.
    pub videos: BTreeMap<String, MediaInfo>,
    /// Optional ReplayMod depth-buffer sequences keyed by projection name.
    #[serde(default)]
    pub depth: BTreeMap<String, DepthInfo>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PovKind {
    /// A bot's first-person view; rows carry pose and action.
    Player,
    /// A planned free camera; rows carry camera only.
    Observer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PovStatus {
    /// Video and frames.jsonl are present and passed QC gates.
    Complete,
    /// The bot never connected to the server; nothing was recorded and the render was skipped by
    /// design. Not an error — the rest of the episode is intact.
    SkippedNoConnect,
    /// Capture or render ran and failed; see `diagnostics/`.
    Failed,
}

/// One rendered video file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MediaInfo {
    /// Path relative to the episode directory. Never absolute, never containing `..`.
    pub path: String,
    pub sha256: String,
    /// Container format, e.g. `"mp4"`.
    pub container: String,
    /// Video codec, e.g. `"h264"`.
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub frame_count: u64,
    pub duration_ms: DurationMs,
    /// True when the file carries spherical (equirectangular) projection metadata.
    pub spherical: bool,
}

/// One optional ReplayMod depth-buffer sequence.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DepthInfo {
    /// Directory relative to the episode directory containing `<frame>.depth.png` files.
    pub path: String,
    /// Depth image width in pixels.
    pub width: u32,
    /// Depth image height in pixels.
    pub height: u32,
    /// Number of contiguous depth frames, aligned 1:1 with the corresponding video.
    pub frame_count: u64,
    /// ReplayMod's PNG depth encoding: float32 bytes stored in BGRA components.
    pub format: String,
}

/// One line of a POV's `frames.jsonl`: everything known about one video frame, pre-joined.
///
/// The row vocabulary is solaris-engine's (docs/dataset-spec.md, recorder =
/// prismarine-viewer-colalab `lib/headless-base64.js`, loader = solaris-wm/solaris
/// `src/data/dataset.py`) plus our superset fields. Solaris columns keep solaris semantics
/// exactly: radians in the mineflayer convention, camelCase timestamp names, `frame_count` as the
/// 0-based row index and `episode_id` as an integer sequence number. Our additions are named to be
/// unmistakable (`camera_pose`, `server_tick`, `held_item`) and never collide with a solaris key.
///
/// Player-state columns (`x..pitch`, `action`, `inventory`, `physicsTime`, `isInWater`,
/// `server_tick`, `held_item`) are absent together on observer rows and on player frames outside
/// the recorded action window — an honest gap the QC report counts, never a nearest-neighbour
/// guess.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FrameRow {
    // ---- solaris columns ----
    /// Feet world position, blocks, full-precision f64 straight from the bot (never rounded).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub x: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub y: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub z: Option<f64>,
    /// Body yaw, **radians, mineflayer convention** (see
    /// [`mineflayer_yaw_from_notchian_rad`]). The notchian degrees live in `camera_pose`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub yaw: Option<f64>,
    /// Body pitch, **radians, mineflayer convention** (positive is up).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pitch: Option<f64>,
    /// The control state at this frame's tick, in solaris's action vocabulary.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<SolarisAction>,
    /// Full inventory snapshot at this frame's tick (solaris ships it per row; its trainer does
    /// not read it, but drop-in compatibility does).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inventory: Option<InventorySnapshot>,
    /// Synthetic linear wall clock, ms: episode `started_epoch_ms` + frame grid time. The
    /// finalize trim puts every POV's frame 0 on the same world tick, so the value is one
    /// linear function of `worldAgeTicks` across every file in the episode and equal wall
    /// clocks mean the same world moment. Solaris stamps `Date.now()` per sample; ours is
    /// derived so the capture side records no wall clock.
    #[serde(rename = "renderTime")]
    pub render_time: EpochMs,
    /// `renderTime` in epoch SECONDS (solaris `epochTime` unit).
    #[serde(rename = "epochTime")]
    pub epoch_time: f64,
    /// Milliseconds since episode start on the constant-frame-rate grid: `round(i * 1000 / fps)`.
    #[serde(rename = "relativeTimeMs")]
    pub relative_time_ms: ReplayTimeMs,
    /// Client physics-frame clock of the matched action row (solaris `getLastPhysicsFrameTime()`;
    /// ours is the bot-process monotonic clock).
    #[serde(rename = "physicsTime", skip_serializing_if = "Option::is_none")]
    pub physics_time: Option<PlayerTimeMs>,
    /// The frame's instant on the authoritative world-age clock, resolved by the metadata join
    /// (solaris `bot.time.age`; the one clock both formats share).
    #[serde(rename = "worldAgeTicks")]
    pub world_age_ticks: WorldAgeTick,
    #[serde(rename = "isInWater", skip_serializing_if = "Option::is_none")]
    pub is_in_water: Option<bool>,
    /// 0-based index into the video: row `i` describes frame `i` (solaris name; unrelated to
    /// [`MediaInfo::frame_count`], which is the total).
    pub frame_count: u64,
    /// Integer episode sequence number (position in the run manifest). The string episode id
    /// stays in `episode.json` and the directory name — deliberate dual-track.
    pub episode_id: i64,
    pub extra_info: ExtraInfo,
    // ---- superset columns ----
    /// The camera pose this frame was actually rendered from (the driver's pose trace, matched by
    /// time): the spectator camera glued to a player's eyes, or an observer's planned free camera.
    /// **Degrees, notchian convention** — the absolute pose is a hard user requirement (decision
    /// 6); solaris only trains on `action.camera` deltas. `None` only where the pose trace has a
    /// hole.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub camera_pose: Option<CameraPose>,
    /// Server tick the matched action row was recorded at, as the client observed it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_tick: Option<ServerTick>,
    /// Canonical item name in the selected hotbar slot, when any — the flat convenience the
    /// inventory snapshot also carries as `heldItem`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub held_item: Option<String>,
}

/// `extra_info` — solaris ships `{"seed": 42}` hardcoded; we send the real world seed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExtraInfo {
    /// World seed as an integer (solaris's type). Non-numeric seeds fall back to 0; the
    /// authoritative string lives in `episode.json` `world.seed`.
    pub seed: i64,
}

/// `<pov>-episode_info.json` — the solaris per-bot episode manifest, projected from our episode
/// state (docs/dataset-spec.md). One per complete POV; the episode-level truth stays
/// in `episode.json`, this file exists so a solaris loader finds the triple it expects.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SolarisEpisodeInfo {
    /// Episode start instant, RFC 3339 UTC.
    pub timestamp: String,
    /// In-game username for a player POV; the POV name for an observer (no body, no username).
    pub bot_name: String,
    /// Same integer sequence number the frame rows carry as `episode_id`.
    pub episode_number: i64,
    /// Scenario name (solaris's episode taxonomy slot).
    pub episode_type: String,
    /// Our string episode id — the run-unique identity the directory name also derives from.
    pub instance_id: String,
    /// Always false in a shipped episode: a POV that errored never gets this file (a failed
    /// player POV fails the whole episode; a failed observer gets no files at all).
    pub encountered_error: bool,
    /// True when any OTHER POV of the episode ended non-complete (in practice: a failed
    /// observer, since player failures fail the episode).
    pub peer_encountered_error: bool,
    /// Constant false — no scenario has a death mechanic.
    pub bot_died: bool,
    /// Constant true: this file is only written next to an actual recording.
    pub episode_recording_started: bool,
    /// Solaris's free-form eval slot; empty for collected (non-eval) episodes.
    pub eval_metadata: BTreeMap<String, serde_json::Value>,
}

/// The solaris `action` object: continuous key state + edge events + the camera delta.
///
/// `mount`/`dismount`/`place_entity` are constant `false` — no scenario in this pipeline rides
/// entities or places entities; the keys exist so the row is a drop-in superset of solaris's.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SolarisAction {
    // Held movement keys (continuous state), same vocabulary as [`MovementKeys`].
    pub forward: bool,
    pub back: bool,
    pub left: bool,
    pub right: bool,
    pub jump: bool,
    pub sprint: bool,
    pub sneak: bool,
    /// `[d_yaw, d_pitch]` — orientation change since the previous row, **radians, mineflayer
    /// convention** (a positive notchian delta lands here negated). `[0, 0]` on the first row and
    /// after a coverage gap.
    pub camera: [f64; 2],
    /// One-shot edge events.
    pub attack: bool,
    #[serde(rename = "use")]
    pub use_item: bool,
    pub mount: bool,
    pub dismount: bool,
    pub place_block: bool,
    pub place_entity: bool,
    /// Continuous state (solaris's one non-edge action flag).
    pub mine: bool,
    /// Hotbar SELECTION EDGES, not slot state: `hotbar.N` is true only on the row where the
    /// selected slot changed to N-1. The slot state lives in `inventory.quickBarSlot`.
    #[serde(rename = "hotbar.1")]
    pub hotbar_1: bool,
    #[serde(rename = "hotbar.2")]
    pub hotbar_2: bool,
    #[serde(rename = "hotbar.3")]
    pub hotbar_3: bool,
    #[serde(rename = "hotbar.4")]
    pub hotbar_4: bool,
    #[serde(rename = "hotbar.5")]
    pub hotbar_5: bool,
    #[serde(rename = "hotbar.6")]
    pub hotbar_6: bool,
    #[serde(rename = "hotbar.7")]
    pub hotbar_7: bool,
    #[serde(rename = "hotbar.8")]
    pub hotbar_8: bool,
    #[serde(rename = "hotbar.9")]
    pub hotbar_9: bool,
}

impl SolarisAction {
    /// Set the `hotbar.N` edge flag for a 0-based slot index.
    pub fn set_hotbar_edge(&mut self, slot: u8) {
        match slot {
            0 => self.hotbar_1 = true,
            1 => self.hotbar_2 = true,
            2 => self.hotbar_3 = true,
            3 => self.hotbar_4 = true,
            4 => self.hotbar_5 = true,
            5 => self.hotbar_6 = true,
            6 => self.hotbar_7 = true,
            7 => self.hotbar_8 = true,
            8 => self.hotbar_9 = true,
            _ => {}
        }
    }

    /// Clear every `hotbar.N` edge flag — for a row whose diff basis was trimmed away: the first
    /// row of a file fires no edges (they are events against the PREVIOUS row, which is gone).
    pub fn clear_hotbar_edges(&mut self) {
        self.hotbar_1 = false;
        self.hotbar_2 = false;
        self.hotbar_3 = false;
        self.hotbar_4 = false;
        self.hotbar_5 = false;
        self.hotbar_6 = false;
        self.hotbar_7 = false;
        self.hotbar_8 = false;
        self.hotbar_9 = false;
    }
}

/// The solaris `inventory` object, grouped exactly as `headless-base64.js` groups the mineflayer
/// player window: `mainInventory` = window slots 9–35, `hotbar` = 36–44, `heldItem` =
/// slot `quickBarSlot + 36`, `offHandItem` = slot 45. Azalea's player menu uses the same vanilla
/// slot indices, so the mapping is the identity.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InventorySnapshot {
    /// Selected hotbar slot, 0-8.
    #[serde(rename = "quickBarSlot")]
    pub quick_bar_slot: u8,
    /// Non-empty stacks in window slots 9–35, each stamped with its `slot`.
    #[serde(rename = "mainInventory")]
    pub main_inventory: Vec<ItemSnapshot>,
    /// Non-empty stacks in window slots 36–44.
    pub hotbar: Vec<ItemSnapshot>,
    #[serde(rename = "heldItem")]
    pub held_item: Option<ItemSnapshot>,
    #[serde(rename = "offHandItem")]
    pub off_hand_item: Option<ItemSnapshot>,
}

/// One item stack, in prismarine-item's `JSON.stringify` shape (PrismarineJS/prismarine-item
/// `index.js`: the constructor's own enumerable properties are `type`, `count`, `metadata`,
/// `nbt`, `stackId`, `name`, `displayName`, `stackSize`; getters like `enchants` never serialize;
/// `maxDurability` is dropped when undefined). `slot` is the window slot index prismarine-windows
/// stamps onto items it holds, which solaris rows therefore carry too.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ItemSnapshot {
    /// Protocol registry id of the item for this Minecraft version.
    #[serde(rename = "type")]
    pub type_id: u32,
    pub count: i32,
    /// Legacy metadata; always 0 on modern versions (prismarine defaults null → 0).
    pub metadata: i32,
    /// NBT / component data. Our scenarios hand out plain items, so this is `null` (prismarine
    /// stores `nbt || null`).
    pub nbt: Option<serde_json::Value>,
    /// Bedrock-only stack id; always `null` on Java (prismarine sets `this.stackId = null`).
    #[serde(rename = "stackId")]
    pub stack_id: Option<i64>,
    /// Registry name without the `minecraft:` prefix, e.g. `"stone_bricks"`.
    pub name: String,
    /// Human-readable name, e.g. `"Stone Bricks"`.
    #[serde(rename = "displayName")]
    pub display_name: String,
    #[serde(rename = "stackSize")]
    pub stack_size: i32,
    /// Player-window slot index (0 craft result … 9–35 main, 36–44 hotbar, 45 off-hand).
    pub slot: i32,
}

/// A rendered camera pose — the superset `camera_pose` column. Degrees for angles (notchian
/// convention), world blocks for position.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CameraPose {
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub yaw: f64,
    pub pitch: f64,
    /// Camera roll. Observer paths may bank; a spectator camera has none, so 0 is the norm.
    #[serde(default)]
    pub roll: f64,
    /// Vertical field of view in degrees (Minecraft's FOV), from the render client's pinned
    /// settings — a camera pose without intrinsics does not determine what the frame shows.
    pub fov_deg: f64,
}

/// The eight movement keys, as held during this tick.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct MovementKeys {
    pub forward: bool,
    pub back: bool,
    pub left: bool,
    pub right: bool,
    pub jump: bool,
    pub sprint: bool,
    pub sneak: bool,
}

/// One line of `events.jsonl`: a discrete scripted action (a block placed, a target attacked),
/// distinct from the dense per-frame control stream.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct EventRow {
    /// Monotonic per-episode sequence number, total order over all POVs.
    pub seq: u64,
    pub event_id: String,
    /// POV name of the acting player (`p00`, ...).
    pub pov: String,
    /// Scenario-defined action type (`place_block`, `attack_peer`, ...). An open vocabulary owned
    /// by the scenarios, not an enum.
    pub action_type: String,
    /// Event instant on the authoritative clock. Look up the frame it lands on in the acting POV's
    /// `frames.jsonl` by this tick.
    pub world_age_tick: WorldAgeTick,
    pub server_tick: ServerTick,
    /// Human-readable summary of what happened.
    pub description: String,
    /// Scenario-specific payload fields, passed through verbatim.
    #[serde(flatten)]
    pub payload: BTreeMap<String, serde_json::Value>,
}

// ---- qc.json ----

/// `qc.json` — machine-checkable quality gates plus the alignment evidence, one file per episode.
/// Read by humans and by corpus-level sweeps; never by the training loader.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QcReport {
    pub verdict: QcVerdict,
    /// Failure descriptions when `verdict` is not `pass`. Empty otherwise.
    pub failures: Vec<String>,
    /// Movement keys never pressed by ANY player in the episode — a key vocabulary hole.
    pub dead_keys: Vec<String>,
    /// Fraction of active frames on which paired players agree (clone detection; 1.0 = clones).
    pub pair_agreement_active: f64,
    /// Fraction of moving frames whose camera bearing tracks the travel bearing (predictability).
    pub camera_travel_agreement: f64,
    /// Spread (max − min, ticks) between the shipped files' first-row `worldAgeTicks`: the
    /// cross-POV time-base disagreement. The tick-aligned trim makes it 0 by construction;
    /// anything above the QC bound fails the episode (a regression to frame-index windows).
    /// Absent in qc.json from older pipeline versions and when no player POV aligned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pov_anchor_spread_ticks: Option<i64>,
    /// Crowd-spacing metrics over the whole swarm.
    pub separation: SeparationMetrics,
    /// Per-POV detail, keyed by POV name. Player POVs only — observers are never listed here.
    pub povs: BTreeMap<String, PovQc>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum QcVerdict {
    Pass,
    Fail,
}

/// QC detail for one POV.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PovQc {
    /// Player POVs only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alignment: Option<AlignmentQc>,
    /// Player POVs only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub degeneracy: Option<DegeneracyMetrics>,
    /// Player POVs only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub motion: Option<MotionMetrics>,
}

/// How this POV's frames were joined to the world-age clock, and how well.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AlignmentQc {
    pub status: AlignmentStatus,
    /// Which anchor won. The chosen delta is baked into every `frames.jsonl` row; the losing
    /// candidates stay in `candidate_deltas` and ONLY there, so a reader cannot pick up a rejected
    /// anchor by a plausible-sounding field name (an earlier HUD failure mode).
    pub anchor: AnchorMode,
    /// The applied delta: `world_age_tick - replay_tick` at the RECORDING start.
    pub delta_world_age_minus_replay_tick: i64,
    /// Render display phase subtracted when mapping ticks onto frames: the rendered picture
    /// lags the action record by the client→server→recording round trip. Measured
    /// over 96 isolated camera-turn events: local {3,4} (n=25), cluster {3:18,4:39,5:13,7:1}
    /// (n=71) — environment-dependent upper tail, but never below 3 in either. The compensation
    /// is therefore the PROVEN LOWER BOUND only; the residual (0..4 frames) is inherent noise.
    pub render_phase_ticks: i64,
    /// Every anchor candidate considered, by name, `null` when unavailable. Diagnostic only.
    pub candidate_deltas: BTreeMap<String, Option<i64>>,
    pub actions_matched: u64,
    pub actions_unavailable: u64,
    /// Action rows dropped from the tail because they fell after the recording stopped.
    pub dropped_trailing_count: u64,
    /// Tolerance, in frames, the validation pass allowed between expected and observed events.
    pub validation_tolerance_frames: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AlignmentStatus {
    /// A delta was chosen and validated; frames carry world-age ticks.
    Applied,
    /// No usable anchor; the POV fails QC.
    Failed,
}

/// The anchor sources the metadata join chooses between, best first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AnchorMode {
    /// Per-player recording-stop world age matched against the last rendered frame. Preferred:
    /// it anchors at the end the recording actually has.
    StopAnchoredFrames,
    /// World age at `replay start`, shared across the episode.
    RecordingStartWorldAge,
    /// World age latched when the `go` signal fired. Least preferred — the recording preroll
    /// before `go` is unverified.
    GoWorldAge,
}

/// Degenerate-behaviour metrics over one POV's action stream (idle stretches, dead inputs,
/// twitchy movement). Thresholds live in the QC code; these are the measurements.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DegeneracyMetrics {
    /// Action rows examined.
    pub frames: u64,
    /// Fraction of frames with no movement key held and no camera motion.
    pub idle_frac: f64,
    /// Longest continuous idle stretch, seconds.
    pub longest_idle_s: f64,
    /// Frames with any movement key held.
    pub moving: u64,
    /// Fraction of movement runs shorter than 200 ms (twitch movement).
    pub short_run_frac: f64,
    /// Count of forward-movement runs sustained past the twitch threshold.
    pub forward_runs_sustained: u64,
    /// Of those, runs that also sprinted.
    pub forward_runs_sustained_sprint: u64,
    /// Frames with camera motion.
    pub camera_frames: u64,
    /// Fraction of moving frames whose camera bearing tracks the travel bearing.
    pub camera_travel_agreement: f64,
    /// Fraction of frames frozen: under 0.5 blocks of net 3D displacement over the trailing
    /// second and no discrete action on the frame. Displacement-based on purpose — a bot holding
    /// W into a tree counts as moving to `idle_frac` but frozen here. `default` so qc.json from
    /// older pipeline versions still parses.
    #[serde(default)]
    pub frozen_frac: f64,
    /// Longest continuous frozen stretch, seconds.
    #[serde(default)]
    pub longest_frozen_s: f64,
    /// Seconds spent stuck: movement keys held through a frozen stretch of 5 s or more
    /// (pushing a wall or a tree). In-place jumping does not clear it; climbing does.
    #[serde(default)]
    pub stuck_total_s: f64,
    /// Fraction of sampled video frames that are near-black (per-frame mean luma YAVG under
    /// 24/255, sampled from the default projection at 2 frames/s). The only degeneracy figure
    /// measured from pixels rather than the action stream, so it is filled in at finalize and
    /// absent (`null`) in qc.json from older pipeline versions and in action-only tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dark_frame_frac: Option<f64>,
    /// Longest continuous near-black stretch, seconds, at the same sampling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub longest_dark_run_s: Option<f64>,
}

/// Movement-coverage metrics for one POV, computed by the capture runtime.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MotionMetrics {
    pub dense_frames: u64,
    pub idle_frame_ratio: f64,
    pub moving_frame_ratio: f64,
    pub low_motion: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub move_run_median_ticks: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub move_runs_under_200ms_frac: Option<f64>,
    /// Total horizontal path length, blocks.
    pub path_length_2d: f64,
    /// Bounding-box diagonal of the horizontal positions visited, blocks.
    pub xz_span: f64,
    pub yaw_coverage_rad: f64,
    pub pitch_coverage_rad: f64,
}

/// Crowd-spacing metrics over the whole swarm, computed by the capture runtime.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SeparationMetrics {
    pub evaluated_player_frames: u64,
    pub crowd_frame_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub crowd_frame_ratio: Option<f64>,
    pub overlap_frame_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overlap_frame_ratio: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_nearest_neighbor_2d: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub median_nearest_neighbor_2d: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p05_nearest_neighbor_2d: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p95_nearest_neighbor_2d: Option<f64>,
}

#[cfg(test)]
mod tests {
    use std::f64::consts::{FRAC_PI_2, FRAC_PI_6, PI};

    use super::*;

    /// Notchian degrees → mineflayer radians, for the tests below that pin headings the way
    /// a heading is usually spelled. Production never enters through degrees: the capture
    /// intermediates already hold radians and the round trip would cost ULPs (see the
    /// radian-domain converters' docs).
    fn mf_yaw(notchian_deg: f64) -> f64 {
        mineflayer_yaw_from_notchian_rad(notchian_deg.to_radians())
    }

    fn mf_pitch(notchian_deg: f64) -> f64 {
        mineflayer_pitch_from_notchian_rad(notchian_deg.to_radians())
    }

    /// Empirical pin of the notchian→mineflayer conversion against known headings, per the
    /// blueprint formula (docs/dataset-spec.md, from mineflayer `lib/conversions.js`:
    /// `fromNotchianYaw = euclideanMod(PI - toRadians(yaw), 2*PI)`,
    /// `fromNotchianPitch = -toRadians(pitch)`).
    #[test]
    fn mineflayer_conversion_matches_known_headings() {
        let close = |a: f64, b: f64| (a - b).abs() < 1e-12;
        // South (+Z) is notchian yaw 0 and mineflayer PI.
        assert!(close(mf_yaw(0.0), PI));
        // North (-Z): notchian 180 → mineflayer 0.
        assert!(close(mf_yaw(180.0), 0.0));
        // East (+X): notchian -90 (aka 270) → mineflayer 3π/2; both spellings agree.
        assert!(close(mf_yaw(-90.0), 3.0 * FRAC_PI_2));
        assert!(close(mf_yaw(270.0), 3.0 * FRAC_PI_2));
        // West (-X): notchian 90 → mineflayer π/2.
        assert!(close(mf_yaw(90.0), FRAC_PI_2));
        // Looking 30° up is notchian pitch -30 and mineflayer +π/6.
        assert!(close(mf_pitch(-30.0), FRAC_PI_6));
        // Straight down: notchian +90 → mineflayer -π/2.
        assert!(close(mf_pitch(90.0), -FRAC_PI_2));
        // Result stays in [0, 2π).
        assert!((0.0..std::f64::consts::TAU).contains(&mf_yaw(-720.5)));
    }

    /// Delta signs flip with the convention: turning right (positive notchian d_yaw) is a
    /// negative mineflayer delta, looking up (negative notchian d_pitch) a positive one.
    #[test]
    fn mineflayer_deltas_flip_sign() {
        let d_yaw = mf_yaw(10.0) - mf_yaw(0.0);
        assert!((d_yaw - (-10.0f64.to_radians())).abs() < 1e-12);
        let d_pitch = mf_pitch(-5.0) - mf_pitch(0.0);
        assert!((d_pitch - 5.0f64.to_radians()).abs() < 1e-12);
    }

    /// Contract round trip of a full player row: the solaris keys serialize under their exact
    /// wire names ("use", "hotbar.N", camelCase timestamps) and deserialize back identically.
    #[test]
    fn frame_row_round_trips_with_solaris_key_names() {
        let mut action = SolarisAction {
            forward: true,
            sprint: true,
            camera: [-0.021, 0.004],
            mine: true,
            ..SolarisAction::default()
        };
        action.set_hotbar_edge(2);
        let row = FrameRow {
            x: Some(12.5),
            y: Some(66.406),
            z: Some(-3.2),
            yaw: Some(mf_yaw(91.0)),
            pitch: Some(mf_pitch(-8.5)),
            action: Some(action),
            inventory: Some(InventorySnapshot {
                quick_bar_slot: 2,
                main_inventory: vec![],
                hotbar: vec![ItemSnapshot {
                    type_id: 42,
                    count: 64,
                    metadata: 0,
                    nbt: None,
                    stack_id: None,
                    name: "stone_bricks".into(),
                    display_name: "Stone Bricks".into(),
                    stack_size: 64,
                    slot: 38,
                }],
                held_item: None,
                off_hand_item: None,
            }),
            render_time: EpochMs(1_755_600_000_050),
            epoch_time: 1_755_600_000.05,
            relative_time_ms: ReplayTimeMs(50),
            physics_time: Some(PlayerTimeMs(1234)),
            world_age_ticks: WorldAgeTick(90_417),
            is_in_water: Some(false),
            frame_count: 1,
            episode_id: 7,
            extra_info: ExtraInfo { seed: 2026 },
            camera_pose: Some(CameraPose {
                x: 12.5,
                y: 68.0,
                z: -3.2,
                yaw: 91.0,
                pitch: -8.5,
                roll: 0.0,
                fov_deg: 70.0,
            }),
            server_tick: Some(ServerTick(2113)),
            held_item: Some("stone_bricks".into()),
        };
        let text = serde_json::to_string(&row).unwrap();
        for key in [
            "\"use\":",
            "\"hotbar.3\":true",
            "\"renderTime\":",
            "\"epochTime\":",
            "\"relativeTimeMs\":",
            "\"physicsTime\":",
            "\"worldAgeTicks\":",
            "\"isInWater\":",
            "\"quickBarSlot\":",
            "\"mainInventory\":",
            "\"heldItem\":",
            "\"offHandItem\":",
            "\"type\":42",
            "\"displayName\":",
            "\"stackSize\":",
            "\"stackId\":",
            "\"place_entity\":false",
            "\"mount\":false",
            "\"camera_pose\":",
        ] {
            assert!(text.contains(key), "{key} missing from {text}");
        }
        assert_eq!(row, serde_json::from_str(&text).unwrap());

        // Observer row: every player-state column absent from the wire.
        let observer = FrameRow {
            x: None,
            y: None,
            z: None,
            yaw: None,
            pitch: None,
            action: None,
            inventory: None,
            physics_time: None,
            is_in_water: None,
            server_tick: None,
            held_item: None,
            ..row
        };
        let text = serde_json::to_string(&observer).unwrap();
        // Keys that cannot collide with camera_pose's own (x/y/z live in both).
        for key in [
            "\"action\"",
            "\"inventory\"",
            "\"isInWater\"",
            "\"physicsTime\"",
            "\"held_item\"",
        ] {
            assert!(!text.contains(key), "{key} should be absent from {text}");
        }
        assert_eq!(observer, serde_json::from_str(&text).unwrap());
    }
}
