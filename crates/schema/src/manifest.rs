//! The run manifest — the human-written JSON that defines a batch of episodes.
//!
//! `deny_unknown_fields` everywhere is the contract: a misspelled key fails at parse time naming
//! the field, instead of running the episode on a silent default (e.g. a typo like
//! `move_timout_ms` would otherwise be ignored). The blocks are typed, and the type IS the
//! accepted-key set.
//!
//! Cross-field rules that a type cannot express (scenario vs player_count vs catalog) live in
//! [`RunManifest::validate`], which returns EVERY violation rather than the first.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::PlayerRunConfig;
use crate::catalog;
use crate::driver::{PoseKeyframe, RenderMethod};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunManifest {
    pub run_id: String,
    pub episodes: Vec<EpisodeSpec>,
    #[serde(default)]
    pub render: RenderConfig,
    /// CPUs a capture unit reserves on its worker.
    #[serde(default = "default_capture_cpus")]
    pub capture_cpus: u32,
}

fn default_capture_cpus() -> u32 {
    16
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EpisodeSpec {
    pub episode_id: String,
    #[serde(default = "default_player_count")]
    pub player_count: u32,
    /// Player-runtime shard count; 0 = auto (one shard per 10 bots, rounded up).
    #[serde(default)]
    pub shards: u32,
    #[serde(default = "default_episode_duration_sec")]
    pub episode_duration_sec: f64,
    #[serde(default)]
    pub world: WorldSpec,
    #[serde(default)]
    pub swarm: SwarmSpec,
    /// Runtime behaviour overrides, in the capture runtime's own config vocabulary. Every field is
    /// optional; defaults live in ONE place, the runtime's `Cfg::resolve`, never here.
    #[serde(default)]
    pub player_behavior: PlayerRunConfig,
    #[serde(default)]
    pub scenario: ScenarioSpec,
}

fn default_player_count() -> u32 {
    50
}

fn default_episode_duration_sec() -> f64 {
    300.0
}

impl EpisodeSpec {
    /// Effective shard count: explicit when positive (capped by bots), else one per 10 bots.
    pub fn resolved_shards(&self) -> u32 {
        if self.shards > 0 {
            self.shards.min(self.player_count)
        } else {
            self.player_count.div_ceil(10).max(1)
        }
    }
}

/// World-generation parameters handed to the capture server.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorldSpec {
    #[serde(default = "default_world_seed")]
    pub seed: String,
    #[serde(default = "default_level_type")]
    pub level_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generator_settings: Option<String>,
    #[serde(default = "default_level_name")]
    pub level_name: String,
}

impl Default for WorldSpec {
    fn default() -> Self {
        serde_json::from_str("{}").expect("empty WorldSpec")
    }
}

fn default_world_seed() -> String {
    "2026".into()
}
fn default_level_type() -> String {
    "minecraft:normal".into()
}
fn default_level_name() -> String {
    "world".into()
}

/// Spawn-spread geometry. `spread_*` configure the coordinator's placement; the rest are echoed to
/// the runtime as swarm geometry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SwarmSpec {
    /// Minimum distance between spread players, blocks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spread_distance: Option<f64>,
    /// Radius of the spread area, blocks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spread_range: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lane_spacing: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub landmark_distance: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub formation_radius: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal_radius: Option<f64>,
}

impl Default for SwarmSpec {
    fn default() -> Self {
        serde_json::from_str("{}").expect("empty SwarmSpec")
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScenarioSpec {
    /// Catalog name, or `script:<family>/<name>`.
    #[serde(default = "default_scenario_name")]
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<u32>,
    #[serde(default = "default_scenery")]
    pub scenery: String,
    /// Players teleported far away before the run, for rendezvous scenarios.
    #[serde(default)]
    pub far_players: u32,
    #[serde(default = "default_far_distance")]
    pub far_distance: f64,
}

impl Default for ScenarioSpec {
    fn default() -> Self {
        serde_json::from_str("{}").expect("empty ScenarioSpec")
    }
}

fn default_scenario_name() -> String {
    "landmark_tour".into()
}
fn default_scenery() -> String {
    "none".into()
}
fn default_far_distance() -> f64 {
    96.0
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RenderConfig {
    #[serde(default = "default_projections")]
    pub projections: Vec<ProjectionSpec>,
    #[serde(default = "default_encoder")]
    pub encoder: String,
    #[serde(default = "default_preset")]
    pub preset: String,
    #[serde(default = "default_crf")]
    pub crf: u8,
    #[serde(default = "default_fps")]
    pub fps: u32,
    #[serde(default = "default_threads_per_render")]
    pub threads_per_render: u32,
    /// Pin the renderer chunk radius (2..=32). Omitted preserves the installed option.
    /// Recording must contain these chunks; this cannot recover terrain absent from a replay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub render_distance_chunks: Option<u32>,
    /// Render only this many of each episode's POVs, chosen by a per-episode seeded sample.
    /// `None` renders all. Capture is unaffected — the whole swarm still plays.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pov_sample: Option<u32>,
    /// Export ReplayMod's per-frame float depth buffers alongside each rendered projection.
    /// The files are promoted as `<pov>-<projection>-depth/` sequences during finalize.
    #[serde(default)]
    pub depth: bool,
    /// Per-projection render timeout, seconds, keyed by projection name.
    #[serde(default = "default_timeout_sec")]
    pub timeout_sec: BTreeMap<String, u32>,
    /// Observer cameras rendered per episode after its player POVs, over the same projections.
    #[serde(default)]
    pub observers: Vec<ObserverSpec>,
    /// After finalize, burn the action HUD into every complete player POV as
    /// `<pov>-hud.mp4`. A review-run knob: the HUD video is a human-inspection
    /// instrument, not training data.
    #[serde(default)]
    pub hud: bool,
}

impl Default for RenderConfig {
    fn default() -> Self {
        serde_json::from_str("{}").expect("empty RenderConfig")
    }
}

fn default_projections() -> Vec<ProjectionSpec> {
    serde_json::from_str(
        r#"[
            {"name": "default", "method": "DEFAULT", "width": 640, "height": 360},
            {"name": "equirect", "method": "EQUIRECTANGULAR", "width": 1024, "height": 512}
        ]"#,
    )
    .expect("default projections")
}
fn default_encoder() -> String {
    "libx264".into()
}
fn default_preset() -> String {
    "veryfast".into()
}
fn default_crf() -> u8 {
    20
}
fn default_fps() -> u32 {
    20
}
fn default_threads_per_render() -> u32 {
    8
}
fn default_timeout_sec() -> BTreeMap<String, u32> {
    BTreeMap::from([("default".into(), 6000), ("equirect".into(), 21600)])
}

/// One projection as the manifest declares it. Codec fields are optional overrides of the
/// run-level defaults; they are resolved into a verbatim ffmpeg fragment before the driver sees
/// anything.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProjectionSpec {
    /// `[a-z0-9_]+`; becomes the media filename stem.
    pub name: String,
    #[serde(default = "default_method")]
    pub method: RenderMethod,
    #[serde(default = "default_width")]
    pub width: u32,
    #[serde(default = "default_height")]
    pub height: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoder: Option<String>,
    #[serde(default = "default_anti_aliasing")]
    pub anti_aliasing: String,
    #[serde(default = "default_fov_x")]
    pub spherical_fov_x: u32,
    #[serde(default = "default_fov_y")]
    pub spherical_fov_y: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crf: Option<u8>,
}

fn default_method() -> RenderMethod {
    RenderMethod::Default
}
fn default_width() -> u32 {
    640
}
fn default_height() -> u32 {
    360
}
fn default_anti_aliasing() -> String {
    "NONE".into()
}
fn default_fov_x() -> u32 {
    360
}
fn default_fov_y() -> u32 {
    180
}

/// One observer camera planned per episode.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ObserverSpec {
    /// `[a-z0-9_]+`, must not collide with the `pNN` player namespace.
    pub name: String,
    pub mode: ObserverMode,
    /// POV whose recording supplies the world. `None` selects the most central player.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_pov: Option<String>,
    /// Horizontal camera distance from the crowd centre is
    /// `clamp(radius_scale * crowd_radius + margin, min_distance, max_distance)`; the maximum
    /// exists because a camera beyond the source recording's loaded chunks renders void.
    #[serde(default = "default_radius_scale")]
    pub radius_scale: f64,
    #[serde(default = "default_margin")]
    pub margin: f64,
    #[serde(default = "default_min_distance")]
    pub min_distance: f64,
    #[serde(default = "default_max_distance")]
    pub max_distance: f64,
    /// Camera height above the crowd centre, blocks. The default is eye height — a third-person
    /// view should look like a PLAYER watching, not a drone: the old 24-block default read as
    /// "particles floating in the sky" in review.
    #[serde(default = "default_observer_height")]
    pub height: f64,
    /// Compass bearing of the camera relative to the crowd centre, degrees; 0 = camera at +Z.
    #[serde(default = "default_bearing")]
    pub bearing_deg: f64,
    /// `crowd_orbit` only: total bearing swept across the episode.
    #[serde(default = "default_sweep")]
    pub sweep_deg: f64,
    /// `crowd_orbit` only: centred moving-average window, in frames, applied to the crowd centre
    /// so per-tick jitter does not shake the camera.
    #[serde(default = "default_smooth_frames")]
    pub smooth_frames: u32,
    /// `keyframes` mode only: the hand-written path.
    #[serde(default)]
    pub keyframes: Vec<PoseKeyframe>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ObserverMode {
    /// Fixed camera looking at the crowd centre.
    CrowdStatic,
    /// Camera orbiting the (smoothed) crowd centre.
    CrowdOrbit,
    /// Hand-written keyframe path.
    Keyframes,
}

fn default_radius_scale() -> f64 {
    1.6
}
fn default_margin() -> f64 {
    12.0
}
fn default_min_distance() -> f64 {
    16.0
}
fn default_max_distance() -> f64 {
    96.0
}
fn default_observer_height() -> f64 {
    1.7
}
fn default_bearing() -> f64 {
    135.0
}
fn default_sweep() -> f64 {
    90.0
}
fn default_smooth_frames() -> u32 {
    41
}

impl RunManifest {
    /// Every cross-field violation in the manifest, empty when valid. Type-level rules (unknown
    /// keys, wrong types) never reach here — serde rejects them at parse time.
    pub fn validate(&self) -> Vec<String> {
        let mut problems = Vec::new();
        // Ids and projection/observer names become path components under the out root, so they
        // are locked to a safe charset — `../`, separators or an empty string in an id would let
        // a mistyped manifest write (or recursively delete) outside its own directories.
        let safe = |value: &str| {
            !value.is_empty()
                && value
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
                && value != "."
                && value != ".."
        };
        if self
            .render
            .render_distance_chunks
            .is_some_and(|n| !(2..=32).contains(&n))
        {
            problems.push("render.render_distance_chunks must be between 2 and 32".into());
        }
        if self.capture_cpus == 0 {
            problems.push("capture_cpus must be a positive integer".into());
        }
        if self.render.threads_per_render == 0 {
            problems.push("render.threads_per_render must be a positive integer".into());
        }
        if !safe(&self.run_id) {
            problems.push(format!(
                "run_id {:?} is not a safe path component",
                self.run_id
            ));
        }
        let mut episode_ids: Vec<&str> = Vec::new();
        for episode in &self.episodes {
            if !safe(&episode.episode_id) {
                problems.push(format!(
                    "episode_id {:?} is not a safe path component",
                    episode.episode_id
                ));
            }
            episode_ids.push(&episode.episode_id);
            episode.validate_into(&mut problems);
        }
        episode_ids.sort_unstable();
        episode_ids.dedup();
        if episode_ids.len() != self.episodes.len() {
            problems
                .push("episode ids must be unique (duplicates share work and status paths)".into());
        }
        for projection in &self.render.projections {
            if !safe(&projection.name) {
                problems.push(format!(
                    "projection {:?} is not a safe path component",
                    projection.name
                ));
            }
        }
        if !self.render.projections.iter().any(|p| p.name == "default") {
            problems.push("render.projections must include one named \"default\" (finalize keys frame counts and <pov>-default.mp4 off it)".into());
        }
        let mut names: Vec<&str> = self
            .render
            .observers
            .iter()
            .map(|o| o.name.as_str())
            .collect();
        names.sort_unstable();
        names.dedup();
        if names.len() != self.render.observers.len() {
            problems.push("observer names must be unique".into());
        }
        for observer in &self.render.observers {
            if !safe(&observer.name) {
                problems.push(format!(
                    "observer {:?} is not a safe path component",
                    observer.name
                ));
            }
            if observer.name.starts_with('p') {
                problems.push(format!(
                    "observer {:?} collides with the pNN POV namespace",
                    observer.name
                ));
            }
            if observer.mode == ObserverMode::Keyframes && observer.keyframes.is_empty() {
                problems.push(format!(
                    "observer {:?} is keyframes mode with no keyframes",
                    observer.name
                ));
            }
        }
        problems
    }
}

impl EpisodeSpec {
    fn validate_into(&self, problems: &mut Vec<String>) {
        let ep = &self.episode_id;
        let sc = &self.scenario;
        if sc.name == crate::regions::SCENARIO && sc.scenery != "none" {
            problems.push(format!(
                "{ep}: regional_capacity_traffic requires scenery=none"
            ));
        }
        if let Some(info) = catalog::scenario(&sc.name) {
            if self.player_count < info.min_players {
                problems.push(format!(
                    "{ep}: scenario {:?} requires player_count >= {}, got {}",
                    sc.name, info.min_players, self.player_count
                ));
            }
            if info.single_process && self.resolved_shards() > 1 {
                problems.push(format!("{ep}: scenario {:?} requires shards == 1", sc.name));
            }
            if sc.far_players >= self.player_count && sc.far_players > 0 {
                problems.push(format!("{ep}: far_players must be < player_count"));
            }
            if sc.far_players > 0 && sc.scenery != "none" {
                problems.push(format!(
                    "{ep}: far_players is incompatible with scenery (relocation teleports everyone)"
                ));
            }
            if sc.far_players > 0 && !info.needs_far_players {
                problems.push(format!(
                    "{ep}: scenario {:?} does not use far_players",
                    sc.name
                ));
            }
            if info.needs_far_players && sc.far_players < 1 {
                problems.push(format!(
                    "{ep}: scenario {:?} requires far_players >= 1",
                    sc.name
                ));
            }
            if sc.far_players > 0 && sc.far_distance <= 0.0 {
                problems.push(format!("{ep}: far_distance must be > 0"));
            }
        } else {
            problems.push(format!("{ep}: unknown scenario {:?}", sc.name));
        }
        if !catalog::KNOWN_SCENERY.contains(&sc.scenery.as_str()) {
            problems.push(format!("{ep}: unknown scenery {:?}", sc.scenery));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::RunManifest;
    use serde_json::json;

    #[test]
    fn regional_staging_requires_one_shared_ledger_and_natural_terrain() {
        let mut m: RunManifest = serde_json::from_value(json!({
            "run_id": "regional", "episodes": [{"episode_id": "test", "player_count": 64,
                "shards": 1, "scenario": {"name": "regional_capacity_traffic", "scenery": "none"}}]
        }))
        .unwrap();
        assert!(m.validate().is_empty());
        m.episodes[0].shards = 2;
        assert!(m.validate().iter().any(|p| p.contains("shards == 1")));
        m.episodes[0].shards = 1;
        m.episodes[0].scenario.scenery = "build_arena".into();
        assert!(
            m.validate()
                .iter()
                .any(|p| p.contains("requires scenery=none"))
        );
    }

    #[test]
    fn render_distance_is_optional_but_bounded_when_requested() {
        let mut manifest: RunManifest = serde_json::from_value(json!({
            "run_id": "distance-test", "episodes": []
        }))
        .unwrap();
        assert_eq!(manifest.render.render_distance_chunks, None);
        for radius in [2, 24, 32] {
            manifest.render.render_distance_chunks = Some(radius);
            assert!(manifest.validate().is_empty());
        }
        for radius in [0, 1, 33] {
            manifest.render.render_distance_chunks = Some(radius);
            assert!(
                manifest
                    .validate()
                    .iter()
                    .any(|p| p.contains("render_distance_chunks"))
            );
        }
    }

    #[test]
    fn cpu_counts_must_be_positive() {
        let manifest: RunManifest = serde_json::from_value(json!({
            "run_id": "test",
            "episodes": [],
            "capture_cpus": 0,
            "render": {"threads_per_render": 0}
        }))
        .unwrap();
        let problems = manifest.validate();
        assert!(
            problems
                .iter()
                .any(|problem| problem.contains("capture_cpus"))
        );
        assert!(
            problems
                .iter()
                .any(|problem| problem.contains("threads_per_render"))
        );
    }
}
