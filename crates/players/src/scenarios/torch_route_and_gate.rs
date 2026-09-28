use super::*;

/// The three trades on site: one lights the route, one raises the gateway, one paves the lane
/// between them.
const TRADES: usize = 3;
const TORCH_BEARER: usize = 0;
const GATE_POST: usize = 1;

/// Draw in `[lo, hi)` from the per-bot stream, so a number this scenario could have hard-coded
/// varies per bot and per pass and still replays from the seed.
fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

/// The trade a bot works this pass. Below a full crew the trades rotate every pass, so a
/// short-handed swarm still lights the route AND raises the gateway instead of dropping two thirds
/// of the scene.
fn trade_of(index: usize, bot_count: usize, pass: usize) -> usize {
    if bot_count >= TRADES {
        index % TRADES
    } else {
        (index + pass) % TRADES
    }
}

/// Nearest peer working `trade` this pass.
fn nearest_in_trade(ctx: &Ctx, trade: usize, pass: usize) -> Option<usize> {
    let own = ctx.position()?;
    let bot_count = ctx.player_count().max(1);
    (0..bot_count)
        .filter(|&j| j != ctx.index() && trade_of(j, bot_count, pass) == trade)
        .filter_map(|j| {
            ctx.peer_position(j)
                .map(|p| (j, (p.0 - own.0).hypot(p.2 - own.2)))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(j, _)| j)
}

/// Triangle camera sweep of `deg` total: out to one edge, across to the other, back to centre.
async fn sweep(ctx: &Ctx, deg: f64) {
    let settle = ((deg.abs() / ctx.cfg().yaw_rate_deg_s.max(1.0) * 1000.0) as u64 + 600).min(4_000);
    for leg in [deg * 0.5, -deg, deg * 0.5] {
        ctx.drag_view(leg, 0.0, settle).await;
    }
}

/// Face a world bearing (Minecraft yaw: 0° is +Z, 90° is -X) and hold forward long enough to cover
/// `blocks`. Open-loop on purpose: no pathfinder goal, so the recorded key bits stay independent of
/// the navigator.
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

/// Walk a route of marker stops, dropping one real floor torch at each.
///
/// `place_ground_torch` hits the top face of the block below the target, which needs a tighter
/// reach than ordinary placement — so the bot closes on each marker before it tries, and a marker
/// it still cannot reach is reported instead of silently skipped.
async fn light_route(ctx: &mut Ctx, anchor: (f64, f64), lane: f64, pass: usize) {
    let index = ctx.index();
    let equipped = ctx.equip("torch").await;
    let mut lit = 0u64;
    let mut tried = 0u64;
    for stop in 0..3 {
        if ctx.target_met() {
            break;
        }
        // A shallow arc of stops west of the gateway: a route, not a circuit.
        let along = vary(ctx, 4.5, 6.5) * stop as f64;
        let drift = vary(ctx, -2.0, 2.0);
        let sx = anchor.0 - 6.0 + lane + along;
        let sz = anchor.1 - 5.0 + drift;
        let timeout = ctx.cfg().move_timeout_ms.min(15_000);
        // Watch the gate crew while walking the route: `move_to` alone turns the camera toward
        // travel on every leg, and the whole trade scored 95.3% camera/travel agreement. The gate
        // stands off to the side of the route, so tracking its crew decorrelates the two.
        match nearest_in_trade(ctx, GATE_POST, pass) {
            Some(peer) => {
                ctx.move_to_watching_peer(sx, sz, peer, timeout, 2.0).await;
            }
            None => {
                ctx.move_to(sx, sz, timeout, 2.0).await;
            }
        }
        let feet_y = ctx.position().map(|p| p.1.floor() as i64).unwrap_or(64);
        let Some(cell) = marker_base_near(ctx, (sx.floor() as i64, feet_y, sz.floor() as i64), 1)
        else {
            continue;
        };
        if !ctx.ground_torch_face_in_reach(cell) {
            let timeout = ctx.cfg().move_timeout_ms.min(8_000);
            ctx.move_to(cell.x as f64 + 0.5, cell.z as f64 + 0.5, timeout, 1.5)
                .await;
        }
        let in_reach = ctx.ground_torch_face_in_reach(cell);
        let floor = cell.down(1).center();
        ctx.look(
            (floor.x, floor.y, floor.z),
            ctx.cfg().look_settle_ms.max(500),
        )
        .await;
        tried += 1;
        let ok = ctx.place_ground_torch(cell).await;
        lit += u64::from(ok);
        ctx.record(
            TYPE_PLACE,
            &format!("vis-torch-{index}-p{pass}-s{stop}"),
            "Drop a floor torch on the walking route.",
            json!({"pass": pass, "stop": stop, "placed": ok, "equipped": equipped,
                "cell": {"x": cell.x, "y": cell.y, "z": cell.z}, "in_reach": in_reach}),
        );
    }
    ctx.record(
        TYPE_LOOK,
        &format!("vis-torch-route-{index}-p{pass}"),
        "Finish the lit route between the markers.",
        json!({"pass": pass, "trade": "torch_bearer", "lit": lit, "attempted": tried,
            "equipped": equipped}),
    );

    let bearing = vary(ctx, 55.0, 95.0);
    let distance = vary(ctx, 9.0, 13.0);
    let sprint = ctx.rng_next() < ctx.cfg().sprint_bout_prob;
    walk_bearing(ctx, bearing, distance, sprint).await;
    let span = vary(ctx, 130.0, 170.0);
    sweep(ctx, span).await;
}

/// Raise the two log posts of a gateway, three blocks apart, and step back to read it.
async fn raise_gateway(ctx: &mut Ctx, anchor: (f64, f64), lane: f64, pass: usize) {
    let index = ctx.index();
    let equipped = ctx.equip("minecraft:oak_log").await;
    let gx = anchor.0 + 5.0 + lane;
    let gz = anchor.1 + 7.0 + vary(ctx, -1.5, 1.5);
    let gap = vary(ctx, 3.0, 4.0).round();
    let height = vary(ctx, 2.0, 4.0).floor().max(2.0) as i64;
    let standoff = vary(ctx, 2.0, 3.0);
    let timeout = ctx.cfg().move_timeout_ms.min(16_000);
    let arrived = ctx
        .move_to(gx + gap * 0.5, gz - standoff, timeout, 2.0)
        .await;

    let feet_y = ctx.position().map(|p| p.1.floor() as i64).unwrap_or(64);
    let mut blocks: Vec<((i64, i64, i64), String)> = Vec::new();
    for post in [0.0, gap] {
        let Some(base) = marker_base_near(
            ctx,
            ((gx + post).floor() as i64, feet_y, gz.floor() as i64),
            height,
        ) else {
            continue;
        };
        for dy in 0..height {
            blocks.push((
                (base.x as i64, base.y as i64 + dy, base.z as i64),
                "minecraft:oak_log".to_string(),
            ));
        }
    }
    let offered = blocks.len();
    let (placed, attempted) = ctx
        .build_blocks(&blocks, |i, _target, _material| {
            (
                format!("vis-gate-place-{index}-p{pass}-{i}"),
                format!("Raise gateway log {}.", i + 1),
                json!({"pass": pass, "log": i}),
            )
        })
        .await;
    ctx.record(
        TYPE_PLACE,
        &format!("vis-gate-{index}-p{pass}"),
        "Raise the two log posts of the gateway.",
        json!({"pass": pass, "trade": "gate_post", "equipped": equipped, "arrived": arrived,
            "gap": r2(gap), "height": height, "placed": placed, "attempted": attempted,
            "cells_offered": offered, "gate": {"x": r2(gx), "z": r2(gz)}}),
    );

    let bearing = vary(ctx, 235.0, 275.0);
    let distance = vary(ctx, 9.0, 14.0);
    let sprint = ctx.rng_next() < ctx.cfg().sprint_bout_prob;
    walk_bearing(ctx, bearing, distance, sprint).await;
    match nearest_in_trade(ctx, TRADES - 1, pass) {
        Some(peer) => {
            let settle = ctx.cfg().look_settle_ms.max(800);
            look_at_peer(ctx, peer, settle).await;
            sleep_ms(vary(ctx, 1_000.0, 2_000.0) as u64).await;
        }
        None => {
            let span = vary(ctx, 110.0, 150.0);
            sweep(ctx, span).await;
        }
    }
}

/// Lay a plank lane across the ground between the gateway and the lit route.
async fn pave_lane(ctx: &mut Ctx, anchor: (f64, f64), lane: f64, pass: usize) {
    let index = ctx.index();
    let equipped = ctx.equip("minecraft:oak_planks").await;
    let px = anchor.0 + 8.0 + lane;
    let pz = anchor.1 - 6.0 + vary(ctx, -1.5, 1.5);
    let length = vary(ctx, 3.0, 5.0).floor().max(3.0) as i64;
    let standoff = vary(ctx, 1.5, 2.5);
    let timeout = ctx.cfg().move_timeout_ms.min(16_000);
    let arrived = ctx
        .move_to(px + length as f64 * 0.5, pz - standoff, timeout, 2.0)
        .await;

    let feet_y = ctx.position().map(|p| p.1.floor() as i64).unwrap_or(64);
    let mut blocks: Vec<((i64, i64, i64), String)> = Vec::new();
    for i in 0..length {
        let Some(base) =
            marker_base_near(ctx, (px.floor() as i64 + i, feet_y, pz.floor() as i64), 1)
        else {
            continue;
        };
        let cell = (base.x as i64, base.y as i64, base.z as i64);
        if blocks.iter().any(|(existing, _)| *existing == cell) {
            continue;
        }
        blocks.push((cell, "minecraft:oak_planks".to_string()));
    }
    let offered = blocks.len();
    let (placed, attempted) = ctx
        .build_blocks(&blocks, |i, _target, _material| {
            (
                format!("vis-pave-place-{index}-p{pass}-{i}"),
                format!("Lay lane plank {}.", i + 1),
                json!({"pass": pass, "plank": i}),
            )
        })
        .await;
    ctx.record(
        TYPE_PLACE,
        &format!("vis-pave-{index}-p{pass}"),
        "Lay the plank lane across the ground between the markers.",
        json!({"pass": pass, "trade": "paver", "equipped": equipped, "arrived": arrived,
            "length": length, "placed": placed, "attempted": attempted,
            "cells_offered": offered, "lane_start": {"x": r2(px), "z": r2(pz)}}),
    );

    let bearing = vary(ctx, 325.0, 365.0);
    let distance = vary(ctx, 10.0, 15.0);
    let sprint = ctx.rng_next() < ctx.cfg().sprint_bout_prob;
    walk_bearing(ctx, bearing, distance, sprint).await;
    match nearest_in_trade(ctx, TORCH_BEARER, pass) {
        Some(peer) => {
            let settle = ctx.cfg().look_settle_ms.max(800);
            look_at_peer(ctx, peer, settle).await;
            sleep_ms(vary(ctx, 1_000.0, 2_000.0) as u64).await;
        }
        None => {
            let span = vary(ctx, 90.0, 130.0);
            sweep(ctx, span).await;
        }
    }
}

/// A lamplighter drops torch markers along a walking route while two other crews raise a log
/// gateway and lay a plank lane across the ground between the markers.
pub(crate) async fn run(ctx: &mut Ctx) {
    let index = ctx.index();
    let bot_count = ctx.player_count().max(1);
    let anchor = ctx.anchor();
    // Every extra full crew works its own site sixteen blocks along, so two gateways are never
    // raised on the same cells and no two bots share a station.
    let lane = (index / TRADES) as f64 * 16.0;
    let mut pass = 0usize;
    loop {
        let trade = trade_of(index, bot_count, pass);
        ctx.record(
            TYPE_LOOK,
            &format!("vis-site-pass-{index}-p{pass}"),
            &format!("Pass {pass}: bot {index} works trade {trade} on the site."),
            json!({"pass": pass, "trade": trade, "lane": r2(lane), "bot_count": bot_count}),
        );
        match trade {
            TORCH_BEARER => light_route(ctx, anchor, lane, pass).await,
            GATE_POST => raise_gateway(ctx, anchor, lane, pass).await,
            _ => pave_lane(ctx, anchor, lane, pass).await,
        }
        witness_peer_edits(ctx).await;
        if ctx.target_met() {
            break;
        }
        pass += 1;
    }
}
