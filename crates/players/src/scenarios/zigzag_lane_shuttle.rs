use super::*;

const LANE_NAMES: [&str; 2] = ["left_lane", "right_lane"];

/// How far a lane may wander from the spawn anchor before its base bearing is turned back. The
/// zigzag is open-loop (no pathfinder goal), so nothing else bounds where it ends up.
const ARENA_LEASH: f64 = 38.0;

/// Below this gap the next leg is steered away from the nearest peer instead of being drawn freely.
const MIN_PEER_GAP: f64 = 4.0;

const WALK_BPS: f64 = 4.317;
const SPRINT_BPS: f64 = 5.612;

fn rand_range(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

fn nearest_peer(ctx: &Ctx) -> Option<(usize, f64, f64)> {
    let own = ctx.position()?;
    (0..ctx.player_count())
        .filter(|&j| j != ctx.index())
        .filter_map(|j| {
            ctx.peer_position(j).map(|p| {
                let bearing = (-(p.0 - own.0)).atan2(p.2 - own.2).to_degrees();
                (j, (p.0 - own.0).hypot(p.2 - own.2), bearing)
            })
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
}

/// World bearing from the bot to `(x, z)`, read as a Minecraft yaw so a heading and the recorded
/// `look_yaw` are the same number: 0° faces +Z, 90° faces -X.
fn bearing_to(from: (f64, f64, f64), to: (f64, f64)) -> f64 {
    (-(to.0 - from.0)).atan2(to.1 - from.2).to_degrees()
}

/// Walk a world bearing open-loop, optionally pulsing jump so the leg can clear broken ground.
/// Facing comes first because `hold_visual` freezes the camera where it finds it. Returns the
/// requested hold and whether the hold ran to completion — `false` means `hold_visual`'s stall
/// guard cut it because the body stopped making progress (i.e. the leg walked into terrain).
async fn walk_leg(
    ctx: &Ctx,
    heading_deg: f64,
    distance: f64,
    sprint: bool,
    jump_period_ms: u64,
) -> (u64, bool) {
    let speed = if sprint { SPRINT_BPS } else { WALK_BPS };
    let hold = ((distance / speed * 1000.0) as u64).clamp(600, 10_000);
    if let Some(p) = ctx.position() {
        let yaw = heading_deg.to_radians();
        let settle = ctx.cfg().look_settle_ms.max(500);
        ctx.look(
            (p.0 - yaw.sin() * 32.0, p.1 + 1.62, p.2 + yaw.cos() * 32.0),
            settle,
        )
        .await;
    }
    let completed = ctx
        .hold_visual(WalkDirection::Forward, sprint, hold, jump_period_ms)
        .await;
    (hold, completed)
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

/// Two lanes cut a tight zigzag across broken ground on opposing diagonals, hopping the gaps and
/// stopping every few legs to look back down the lane they just crossed.
///
/// Each leg is an open-loop bearing hold, which is what makes the gaps get jumped rather than
/// pathfound around: the zig sign alternates, its amplitude, the leg length, the sprint bit and the
/// jump cadence are four independent per-leg draws, and the base bearing turns back toward the
/// anchor whenever the lane has wandered past its leash. The look-back beat lands on a random
/// two-to-four-leg period rather than a fixed one.
pub(crate) async fn run(ctx: &mut Ctx) {
    let index = ctx.index();
    let anchor = ctx.anchor();
    let lane = index % 2;
    let sprint_prob = ctx.cfg().sprint_bout_prob;

    // Opposing diagonals, each bot nudged off its lane's centre so two bots in one lane cut
    // different lines across the same ground.
    let mut base = if lane == 0 { 135.0 } else { 315.0 } + rand_range(ctx, -20.0, 20.0);
    let mut zig = if ctx.rng_next() < 0.5 { 1.0 } else { -1.0 };
    let mut until_lookback = 2 + (ctx.rng_next() * 3.0) as u32;

    ctx.record(
        TYPE_LOOK,
        &format!("vis-zigzag-{index}"),
        "Zigzag lane shuttle: cut a tight zigzag on this lane's diagonal, hopping the gaps.",
        json!({"role": LANE_NAMES[lane], "base_bearing_deg": r2(base), "zig": zig}),
    );

    let mut leg = 0u32;
    let mut stalls = 0u32;
    while !ctx.target_met() {
        // Turn the lane back before an open-loop leg walks it out of the arena.
        let leashed = ctx
            .position()
            .map(|p| (p.0 - anchor.0).hypot(p.2 - anchor.1) > ARENA_LEASH)
            .unwrap_or(false);
        if leashed && let Some(p) = ctx.position() {
            let home = bearing_to(p, anchor);
            base = home + rand_range(ctx, -22.0, 22.0);
        }

        // A peer inside the gap wins over the drawn heading: this scenario's lanes cross, and two
        // bots occupying one cell is the head-box occlusion the corpus audit measured.
        let close = nearest_peer(ctx).filter(|(_, gap, _)| *gap < MIN_PEER_GAP);
        let heading = match close {
            Some((_, _, toward)) => toward + 180.0 + rand_range(ctx, -35.0, 35.0),
            None => base + zig * rand_range(ctx, 35.0, 65.0),
        };
        let distance = rand_range(ctx, 9.0, 16.0);
        let sprint = ctx.rng_next() < sprint_prob;
        // Jump is its own bit: some legs hop the broken ground, some walk into it and stop.
        let jump_period = if ctx.rng_next() < 0.55 {
            rand_range(ctx, 900.0, 2_200.0) as u64
        } else {
            0
        };
        let before = ctx.position();
        let (hold, completed) = walk_leg(ctx, heading, distance, sprint, jump_period).await;
        let travelled = match (before, ctx.position()) {
            (Some(a), Some(b)) => Some(r2((b.0 - a.0).hypot(b.2 - a.2))),
            _ => None,
        };
        ctx.record(
            TYPE_MOVE,
            &format!("vis-zigzag-leg-{index}-{leg}"),
            &format!("Cut zigzag leg {leg} across the broken ground."),
            json!({
                "role": LANE_NAMES[lane], "leg": leg,
                "heading_deg": r2(heading), "requested_distance": r2(distance),
                "travelled": travelled, "sprint": sprint, "jump_period_ms": jump_period,
                "hold_ms": hold, "stalled": !completed, "leashed_back": leashed,
                "peer_avoidance": close.map(|(peer, _, _)| peer),
            }),
        );
        if completed {
            stalls = 0;
        } else {
            stalls += 1;
            // The stall guard cut the hold: this lane is walking into terrain. Turning the head
            // is not enough — the next leg redraws from `base`, whose ±65° cone points at the
            // same wall (the leash never fires this close to the anchor, so nothing else ever
            // rewrites `base`, and the 08-19 audit shows 12–13 consecutive zero-travel legs
            // against one block face). The wall event itself turns the lane around.
            base = heading + 180.0 + rand_range(ctx, -40.0, 40.0);
            if stalls >= 2 {
                stalls = 0;
                // Still pinned after a turnaround: a two-block trench under an overhang, where
                // jumping cannot clear and the fresh cone may still scrape the walls. Walk the
                // reversed base open-loop with a fast jump pulse to climb back out the way the
                // lane came in.
                let escape = base + rand_range(ctx, -25.0, 25.0);
                walk_leg(ctx, escape, 8.0, false, 500).await;
            }
        }
        zig = -zig;
        leg += 1;
        if ctx.target_met() {
            break;
        }

        until_lookback = until_lookback.saturating_sub(1);
        if until_lookback > 0 {
            continue;
        }
        until_lookback = 2 + (ctx.rng_next() * 3.0) as u32;
        // Stop and read the lane just crossed: either a wide sweep back down it, or the peer whose
        // diagonal crosses ours.
        if ctx.rng_next() < 0.5 {
            let span = rand_range(ctx, 90.0, 175.0);
            let pitch = rand_range(ctx, -18.0, 12.0);
            sweep(ctx, span, pitch).await;
            ctx.record(
                TYPE_LOOK,
                &format!("vis-zigzag-lookback-{index}-{leg}"),
                "Stop and sweep back down the lane just crossed.",
                json!({"leg": leg, "sweep_deg": r2(span), "pitch_deg": r2(pitch)}),
            );
        } else {
            let settle = rand_range(ctx, 1_200.0, 3_500.0) as u64;
            if let Some((peer, gap, _)) = nearest_peer(ctx) {
                look_at_peer(ctx, peer, settle).await;
                ctx.record(
                    TYPE_LOOK,
                    &format!("vis-zigzag-peer-{index}-{leg}"),
                    &format!("Watch bot {peer} cross the other diagonal."),
                    json!({"leg": leg, "peer": peer, "gap": r2(gap), "settle_ms": settle}),
                );
            }
        }
    }
}
