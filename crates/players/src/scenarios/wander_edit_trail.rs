use super::*;

/// The whole block kit, drawn per edit so the trail mixes materials.
const MATERIALS: [&str; 6] = [
    "minecraft:stone_bricks",
    "minecraft:oak_planks",
    "minecraft:oak_log",
    "minecraft:cobblestone",
    "minecraft:bricks",
    "minecraft:stone",
];

/// Soft leash radius around the swarm anchor. Wanderers may not drift further than this from the
/// anchor, which bounds any pair at twice the leash — inside `witness_peer_edits`'s 64-block
/// horizon, so the sparse edits keep finding witnesses.
const LEASH: f64 = 26.0;

/// Draw in `[lo, hi)` from the per-bot stream, so a number this scenario could have hard-coded
/// varies per bot and per leg and still replays from the seed.
fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

/// Uniform draw in `0..n` from the per-bot stream.
fn pick(ctx: &mut Ctx, n: usize) -> usize {
    ((ctx.rng_next() * n as f64) as usize).min(n.saturating_sub(1))
}

/// Nearest live peer, for camera-on-peer travel legs.
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

/// Next waypoint: mostly persist the current heading with jitter, sometimes turn hard, always bend
/// back toward the anchor once past the leash, and redraw away from any peer already standing near
/// the candidate.
fn draw_waypoint(ctx: &mut Ctx, heading: &mut f64) -> Option<(f64, f64)> {
    let own = ctx.position()?;
    let anchor = ctx.anchor();
    *heading += if ctx.rng_next() < 0.18 {
        vary(ctx, -std::f64::consts::PI, std::f64::consts::PI)
    } else {
        vary(ctx, -0.9, 0.9)
    };
    let dist = vary(ctx, 8.0, 22.0);
    for _ in 0..3 {
        let cand = (own.0 + heading.cos() * dist, own.2 + heading.sin() * dist);
        if (cand.0 - anchor.0).hypot(cand.1 - anchor.1) > LEASH {
            *heading = (anchor.1 - own.2).atan2(anchor.0 - own.0) + vary(ctx, -0.6, 0.6);
            continue;
        }
        let crowded = (0..ctx.player_count())
            .filter(|&j| j != ctx.index())
            .filter_map(|j| ctx.peer_position(j))
            .any(|p| (p.0 - cand.0).hypot(p.2 - cand.1) < MIN_PEER_GAP + 0.5);
        if crowded {
            *heading += vary(ctx, 1.2, 2.4);
            continue;
        }
        return Some(cand);
    }
    Some((own.0 + heading.cos() * dist, own.2 + heading.sin() * dist))
}

/// A ground cell 1–2.5 blocks ahead of the bot along `heading` that can take a placement.
fn trail_cell(ctx: &mut Ctx, heading: f64, height: i64) -> Option<BlockPos> {
    let own = ctx.position()?;
    let ahead = vary(ctx, 1.2, 2.5);
    let desired = (
        (own.0 + heading.cos() * ahead).floor() as i64,
        own.1.floor() as i64,
        (own.2 + heading.sin() * ahead).floor() as i64,
    );
    marker_base_near(ctx, desired, height)
}

/// Drop 1–3 path blocks of one material along the trail.
async fn pave(ctx: &mut Ctx, heading: f64, leg: usize) {
    let material = MATERIALS[pick(ctx, MATERIALS.len())];
    let count = 1 + pick(ctx, 3);
    let mut cells: Vec<((i64, i64, i64), String)> = Vec::new();
    for _ in 0..count {
        let bearing = heading + vary(ctx, -0.5, 0.5);
        let Some(base) = trail_cell(ctx, bearing, 1) else {
            continue;
        };
        let cell = (base.x as i64, base.y as i64, base.z as i64);
        if !cells.iter().any(|(existing, _)| *existing == cell) {
            cells.push((cell, material.to_string()));
        }
    }
    let index = ctx.index();
    let (placed, attempted) = ctx
        .build_blocks(&cells, |i, _t, _m| {
            (
                format!("vis-trail-pave-{index}-l{leg}-{i}"),
                format!("Lay trail path block {}.", i + 1),
                json!({"leg": leg, "edit": "pave"}),
            )
        })
        .await;
    ctx.record(
        TYPE_PLACE,
        &format!("vis-trail-pave-done-{index}-l{leg}"),
        "Pave a few path blocks along the wandering trail.",
        json!({"leg": leg, "edit": "pave", "material": material,
            "placed": placed, "attempted": attempted}),
    );
}

/// Plant one floor torch where the bot stands.
async fn plant_torch(ctx: &mut Ctx, heading: f64, leg: usize) {
    let equipped = ctx.equip("torch").await;
    let Some(cell) = trail_cell(ctx, heading, 1) else {
        return;
    };
    if !ctx.ground_torch_face_in_reach(cell) {
        let timeout = ctx.cfg().move_timeout_ms.min(6_000);
        ctx.move_to(cell.x as f64 + 0.5, cell.z as f64 + 0.5, timeout, 1.5)
            .await;
    }
    let floor = cell.down(1).center();
    ctx.look(
        (floor.x, floor.y, floor.z),
        ctx.cfg().look_settle_ms.max(500),
    )
    .await;
    let placed = ctx.place_ground_torch(cell).await;
    ctx.record(
        TYPE_PLACE,
        &format!("vis-trail-torch-{}-l{leg}", ctx.index()),
        "Plant a floor torch on the wandering trail.",
        json!({"leg": leg, "edit": "torch", "placed": placed, "equipped": equipped,
            "cell": {"x": cell.x, "y": cell.y, "z": cell.z}}),
    );
}

/// Raise a 1–2 block accent stack beside the trail.
async fn accent(ctx: &mut Ctx, heading: f64, leg: usize) {
    let material = MATERIALS[pick(ctx, MATERIALS.len())];
    let height = 1 + pick(ctx, 2) as i64;
    let Some(base) = trail_cell(ctx, heading, height) else {
        return;
    };
    let cells: Vec<((i64, i64, i64), String)> = (0..height)
        .map(|dy| {
            (
                (base.x as i64, base.y as i64 + dy, base.z as i64),
                material.to_string(),
            )
        })
        .collect();
    let index = ctx.index();
    let (placed, attempted) = ctx
        .build_blocks(&cells, |i, _t, _m| {
            (
                format!("vis-trail-accent-{index}-l{leg}-{i}"),
                format!("Raise accent block {}.", i + 1),
                json!({"leg": leg, "edit": "accent"}),
            )
        })
        .await;
    ctx.record(
        TYPE_PLACE,
        &format!("vis-trail-accent-done-{index}-l{leg}"),
        "Raise a small accent stack beside the trail.",
        json!({"leg": leg, "edit": "accent", "material": material, "height": height,
            "placed": placed, "attempted": attempted}),
    );
}

/// Dig one surface block out of the ground ahead and, usually, fill the hole back with a kit
/// material — a dig-then-heal edit that stays one block deep and in full daylight.
async fn dig_and_fill(ctx: &mut Ctx, heading: f64, leg: usize) {
    let Some(cell) = trail_cell(ctx, heading, 1) else {
        return;
    };
    let target = cell.down(1);
    if !ctx.target_in_reach(target) {
        let timeout = ctx.cfg().move_timeout_ms.min(6_000);
        ctx.move_to(cell.x as f64 + 0.5, cell.z as f64 + 0.5, timeout, 2.0)
            .await;
    }
    ctx.equip("diamond_pickaxe").await;
    let dug = ctx.dig(target).await;
    ctx.record(
        TYPE_MINE,
        &format!("vis-trail-dig-{}-l{leg}", ctx.index()),
        "Dig one surface block out of the trail.",
        json!({"leg": leg, "edit": "dig_fill", "dug": dug,
            "block_position": {"x": target.x, "y": target.y, "z": target.z}}),
    );
    if dug && ctx.rng_next() < 0.8 {
        let material = MATERIALS[pick(ctx, MATERIALS.len())];
        if ctx.equip(material).await {
            let placed = ctx.place_block(target, material).await;
            ctx.record(
                TYPE_PLACE,
                &format!("vis-trail-fill-{}-l{leg}", ctx.index()),
                "Fill the fresh hole back with a kit block.",
                json!({"leg": leg, "edit": "dig_fill", "placed": placed, "block": material,
                    "block_position": {"x": target.x, "y": target.y, "z": target.z}}),
            );
        }
    }
}

/// Bots wander the surface on diverse random trajectories — persistent headings, hard turns,
/// sprint bursts, camera-on-peer and camera-on-skyline travel legs — leaving a sparse but steady
/// trail of world edits behind them: path blocks, floor torches, accent stacks, and one-block
/// dig-and-fill scars. Between legs they glance at the nearest peer and turn to witness each
/// other's fresh edits. A soft leash around the anchor keeps every pair inside witness range.
pub(super) async fn run(ctx: &mut Ctx) {
    let index = ctx.index();
    let bot_count = ctx.player_count().max(1);
    // Fan the initial headings out by index so the swarm disperses instead of walking one line.
    let mut heading =
        (index as f64 / bot_count as f64) * std::f64::consts::TAU + vary(ctx, -0.4, 0.4);
    let mut leg = 0usize;
    loop {
        let Some(waypoint) = draw_waypoint(ctx, &mut heading) else {
            sleep_ms(500).await;
            continue;
        };
        // Occasional open-loop sprint burst toward the waypoint before the planner takes over.
        if ctx.rng_next() < 0.3 {
            if let Some(p) = ctx.position() {
                ctx.look(
                    (waypoint.0, p.1 + 1.2, waypoint.1),
                    ctx.cfg().look_settle_ms.max(400),
                )
                .await;
            }
            let sprint = ctx.rng_next() < 0.6;
            let burst = vary(ctx, 800.0, 1_800.0) as u64;
            ctx.hold_visual(WalkDirection::Forward, sprint, burst, 300)
                .await;
        }
        // Travel style: camera on the path, on the nearest peer, or on an off-path skyline point.
        let style = ctx.rng_next();
        let style_name = if style < 0.35 {
            if let Some(peer) = nearest_peer(ctx) {
                let timeout = ctx.cfg().move_timeout_ms.min(12_000);
                ctx.move_to_watching_peer(waypoint.0, waypoint.1, peer, timeout, 2.0)
                    .await;
                "watch_peer"
            } else {
                travel_to(ctx, waypoint, TRAVEL_HOP, 2.0).await;
                "hops"
            }
        } else if style < 0.55 {
            let focus = (
                waypoint.0 + vary(ctx, -10.0, 10.0),
                ctx.position().map(|p| p.1).unwrap_or(64.0) + vary(ctx, 0.0, 6.0),
                waypoint.1 + vary(ctx, -10.0, 10.0),
            );
            let timeout = ctx.cfg().move_timeout_ms.min(12_000);
            ctx.move_to_watching_point(waypoint.0, waypoint.1, focus, timeout, 2.0)
                .await;
            "watch_point"
        } else {
            travel_to(ctx, waypoint, TRAVEL_HOP, 2.0).await;
            "hops"
        };
        ctx.record(
            TYPE_MOVE,
            &format!("vis-trail-leg-{index}-l{leg}"),
            &format!("Wander leg {leg}: travel ({style_name}) and edit the trail."),
            json!({"leg": leg, "style": style_name, "heading_rad": r2(heading),
                "waypoint": {"x": r2(waypoint.0), "z": r2(waypoint.1)}}),
        );

        // Sparse edit beat: usually one small persistent mark, sometimes none.
        let roll = ctx.rng_next();
        if roll < 0.3 {
            pave(ctx, heading, leg).await;
        } else if roll < 0.5 {
            plant_torch(ctx, heading, leg).await;
        } else if roll < 0.7 {
            accent(ctx, heading, leg).await;
        } else if roll < 0.85 {
            dig_and_fill(ctx, heading, leg).await;
        } else {
            // No edit this leg: read the horizon instead.
            let dyaw = vary(ctx, -120.0, 120.0);
            let dpitch = vary(ctx, -15.0, 10.0);
            ctx.drag_view(dyaw, dpitch, 1_500).await;
        }
        if ctx.rng_next() < 0.5 {
            glance_at_nearest_peer(ctx, None).await;
        }
        witness_peer_edits(ctx).await;
        if ctx.target_met() {
            break;
        }
        leg += 1;
    }
}
