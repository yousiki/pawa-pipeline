use super::*;

/// The two pad rings. Near pads sit inside the arena, far pads out on a wide ring, so the two
/// halves of the swarm cross different amounts of ground to reach one cell each.
const PADS: usize = 2;

/// One material per pad ring, so a single block is still attributable to the bot that set it.
const PAD_MATERIAL: [&str; PADS] = ["minecraft:oak_planks", "minecraft:stone_bricks"];

/// Draw in `[lo, hi)` from the per-bot stream, so a number this scenario could have hard-coded
/// varies per bot and per pass and still replays from the seed.
fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

/// Triangle camera sweep of `deg` total: out to one edge, across to the other, back to centre.
async fn sweep(ctx: &Ctx, deg: f64) {
    let settle = ((deg.abs() / ctx.cfg().yaw_rate_deg_s.max(1.0) * 1000.0) as u64 + 600).min(4_000);
    for leg in [deg * 0.5, -deg, deg * 0.5] {
        ctx.drag_view(leg, 0.0, settle).await;
    }
}

/// Face a world bearing (Minecraft yaw: 0° is +Z, 90° is -X) and hold forward long enough to cover
/// `blocks`. Open-loop on purpose: leaving is walked, not pathfound, so the recorded key bits stay
/// independent of the navigator.
async fn walk_bearing(ctx: &Ctx, yaw_deg: f64, blocks: f64, sprint: bool) {
    let Some(p) = ctx.position() else { return };
    let yaw = yaw_deg.to_radians();
    let (fx, fz) = (-yaw.sin(), yaw.cos());
    ctx.look(
        (p.0 + fx * 32.0, p.1 + 1.62, p.2 + fz * 32.0),
        ctx.cfg().look_settle_ms.max(500),
    )
    .await;
    let speed = if sprint { 5.6 } else { 4.3 };
    let hold = (((blocks / speed) * 1000.0) as u64).clamp(500, 12_000);
    ctx.hold_visual(WalkDirection::Forward, sprint, hold, 0)
        .await;
}

/// Each bot crosses open ground to one empty supported cell, sets a single block there with a real
/// use action, waits, and leaves in a different direction than it came.
pub(crate) async fn run(ctx: &mut Ctx) {
    let index = ctx.index();
    let bot_count = ctx.player_count().max(1);
    let anchor = ctx.anchor();
    let pad = index % PADS;
    let material = PAD_MATERIAL[pad];
    // Bots keep their own angular sector of the arena for the whole episode, so however the pads
    // are drawn two bots never walk onto the same cell.
    let sector = (index as f64 + 0.5) / bot_count as f64 * std::f64::consts::TAU;
    let mut pass = 0usize;
    loop {
        let equipped = ctx.equip(material).await;
        // Near pads land 5–9 blocks out, far pads 9–14, both inside this bot's own sector.
        let radius = if pad == 0 {
            vary(ctx, 5.0, 9.0)
        } else {
            vary(ctx, 9.0, 14.0)
        };
        let wobble = vary(ctx, -0.5, 0.5);
        let bearing = sector + wobble + pass as f64 * 0.9;
        let pad_x = anchor.0 + bearing.cos() * radius;
        let pad_z = anchor.1 + bearing.sin() * radius;

        let approached_from = ctx.position().map(|p| (p.0, p.2));
        let timeout = ctx.cfg().move_timeout_ms.min(20_000);
        let arrived = ctx.move_to(pad_x, pad_z, timeout, 2.0).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-pad-cross-{index}-p{pass}"),
            "Cross open ground to a single empty cell.",
            json!({"pass": pass, "pad": pad, "equipped": equipped, "arrived": arrived,
                "pad_target": {"x": r2(pad_x), "z": r2(pad_z)}, "radius": r2(radius)}),
        );

        // Read the ground before setting anything: the block goes on a checked support, and a pad
        // that has none is reported rather than faked.
        let span = vary(ctx, 70.0, 140.0);
        sweep(ctx, span).await;
        let feet_y = ctx.position().map(|p| p.1.floor() as i64).unwrap_or(64);
        let cell = marker_base_near(ctx, (pad_x.floor() as i64, feet_y, pad_z.floor() as i64), 1);
        let blocks: Vec<((i64, i64, i64), String)> = cell
            .map(|c| vec![((c.x as i64, c.y as i64, c.z as i64), material.to_string())])
            .unwrap_or_default();
        let (placed, attempted) = ctx
            .build_blocks(&blocks, |_i, _target, _material| {
                (
                    format!("vis-pad-place-{index}-p{pass}"),
                    "Set a single block on the checked pad cell.".to_string(),
                    json!({"pass": pass, "pad": pad}),
                )
            })
            .await;
        ctx.record(
            TYPE_PLACE,
            &format!("vis-pad-set-{index}-p{pass}"),
            "Set one block, alone, on open ground.",
            json!({"pass": pass, "pad": pad, "material": material, "placed": placed,
                "attempted": attempted, "support_found": cell.is_some(),
                "cell": cell.map(|c| json!({"x": c.x, "y": c.y, "z": c.z}))}),
        );

        // A short, capped beat on the finished cell — long enough to read it, nowhere near dead air.
        let beat = vary(ctx, 1_400.0, 2_600.0) as u64;
        ctx.still(beat).await;

        // Leave on a bearing well away from the one the bot arrived on, so departure never
        // retraces the approach.
        let arrival_deg = approached_from
            .and_then(|from| {
                ctx.position().map(|now| {
                    let (dx, dz) = (now.0 - from.0, now.2 - from.1);
                    (-dx).atan2(dz).to_degrees()
                })
            })
            .unwrap_or(0.0);
        let turn = vary(ctx, 110.0, 250.0);
        let leave = arrival_deg + turn;
        let distance = vary(ctx, 10.0, 15.0);
        let sprint = ctx.rng_next() < ctx.cfg().sprint_bout_prob;
        walk_bearing(ctx, leave, distance, sprint).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-pad-leave-{index}-p{pass}"),
            "Leave the pad on a different bearing than the approach.",
            json!({"pass": pass, "pad": pad, "arrival_deg": r2(arrival_deg),
                "leave_deg": r2(leave), "turn_deg": r2(turn), "distance": r2(distance),
                "sprint": sprint}),
        );
        if bot_count > 1 {
            witness_peer_edits(ctx).await;
            glance_at_nearest_peer(ctx, None).await;
        }
        if ctx.target_met() {
            break;
        }
        pass += 1;
    }
}
