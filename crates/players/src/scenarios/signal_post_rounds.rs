use super::*;

const TAU: f64 = std::f64::consts::TAU;

/// Post shaft materials, one drawn per bot so neighbouring signal points read differently.
const POST_MATERIALS: [&str; 4] = [
    "minecraft:stone_bricks",
    "minecraft:oak_log",
    "minecraft:cobblestone",
    "minecraft:bricks",
];

/// Sideways wander allowed when siting or re-siting a post; `station_ring` absorbs it.
const SITE_JITTER: f64 = 2.0;

/// Draw in `[lo, hi)` from the per-bot stream, so a number this scenario could have hard-coded
/// varies per bot and per round and still replays from the seed.
fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

/// Shared, index-only formula for bot `j`'s signal station, so every bot can aim its patrol at a
/// peer's signal point without any cross-bot channel.
fn station_of(anchor: (f64, f64), j: usize, bot_count: usize) -> (f64, f64) {
    let n = bot_count.max(1);
    let ang = (j as f64 / n as f64) * TAU;
    let r = station_ring(n, SITE_JITTER);
    (anchor.0 + ang.cos() * r, anchor.1 + ang.sin() * r)
}

/// The cell the torch lives in: the first air-over-solid cell scanning the shaft from its top.
/// A partially built shaft (a placement bailed) still yields a usable, lower torch cell.
fn torch_cell_of(ctx: &Ctx, base: BlockPos, height: i64) -> Option<BlockPos> {
    (0..=height as i32).rev().find_map(|dy| {
        let cell = base.up(dy);
        (block_is_air(ctx, cell) && block_is_safe_support(ctx, cell.down(1))).then_some(cell)
    })
}

/// Make sure this bot's signal point exists: a 0–2 block shaft near the station with an air cell
/// on top for the torch. Returns the torch cell; the shaft is rebuilt only when its support has
/// vanished, so the point stays put across rounds and peers can keep finding it.
async fn ensure_post(
    ctx: &mut Ctx,
    station: (f64, f64),
    post: &mut Option<BlockPos>,
    material: &str,
    round: u32,
) -> Option<BlockPos> {
    if let Some(cell) = *post {
        if block_is_safe_support(ctx, cell.down(1)) {
            return Some(cell);
        }
        *post = None;
    }
    let height = (ctx.rng_next() * 3.0) as i64; // 0..=2: a bare ground point is a valid post
    let sx = station.0 + vary(ctx, -SITE_JITTER, SITE_JITTER);
    let sz = station.1 + vary(ctx, -SITE_JITTER, SITE_JITTER);
    let timeout = ctx.cfg().move_timeout_ms.min(12_000);
    ctx.move_to(sx, sz, timeout, 2.0).await;
    let feet_y = ctx.position().map(|p| p.1.floor() as i64).unwrap_or(64);
    let base = marker_base_near(
        ctx,
        (sx.floor() as i64, feet_y, sz.floor() as i64),
        height + 1,
    )?;
    if height > 0 {
        let index = ctx.index();
        let cells: Vec<((i64, i64, i64), String)> = (0..height)
            .map(|dy| {
                (
                    (base.x as i64, base.y as i64 + dy, base.z as i64),
                    material.to_string(),
                )
            })
            .collect();
        let (placed, attempted) = ctx
            .build_blocks(&cells, |i, _t, _m| {
                (
                    format!("vis-signal-shaft-{index}-r{round}-{i}"),
                    format!("Raise signal post block {}.", i + 1),
                    json!({"round": round, "edit": "post"}),
                )
            })
            .await;
        ctx.record(
            TYPE_PLACE,
            &format!("vis-signal-post-{index}-r{round}"),
            "Raise this bot's signal post.",
            json!({"round": round, "height": height, "material": material,
                "placed": placed, "attempted": attempted,
                "base": {"x": base.x, "y": base.y, "z": base.z}}),
        );
    }
    let cell = torch_cell_of(ctx, base, height)?;
    *post = Some(cell);
    Some(cell)
}

/// One blink action at the torch cell: light it when dark, pull it when lit. Both directions are
/// verified world edits and both broadcast, so peers can turn and witness the signal change.
async fn toggle_signal(ctx: &mut Ctx, cell: BlockPos, round: u32, beat: &str) -> bool {
    let lit = ctx.block_id_at(cell).as_deref() == Some("torch");
    if !ctx.ground_torch_face_in_reach(cell) {
        let timeout = ctx.cfg().move_timeout_ms.min(8_000);
        ctx.move_to(cell.x as f64 + 0.5, cell.z as f64 + 0.5, timeout, 1.6)
            .await;
    }
    let ok = if lit {
        ctx.equip("diamond_pickaxe").await;
        ctx.dig(cell).await
    } else {
        ctx.equip("torch").await;
        let floor = cell.down(1).center();
        ctx.look(
            (floor.x, floor.y, floor.z),
            ctx.cfg().look_settle_ms.max(500),
        )
        .await;
        ctx.place_ground_torch(cell).await
    };
    ctx.record(
        if lit { TYPE_MINE } else { TYPE_PLACE },
        &format!("vis-signal-toggle-{}-r{round}-{beat}", ctx.index()),
        if lit {
            "Pull the signal torch off the post."
        } else {
            "Light the signal torch on the post."
        },
        json!({"round": round, "beat": beat, "was_lit": lit, "ok": ok,
            "cell": {"x": cell.x, "y": cell.y, "z": cell.z}}),
    );
    ok
}

/// Walk to a random peer's signal point on a random approach bearing and read whether its torch is
/// currently lit. The camera rides the peer or their post during the leg, not the travel bearing.
async fn patrol_to_peer(ctx: &mut Ctx, anchor: (f64, f64), round: u32) {
    let index = ctx.index();
    let bot_count = ctx.player_count().max(1);
    if bot_count < 2 {
        return;
    }
    let mut peer = (ctx.rng_next() * (bot_count - 1) as f64) as usize % (bot_count - 1);
    if peer >= index {
        peer += 1;
    }
    let target = station_of(anchor, peer, bot_count);
    let bearing = ctx.rng_next() * TAU;
    let standoff = vary(ctx, 4.5, 7.5);
    let goal = (
        target.0 + bearing.cos() * standoff,
        target.1 + bearing.sin() * standoff,
    );
    let own_y = ctx.position().map(|p| p.1).unwrap_or(64.0);
    let timeout = ctx.cfg().move_timeout_ms.min(14_000);
    let style = ctx.rng_next();
    let style_name = if style < 0.4 && ctx.peer_position(peer).is_some() {
        ctx.move_to_watching_peer(goal.0, goal.1, peer, timeout, 2.5)
            .await;
        "watch_peer"
    } else if style < 0.7 {
        let focus = (target.0, own_y + vary(ctx, 0.5, 2.5), target.1);
        ctx.move_to_watching_point(goal.0, goal.1, focus, timeout, 2.5)
            .await;
        "watch_post"
    } else {
        travel_to(ctx, goal, TRAVEL_HOP, 2.5).await;
        "hops"
    };
    // Read the peer's signal: their post may sit up to SITE_JITTER off the shared station formula
    // and its torch may ride a 0–2 block shaft, so probe the same neighbourhood a placer may use.
    let feet_y = ctx.position().map(|p| p.1.floor() as i32).unwrap_or(64);
    let found = area_find_block(
        ctx,
        (target.0.floor() as i32, target.1.floor() as i32),
        (feet_y - 5, feet_y + 7),
        "torch",
    );
    let focus = match found {
        Some(cell) => {
            let c = cell.center();
            (c.x, c.y, c.z)
        }
        None => (target.0, own_y + 1.0, target.1),
    };
    ctx.look(focus, ctx.cfg().look_settle_ms.max(600)).await;
    let dwell = vary(ctx, 1_000.0, 2_200.0) as u64;
    sleep_ms(dwell).await;
    ctx.record(
        TYPE_LOOK,
        &format!("vis-signal-read-{index}-r{round}"),
        &format!("Read bot {peer}'s signal point on patrol."),
        json!({"round": round, "peer": peer, "lit": found.is_some(), "style": style_name,
            "standoff": r2(standoff), "dwell_ms": dwell}),
    );
}

/// Every bot keeps one small signal point on a shared ring — a 0–2 block shaft with a torch cell
/// on top — and works rounds of a signal rota: blink its own torch (place when dark, pull when
/// lit; sometimes a fast double blink, sometimes a skipped beat), step back to read its own point,
/// then patrol to a random peer's point to read theirs. Every toggle broadcasts, so the witness
/// beat catches peers' signal changes; the toggle action and the turning heads ARE the signal —
/// daylight torch light is not relied on.
pub(super) async fn run(ctx: &mut Ctx) {
    let index = ctx.index();
    let bot_count = ctx.player_count().max(1);
    let anchor = ctx.anchor();
    let material = POST_MATERIALS
        [(ctx.rng_next() * POST_MATERIALS.len() as f64) as usize % POST_MATERIALS.len()];
    let home = station_of(anchor, index, bot_count);
    let mut post: Option<BlockPos> = None;
    let mut round = 0u32;
    ctx.record(
        TYPE_LOOK,
        &format!("vis-signal-rota-{index}"),
        "Signal post rounds: keep one signal point, blink it, and patrol the others.",
        json!({"station": {"x": r2(home.0), "z": r2(home.1)}, "material": material,
            "bot_count": bot_count}),
    );
    loop {
        let Some(cell) = ensure_post(ctx, home, &mut post, material, round).await else {
            // No placeable column here this round: report, wander off the bad ground, retry.
            ctx.record(
                TYPE_LOOK,
                &format!("vis-signal-nosite-{index}-r{round}"),
                "No placeable signal column near the station this round.",
                json!({"round": round}),
            );
            shake_loose(ctx).await;
            if ctx.target_met() {
                break;
            }
            round += 1;
            continue;
        };
        // Blink rhythm, drawn per round: mostly a single toggle, sometimes a fast double blink,
        // sometimes a skipped beat so the rota never locks into one period.
        let roll = ctx.rng_next();
        if roll < 0.2 {
            let span = vary(ctx, -140.0, 140.0);
            let nod = vary(ctx, -12.0, 8.0);
            ctx.drag_view(span, nod, 2_500).await;
        } else {
            toggle_signal(ctx, cell, round, "a").await;
            if roll > 0.75 && !ctx.target_met() {
                // Double blink: circle off the post watching it, then toggle straight back.
                let arc = ctx.rng_next() * TAU;
                let out = vary(ctx, 3.5, 5.0);
                let focus = {
                    let c = cell.center();
                    (c.x, c.y, c.z)
                };
                let timeout = ctx.cfg().move_timeout_ms.min(8_000);
                ctx.move_to_watching_point(
                    cell.x as f64 + 0.5 + arc.cos() * out,
                    cell.z as f64 + 0.5 + arc.sin() * out,
                    focus,
                    timeout,
                    1.5,
                )
                .await;
                sleep_ms(vary(ctx, 600.0, 1_400.0) as u64).await;
                toggle_signal(ctx, cell, round, "b").await;
            }
        }
        // Step back off the post and hold it in frame briefly before leaving on patrol.
        let back = ctx.rng_next() * TAU;
        let dist = vary(ctx, 3.0, 5.5);
        let focus = {
            let c = cell.center();
            (c.x, c.y, c.z)
        };
        let timeout = ctx.cfg().move_timeout_ms.min(8_000);
        ctx.move_to_watching_point(
            cell.x as f64 + 0.5 + back.cos() * dist,
            cell.z as f64 + 0.5 + back.sin() * dist,
            focus,
            timeout,
            2.0,
        )
        .await;
        witness_peer_edits(ctx).await;
        if ctx.target_met() {
            break;
        }
        patrol_to_peer(ctx, anchor, round).await;
        witness_peer_edits(ctx).await;
        if ctx.rng_next() < 0.5 {
            glance_at_nearest_peer(ctx, None).await;
        }
        if ctx.target_met() {
            break;
        }
        round += 1;
    }
}
