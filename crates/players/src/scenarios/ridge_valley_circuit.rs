use super::*;

const TAU: f64 = std::f64::consts::TAU;

/// Radial band around the anchor the circuit samples its waypoints from. The outer edge stays
/// inside loaded, prepared ground; the inner edge keeps legs long enough to read as travel.
const RADIUS_MIN: f64 = 6.0;
const RADIUS_MAX: f64 = 26.0;

/// Waypoint candidates drawn per leg; the circuit walks the one with the most vertical relief.
const CANDIDATES: usize = 6;

/// Vertical span scanned around the bot's own altitude when resolving a candidate's surface.
const SCAN_SPAN: i32 = 24;

/// A leg shorter than this is a shuffle, not a traversal.
const MIN_LEG: f64 = 6.0;

const WALK_BPS: f64 = 4.317;
const SPRINT_BPS: f64 = 5.612;

/// Ground-probe stride ahead of a blind bounding run (same rationale as `no_contact_pair_chase`:
/// a `hold_visual` leg never consults the pathfinder, so pits must truncate it up front).
const PROBE: f64 = 1.5;

fn rand_range(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

/// One sampled surface waypoint: where to stand and how high that stand is.
#[derive(Clone, Copy)]
struct Waypoint {
    x: f64,
    z: f64,
    feet_y: i32,
}

/// Feet altitude of the OPEN-SKY surface in column `(x, z)`, or `None` when the column cannot be
/// walked on. The scan descends from well above the bot's own altitude, so the first support it
/// meets is by construction the topmost — a daylight surface, never a cave floor: this scenario's
/// whole route stays under open sky by sampling instead of by trusting the pathfinder. Water and
/// lava columns are rejected outright, canopy (leaves, logs) is not a route, and a few blocks of
/// foliage (grass, flowers, snow layers) are allowed to sit above the real ground.
fn skylit_feet_y(ctx: &Ctx, x: i32, z: i32, y_hint: i32) -> Option<i32> {
    let mut fluff = 0u32;
    for y in ((y_hint - SCAN_SPAN)..=(y_hint + SCAN_SPAN)).rev() {
        let cell = BlockPos::new(x, y, z);
        let id = ctx.block_id_at(cell)?;
        if block_is_safe_support(ctx, cell) {
            if id.contains("leaves") || id.ends_with("log") {
                return None;
            }
            return Some(y + 1);
        }
        if id.contains("water") || id.contains("lava") {
            return None;
        }
        if !block_is_air(ctx, cell) {
            fluff += 1;
            if fluff > 3 {
                return None;
            }
        }
    }
    None
}

/// How far along `heading_rad` the ground keeps holding the bot up; one or two blocks of drop is a
/// slope, deeper is a gully the bounding run must stop at.
fn safe_run_distance(ctx: &Ctx, heading_rad: f64, want: f64) -> f64 {
    let Some(p) = ctx.position() else { return 0.0 };
    let feet_y = p.1.floor() as i32;
    let mut safe = 0.0;
    while safe + PROBE <= want {
        let step = safe + PROBE;
        let cell = BlockPos::new(
            (p.0 - heading_rad.sin() * step).floor() as i32,
            feet_y,
            (p.2 + heading_rad.cos() * step).floor() as i32,
        );
        if !block_is_safe_support(ctx, cell.down(1)) && !block_is_safe_support(ctx, cell.down(2)) {
            break;
        }
        safe = step;
    }
    safe
}

/// Draw waypoint candidates inside this bot's private sector and keep the ones that resolved to an
/// open-sky surface, are a real leg away, and stand clear of every peer.
fn draw_candidates(ctx: &mut Ctx, sector_center: f64, sector_half: f64) -> Vec<Waypoint> {
    let Some(own) = ctx.position() else {
        return Vec::new();
    };
    let anchor = ctx.anchor();
    let y_hint = own.1.floor() as i32;
    let mut out = Vec::new();
    for _ in 0..CANDIDATES {
        let bearing = sector_center + rand_range(ctx, -sector_half, sector_half);
        let radius = rand_range(ctx, RADIUS_MIN, RADIUS_MAX);
        let (x, z) = (
            anchor.0 + bearing.cos() * radius,
            anchor.1 + bearing.sin() * radius,
        );
        if (x - own.0).hypot(z - own.2) < MIN_LEG {
            continue;
        }
        let crowded = (0..ctx.player_count())
            .filter(|&j| j != ctx.index())
            .filter_map(|j| ctx.peer_position(j))
            .any(|p| (p.0 - x).hypot(p.2 - z) < MIN_PEER_GAP + 0.5);
        if crowded {
            continue;
        }
        if let Some(feet_y) = skylit_feet_y(ctx, x.floor() as i32, z.floor() as i32, y_hint) {
            out.push(Waypoint { x, z, feet_y });
        }
    }
    out
}

fn nearest_peer(ctx: &Ctx) -> Option<usize> {
    let own = ctx.position()?;
    (0..ctx.player_count())
        .filter(|&j| j != ctx.index())
        .filter_map(|j| {
            ctx.peer_position(j)
                .map(|p| (j, (p.0 - own.0).hypot(p.2 - own.2)))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(j, _)| j)
}

async fn sweep(ctx: &Ctx, span_deg: f64, pitch_deg: f64) {
    let rate = ctx.cfg().yaw_rate_deg_s.max(1.0);
    for (yaw, pitch) in [
        (span_deg * 0.5, pitch_deg),
        (-span_deg, -pitch_deg),
        (span_deg * 0.5, 0.0),
    ] {
        if ctx.target_met() {
            return;
        }
        let settle = (yaw.abs() / rate * 1000.0) as u64 + 700;
        ctx.drag_view(yaw, pitch, settle).await;
    }
}

/// Bound toward `(x, z)` in probe-truncated `hold_visual` runs with jump pulses — the uphill
/// sprint / hop-the-gully gait. Gives up on the blind gait (returns the remaining distance) as
/// soon as the probe refuses a segment; the caller finishes with the planner.
async fn bound_toward(ctx: &mut Ctx, x: f64, z: f64, sprint: bool) -> f64 {
    for _ in 0..3 {
        if ctx.target_met() {
            break;
        }
        let Some(own) = ctx.position() else { break };
        let dist = (x - own.0).hypot(z - own.2);
        if dist <= 3.0 {
            return dist;
        }
        let (ux, uz) = ((x - own.0) / dist, (z - own.2) / dist);
        let heading = (-ux).atan2(uz);
        let safe = safe_run_distance(ctx, heading, dist.min(10.0));
        if safe < PROBE {
            break;
        }
        let speed = if sprint { SPRINT_BPS } else { WALK_BPS };
        let hold = ((safe / speed * 1000.0) as u64).clamp(600, 5_000);
        ctx.look(
            (own.0 + ux * 32.0, own.1 + 1.62, own.2 + uz * 32.0),
            ctx.cfg().look_settle_ms.max(500),
        )
        .await;
        ctx.hold_visual(WalkDirection::Forward, sprint, hold, 950)
            .await;
    }
    ctx.position()
        .map(|p| (x - p.0).hypot(z - p.2))
        .unwrap_or(f64::INFINITY)
}

/// Walk a circuit over natural relief: every leg is drawn by sampling the terrain's open-sky
/// surface heights inside a private sector and walking to the waypoint with the most vertical
/// relief — uphill sprints, downhill bends, bounding runs with jump pulses over the gullies, all
/// of it on daylight surface routes (the waypoint sampler resolves the TOPMOST support in each
/// column, so a cave floor can never become a goal).
///
/// The camera owes the path nothing: legs alternate between plain travel, travel with the view
/// pinned on the crest of the sampled ridge, and blind bounding runs, and every waypoint ends in
/// an independently drawn garnish (a valley sweep pitched by how far the leg climbed, a glance at
/// the nearest peer, a held look back along the route).
pub(crate) async fn run(ctx: &mut Ctx) {
    let index = ctx.index();
    let n = ctx.player_count().max(1);
    let anchor = ctx.anchor();
    let sprint_prob = ctx.cfg().sprint_bout_prob;
    let move_timeout = ctx.cfg().move_timeout_ms.min(14_000);
    // A private sector per bot keeps circuits apart without any coordination; a lone bot gets the
    // full circle.
    let sector_center = index as f64 / n as f64 * TAU + rand_range(ctx, -0.2, 0.2);
    let sector_half = (std::f64::consts::PI / n as f64).max(0.55);

    ctx.record(
        TYPE_LOOK,
        &format!("vis-ridge-init-{index}"),
        "Ridge-valley circuit: sample the terrain's relief and walk its ups and downs.",
        json!({
            "sector_center_rad": r2(sector_center),
            "sector_half_rad": r2(sector_half),
            "radius_band": [RADIUS_MIN, RADIUS_MAX],
        }),
    );

    let mut leg = 0u32;
    while !ctx.target_met() {
        let candidates = draw_candidates(ctx, sector_center, sector_half);
        let Some(own) = ctx.position() else {
            sleep_ms(250).await;
            continue;
        };
        let own_y = own.1.floor() as i32;
        // The waypoint with the most relief wins, under a little noise so the circuit never locks
        // onto one hill. The crest (highest candidate) doubles as a camera focus for watched legs.
        let mut goal: Option<(f64, Waypoint)> = None;
        for w in &candidates {
            let score = (w.feet_y - own_y).abs() as f64 + ctx.rng_next() * 2.5;
            if goal.as_ref().map(|g| score > g.0).unwrap_or(true) {
                goal = Some((score, *w));
            }
        }
        let goal = goal.map(|(_, w)| w);
        let crest = candidates
            .iter()
            .max_by_key(|w| w.feet_y)
            .map(|w| (w.x, w.feet_y as f64 + 1.0, w.z));
        let Some(goal) = goal else {
            // Every draw landed on water, canopy or peers: relocate toward the anchor and redraw.
            let home = (
                anchor.0 + rand_range(ctx, -4.0, 4.0),
                anchor.1 + rand_range(ctx, -4.0, 4.0),
            );
            ctx.move_to(home.0, home.1, move_timeout, 3.0).await;
            leg += 1;
            continue;
        };

        let climb = goal.feet_y - own_y;
        let uphill = climb > 1;
        // Uphill legs sprint more often — the up-slope rush is this scenario's signature gait.
        let sprint = ctx.rng_next() < if uphill { 0.75 } else { sprint_prob };
        let mode = ctx.rng_next();
        let gait;
        if mode < 0.45 {
            gait = "travel";
            travel_to(ctx, (goal.x, goal.z), TRAVEL_HOP, 2.5).await;
        } else if mode < 0.80 {
            gait = "crest_watch";
            let focus = crest.unwrap_or((goal.x, goal.feet_y as f64 + 1.0, goal.z));
            ctx.move_to_watching_point(goal.x, goal.z, focus, move_timeout, 2.5)
                .await;
        } else {
            gait = "bounding";
            let remaining = bound_toward(ctx, goal.x, goal.z, sprint).await;
            if remaining > 3.0 {
                travel_to(ctx, (goal.x, goal.z), TRAVEL_HOP, 2.5).await;
            }
        }
        let arrived_y = ctx.position().map(|p| p.1.floor() as i32).unwrap_or(own_y);
        ctx.record(
            TYPE_MOVE,
            &format!("vis-ridge-leg-{index}-{leg}"),
            &format!(
                "Traverse leg {leg}: {} the relief to a waypoint {} blocks {}.",
                gait,
                climb.abs(),
                if climb >= 0 { "up" } else { "down" }
            ),
            json!({
                "leg": leg, "gait": gait, "sprint": sprint,
                "goal": {"x": r2(goal.x), "z": r2(goal.z), "feet_y": goal.feet_y},
                "planned_climb": climb, "arrived_climb": arrived_y - own_y,
                "candidates": candidates.len(),
            }),
        );
        if ctx.target_met() {
            break;
        }

        // Garnish, drawn independently of the leg that preceded it.
        match (ctx.rng_next() * 4.0) as u32 {
            0 => {
                // Sweep across the valley; the pitch leans into the relief just walked (look back
                // down after a climb, up after a descent).
                let span = rand_range(ctx, 70.0, 170.0);
                let pitch = if climb > 2 {
                    rand_range(ctx, 8.0, 20.0)
                } else if climb < -2 {
                    rand_range(ctx, -20.0, -8.0)
                } else {
                    rand_range(ctx, -14.0, 14.0)
                };
                sweep(ctx, span, pitch).await;
                ctx.record(
                    TYPE_LOOK,
                    &format!("vis-ridge-sweep-{index}-{leg}"),
                    "Sweep the camera across the valley from the waypoint.",
                    json!({"leg": leg, "sweep_deg": r2(span), "pitch_deg": r2(pitch)}),
                );
            }
            1 => {
                if let Some(peer) = nearest_peer(ctx) {
                    let settle = rand_range(ctx, 800.0, 2_000.0) as u64;
                    look_at_peer(ctx, peer, settle).await;
                    ctx.record(
                        TYPE_LOOK,
                        &format!("vis-ridge-peer-{index}-{leg}"),
                        &format!("Pick out bot {peer} across the terrain."),
                        json!({"leg": leg, "peer": peer, "settle_ms": settle}),
                    );
                }
            }
            2 => {
                // Hold a look back along the route — the reverse-angle beat.
                let back = (
                    own.0 + rand_range(ctx, -2.0, 2.0),
                    own.1 + 1.0,
                    own.2 + rand_range(ctx, -2.0, 2.0),
                );
                ctx.look(back, ctx.cfg().look_settle_ms.max(700)).await;
                sleep_ms(rand_range(ctx, 500.0, 1_200.0) as u64).await;
                ctx.record(
                    TYPE_LOOK,
                    &format!("vis-ridge-back-{index}-{leg}"),
                    "Look back along the leg just walked.",
                    json!({"leg": leg}),
                );
            }
            _ => {} // Straight into the next leg.
        }
        leg += 1;
    }
}
