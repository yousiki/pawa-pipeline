use super::*;

/// The two sides of the work area. Each keeps its own row, so the two replacements happen side by
/// side instead of over each other.
const SIDES: usize = 2;

/// Blocks in one work row.
const ROW_LEN: i32 = 3;

/// Materials the row is re-laid in, one per pass, so every replacement is visibly a different
/// material from the row it replaced.
const ROW_MATERIAL: [&str; 3] = [
    "minecraft:oak_planks",
    "minecraft:bricks",
    "minecraft:cobblestone",
];

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
/// `blocks`. Open-loop on purpose: the detour is walked, not pathfound, so the recorded key bits
/// stay independent of the navigator.
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

/// Break whatever stands in the row cells, at the height the last pass left them.
///
/// Scans a two-cell window rather than one guessed Y: `marker_base_near` may have laid the previous
/// row a block off the bot's own feet level, and a replacement that digs the wrong Y replaces
/// nothing the video can show.
async fn break_row(ctx: &mut Ctx, cells: &[(i32, i32)], feet_y: i32) -> (u64, u64) {
    let mut attempted = 0;
    let mut dug = 0;
    for &(x, z) in cells {
        for dy in [0, -1] {
            if ctx.target_met() {
                break;
            }
            let cell = BlockPos::new(x, feet_y + dy, z);
            if block_is_air(ctx, cell) {
                continue;
            }
            attempted += 1;
            if ctx.dig(cell).await {
                dug += 1;
            }
            break;
        }
    }
    (attempted, dug)
}

/// The placeable cells of a row, each validated for solid support and clear air first. Duplicates
/// are dropped: the neighbourhood search two adjacent cells run can converge on the same recovery
/// cell.
fn row_blocks(
    ctx: &Ctx,
    cells: &[(i32, i32)],
    feet_y: i64,
    material: &str,
) -> Vec<((i64, i64, i64), String)> {
    let mut blocks: Vec<((i64, i64, i64), String)> = Vec::new();
    for &(x, z) in cells {
        let Some(base) = marker_base_near(ctx, (x as i64, feet_y, z as i64), 1) else {
            continue;
        };
        let cell = (base.x as i64, base.y as i64, base.z as i64);
        if blocks.iter().any(|(existing, _)| *existing == cell) {
            continue;
        }
        blocks.push((cell, material.to_string()));
    }
    blocks
}

/// Each bot walks well away from its work row first, turns and comes back at it from the side,
/// breaks the row out and lays it again in a different material before backing off to look.
pub(crate) async fn run(ctx: &mut Ctx) {
    let index = ctx.index();
    let bot_count = ctx.player_count().max(1);
    let anchor = ctx.anchor();
    let side = index % SIDES;
    // Left rows sit west of the anchor and right rows east; every extra pair works six blocks
    // further out, so no two bots ever share a row or stand inside each other.
    let sign = if side == 0 { -1.0 } else { 1.0 };
    let lane = (index / SIDES) as f64 * 6.0;
    let row_x = (anchor.0 + sign * (9.0 + lane)).floor() as i32;
    let row_z = (anchor.1 + sign * 3.0).floor() as i32;
    let cells: Vec<(i32, i32)> = (0..ROW_LEN).map(|i| (row_x + i, row_z)).collect();
    let mut pass = 0usize;
    loop {
        let material = ROW_MATERIAL[pass % ROW_MATERIAL.len()];
        // Read the site before leaving it, so the detour has something to come back to.
        let span = vary(ctx, 100.0, 150.0);
        sweep(ctx, span).await;

        // The detour: cross to the far side of the arena, well away from the row.
        let away = vary(ctx, 12.0, 17.0);
        let drift = vary(ctx, -4.0, 4.0);
        let timeout = ctx.cfg().move_timeout_ms.min(18_000);
        let out = ctx
            .move_to(
                row_x as f64 - sign * away,
                row_z as f64 + drift,
                timeout,
                2.5,
            )
            .await;
        let bearing = if side == 0 {
            vary(ctx, 70.0, 110.0)
        } else {
            vary(ctx, 250.0, 290.0)
        };
        let distance = vary(ctx, 10.0, 15.0);
        let sprint = ctx.rng_next() < ctx.cfg().sprint_bout_prob;
        walk_bearing(ctx, bearing, distance, sprint).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-detour-{index}-p{pass}"),
            "Walk well away from the work row before turning back on it.",
            json!({"pass": pass, "side": side, "left_site": out, "heading_deg": r2(bearing),
                "distance": r2(distance), "sprint": sprint,
                "row": {"x": row_x, "z": row_z, "len": ROW_LEN}}),
        );

        // Come back at the row from the side, not down its own line, so the approach reads as an
        // approach and the row stays in frame while the bot closes on it.
        let flank = if pass.is_multiple_of(2) { -1.0 } else { 1.0 };
        let approach = vary(ctx, 2.0, 3.5);
        let timeout = ctx.cfg().move_timeout_ms.min(16_000);
        let back = ctx
            .move_to(
                row_x as f64 + ROW_LEN as f64 * 0.5,
                row_z as f64 + flank * approach,
                timeout,
                2.0,
            )
            .await;
        let equipped_tool = ctx.equip("diamond_pickaxe").await;
        let feet_y = ctx.position().map(|p| p.1.floor() as i32).unwrap_or(64);
        let (attempted, dug) = break_row(ctx, &cells, feet_y).await;
        ctx.record(
            TYPE_MINE,
            &format!("vis-row-break-{index}-p{pass}"),
            "Break the work row out from the flank it was approached from.",
            json!({"pass": pass, "side": side, "flank": flank, "returned": back,
                "equipped_tool": equipped_tool, "attempted": attempted, "dug": dug,
                "row": {"x": row_x, "y": feet_y, "z": row_z}}),
        );

        let equipped_material = ctx.equip(material).await;
        let feet_y = ctx.position().map(|p| p.1.floor() as i64).unwrap_or(64);
        let blocks = row_blocks(ctx, &cells, feet_y, material);
        let offered = blocks.len();
        let (placed, tried) = ctx
            .build_blocks(&blocks, |i, _target, _material| {
                (
                    format!("vis-row-place-{index}-p{pass}-{i}"),
                    format!("Lay row block {} in the replacement material.", i + 1),
                    json!({"pass": pass, "side": side, "cell": i}),
                )
            })
            .await;
        ctx.record(
            TYPE_PLACE,
            &format!("vis-row-replace-{index}-p{pass}"),
            &format!("Lay the row again in {material}."),
            json!({"pass": pass, "side": side, "material": material,
                "equipped_material": equipped_material, "placed": placed, "attempted": tried,
                "cells_offered": offered}),
        );

        // Back off far enough to see the finished row, and take in whoever else is working.
        let bearing = vary(ctx, 180.0, 220.0);
        let distance = vary(ctx, 8.0, 12.0);
        let sprint = ctx.rng_next() < ctx.cfg().sprint_bout_prob;
        walk_bearing(ctx, bearing, distance, sprint).await;
        if bot_count > 1 {
            witness_peer_edits(ctx).await;
            glance_at_nearest_peer(ctx, None).await;
        }
        let span = vary(ctx, 120.0, 160.0);
        sweep(ctx, span).await;
        if ctx.target_met() {
            break;
        }
        pass += 1;
    }
}
