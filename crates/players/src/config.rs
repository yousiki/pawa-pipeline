use pawa_schema::{PLAYER_CONFIG_ENV, PlayerRunConfig};

use crate::*;

// ---------------------------------------------------------------------------------------------
// Configuration: ONE JSON document, named by ONE environment variable.
//
// A document the runtime cannot parse, or a key it does not recognise, stops the run naming the
// problem; nothing falls back silently to a default. See `PlayerRunConfig` for the contract.
//
// Defaults and clamps live together in `Cfg::resolve` below.
// ---------------------------------------------------------------------------------------------

/// Environment variable naming the config document. The only one this crate reads; defined in
/// `pawa_schema` because the capture stage sets it and must agree on the name.
pub(crate) const CONFIG_ENV: &str = PLAYER_CONFIG_ENV;

#[derive(Clone, Debug, Default)]
pub(crate) struct Cfg {
    pub(crate) bot_count: usize,
    /// This process's shard of a multi-process swarm (0-based). One shard == today's behavior.
    pub(crate) shard_index: usize,
    pub(crate) shard_count: usize,
    /// Global index of this shard's bot 0 — usernames/player-ids stay unique across shards.
    pub(crate) index_offset: usize,
    /// Global swarm size N across all shards; `bot_count` stays the LOCAL per-shard count.
    pub(crate) total_bot_count: usize,
    pub(crate) far_players: usize,
    /// Radius (blocks) of the ring that offsets each shard's plan anchor; 0 = shared anchor.
    pub(crate) shard_spread: f64,
    pub(crate) tokio_threads: usize,
    pub(crate) runtime_topology: String,
    pub(crate) episode_id: String,
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) view_distance: u8,
    pub(crate) out_dir: PathBuf,
    pub(crate) signal_dir: PathBuf,
    pub(crate) go_timeout_ms: u64,
    /// The configured hard timeout AFTER the episode duration and margin have been folded in. The
    /// unfolded value is a local in `resolve` and was once also a write-only field — nothing
    /// ever read it, so keeping it invited reading the wrong one.
    pub(crate) effective_hard_timeout_ms: u64,
    pub(crate) episode_duration_ms: i64,
    pub(crate) lane_spacing: f64,
    pub(crate) landmark_distance: f64,
    pub(crate) formation_radius: f64,
    pub(crate) goal_radius: f64,
    pub(crate) allow_sprinting: bool,
    pub(crate) move_timeout_ms: u64,
    /// Abort a `move_to` early when the bot has not translated >= 0.5 blocks for this long —
    /// the unreachable-goal signature (azalea keeps recalculating a degenerate partial path whose
    /// frontier is where the bot already stands, so the controller parks). Without this the bot
    /// stands dead-still for the FULL move_timeout_ms staring at whatever blocks its way, over
    /// and over at every unreachable waypoint (seen as frozen-wall videos in early testing).
    /// 0 disables.
    pub(crate) move_no_progress_ms: u64,
    pub(crate) look_sweep_yaw_rad: f64,
    pub(crate) look_sweep_duration_ms: u64,
    pub(crate) look_sweep_steps: u64,
    pub(crate) look_settle_ms: u64,
    pub(crate) action_hold_ms: u64,
    pub(crate) operation_pause_ms: u64,
    pub(crate) inter_round_still_ms: u64,
    pub(crate) stagger_ms: u64,
    pub(crate) scenario_name: String,
    pub(crate) scenario_seed_raw: Option<String>,
    pub(crate) landmarks: Vec<Landmark>,
    // Decoupled quantized control (v7).
    pub(crate) yaw_rate_deg_s: f64,
    pub(crate) pitch_rate_deg_s: f64,
    /// "planned" (pathfinder waypoints, default) or "straight" (direct line).
    pub(crate) nav_mode: String,
    /// Gap-jump capability; default off until validated on the build host.
    pub(crate) allow_parkour: bool,
    /// Minimum ticks a non-stop WalkDirection identity must stand before the controller may
    /// switch to a different one — the clock keeps ticking through brief stops, so a 1-2-tick
    /// `None` gap can't smuggle an instant flip through (see control::latch_step). 0 disables
    /// the latch AND the overshoot settle. Same-direction resumes are never delayed; a TRANSIENT
    /// stop is now also held off while the FINAL GOAL (not the corridor node — see latch_step) is
    /// further than REVERSAL_ARRIVE_DIST, because the 2026-08 action audit found 27-49% of
    /// movement-key presses lasting under 0.5 s, which is this dwell floor being escaped through
    /// the stop path rather than any real decision.
    pub(crate) min_dwell_ticks: u32,
    /// Probability that a movement bout sprints, drawn ONCE per bout from the per-bot seed.
    /// Sprint used to be a pure function of "moving forward and still further than
    /// SPRINT_APPROACH_DIST from the goal", which made the recorded sprint bit carry no
    /// independent information: the 2026-08 audit found only 20 sustained (>= 1 s) forward runs
    /// in 487,160 frames that never sprinted. A model cannot learn a bit that is decided by the
    /// rest of the state. 0.0 reproduces walk-only data, 1.0 the old always-sprint behaviour.
    pub(crate) sprint_bout_prob: f64,
    /// Post-finish ambient behaviour: once its script is done, keep a bot moving inside a leash
    /// around where it finished instead of standing still until the recording stops. The same
    /// audit measured 59.9% of frames idle, with single frozen stretches up to 114.5 s — a
    /// 189-frame training window is 9.45 s, so those are runs of 5-12 consecutive windows in
    /// which the action conditioning is constant and the video is nearly static.
    pub(crate) ambient_when_finished: bool,
    /// Radius (blocks) of the post-finish wander. Deliberately small: a finished bot must not
    /// wander into peers whose scripts are still running, which is the head-box occlusion the
    /// same review found in 11.95% of posed frames.
    pub(crate) ambient_leash: f64,
    /// Ticks each ambient target stands before the next one is drawn.
    pub(crate) ambient_switch_ticks: u32,
    /// Consecutive ticks of NO scenario intent (Idle loco + Hold camera) after which the controller
    /// takes over with the ambient wander. This is the mid-script half of the dead-air fix: the
    /// worst scenarios never finish, they reach a terminal branch and loop in it until the episode
    /// duration is met, so the post-finish path never fires for them. Generous on purpose — the
    /// default is 10 s, and the freezes this targets ran 45-154 s — so a scenario's deliberate
    /// one-second beat is never interrupted. 0 disables the takeover.
    pub(crate) ambient_idle_ticks: u32,
    /// Max eye→face distance (blocks) a real block placement will reach for. Vanilla survival
    /// block-interaction range is ~4.5; we gate a touch tighter so placements stay in clear LOS.
    pub(crate) place_reach: f64,
    /// Hard cap (ms) on a single `Ctx::dig` (real `Client::mine`) — solaris digWithTimeout parity.
    /// Bounds the wait so an unreachable/stuck break can't hang a mining episode.
    pub(crate) dig_timeout_ms: u64,
}

impl Cfg {
    /// Read and resolve the config document named by [`CONFIG_ENV`].
    ///
    /// A missing variable, an unreadable file, or a document with an unknown or ill-typed field is
    /// fatal and says which: the runtime cannot report "I ran on defaults because your key was
    /// misspelled" after the fact, so it refuses to start instead.
    pub(crate) fn load() -> Result<Cfg, String> {
        let path = env::var(CONFIG_ENV)
            .ok()
            .filter(|v| !v.is_empty())
            .ok_or_else(|| format!("{CONFIG_ENV} is not set; it must name a config JSON file"))?;
        let text = fs::read_to_string(&path).map_err(|e| format!("{CONFIG_ENV}={path}: {e}"))?;
        let raw: PlayerRunConfig =
            serde_json::from_str(&text).map_err(|e| format!("{path}: {e}"))?;
        Ok(Cfg::resolve(raw))
    }

    /// Apply this runtime's defaults and range clamps to a parsed document.
    pub(crate) fn resolve(raw: PlayerRunConfig) -> Cfg {
        let bot_count = raw.bot_count.unwrap_or(1).max(1) as usize;
        let shard_count = raw.shard_count.unwrap_or(1).max(1) as usize;
        let shard_index = (raw.shard_index.unwrap_or(0).max(0) as usize).min(shard_count - 1);
        let index_offset = raw.bot_index_offset.unwrap_or(0).max(0) as usize;
        let total_bot_count = raw
            .total_bot_count
            .unwrap_or(bot_count as i64)
            .max(bot_count as i64) as usize;
        let far_players = raw.far_players.unwrap_or(0).max(0) as usize;
        // `multi` only picks the timeout default and informational topology label.
        let multi = total_bot_count > 1 || shard_count > 1;
        let episode_duration_ms = raw.episode_duration_ms.unwrap_or(0).max(0);
        let episode_timeout_margin_ms = raw.episode_timeout_margin_ms.unwrap_or(120000).max(0);
        let hard_timeout_ms = raw
            .hard_timeout_ms
            .unwrap_or(if multi { 240000 } else { 180000 })
            .max(1) as u64;
        let effective_hard_timeout_ms = if episode_duration_ms > 0 {
            hard_timeout_ms.max((episode_duration_ms + episode_timeout_margin_ms) as u64)
        } else {
            hard_timeout_ms
        };
        let lane_spacing = raw.lane_spacing.unwrap_or(8.0);
        let landmark_distance = raw.landmark_distance.unwrap_or(9.0);
        // Read once and reused for the parkour implication below, so the two can never disagree.
        let scenario_name = raw
            .scenario
            .clone()
            .unwrap_or_else(|| "landmark_tour".into());
        let landmarks = vec![
            Landmark {
                id: "L0".into(),
                ordinal: 0,
                x: landmark_distance,
                z: 0.0,
            },
            Landmark {
                id: "L1".into(),
                ordinal: 1,
                x: 0.0,
                z: landmark_distance,
            },
            Landmark {
                id: "L2".into(),
                ordinal: 2,
                x: -landmark_distance,
                z: 0.0,
            },
            Landmark {
                id: "L3".into(),
                ordinal: 3,
                x: 0.0,
                z: -landmark_distance,
            },
        ];
        Cfg {
            bot_count,
            shard_index,
            shard_count,
            index_offset,
            total_bot_count,
            far_players,
            shard_spread: raw
                .shard_spread
                .unwrap_or(if shard_count > 1 { 24.0 } else { 0.0 })
                .max(0.0),
            tokio_threads: raw.tokio_threads.unwrap_or(1).max(1) as usize,
            runtime_topology: if shard_count > 1 {
                "multi_process_sharded"
            } else if multi {
                "single_process_multi_player"
            } else {
                "single_process_single_player"
            }
            .to_string(),
            episode_id: raw.episode_id.unwrap_or_else(|| "cs-episode-001".into()),
            host: raw.host.unwrap_or_else(|| "127.0.0.1".into()),
            port: raw.port.unwrap_or(25565).clamp(1, 65535) as u16,
            view_distance: raw.view_distance.unwrap_or(8).clamp(2, 32) as u8,
            out_dir: PathBuf::from(raw.out_dir.unwrap_or_else(|| "/shared/capture".into())),
            signal_dir: PathBuf::from(raw.signal_dir.unwrap_or_else(|| "/shared/signal".into())),
            go_timeout_ms: raw.go_timeout_ms.unwrap_or(120000).max(1) as u64,
            effective_hard_timeout_ms,
            episode_duration_ms,
            lane_spacing,
            landmark_distance,
            formation_radius: raw.formation_radius.unwrap_or(lane_spacing.max(0.0)),
            goal_radius: raw.goal_radius.unwrap_or(2.0),
            allow_sprinting: raw.allow_sprinting.unwrap_or(true),
            move_timeout_ms: raw.move_timeout_ms.unwrap_or(30000).max(1) as u64,
            move_no_progress_ms: raw.move_no_progress_ms.unwrap_or(2500).max(0) as u64,
            look_sweep_yaw_rad: raw.look_sweep_yaw_rad.unwrap_or(0.45).max(0.0),
            look_sweep_duration_ms: raw.look_sweep_duration_ms.unwrap_or(1800).max(0) as u64,
            look_sweep_steps: raw.look_sweep_steps.unwrap_or(24).max(1) as u64,
            look_settle_ms: raw.look_settle_ms.unwrap_or(500).max(0) as u64,
            action_hold_ms: raw.action_hold_ms.unwrap_or(1200).max(0) as u64,
            operation_pause_ms: raw.operation_pause_ms.unwrap_or(700).max(0) as u64,
            inter_round_still_ms: raw.inter_round_still_ms.unwrap_or(1500).max(0) as u64,
            stagger_ms: raw.stagger_ms.unwrap_or(300).max(0) as u64,
            scenario_name: scenario_name.clone(),
            scenario_seed_raw: raw.scenario_seed.filter(|v| !v.is_empty()),
            landmarks,
            yaw_rate_deg_s: raw
                .yaw_rate_deg_s
                .unwrap_or(control::DEFAULT_YAW_RATE_DEG_S)
                .max(1.0),
            pitch_rate_deg_s: raw
                .pitch_rate_deg_s
                .unwrap_or(control::DEFAULT_PITCH_RATE_DEG_S)
                .max(1.0),
            nav_mode: raw.nav_mode.unwrap_or_else(|| "planned".into()),
            // The gap_jump scenario is pointless without parkour (its whole job is crossing the
            // gap_jump scenery's gaps), so it implies parkour regardless of the allow_parkour
            // default — you don't have to remember to enable both. Parkour stays opt-in elsewhere.
            allow_parkour: raw.allow_parkour.unwrap_or(false) || scenario_name == "gap_jump",
            // 10 ticks = 0.5 s: the dataset-quality floor for a WASD press (user spec: every
            // press >= 0.5 s, short presses only at a movement segment's tail — stops and
            // same-direction resumes are never delayed, so arrival taps stay possible).
            min_dwell_ticks: raw.min_dwell_ticks.unwrap_or(10).max(0) as u32,
            sprint_bout_prob: raw.sprint_bout_prob.unwrap_or(0.5).clamp(0.0, 1.0),
            ambient_when_finished: raw.ambient_when_finished.unwrap_or(true),
            ambient_leash: raw.ambient_leash.unwrap_or(4.0).clamp(0.0, 32.0),
            ambient_switch_ticks: raw.ambient_switch_ticks.unwrap_or(30).clamp(1, 400) as u32,
            ambient_idle_ticks: raw.ambient_idle_ticks.unwrap_or(200).max(0) as u32,
            place_reach: raw.place_reach.unwrap_or(4.0).clamp(2.5, 4.5),
            dig_timeout_ms: raw.dig_timeout_ms.unwrap_or(8000).max(1000) as u64,
        }
    }

    /// The swarm's identity in every recorded document. Constant, not configuration: the
    /// prefixes were config keys the one writer always set to these exact strings, so nothing
    /// could ever observe another value.
    pub(crate) fn username(&self, index: usize) -> String {
        format!("CapBot{}", self.index_offset + index)
    }

    pub(crate) fn player_id(&self, index: usize) -> String {
        format!("player-{}", self.index_offset + index)
    }

    /// Signals each shard raises independently get a `-<shard>` suffix so the server
    /// coordinator can wait for all P of them; single-shard swarms keep the bare names.
    pub(crate) fn shard_signal(&self, name: &str) -> String {
        if self.shard_count > 1 {
            format!("{}-{}", name, self.shard_index)
        } else {
            name.to_string()
        }
    }

    pub(crate) fn engine_config(&self) -> EngineConfig {
        EngineConfig {
            lane_spacing: self.lane_spacing,
            landmark_distance: self.landmark_distance,
            formation_radius: self.formation_radius,
            move_timeout_ms: self.move_timeout_ms,
        }
    }
}

#[cfg(test)]
mod tests {
    use pawa_schema::PlayerRunConfig;

    use super::Cfg;

    /// An empty document must produce exactly the runtime's documented defaults.
    #[test]
    fn an_empty_document_yields_the_documented_defaults() {
        let cfg = Cfg::resolve(PlayerRunConfig::default());
        assert_eq!(cfg.bot_count, 1);
        assert_eq!(cfg.shard_count, 1);
        assert_eq!(cfg.shard_index, 0);
        assert_eq!(cfg.total_bot_count, 1);
        assert_eq!(cfg.host, "127.0.0.1");
        assert_eq!(cfg.port, 25565);
        assert_eq!(cfg.view_distance, 8);
        assert_eq!(cfg.go_timeout_ms, 120000);
        assert_eq!(cfg.effective_hard_timeout_ms, 180000); // single-bot default
        assert_eq!(cfg.lane_spacing, 8.0);
        assert_eq!(cfg.landmark_distance, 9.0);
        assert_eq!(cfg.formation_radius, 8.0); // defaults to lane_spacing
        assert_eq!(cfg.goal_radius, 2.0);
        assert!(cfg.allow_sprinting);
        assert!(!cfg.allow_parkour);
        assert_eq!(cfg.move_timeout_ms, 30000);
        assert_eq!(cfg.min_dwell_ticks, 10);
        assert_eq!(cfg.sprint_bout_prob, 0.5);
        assert!(cfg.ambient_when_finished);
        assert_eq!(cfg.ambient_leash, 4.0);
        assert_eq!(cfg.ambient_switch_ticks, 30);
        assert_eq!(cfg.ambient_idle_ticks, 200);
        assert_eq!(cfg.place_reach, 4.0);
        assert_eq!(cfg.dig_timeout_ms, 8000);
        assert_eq!(cfg.tokio_threads, 1);
        assert_eq!(cfg.nav_mode, "planned");
        assert_eq!(cfg.scenario_name, "landmark_tour");
        assert_eq!(cfg.landmarks.len(), 4);
        assert_eq!(cfg.runtime_topology, "single_process_single_player");
    }

    /// The clamps are the validation layer. A document may ask for anything; the runtime still has
    /// to stay in a range it can actually operate in, and it must be visible where that happens.
    #[test]
    fn out_of_range_values_are_clamped_not_accepted() {
        let raw = PlayerRunConfig {
            bot_count: Some(-5),
            shard_count: Some(2),
            shard_index: Some(99), // beyond the last shard
            port: Some(999_999),
            view_distance: Some(999),
            place_reach: Some(100.0),    // far past block-interaction range
            sprint_bout_prob: Some(3.0), // not a probability
            ambient_switch_ticks: Some(0),
            ambient_leash: Some(-1.0),
            dig_timeout_ms: Some(1),
            tokio_threads: Some(0),
            ..Default::default()
        };
        let cfg = Cfg::resolve(raw);
        assert_eq!(cfg.bot_count, 1, "bot_count floors at 1");
        assert_eq!(cfg.shard_index, 1, "shard_index is bounded by shard_count");
        assert_eq!(cfg.port, 65535);
        assert_eq!(cfg.view_distance, 32);
        assert_eq!(cfg.place_reach, 4.5);
        assert_eq!(cfg.sprint_bout_prob, 1.0);
        assert_eq!(cfg.ambient_switch_ticks, 1);
        assert_eq!(cfg.ambient_leash, 0.0);
        assert_eq!(cfg.dig_timeout_ms, 1000);
        assert_eq!(cfg.tokio_threads, 1);
    }

    /// A key nobody recognises stops the run and names itself, instead of being silently replaced
    /// by a default.
    #[test]
    fn an_unknown_field_is_rejected_by_name() {
        let err = serde_json::from_str::<PlayerRunConfig>(r#"{"move_timout_ms": 9999}"#)
            .expect_err("a misspelled key must not deserialize");
        let message = err.to_string();
        assert!(
            message.contains("move_timout_ms"),
            "the error must name the offending field, got: {message}"
        );
    }

    #[test]
    fn a_wrongly_typed_field_is_rejected() {
        assert!(serde_json::from_str::<PlayerRunConfig>(r#"{"bot_count": "eight"}"#).is_err());
        assert!(serde_json::from_str::<PlayerRunConfig>(r#"{"allow_parkour": 1}"#).is_err());
    }

    /// Two derivations that are not simple defaults, and would be easy to lose in the move.
    #[test]
    fn derived_values_survive_the_move() {
        let multi = Cfg::resolve(PlayerRunConfig {
            bot_count: Some(4),
            ..Default::default()
        });
        assert_eq!(multi.effective_hard_timeout_ms, 240000, "multi-bot default");
        assert_eq!(multi.runtime_topology, "single_process_multi_player");

        let sharded = Cfg::resolve(PlayerRunConfig {
            bot_count: Some(4),
            shard_count: Some(3),
            ..Default::default()
        });
        assert_eq!(sharded.runtime_topology, "multi_process_sharded");
        assert_eq!(sharded.shard_spread, 24.0, "sharded default spread");

        // episode_duration_ms + margin raises the hard timeout above the bare default.
        let long = Cfg::resolve(PlayerRunConfig {
            episode_duration_ms: Some(600_000),
            ..Default::default()
        });
        assert_eq!(long.effective_hard_timeout_ms, 600_000 + 120_000);

        // gap_jump implies parkour even when the document leaves it off.
        let gap = Cfg::resolve(PlayerRunConfig {
            scenario: Some("gap_jump".into()),
            ..Default::default()
        });
        assert!(gap.allow_parkour, "gap_jump is pointless without parkour");
    }
}
