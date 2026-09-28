//! Scenario engine: a parity port of player-mineflayer/bot/scenarios.js for the Azalea
//! capture runtime. SAME seeded-parameter semantics: mulberry32 RNG, FNV-1a (`hashString`)
//! string seeding, one shared plan built from the swarm-level RNG (identical for every bot
//! in the single process), per-bot RNG `mulberry32(seed ^ imul(index+1, 0x9E3779B9))`, and
//! time-sliced phases driven by elapsed wall-clock ms since the episode epoch.
//!
//! The Azalea route implements the FULL Mineflayer scenario set (6 parity scenarios):
//!   landmark_tour, follow_leader, formation_walk, group_orbit, audience_build, pair_chase,
//! PLUS a family of N>2-native multiplayer episodes ported from the solaris reference
//! (mob_chase, straight_line_walk, collector, build_house, walk_look, walk_look_away,
//! heading_mine, build_tower, independent_build_structure, independent_build_tower).
//!
//! For the 6 PARITY scenarios the shared-plan parameter draws match buildShared() in
//! scenarios.js EXACTLY (same mulberry32 RNG call order), so a given seed yields the same
//! parameters as Mineflayer. The N>2 episodes have no JS counterpart: they sample from
//! the same seeded swarm RNG but are not bit-exact to anything — only correct, terminating,
//! grounded, and N-scalable.
//!
//! `auto` picks from the FULL implemented name list (one swarm-RNG draw, like the JS engine).
//! Because the candidate list matches Mineflayer's 6 for the parity prefix but also
//! includes the N>2 episodes, `auto` can resolve to any implemented scenario. An unknown
//! scenario name falls back to landmark_tour; `requested` always records what was asked for.
//! `pair_chase` with bot_count < 2 falls back to landmark_tour (matching the JS engine).

use pawa_schema::catalog;
use std::collections::HashMap;

use parking_lot::Mutex;
use std::time::Instant;

/// Native multiplayer episodes with no JS counterpart. They are grounded and deterministic,
/// with rank-stable roles by global player index.
pub const NATIVE_SCENARIO_NAMES: [&str; 20] = [
    "mob_chase",
    "straight_line_walk",
    "gap_jump",
    "collector",
    "build_house",
    "walk_look",
    "walk_look_away",
    "heading_mine",
    "build_tower",
    "independent_build_structure",
    "independent_build_tower",
    // --- combat (real azalea Client::attack) ---
    "pvp",
    "pve",
    // --- calibration / QA (not a swarm scenario; every bot runs the same scripted reference) ---
    "calibration",
    // --- many-player spatial episodes (Research Idea 13: cross-region + long-horizon) ---
    "cross_region_rendezvous",
    "rendezvous_after_edits",
    "split_and_reunite",
    "depart_and_revisit",
    "crowded_interaction",
    "regional_capacity_traffic",
];

/// Whether the runtime implements this scenario name.
fn is_known_scenario(name: &str) -> bool {
    catalog::scenario(name).is_some()
}

// ---------------------------------------------------------------------------------------------
// RNG primitives (bit-exact ports of the JS implementations).
// ---------------------------------------------------------------------------------------------

/// mulberry32: identical sequence to the JS `mulberry32(seed)` for the same 32-bit seed.
pub struct Mulberry32 {
    a: u32,
}

impl Mulberry32 {
    pub fn new(seed: u32) -> Self {
        Self { a: seed }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> f64 {
        self.a = self.a.wrapping_add(0x6D2B79F5);
        let a = self.a;
        let mut t = (a ^ (a >> 15)).wrapping_mul(a | 1);
        t = t.wrapping_add((t ^ (t >> 7)).wrapping_mul(t | 61)) ^ t;
        ((t ^ (t >> 14)) as f64) / 4294967296.0
    }
}

/// FNV-1a over UTF-16 code units — identical to the JS `hashString` (charCodeAt).
pub fn hash_string(s: &str) -> u32 {
    let mut h: u32 = 2166136261;
    for cu in s.encode_utf16() {
        h ^= cu as u32;
        h = h.wrapping_mul(16777619);
    }
    h
}

/// JS `x >>> 0` (ToUint32) for finite numeric seeds.
pub fn to_uint32(v: f64) -> u32 {
    if !v.is_finite() {
        return 0;
    }
    let t = v.trunc();
    let m = t.rem_euclid(4294967296.0);
    m as u32
}

fn shuffled(rng: &mut Mulberry32, len: usize) -> Vec<usize> {
    let mut out: Vec<usize> = (0..len).collect();
    let mut i = len;
    while i > 1 {
        i -= 1;
        let j = (rng.next() * ((i + 1) as f64)).floor() as usize;
        out.swap(i, j);
    }
    out
}

fn pick_index(rng: &mut Mulberry32, len: usize) -> usize {
    ((rng.next() * len as f64).floor() as usize).min(len.saturating_sub(1))
}

fn range(rng: &mut Mulberry32, lo: f64, hi: f64) -> f64 {
    lo + rng.next() * (hi - lo)
}

pub fn r2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

// ---------------------------------------------------------------------------------------------
// Shared per-swarm plan (computed ONCE from the swarm-level rng; identical for every bot).
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Landmark {
    pub id: String,
    pub ordinal: usize,
    pub x: f64,
    pub z: f64,
}

#[derive(Clone, Debug, Default)]
pub struct EngineConfig {
    pub lane_spacing: f64,
    pub landmark_distance: f64,
    pub formation_radius: f64,
    pub move_timeout_ms: u64,
}

#[derive(Clone, Debug)]
pub struct BuiltLandmark {
    pub by: String,
    pub x: i64,
    pub y: i64,
    pub z: i64,
}

/// One world edit (place or dig) a bot committed, kept so OTHER bots can turn and witness it.
/// Pushed by the Ctx world-action primitives on every verified edit; consumed by
/// `scenarios::common::witness_peer_edits`. Same swarm-shared role as [`BuiltLandmark`], but for
/// every edit rather than landmark markers only.
#[derive(Clone, Copy, Debug)]
pub struct RecentEdit {
    /// Shard-local index of the editor.
    pub by: usize,
    pub x: i64,
    pub y: i64,
    pub z: i64,
    pub at: Instant,
}

/// A formation_walk leg: a waypoint plus the formation shape held while walking to it.
#[derive(Clone, Debug)]
pub struct FormationLeg {
    pub x: f64,
    pub z: f64,
    pub shape: String,
}

/// The seeded shared plan. Field draws replicate buildShared() in scenarios.js exactly
/// (same rng call order), so the same seed yields the same parameters as Mineflayer for
/// the scenarios both runtimes implement.
pub struct SharedPlan {
    pub regional_traffic: Mutex<pawa_schema::regions::Traffic>,
    pub epoch_t0: Mutex<Option<Instant>>,
    // follow_leader
    pub leader_order: Vec<usize>,
    pub leader_term_ms: f64,
    pub waypoints: Vec<(f64, f64)>,
    pub follow_gap: f64,
    // group_orbit
    pub center: (f64, f64),
    pub pivot_order: Vec<usize>,
    pub pivot_term_ms: f64,
    pub direction: f64,
    pub radii: Vec<f64>,
    pub angular_step_rad: f64,
    // landmark_tour
    pub round_rotation_rad: f64,
    pub built_landmarks: Mutex<HashMap<String, BuiltLandmark>>,
    /// Append-only log of the swarm's world edits this episode (see [`RecentEdit`]). Bounded by
    /// what an episode can physically place/dig, so it is never trimmed; each bot keeps its own
    /// read cursor (`Ctx::witness_cursor`).
    pub recent_edits: Mutex<Vec<RecentEdit>>,
    // formation_walk
    pub leg_period_ms: f64,
    pub legs: Vec<FormationLeg>,
    pub slot_spacing: f64,
    // audience_build
    pub builder_order: Vec<usize>,
    pub round_ms: f64,
    pub stages: Vec<(f64, f64)>,
    pub patterns: Vec<String>,
    pub arc_radius: f64,
    // pair_chase
    pub pairing: Vec<usize>,
    pub pair_count: usize,
    pub sector_radius: (f64, f64),
    pub tag_distance: f64,
    pub evade_waypoints_per_sector: Vec<Vec<(f64, f64)>>,
    // --- N>2 native episodes (no JS counterpart) ---
    // generic waypoints / scatter points reused by several native episodes
    pub native_round_ms: f64,
    pub native_waypoints: Vec<(f64, f64)>, // straight_line_walk / formation translations
    pub native_spacing: f64,
    pub native_scatter: Vec<(f64, f64)>, // collector scatter points
    pub native_claimed: Mutex<Vec<bool>>, // collector first-come claim flags
    pub native_house_size: i64,          // build_house footprint NxN
    pub native_house_height: i64,
    pub native_block_pattern: String, // chosen material/pattern id
    pub native_heading_rad: f64,      // heading_mine direction
    pub native_trail_len: i64,
    pub native_tower_height: i64,
    pub native_structure_kind: String,
    // combat episodes (pvp/pve)
    pub native_combat_duration_ms: f64, // pvp: per-pair combat-loop budget (~10-15 s)
    pub native_mob_cycles: i64,         // pve: how many find-kill-return cycles per bot (~2-5)
}

impl SharedPlan {
    fn empty() -> Self {
        SharedPlan {
            regional_traffic: Mutex::new(pawa_schema::regions::Traffic::new(0)),
            epoch_t0: Mutex::new(None),
            leader_order: Vec::new(),
            leader_term_ms: 0.0,
            waypoints: Vec::new(),
            follow_gap: 0.0,
            center: (0.0, 0.0),
            pivot_order: Vec::new(),
            pivot_term_ms: 0.0,
            direction: 1.0,
            radii: Vec::new(),
            angular_step_rad: 0.0,
            round_rotation_rad: 0.0,
            built_landmarks: Mutex::new(HashMap::new()),
            recent_edits: Mutex::new(Vec::new()),
            leg_period_ms: 0.0,
            legs: Vec::new(),
            slot_spacing: 0.0,
            builder_order: Vec::new(),
            round_ms: 0.0,
            stages: Vec::new(),
            patterns: Vec::new(),
            arc_radius: 0.0,
            pairing: Vec::new(),
            pair_count: 0,
            sector_radius: (0.0, 0.0),
            tag_distance: 0.0,
            evade_waypoints_per_sector: Vec::new(),
            native_round_ms: 0.0,
            native_waypoints: Vec::new(),
            native_spacing: 0.0,
            native_scatter: Vec::new(),
            native_claimed: Mutex::new(Vec::new()),
            native_house_size: 0,
            native_house_height: 0,
            native_block_pattern: String::new(),
            native_heading_rad: 0.0,
            native_trail_len: 0,
            native_tower_height: 0,
            native_structure_kind: String::new(),
            native_combat_duration_ms: 0.0,
            native_mob_cycles: 0,
        }
    }

    /// Phase index since episode start: all bots share epoch_t0 (set on first use).
    pub fn phase_at(&self, period_ms: f64) -> i64 {
        let mut epoch = self.epoch_t0.lock();
        let t0 = *epoch.get_or_insert_with(Instant::now);
        let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;
        (elapsed_ms / period_ms.max(1.0)).floor() as i64
    }
}

fn waypoint(rng: &mut Mulberry32, rmin: f64, rmax: f64) -> (f64, f64) {
    let ang = rng.next() * std::f64::consts::PI * 2.0;
    let rad = range(rng, rmin, rmax);
    (r2(ang.cos() * rad), r2(ang.sin() * rad))
}

fn plan_name(name: &str) -> &str {
    match name {
        // Generation-7 pilot aliases reuse reviewed handlers, but still need the handler's shared
        // plan draws. Keeping this translation here avoids empty builder/formation/pivot vectors.
        "v7_interact_cairn"
        | "v7_interact_ferry"
        | "v7_interact_layers"
        | "v7_interact_handoff" => "audience_build",
        _ => name,
    }
}

fn build_shared(
    name: &str,
    rng: &mut Mulberry32,
    bot_count: usize,
    config: &EngineConfig,
) -> SharedPlan {
    let bound = (config.landmark_distance + config.formation_radius).max(24.0);
    let mut shared = SharedPlan::empty();
    if name == pawa_schema::regions::SCENARIO {
        shared.regional_traffic = Mutex::new(pawa_schema::regions::Traffic::new(bot_count));
    }
    let name = plan_name(name);
    match name {
        "follow_leader" => {
            shared.leader_order = shuffled(rng, bot_count);
            shared.leader_term_ms = range(rng, 40000.0, 70000.0).round();
            shared.waypoints = (0..64)
                .map(|_| waypoint(rng, bound * 0.35, bound))
                .collect();
            shared.follow_gap = range(rng, 2.5, 4.5);
        }
        "formation_walk" => {
            // JS: legPeriod = max(moveTimeoutMs + 12000, 40000)
            let leg_period = ((config.move_timeout_ms as f64) + 12000.0).max(40000.0);
            shared.leg_period_ms = range(rng, leg_period, leg_period + 15000.0).round();
            // 16 legs: each draws a waypoint (ang, rad) THEN a shape pick — in that order.
            let shapes = ["line", "wedge", "circle", "column"];
            shared.legs = (0..16)
                .map(|_| {
                    let (x, z) = waypoint(rng, bound * 0.3, bound);
                    let shape = shapes[pick_index(rng, shapes.len())].to_string();
                    FormationLeg { x, z, shape }
                })
                .collect();
            shared.slot_spacing = range(rng, 2.5, 4.0);
        }
        "group_orbit" => {
            shared.center = waypoint(rng, 0.0, (config.landmark_distance * 0.5).max(4.0));
            shared.pivot_order = shuffled(rng, bot_count);
            shared.pivot_term_ms = range(rng, 50000.0, 80000.0).round();
            shared.direction = if rng.next() < 0.5 { 1.0 } else { -1.0 };
            shared.radii = (0..bot_count)
                .map(|i| {
                    r2((config.formation_radius * 0.75).max(6.0)
                        + ((i % 4) as f64) * 3.0
                        + range(rng, -1.0, 1.0))
                })
                .collect();
            shared.angular_step_rad =
                range(rng, std::f64::consts::PI / 7.0, std::f64::consts::PI / 4.0);
        }
        "audience_build" => {
            shared.builder_order = shuffled(rng, bot_count);
            shared.round_ms = range(rng, 45000.0, 70000.0).round();
            shared.stages = (0..12)
                .map(|_| waypoint(rng, bound * 0.25, bound * 0.8))
                .collect();
            let patterns = ["pillar", "wall", "arch", "tower", "cross"];
            shared.patterns = (0..12)
                .map(|_| patterns[pick_index(rng, patterns.len())].to_string())
                .collect();
            shared.arc_radius = range(rng, 6.0, 9.0);
        }
        "pair_chase" => {
            shared.pairing = shuffled(rng, bot_count);
            let pair_count = (bot_count / 2).max(1);
            shared.pair_count = pair_count;
            shared.sector_radius = ((bound * 0.3).max(8.0), bound);
            shared.tag_distance = range(rng, 3.0, 4.0);
            let (smin, smax) = shared.sector_radius;
            shared.evade_waypoints_per_sector = (0..pair_count)
                .map(|p| {
                    let sector_base = (p as f64 / pair_count as f64) * std::f64::consts::PI * 2.0;
                    let sector_width = (std::f64::consts::PI * 2.0) / pair_count as f64;
                    (0..24)
                        .map(|_| {
                            let ang = sector_base + rng.next() * sector_width;
                            let rad = range(rng, smin, smax);
                            (r2(ang.cos() * rad), r2(ang.sin() * rad))
                        })
                        .collect()
                })
                .collect();
        }
        _ if NATIVE_SCENARIO_NAMES.contains(&name) => {
            build_native_shared(name, rng, bot_count, config, &mut shared, bound);
        }
        // Every other scenario draws NOTHING from the swarm RNG: its randomness comes from the
        // per-bot stream, so one scenario's parameters can never depend on which others exist.
        // The rotation below is landmark_tour's — the only reader of the field — and is inert
        // for the rest.
        _ => {
            shared.round_rotation_rad = std::f64::consts::PI / 8.0;
        }
    }
    shared
}

/// Shared-plan draws for the N>2 native episodes (no JS counterpart): the draw order is
/// our own, deterministic per seed, designed so every bot in the single process computes the
/// identical plan. All values are grounded/bounded so the episodes terminate and stay near
/// spawn.
fn build_native_shared(
    name: &str,
    rng: &mut Mulberry32,
    bot_count: usize,
    config: &EngineConfig,
    shared: &mut SharedPlan,
    bound: f64,
) {
    // A short per-round budget keeps phase-driven episodes terminating with the episode target.
    shared.native_round_ms = range(rng, 18000.0, 30000.0).round();
    match name {
        "mob_chase" => {
            // ~2 flee rounds; each round the prey re-flees from the group centroid. The flee
            // headings are seeded so all bots agree on the prey's coarse heading per round.
            let _ = range(rng, bound * 0.5, bound).max(10.0); // retired native_radius draw
            // up to 6 rounds of seeded flee headings (we only use ~2-3 before target_met).
            shared.native_waypoints = (0..6)
                .map(|_| {
                    let ang = rng.next() * std::f64::consts::TAU;
                    let rad = range(rng, bound * 0.4, bound);
                    (r2(ang.cos() * rad), r2(ang.sin() * rad))
                })
                .collect();
            shared.tag_distance = range(rng, 2.5, 3.5); // proximity = capture signal (no PvP)
        }
        "straight_line_walk" => {
            shared.native_spacing = range(rng, 2.5, 4.0);
            // A sequence of waypoints (the shared line marches through them).
            shared.native_waypoints = (0..5)
                .map(|_| {
                    let ang = rng.next() * std::f64::consts::TAU;
                    let rad = range(rng, bound * 0.5, bound);
                    (r2(ang.cos() * rad), r2(ang.sin() * rad))
                })
                .collect();
        }
        "collector" => {
            // K scatter points; first-come claim via a shared mutex flag vector.
            let k = (bot_count * 3).clamp(6, 24);
            shared.native_scatter = (0..k)
                .map(|_| {
                    let ang = rng.next() * std::f64::consts::TAU;
                    let rad = range(rng, bound * 0.2, bound);
                    (r2(ang.cos() * rad), r2(ang.sin() * rad))
                })
                .collect();
            *shared.native_claimed.lock() = vec![false; k];
        }
        "build_house" => {
            shared.native_house_size = 6 + (rng.next() * 3.0).floor() as i64; // 6..8 footprint
            shared.native_house_height = 3 + (rng.next() * 2.0).floor() as i64; // 3..4 high
            let mats = [
                "minecraft:oak_planks",
                "minecraft:stone_bricks",
                "minecraft:bricks",
            ];
            shared.native_block_pattern = mats[pick_index(rng, mats.len())].to_string();
        }
        "walk_look" | "walk_look_away" => {
            shared.native_waypoints = (0..6)
                .map(|_| {
                    let ang = rng.next() * std::f64::consts::TAU;
                    let rad = range(rng, bound * 0.3, bound);
                    (r2(ang.cos() * rad), r2(ang.sin() * rad))
                })
                .collect();
        }
        "heading_mine" => {
            shared.native_heading_rad = rng.next() * std::f64::consts::TAU;
            shared.native_trail_len = 5; // complete 1x2 tunnel plus scheduled verified torch
            let mats = [
                "minecraft:cobblestone",
                "minecraft:stone",
                "minecraft:andesite",
            ];
            shared.native_block_pattern = mats[pick_index(rng, mats.len())].to_string();
            shared.native_spacing = range(
                rng,
                config.lane_spacing.max(6.0),
                config.lane_spacing.max(6.0) + 6.0,
            );
        }
        "build_tower" | "independent_build_tower" => {
            shared.native_tower_height = 8 + (rng.next() * 5.0).floor() as i64; // 8..12 high
            let mats = [
                "minecraft:oak_planks",
                "minecraft:stone_bricks",
                "minecraft:bricks",
            ];
            shared.native_block_pattern = mats[pick_index(rng, mats.len())].to_string();
            shared.native_spacing = range(
                rng,
                config.formation_radius.max(8.0),
                config.formation_radius.max(8.0) + 6.0,
            );
        }
        "independent_build_structure" => {
            let kinds = ["pillar", "cross", "wall", "arch"];
            shared.native_structure_kind = kinds[pick_index(rng, kinds.len())].to_string();
            let mats = [
                "minecraft:oak_planks",
                "minecraft:stone_bricks",
                "minecraft:bricks",
            ];
            shared.native_block_pattern = mats[pick_index(rng, mats.len())].to_string();
            shared.native_spacing = range(
                rng,
                config.formation_radius.max(8.0),
                config.formation_radius.max(8.0) + 6.0,
            );
        }
        "pvp" => {
            // Per-pair melee-combat-loop budget, seeded so both bots in a pair stop together
            // (solaris PVP_DURATION_MS_MIN/MAX = 10-15 s). The episode target_duration still bounds
            // the whole run; this only sizes one engagement.
            shared.native_combat_duration_ms = range(rng, 10000.0, 15000.0).round();
            shared.native_spacing = range(rng, 2.5, 3.5); // melee approach radius (solaris APPROACH_DISTANCE)
        }
        "pve" => {
            // How many find-nearest-hostile / kill / return-to-post cycles each bot runs
            // (solaris MIN_MOBS..MAX_MOBS = 2-5). Bounded so the episode terminates even if the
            // coordinator never summons mobs (each cycle then times out gracefully).
            shared.native_mob_cycles = 2 + (rng.next() * 4.0).floor() as i64; // 2..5
            shared.native_spacing = range(rng, 2.5, 3.5); // melee approach radius to the mob
        }
        "cross_region_rendezvous" | "rendezvous_after_edits" => {
            shared.native_spacing = range(rng, 3.0, 6.0);
            shared.tag_distance = range(rng, 4.0, 6.0);
            let mats = [
                "minecraft:oak_planks",
                "minecraft:stone_bricks",
                "minecraft:bricks",
            ];
            shared.native_block_pattern = mats[pick_index(rng, mats.len())].to_string();
            shared.native_tower_height = 3 + (rng.next() * 2.0).floor() as i64;
            shared.native_waypoints = (0..6)
                .map(|_| waypoint(rng, 2.0, shared.native_spacing.max(3.0) * 2.0))
                .collect();
        }
        "split_and_reunite" => {
            let ang = rng.next() * std::f64::consts::TAU;
            let rad = range(rng, bound * 0.6, bound);
            shared.native_waypoints = vec![
                (r2(ang.cos() * rad), r2(ang.sin() * rad)),
                (r2(-ang.cos() * rad), r2(-ang.sin() * rad)),
            ];
            shared.native_spacing = range(rng, 2.5, 4.0);
            shared.tag_distance = range(rng, 4.0, 6.0);
        }
        "depart_and_revisit" => {
            shared.native_waypoints = (0..8).map(|_| waypoint(rng, bound * 0.5, bound)).collect();
            let mats = [
                "minecraft:oak_planks",
                "minecraft:stone_bricks",
                "minecraft:bricks",
            ];
            shared.native_block_pattern = mats[pick_index(rng, mats.len())].to_string();
            shared.tag_distance = range(rng, 3.0, 5.0);
        }
        "crowded_interaction" => {
            shared.native_spacing = range(rng, 1.5, 2.5);
            shared.native_waypoints = (0..12).map(|_| waypoint(rng, 2.0, 8.0)).collect();
            shared.tag_distance = range(rng, 2.0, 3.0);
        }
        _ if catalog::scenario(name).is_some_and(|info| info.generation == 6) => {
            // Generation-6 scripts are deliberately authored in separate files, but many of them
            // still need one shared seeded axis/waypoint list so every bot derives the same geometry.
            // Give them that variation here without touching the named v5 arms above: adding a
            // default to every unknown name would silently change the old generation's behavior.
            shared.native_waypoints = (0..12)
                .map(|_| waypoint(rng, bound * 0.20, bound * 0.70))
                .collect();
            shared.native_spacing = range(rng, 4.5, 8.0);
            shared.tag_distance = range(rng, 3.0, 5.0);
            shared.native_block_pattern = [
                "minecraft:oak_planks",
                "minecraft:stone_bricks",
                "minecraft:bricks",
            ][pick_index(rng, 3)]
            .to_string();
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------------------------
// Engine factory.
// ---------------------------------------------------------------------------------------------

pub struct ScenarioEngine {
    pub name: String,
    pub requested: String,
    pub seed: u32,
    pub shared: SharedPlan,
}

/// Seed resolution parity with swarm-capture.js: a numeric scenario_seed is used as
/// `seed >>> 0`; any other non-empty string (or the episode id default) goes through
/// hashString.
pub fn resolve_seed(raw: Option<&str>, episode_id: &str) -> u32 {
    let value = match raw {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => episode_id.to_string(),
    };
    match value.trim().parse::<f64>() {
        Ok(num) if num.is_finite() && !value.trim().is_empty() => to_uint32(num),
        _ => hash_string(if value.is_empty() { "episode" } else { &value }),
    }
}

impl ScenarioEngine {
    pub fn create(requested: &str, seed: u32, bot_count: usize, config: &EngineConfig) -> Self {
        let requested = if requested.trim().is_empty() {
            "landmark_tour"
        } else {
            requested.trim()
        };
        let mut swarm_rng = Mulberry32::new(seed);
        let mut resolved: String = if requested == "auto" {
            // Presentation-only scenarios require explicit selection. Filtering them preserves
            // the historical auto candidate order and seeded choice for existing collections.
            let candidates: Vec<_> = catalog::SCENARIOS
                .iter()
                .filter(|s| s.generation != 0)
                .collect();
            candidates[pick_index(&mut swarm_rng, candidates.len())]
                .name
                .to_string()
        } else {
            requested.to_string()
        };
        // pair_chase needs >= 2 bots (matches the JS engine fallback).
        if resolved == "pair_chase" && bot_count < 2 {
            resolved = "landmark_tour".to_string();
        }
        if !is_known_scenario(&resolved) {
            // Unknown scenario: fall back (recorded via `requested`).
            resolved = "landmark_tour".to_string();
        }
        let shared = build_shared(&resolved, &mut swarm_rng, bot_count, config);
        ScenarioEngine {
            name: resolved,
            requested: requested.to_string(),
            seed,
            shared,
        }
    }

    /// Per-bot RNG: mulberry32((seed ^ imul(index+1, 0x9E3779B9)) >>> 0).
    pub fn bot_rng(&self, index: usize) -> Mulberry32 {
        Mulberry32::new(self.seed ^ ((index as u32).wrapping_add(1)).wrapping_mul(0x9E3779B9))
    }
}

// ---------------------------------------------------------------------------------------------
// Scenario RNG/plan tests: the expected vectors were generated from the JS reference
// implementation and are baked in here.
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn reference_config() -> EngineConfig {
        EngineConfig {
            lane_spacing: 8.0,
            landmark_distance: 9.0,
            formation_radius: 8.0,
            move_timeout_ms: 30000,
        }
    }

    #[test]
    fn mulberry32_matches_js_sequence() {
        // node -e "const{mulberry32}=require('./scenarios.js'); const r=mulberry32(12345);
        //          console.log([r(),r(),r(),r(),r()])"
        let mut rng = Mulberry32::new(12345);
        let expected = [
            0.9797282677609473,
            0.3067522644996643,
            0.484205421525985,
            0.817934412509203,
            0.5094283693470061,
        ];
        for e in expected {
            assert!((rng.next() - e).abs() < 1e-15);
        }
    }

    #[test]
    fn hash_string_matches_js() {
        // node -e "const{hashString}=require('./scenarios.js');
        //          console.log(hashString('cs-episode-001'), hashString('episode'))"
        assert_eq!(hash_string("cs-episode-001"), 2311303453);
        assert_eq!(hash_string("episode"), 4043815890);
    }

    #[test]
    fn follow_leader_shared_plan_matches_js() {
        // node: createScenarioEngine({name:'follow_leader', seed:42, botCount:4,
        //   landmarks:[], config:{laneSpacing:8, landmarkDistance:9, formationRadius:8,
        //   goalRadius:2, moveTimeoutMs:30000, actionHoldMs:1200, operationPauseMs:700,
        //   interRoundStillMs:1500, lookSweepDurationMs:1800, lookSettleMs:500}}).shared
        let engine = ScenarioEngine::create("follow_leader", 42, 4, &reference_config());
        assert_eq!(engine.name, "follow_leader");
        assert_eq!(engine.requested, "follow_leader");
        assert_eq!(engine.shared.leader_order, vec![0, 3, 1, 2]);
        assert_eq!(engine.shared.leader_term_ms, 60092.0);
        assert_eq!(engine.shared.waypoints.len(), 64);
        assert_eq!(engine.shared.waypoints[0], (7.56, 14.8));
        assert_eq!(engine.shared.waypoints[1], (-2.64, 17.95));
        assert_eq!(engine.shared.waypoints[2], (10.46, -11.8));
        assert!((engine.shared.follow_gap - 3.0829842323437333).abs() < 1e-12);
    }

    #[test]
    fn group_orbit_shared_plan_matches_js() {
        // node: createScenarioEngine({name:'group_orbit', seed:2026, botCount:5, ...}).shared
        let engine = ScenarioEngine::create("group_orbit", 2026, 5, &reference_config());
        assert_eq!(engine.shared.center, (-1.33, 0.38));
        assert_eq!(engine.shared.pivot_order, vec![1, 4, 0, 2, 3]);
        assert_eq!(engine.shared.pivot_term_ms, 70486.0);
        assert_eq!(engine.shared.direction, -1.0);
        assert_eq!(engine.shared.radii, vec![5.59, 8.09, 11.13, 15.24, 5.78]);
        assert!((engine.shared.angular_step_rad - 0.48734041961277863).abs() < 1e-12);
    }

    #[test]
    fn auto_resolves_within_azalea_set_and_unknown_falls_back() {
        let config = EngineConfig::default();
        let engine = ScenarioEngine::create("auto", 7, 3, &config);
        assert!(catalog::scenario(&engine.name).is_some());
        assert_eq!(engine.requested, "auto");
        // A presentation-only addition must not change historical seeded auto selection.
        let previous: Vec<_> = catalog::SCENARIOS
            .iter()
            .filter(|s| s.name != pawa_schema::regions::SCENARIO)
            .collect();
        for seed in 0..256 {
            let mut rng = Mulberry32::new(seed);
            let expected = previous[pick_index(&mut rng, previous.len())].name;
            assert_eq!(
                ScenarioEngine::create("auto", seed, 4, &config).name,
                expected
            );
        }

        // An unknown scenario name falls back to landmark_tour (recorded via `requested`).
        let fallback = ScenarioEngine::create("totally_unknown", 7, 3, &config);
        assert_eq!(fallback.name, "landmark_tour");
        assert_eq!(fallback.requested, "totally_unknown");

        // pair_chase is a first-class parity scenario for bot_count >= 2 ...
        let ok = ScenarioEngine::create("pair_chase", 7, 3, &config);
        assert_eq!(ok.name, "pair_chase");
        // ... but falls back to landmark_tour when there are too few bots (JS parity).
        let too_few = ScenarioEngine::create("pair_chase", 7, 1, &config);
        assert_eq!(too_few.name, "landmark_tour");
        assert_eq!(too_few.requested, "pair_chase");
    }

    #[test]
    fn formation_walk_shared_plan_matches_js() {
        // node: createScenarioEngine({name:'formation_walk', seed:42, botCount:4, ...}).shared
        let engine = ScenarioEngine::create("formation_walk", 42, 4, &reference_config());
        let s = &engine.shared;
        assert_eq!(s.leg_period_ms, 51017.0);
        assert!((s.slot_spacing - 2.939487455529161).abs() < 1e-12);
        assert_eq!(s.legs.len(), 16);
        assert_eq!(
            (s.legs[0].x, s.legs[0].z, s.legs[0].shape.as_str()),
            (-20.4, 6.87, "circle")
        );
        assert_eq!(
            (s.legs[1].x, s.legs[1].z, s.legs[1].shape.as_str()),
            (7.3, 14.29, "wedge")
        );
        assert_eq!(
            (s.legs[15].x, s.legs[15].z, s.legs[15].shape.as_str()),
            (-10.62, -0.53, "wedge")
        );
        let shapes: Vec<&str> = s.legs.iter().map(|l| l.shape.as_str()).collect();
        assert_eq!(
            shapes,
            vec![
                "circle", "wedge", "wedge", "circle", "circle", "line", "line", "wedge", "column",
                "line", "column", "line", "circle", "line", "column", "wedge",
            ]
        );
    }

    #[test]
    fn audience_build_shared_plan_matches_js() {
        // node: createScenarioEngine({name:'audience_build', seed:2026, botCount:5, ...}).shared
        let engine = ScenarioEngine::create("audience_build", 2026, 5, &reference_config());
        let s = &engine.shared;
        assert_eq!(s.builder_order, vec![0, 4, 3, 1, 2]);
        assert_eq!(s.round_ms, 48807.0);
        assert!((s.arc_radius - 8.029403489548713).abs() < 1e-12);
        assert_eq!(s.stages.len(), 12);
        assert_eq!(s.stages[0], (2.64, 14.78));
        assert_eq!(s.stages[11], (-1.73, 11.33));
        assert_eq!(
            s.patterns,
            vec![
                "wall", "tower", "cross", "wall", "cross", "pillar", "pillar", "wall", "tower",
                "tower", "wall", "cross",
            ]
        );
    }

    #[test]
    fn generation_seven_aliases_build_their_base_plans() {
        let config = reference_config();
        let audience = ScenarioEngine::create("v7_interact_cairn", 7, 4, &config);
        assert_eq!(audience.shared.builder_order.len(), 4);
        assert_eq!(audience.shared.stages.len(), 12);

        let ground = ScenarioEngine::create("v7_ground_crest", 7, 4, &config);
        assert!(ground.shared.leader_order.is_empty());
        assert!(ground.shared.native_waypoints.is_empty());
    }

    #[test]
    fn pair_chase_shared_plan_matches_js() {
        // node: createScenarioEngine({name:'pair_chase', seed:7, botCount:4, ...}).shared
        let engine = ScenarioEngine::create("pair_chase", 7, 4, &reference_config());
        let s = &engine.shared;
        assert_eq!(s.pairing, vec![2, 1, 3, 0]);
        assert_eq!(s.pair_count, 2);
        assert_eq!(s.sector_radius, (8.0, 24.0));
        assert!((s.tag_distance - 3.699028705712408).abs() < 1e-12);
        assert_eq!(s.evade_waypoints_per_sector.len(), 2);
        assert_eq!(s.evade_waypoints_per_sector[0][0], (-0.98, 14.46));
        assert_eq!(s.evade_waypoints_per_sector[0][23], (6.95, 11.81));
        assert_eq!(s.evade_waypoints_per_sector[1][0], (-9.18, -4.28));

        // N=3 -> one pair (odd leftover handled in the script).
        let e3 = ScenarioEngine::create("pair_chase", 7, 3, &reference_config());
        assert_eq!(e3.shared.pairing, vec![1, 2, 0]);
        assert_eq!(e3.shared.pair_count, 1);
        assert!((e3.shared.tag_distance - 3.97690763277933).abs() < 1e-12);
        assert_eq!(e3.shared.evade_waypoints_per_sector[0][0], (-5.15, -15.51));
        assert_eq!(e3.shared.evade_waypoints_per_sector[0][23], (-5.02, -12.3));
    }

    #[test]
    fn native_episodes_are_known_and_deterministic() {
        let config = reference_config();
        for name in NATIVE_SCENARIO_NAMES {
            let a = ScenarioEngine::create(name, 99, 4, &config);
            let b = ScenarioEngine::create(name, 99, 4, &config);
            assert_eq!(
                a.name, name,
                "native scenario {name} should resolve to itself"
            );
            // Same seed/bots -> identical native shared plan (every bot computes the same).
            assert_eq!(a.shared.native_round_ms, b.shared.native_round_ms);
            assert_eq!(a.shared.native_waypoints, b.shared.native_waypoints);
            assert_eq!(a.shared.native_scatter, b.shared.native_scatter);
            assert_eq!(a.shared.native_block_pattern, b.shared.native_block_pattern);
            assert_eq!(a.shared.native_tower_height, b.shared.native_tower_height);
            assert_eq!(
                a.shared.native_combat_duration_ms,
                b.shared.native_combat_duration_ms
            );
            assert_eq!(a.shared.native_mob_cycles, b.shared.native_mob_cycles);
        }
        // collector seeds a claim vector sized to the scatter set.
        let coll = ScenarioEngine::create("collector", 99, 4, &config);
        assert_eq!(
            coll.shared.native_claimed.lock().len(),
            coll.shared.native_scatter.len()
        );
        assert!(!coll.shared.native_scatter.is_empty());
        // pvp/pve seed bounded combat budgets.
        let pvp = ScenarioEngine::create("pvp", 99, 4, &config);
        assert!(
            pvp.shared.native_combat_duration_ms >= 10000.0
                && pvp.shared.native_combat_duration_ms <= 15000.0
        );
        let pve = ScenarioEngine::create("pve", 99, 4, &config);
        assert!(pve.shared.native_mob_cycles >= 2 && pve.shared.native_mob_cycles <= 5);
    }

    #[test]
    fn many_player_spatial_scenarios_are_registered() {
        for name in [
            "cross_region_rendezvous",
            "rendezvous_after_edits",
            "split_and_reunite",
            "depart_and_revisit",
            "crowded_interaction",
        ] {
            assert!(NATIVE_SCENARIO_NAMES.contains(&name));
            assert!(catalog::scenario(name).is_some());
            let engine = ScenarioEngine::create(name, 99, 8, &reference_config());
            assert_eq!(engine.name, name);
            assert!(engine.shared.tag_distance > 0.0);
        }
        let split = ScenarioEngine::create("split_and_reunite", 99, 8, &reference_config());
        assert_eq!(split.shared.native_waypoints.len(), 2);

        for name in NATIVE_SCENARIO_NAMES {
            assert!(catalog::scenario(name).is_some());
        }
        let mut seen = std::collections::HashSet::new();
        assert!(catalog::SCENARIOS.iter().all(|info| seen.insert(info.name)));
    }

    #[test]
    fn bot_rng_seed_mix_matches_js() {
        // JS: (42 ^ Math.imul(1, 0x9E3779B9)) >>> 0  === 2654435731
        let engine = ScenarioEngine::create("landmark_tour", 42, 2, &EngineConfig::default());
        let mut rng = engine.bot_rng(0);
        let mut reference = Mulberry32::new(2654435731);
        for _ in 0..3 {
            assert_eq!(rng.next(), reference.next());
        }
    }
}
