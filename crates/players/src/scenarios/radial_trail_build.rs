use super::*;

/// Block materials a trail marker may draw from — kit blocks only.
const MATERIALS: [&str; 4] = [
    "minecraft:cobblestone",
    "minecraft:stone_bricks",
    "minecraft:bricks",
    "minecraft:oak_planks",
];

/// How far past the inner ring a lane reaches. `ring_radius` floors the inner ring at 8, so at the
/// swarm sizes we record the whole lane stays on the levelled arena.
const LANE_LENGTH: f64 = 10.0;

/// Maximum sideways wander off the spoke at the OUTER end of the lane. The wander grows linearly
/// from zero at the inner ring (where adjacent lanes are closest) to this at the outer end, so the
/// strip of markers widens like a trumpet without two neighbouring lanes ever closing on the
/// 4.5-block peer gap.
const LANE_FLARE: f64 = 2.5;

/// Draw in `[lo, hi)` from the per-bot stream, so a number this scenario could have hard-coded
/// varies per bot and per pass and still replays from the seed.
fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

/// Open-loop walk toward `target`: one long key hold sized by distance, then at most one
/// pathfinder correction if the hold drifted or stalled short. Round 1 travelled hop-wise
/// (`travel_to`) and every POV failed key chatter at ~50% short presses — a lane scenario is
/// nothing but walking, so the walking must be long held keys, not 4-block pathfinder taps.
/// `hold_visual` cuts itself short when the body stops translating, so obstacles are bounded.
async fn walk_toward(ctx: &mut Ctx, target: (f64, f64), sprint: bool) {
    if let Some(p) = ctx.position() {
        let dist = (target.0 - p.0).hypot(target.1 - p.2);
        if dist > 1.5 {
            ctx.look(
                (target.0, p.1 + 1.62, target.1),
                ctx.cfg().look_settle_ms.max(400),
            )
            .await;
            let speed = if sprint { 5.6 } else { 4.3 };
            let hold = ((dist / speed * 1000.0) as u64).clamp(500, 6_000);
            ctx.hold_visual(WalkDirection::Forward, sprint, hold, 0)
                .await;
        }
    }
    let close = ctx
        .position()
        .map(|q| (target.0 - q.0).hypot(target.1 - q.2) <= 2.5)
        .unwrap_or(false);
    if !close {
        let timeout = ctx.cfg().move_timeout_ms.min(8_000);
        ctx.move_to(target.0, target.1, timeout, 1.8).await;
    }
}

/// Every bot works its own radial lane at the same time: shuttle outward along a private spoke,
/// dropping small trail markers (a block, a two-stack, sometimes a torch on top) at irregular
/// spacing and sideways wander, then walk the lane back in while reviewing its own trail — and at
/// every beat, randomly turn to witness a neighbour's fresh edits. Nobody waits on anybody.
pub(super) async fn run(ctx: &mut Ctx) {
    let index = ctx.index();
    let bot_count = ctx.player_count().max(1);
    let anchor = ctx.anchor();
    let bearing = (index as f64 / bot_count as f64) * std::f64::consts::TAU;
    let (ux, uz) = (bearing.cos(), bearing.sin());
    let (tx, tz) = (-uz, ux);
    let inner = ring_radius(bot_count);
    let outer = inner + LANE_LENGTH;
    let mut trail: Vec<BlockPos> = Vec::new();
    let mut leg = 0usize;
    loop {
        // --- Outbound: walk the spoke, dropping markers at irregular spacing. ---
        let sprint = ctx.rng_next() < ctx.cfg().sprint_bout_prob;
        walk_toward(ctx, (anchor.0 + ux * inner, anchor.1 + uz * inner), sprint).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-lane-leg-{index}-l{leg}"),
            &format!("Bot {index} starts lane leg {leg} on its own spoke."),
            json!({"leg": leg, "bearing_deg": r2(bearing.to_degrees()),
                "inner": r2(inner), "outer": r2(outer)}),
        );
        let mut r = inner;
        while r < outer && !ctx.target_met() {
            // Cap at the lane end so the final marker never overshoots the levelled arena.
            r = (r + vary(ctx, 3.5, 5.5)).min(outer);
            let flare = LANE_FLARE * ((r - inner) / LANE_LENGTH).clamp(0.0, 1.0);
            let lateral = vary(ctx, -1.0, 1.0) * flare;
            let wx = anchor.0 + ux * r + tx * lateral;
            let wz = anchor.1 + uz * r + tz * lateral;
            // Decoupled-camera legs: round 2 failed with 97% of moving-camera turns restating
            // the travel direction (cap 95%). Held-key walks have a STATIC camera, so they add
            // nothing to either side of that ratio — the decoupling has to come from legs walked
            // with the camera pinned on an off-path focus: a peer, or an own earlier marker. Kept
            // to about a third of legs so the pathfinder's short key taps stay under the 35%
            // chatter cap the held walks bought back.
            let timeout = ctx.cfg().move_timeout_ms.min(8_000);
            let gait = ctx.rng_next();
            if gait < 0.15 && bot_count > 1 {
                let mut peer = (ctx.rng_next() * bot_count as f64) as usize % bot_count;
                if peer == index {
                    peer = (peer + 1) % bot_count;
                }
                ctx.move_to_watching_peer(wx, wz, peer, timeout, 1.8).await;
            } else if gait < 0.4 && !trail.is_empty() {
                let m = trail[(ctx.rng_next() * trail.len() as f64) as usize % trail.len()];
                let focus = (m.x as f64 + 0.5, m.y as f64 + 1.0, m.z as f64 + 0.5);
                ctx.move_to_watching_point(wx, wz, focus, timeout, 1.8)
                    .await;
            } else {
                let sprint = ctx.rng_next() < ctx.cfg().sprint_bout_prob;
                walk_toward(ctx, (wx, wz), sprint).await;
            }
            // Marker sits a couple of blocks further out along the spoke, clear of the hitbox.
            let feet_y = ctx.position().map(|p| p.1.floor() as i64).unwrap_or(64);
            let desired = (
                (wx + ux * 2.0).floor() as i64,
                feet_y,
                (wz + uz * 2.0).floor() as i64,
            );
            let height = if ctx.rng_next() < 0.4 { 2 } else { 1 };
            if let Some(base) = marker_base_near(ctx, desired, height) {
                let material =
                    MATERIALS[(ctx.rng_next() * MATERIALS.len() as f64) as usize % MATERIALS.len()];
                let blocks: Vec<((i64, i64, i64), String)> = (0..height)
                    .map(|dy| {
                        (
                            (base.x as i64, base.y as i64 + dy, base.z as i64),
                            material.to_string(),
                        )
                    })
                    .collect();
                let (placed, attempted) = ctx
                    .build_blocks(&blocks, |b, _t, _m| {
                        (
                            format!("vis-lane-place-{index}-l{leg}-r{}-{b}", r as i64),
                            format!("Drop trail marker layer {}.", b + 1),
                            json!({"leg": leg, "layer": b}),
                        )
                    })
                    .await;
                let mut torch = false;
                if placed > 0 && ctx.rng_next() < 0.35 && ctx.equip("torch").await {
                    torch = ctx.place_ground_torch(base.up(height as i32)).await;
                }
                if placed > 0 {
                    trail.push(base);
                }
                ctx.record(
                    TYPE_PLACE,
                    &format!("vis-lane-marker-{index}-l{leg}-r{}", r as i64),
                    &format!(
                        "Trail marker at radius {:.1}: {placed}/{attempted} blocks.",
                        r
                    ),
                    json!({"leg": leg, "radius": r2(r), "lateral": r2(lateral),
                        "height": height, "placed": placed, "attempted": attempted,
                        "torch": torch,
                        "cell": {"x": base.x, "y": base.y, "z": base.z}}),
                );
            } else {
                shake_loose(ctx).await;
            }
            witness_peer_edits(ctx).await;
            if ctx.rng_next() < 0.25 {
                glance_at_nearest_peer(ctx, None).await;
            }
        }
        if ctx.target_met() {
            break;
        }

        // --- Turn: read the whole trail back down the spoke before walking it. ---
        let feet = ctx.position().map(|p| p.1).unwrap_or(64.0);
        ctx.look_default((anchor.0 + ux * inner, feet + 1.0, anchor.1 + uz * inner))
            .await;
        sleep_ms(vary(ctx, 600.0, 1_400.0) as u64).await;

        // --- Inbound: walk back via a midpoint, reviewing one of this bot's own markers on the
        // way — the persistence beat: the trail is still there on the return pass. ---
        let mid_r = inner + LANE_LENGTH * vary(ctx, 0.35, 0.65);
        walk_toward(ctx, (anchor.0 + ux * mid_r, anchor.1 + uz * mid_r), false).await;
        if !trail.is_empty() {
            let k = (ctx.rng_next() * trail.len() as f64) as usize % trail.len();
            let m = trail[k];
            let focus = (m.x as f64 + 0.5, m.y as f64 + 0.5, m.z as f64 + 0.5);
            ctx.look_default(focus).await;
            sleep_ms(vary(ctx, 700.0, 1_500.0) as u64).await;
            ctx.record(
                TYPE_LOOK,
                &format!("vis-lane-review-{index}-l{leg}"),
                "Review an own trail marker on the return pass.",
                json!({"leg": leg, "marker": {"x": m.x, "y": m.y, "z": m.z},
                    "trail_len": trail.len()}),
            );
            // Finish the return with the marker held in frame while the body walks away from
            // it — a decoupled-camera leg AND the persistence beat in one motion.
            ctx.move_to_watching_point(
                anchor.0 + ux * inner,
                anchor.1 + uz * inner,
                focus,
                ctx.cfg().move_timeout_ms.min(8_000),
                2.0,
            )
            .await;
        }
        witness_peer_edits(ctx).await;
        if ctx.target_met() {
            break;
        }
        leg += 1;
    }
}
