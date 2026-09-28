use super::*;

/// The two crews shuttling between an outside checkpoint and the work row.
const CREWS: usize = 2;

/// Blocks in one work row. Short on purpose: the row has to be broken and re-laid inside a single
/// shuttle, so a viewer sees the whole repair rather than a fragment of it.
const ROW_LEN: i32 = 3;

/// Checkpoint each crew shuttles out to, as an anchor offset. Opposite corners, so the two crews
/// cross the arena in view of each other instead of orbiting the same side.
const CHECKPOINT: [(f64, f64); CREWS] = [(-8.0, -5.0), (9.0, 6.0)];

/// Where each crew's work row sits, as an anchor offset. Close to the middle and to each other, so
/// the two repairs share one frame.
const ROW_OFFSET: [(f64, f64); CREWS] = [(3.0, -1.0), (-3.0, 2.0)];

/// Materials the row is re-laid in. A pass takes the next one, so every repair is visibly a
/// different material from the row it replaced.
const REPAIR_MATERIAL: [&str; 3] = [
    "minecraft:oak_planks",
    "minecraft:bricks",
    "minecraft:stone_bricks",
];

/// Draw in `[lo, hi)` from the per-bot stream, so a number this scenario could have hard-coded
/// varies per bot and per pass and still replays from the seed.
fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

/// The crew a bot shuttles with. With a single bot the crew alternates every pass, so one bot still
/// works both rows instead of leaving one of them untouched for the episode.
fn crew_of(index: usize, bot_count: usize, pass: usize) -> usize {
    if bot_count >= CREWS {
        index % CREWS
    } else {
        (index + pass) % CREWS
    }
}

/// Triangle camera sweep of `deg` total: out to one edge, across to the other, back to centre.
async fn sweep(ctx: &Ctx, deg: f64) {
    let settle = ((deg.abs() / ctx.cfg().yaw_rate_deg_s.max(1.0) * 1000.0) as u64 + 600).min(4_000);
    for leg in [deg * 0.5, -deg, deg * 0.5] {
        ctx.drag_view(leg, 0.0, settle).await;
    }
}

/// Face a world bearing (Minecraft yaw: 0° is +Z, 90° is -X) and hold forward long enough to cover
/// `blocks`. Open-loop on purpose: the crossing is walked, not pathfound, so the recorded key bits
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

/// The farthest peer whose position has replicated — the other crew, from the middle of the arena.
fn farthest_peer(ctx: &Ctx) -> Option<usize> {
    let own = ctx.position()?;
    (0..ctx.player_count())
        .filter(|&j| j != ctx.index())
        .filter_map(|j| {
            ctx.peer_position(j)
                .map(|p| (j, (p.0 - own.0).hypot(p.2 - own.2)))
        })
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(j, _)| j)
}

/// Break whatever currently stands in the row cells, at the height the last pass left them.
///
/// Scans a two-cell window rather than one guessed Y: `marker_base_near` may have laid the previous
/// row a block off the bot's own feet level, and a repair that digs the wrong Y is a repair the
/// video does not show.
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

/// The placeable cells of a row, each validated for solid support and clear air before it is
/// offered to `build_blocks`. Duplicates are dropped: the neighbourhood search two adjacent cells
/// run can converge on the same recovery cell.
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

/// Two crews shuttle between an outside checkpoint and a short work row: they break part of the
/// row, cross to the far side, come back, and lay the row again in a different material.
pub(crate) async fn run(ctx: &mut Ctx) {
    let index = ctx.index();
    let bot_count = ctx.player_count().max(1);
    let anchor = ctx.anchor();
    // Every extra pair of bots works its own row seven blocks along, so two crews never repair the
    // same cells and never stand inside each other.
    let lane = (index / CREWS) as f64 * 7.0;
    let mut pass = 0usize;
    loop {
        let crew = crew_of(index, bot_count, pass);
        let material = REPAIR_MATERIAL[pass % REPAIR_MATERIAL.len()];
        let (cpx, cpz) = CHECKPOINT[crew];
        let (rox, roz) = ROW_OFFSET[crew];
        let row_x = (anchor.0 + rox + lane).floor() as i32;
        let row_z = (anchor.1 + roz + lane).floor() as i32;
        let cells: Vec<(i32, i32)> = (0..ROW_LEN).map(|i| (row_x + i, row_z)).collect();

        // Out to the checkpoint, jittered so two bots of one crew do not converge on the same spot.
        let jx = vary(ctx, -3.0, 3.0);
        let jz = vary(ctx, -3.0, 3.0);
        let timeout = ctx.cfg().move_timeout_ms.min(18_000);
        let at_checkpoint = ctx
            .move_to(
                anchor.0 + cpx + jx + lane,
                anchor.1 + cpz + jz,
                timeout,
                2.5,
            )
            .await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-shuttle-out-{index}-p{pass}"),
            &format!("Crew {crew} shuttles out to its checkpoint before the repair."),
            json!({"crew": crew, "pass": pass, "arrived": at_checkpoint,
                "checkpoint": {"x": r2(anchor.0 + cpx + jx + lane), "z": r2(anchor.1 + cpz + jz)}}),
        );

        // In to the row, tool first.
        let equipped_tool = ctx.equip("diamond_pickaxe").await;
        let approach = vary(ctx, 1.5, 2.5);
        let timeout = ctx.cfg().move_timeout_ms.min(16_000);
        ctx.move_to(
            row_x as f64 + 0.5,
            row_z as f64 + 0.5 - approach,
            timeout,
            2.0,
        )
        .await;
        let feet_y = ctx.position().map(|p| p.1.floor() as i32).unwrap_or(64);
        let (attempted, dug) = break_row(ctx, &cells, feet_y).await;
        ctx.record(
            TYPE_MINE,
            &format!("vis-shuttle-break-{index}-p{pass}"),
            &format!("Crew {crew} breaks part of its work row."),
            json!({"crew": crew, "pass": pass, "equipped_tool": equipped_tool,
                "row": {"x": row_x, "y": feet_y, "z": row_z, "len": ROW_LEN},
                "attempted": attempted, "dug": dug}),
        );

        // Cross to the far side and back, so the repair is a shuttle and not a stand-and-build.
        let bearing = if crew == 0 {
            vary(ctx, 170.0, 210.0)
        } else {
            vary(ctx, 0.0, 40.0)
        };
        let distance = vary(ctx, 8.0, 12.0);
        let sprint = ctx.rng_next() < ctx.cfg().sprint_bout_prob;
        walk_bearing(ctx, bearing, distance, sprint).await;
        let span = vary(ctx, 110.0, 160.0);
        sweep(ctx, span).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-shuttle-cross-{index}-p{pass}"),
            &format!("Crew {crew} crosses to the far side and looks back at the row."),
            json!({"crew": crew, "pass": pass, "heading_deg": r2(bearing),
                "distance": r2(distance), "sprint": sprint, "sweep_deg": r2(span)}),
        );

        let equipped_material = ctx.equip(material).await;
        let approach = vary(ctx, 1.5, 2.5);
        let timeout = ctx.cfg().move_timeout_ms.min(16_000);
        let back = ctx
            .move_to(
                row_x as f64 + 0.5,
                row_z as f64 + 0.5 - approach,
                timeout,
                2.0,
            )
            .await;
        let feet_y = ctx.position().map(|p| p.1.floor() as i64).unwrap_or(64);
        let blocks = row_blocks(ctx, &cells, feet_y, material);
        let (placed, tried) = ctx
            .build_blocks(&blocks, |i, _target, _material| {
                (
                    format!("vis-shuttle-place-{index}-p{pass}-{i}"),
                    format!("Lay row block {} in the repair material.", i + 1),
                    json!({"crew": crew, "pass": pass, "cell": i}),
                )
            })
            .await;
        ctx.record(
            TYPE_PLACE,
            &format!("vis-shuttle-repair-{index}-p{pass}"),
            &format!("Crew {crew} lays the row again in {material}."),
            json!({"crew": crew, "pass": pass, "material": material, "returned": back,
                "equipped_material": equipped_material, "placed": placed, "attempted": tried,
                "cells_offered": blocks.len()}),
        );

        // Frame the other crew's repair before the next shuttle, so both rows stay in the episode.
        // The witness gaze targets the actual fresh edit; the peer look is the fallback framing.
        if !witness_peer_edits(ctx).await {
            match farthest_peer(ctx) {
                Some(peer) => {
                    let settle = ctx.cfg().look_settle_ms.max(800);
                    look_at_peer(ctx, peer, settle).await;
                    sleep_ms(vary(ctx, 1_000.0, 2_000.0) as u64).await;
                }
                None => {
                    let span = vary(ctx, 130.0, 170.0);
                    sweep(ctx, span).await;
                }
            }
        }
        if ctx.target_met() {
            break;
        }
        pass += 1;
    }
}
