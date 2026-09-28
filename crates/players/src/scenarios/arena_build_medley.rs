use super::*;

/// The whole block kit, so one episode shows every placeable material instead of one per bot.
const MATERIALS: [&str; 6] = [
    "minecraft:stone_bricks",
    "minecraft:oak_planks",
    "minecraft:oak_log",
    "minecraft:cobblestone",
    "minecraft:bricks",
    "minecraft:stone",
];

/// The two radial rings sites alternate between. Shapes are one block thin radially, so the two
/// rings plus the arena rim leave three open radial corridors on every spoke.
const RING_INNER: f64 = 11.5;
const RING_OUTER: f64 = 15.5;

/// Tangential slot a pass adds on top of its ring, so a third visit to the same ring lands on a
/// fresh arc instead of inside the first structure. Clamped to the bot's own arc at runtime.
const TANGENT_SLOTS: [f64; 4] = [0.0, 0.0, 4.5, -4.5];

/// Draw in `[lo, hi)` from the per-bot stream, so a number this scenario could have hard-coded
/// varies per bot and per pass and still replays from the seed.
fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

/// Uniform draw in `0..n` from the per-bot stream.
fn pick(ctx: &mut Ctx, n: usize) -> usize {
    ((ctx.rng_next() * n as f64) as usize).min(n.saturating_sub(1))
}

/// Ground level of the site column: the highest air-over-solid cell in a band around the bot's own
/// feet. On the arena slab this is the floor; on a cell an earlier pass built on, it is that
/// structure's top, which stacks the new shape instead of grinding unplaceable cells.
fn site_ground_y(ctx: &Ctx, sx: i64, sz: i64, feet_y: i64) -> i64 {
    for y in ((feet_y - 8)..=(feet_y + 16)).rev() {
        let pos = BlockPos::new(sx as i32, y as i32, sz as i32);
        if block_is_air(ctx, pos) && block_is_safe_support(ctx, pos.down(1)) {
            return y;
        }
    }
    feet_y
}

/// One placement: world cell plus the material to put there — `Ctx::build_blocks`'s row type.
type Placement = ((i64, i64, i64), String);

/// A freshly drawn freeform shape. Cells extend TANGENTIALLY (along `axis`, centred on the site),
/// so every structure is one block thin radially and the bot's own spoke stays an open walking
/// corridor. Two materials are drawn per shape and mixed per block, so no two sites read alike.
fn medley_blocks(
    ctx: &mut Ctx,
    sx: i64,
    sy: i64,
    sz: i64,
    axis: (i64, i64),
) -> (Vec<Placement>, &'static str) {
    let mat_a = MATERIALS[pick(ctx, MATERIALS.len())];
    let mat_b = MATERIALS[pick(ctx, MATERIALS.len())];
    let (ax, az) = axis;
    let at = |i: i64| (sx + ax * i, sz + az * i);
    let mut cells: Vec<(i64, i64, i64)> = Vec::new();
    let shape = match pick(ctx, 4) {
        0 => {
            // Cluster of 2–3 columns of independent heights, two blocks apart, centred.
            let cols = 2 + pick(ctx, 2) as i64;
            for c in 0..cols {
                let (x, z) = at((c - cols / 2) * 2);
                let height = 2 + pick(ctx, 3) as i64;
                for dy in 0..height {
                    cells.push((x, sy + dy, z));
                }
            }
            "columns"
        }
        1 => {
            // Low wall of 3–5 blocks whose upper course has random gaps.
            let len = 3 + pick(ctx, 3) as i64;
            for i in 0..len {
                let (x, z) = at(i - len / 2);
                cells.push((x, sy, z));
                if ctx.rng_next() < 0.7 {
                    cells.push((x, sy + 1, z));
                }
            }
            "gapped_wall"
        }
        2 => {
            // Solid staircase rising 3–4 cells; every column is self-supported.
            let len = 3 + pick(ctx, 2) as i64;
            for i in 0..len {
                let (x, z) = at(i - len / 2);
                for dy in 0..=i {
                    cells.push((x, sy + dy, z));
                }
            }
            "steps"
        }
        _ => {
            // Ground plus-shape with a 1–2 block cap in the middle.
            for (dx, dz) in [(0, 0), (1, 0), (-1, 0), (0, 1), (0, -1)] {
                cells.push((sx + dx, sy, sz + dz));
            }
            cells.push((sx, sy + 1, sz));
            if ctx.rng_next() < 0.5 {
                cells.push((sx, sy + 2, sz));
            }
            "capped_plus"
        }
    };
    let blocks = cells
        .into_iter()
        .map(|cell| {
            let material = if ctx.rng_next() < 0.5 { mat_a } else { mat_b };
            (cell, material.to_string())
        })
        .collect();
    (blocks, shape)
}

/// Every bot builds AT THE SAME TIME on its own spoke of the arena: freeform mixed-material shapes
/// go up in short bursts, and between bursts each builder randomly turns to witness a peer's fresh
/// edits (`witness_peer_edits`) or glances at the nearest peer, then works its site from a fresh
/// spot. A finished shape may get one remodel beat — dig a random cell back out and patch it with
/// a different material — before the builder retreats inward, reads its work, strolls its own
/// corridor, and re-sites for the next pass. No shared barriers: bots only ever meet through the
/// edit broadcast.
///
/// GEOMETRY, all inside the build_arena slab (cleared flat floor, radius 20, no caves): sites
/// alternate between two rings (11.5 / 15.5) with stepping tangential slots, shapes extend
/// tangentially and are one block thin radially, and every body movement — approach, side-step,
/// retreat, stroll — runs radially along the bot's own spoke, which those thin shapes never wall
/// off. The predecessor (parallel_build_medley) let sites and sprint strolls drift onto natural
/// terrain (one bot filmed the inside of a cave), and its arena retrofit stacked passes onto one
/// narrow band whose structures then blocked its own pathfinder goals for 27 s at a time.
///
/// ponytail: the fixed rings cap the comfortable swarm at ~8-10 bots (arc per bot at 11.5 is 9
/// at n=8); beyond that, shrink TANGENT_SLOTS or add a third ring.
pub(super) async fn run(ctx: &mut Ctx) {
    let cfg = ctx.cfg().clone();
    let index = ctx.index();
    let bot_count = ctx.player_count().max(1);
    let anchor = ctx.anchor();
    let ang = (index as f64 / bot_count as f64) * std::f64::consts::TAU;
    let (rx, rz) = (ang.cos(), ang.sin());
    // Cardinal axis closest to this bot's own TANGENT: shapes grow sideways, never radially.
    let axis = if rz.abs() >= rx.abs() {
        ((-rz.signum()) as i64, 0)
    } else {
        (0, rx.signum() as i64)
    };
    let mut pass = 0usize;
    loop {
        let ring = if pass.is_multiple_of(2) {
            RING_INNER
        } else {
            RING_OUTER
        };
        // Own arc half-width at this ring, minus room for the shape's own 2.5-cell reach.
        let max_tangent = (std::f64::consts::PI * ring / bot_count as f64 - 3.0).max(0.0);
        let tangent = (TANGENT_SLOTS[pass % TANGENT_SLOTS.len()] + vary(ctx, -1.0, 1.0))
            .clamp(-max_tangent, max_tangent);
        let radius = ring + vary(ctx, -0.7, 0.7);
        let sx = anchor.0.round() as i64 + (rx * radius - rz * tangent).round() as i64;
        let sz = anchor.1.round() as i64 + (rz * radius + rx * tangent).round() as i64;
        // Approach on the inward side of the site: the inward corridor is always open.
        ctx.move_to(
            sx as f64 - rx * 2.5,
            sz as f64 - rz * 2.5,
            cfg.move_timeout_ms.min(8_000),
            2.0,
        )
        .await;
        let feet_y = ctx.position().map(|p| p.1).unwrap_or(64.0).floor() as i64;
        let base_y = site_ground_y(ctx, sx, sz, feet_y);
        let (blocks, shape) = medley_blocks(ctx, sx, base_y, sz, axis);
        let total = blocks.len();
        ctx.record(
            TYPE_LOOK,
            &format!("vis-arena-medley-start-{index}-p{pass}"),
            &format!("Bot {index} starts a {shape} medley of {total} blocks at ({sx}, {sz})."),
            json!({"pass": pass, "shape": shape, "blocks": total, "ring": ring,
                "origin": {"x": sx, "y": base_y, "z": sz}}),
        );

        // Build in short bursts; between bursts the builder witnesses peers and shifts along its
        // own corridor, so parallel construction never degrades into a statue. Bursts are capped
        // at 4 cells: a mis-sited burst grinds flagless (no place packet, so QC-frozen accrues),
        // and four bail-outs stay safely under the 20 s frozen gate before the gap's real
        // displacement resets the run.
        let mut placed = 0u64;
        let mut attempted = 0u64;
        let mut done = 0usize;
        while done < total && !ctx.target_met() {
            let end = (done + 3 + pick(ctx, 2)).min(total);
            let offset = done;
            let (ok, tried) = ctx
                .build_blocks(&blocks[done..end], |b, _t, _m| {
                    (
                        format!("vis-arena-medley-{index}-p{pass}-{}", offset + b),
                        format!("Place {shape} block {}/{total}.", offset + b + 1),
                        json!({"pass": pass, "shape": shape, "block_index": offset + b}),
                    )
                })
                .await;
            placed += ok;
            attempted += tried;
            done = end;
            if done < total && !ctx.target_met() {
                if ctx.rng_next() < 0.6 {
                    witness_peer_edits(ctx).await;
                }
                if ctx.rng_next() < 0.35 {
                    glance_at_nearest_peer(ctx, None).await;
                }
                // Fresh working spot INWARD of the next burst's first cell — guaranteed-open
                // floor, real displacement (a hold alone stalled against the structure and grew
                // a 20.9 s frozen run in an earlier round), and it pre-positions the builder in
                // reach of the coming cells, cutting the per-cell pathfinder nudges that
                // dominate the sub-0.5 s tap count the chatter gate counts.
                let (next_cell, _) = &blocks[done];
                let standoff = vary(ctx, 2.0, 3.0);
                let step = (
                    next_cell.0 as f64 + 0.5 - rx * standoff,
                    next_cell.2 as f64 + 0.5 - rz * standoff,
                );
                ctx.move_to(step.0, step.1, cfg.move_timeout_ms.min(6_000), 1.8)
                    .await;
                // One sustained press for the chatter budget: walk back out toward the site with
                // enough veer to usually pass beside the one-block-thin shape.
                if let Some(p) = ctx.position() {
                    let veer = vary(ctx, -0.5, 0.5);
                    let (dx, dz) = (rx + (-rz) * veer, rz + rx * veer);
                    ctx.look(
                        (p.0 + dx * 16.0, p.1 + 1.2, p.2 + dz * 16.0),
                        cfg.look_settle_ms.max(400),
                    )
                    .await;
                }
                let hold = vary(ctx, 1_100.0, 1_600.0) as u64;
                ctx.hold_visual(WalkDirection::Forward, false, hold, 300)
                    .await;
            }
        }
        ctx.record(
            TYPE_MOVE,
            &format!("vis-arena-medley-done-{index}-p{pass}"),
            &format!("Medley {shape}: placed {placed}/{attempted}/{total} blocks."),
            json!({"pass": pass, "shape": shape, "placed": placed, "attempted": attempted,
                "expected": total}),
        );

        // Remodel beat: dig one random cell back out and, usually, patch the hole with a fresh
        // material — a persistent edit the witnesses can turn to.
        if !ctx.target_met() && placed > 0 && ctx.rng_next() < 0.5 {
            let (cell, _) = &blocks[pick(ctx, total)];
            let target = BlockPos::new(cell.0 as i32, cell.1 as i32, cell.2 as i32);
            if !ctx.target_in_reach(target) {
                ctx.move_to(
                    cell.0 as f64 - rx * 1.5,
                    cell.2 as f64 - rz * 1.5,
                    cfg.move_timeout_ms.min(6_000),
                    (cfg.place_reach - 1.0).max(1.5),
                )
                .await;
            }
            ctx.equip("diamond_pickaxe").await;
            let dug = ctx.dig(target).await;
            ctx.record(
                TYPE_MINE,
                &format!("vis-arena-medley-remodel-dig-{index}-p{pass}"),
                "Remodel: dig one block back out of the fresh medley.",
                json!({"pass": pass, "dug": dug,
                    "block_position": {"x": target.x, "y": target.y, "z": target.z}}),
            );
            if dug && ctx.rng_next() < 0.75 {
                let patch = MATERIALS[pick(ctx, MATERIALS.len())];
                if ctx.equip(patch).await {
                    let ok = ctx.place_block(target, patch).await;
                    ctx.record(
                        TYPE_PLACE,
                        &format!("vis-arena-medley-remodel-patch-{index}-p{pass}"),
                        "Remodel: patch the dug cell with a different material.",
                        json!({"pass": pass, "placed": ok, "block": patch,
                            "block_position": {"x": target.x, "y": target.y, "z": target.z}}),
                    );
                }
            }
        }

        // Retreat inward with the structure pinned in frame (the camera owes nothing to the
        // path), then read the site and the neighbours.
        let structure_focus = (sx as f64 + 0.5, base_y as f64 + 1.4, sz as f64 + 0.5);
        let retreat = vary(ctx, 4.5, 6.5);
        ctx.move_to_watching_point(
            sx as f64 - rx * retreat,
            sz as f64 - rz * retreat,
            structure_focus,
            cfg.move_timeout_ms.min(8_000),
            2.5,
        )
        .await;
        ctx.look_default(structure_focus).await;
        witness_peer_edits(ctx).await;
        glance_at_nearest_peer(ctx, None).await;
        sleep_ms(vary(ctx, 600.0, 1_400.0) as u64).await;
        // Out-and-back stroll along the bot's own corridor: two sustained presses per pass for
        // the chatter budget. The outward leg never sprints, so the walk stays on the slab.
        for leg in 0..2 {
            if ctx.target_met() {
                break;
            }
            let outward = leg == 0;
            if let Some(p) = ctx.position() {
                let veer = vary(ctx, -0.4, 0.4);
                let sign = if outward { 1.0 } else { -1.0 };
                let (dx, dz) = (rx * sign + (-rz) * veer, rz * sign + rx * veer);
                ctx.look(
                    (p.0 + dx * 24.0, p.1 + 1.2, p.2 + dz * 24.0),
                    cfg.look_settle_ms.max(400),
                )
                .await;
            }
            let sprint = !outward && ctx.rng_next() < cfg.sprint_bout_prob;
            let hold = if outward {
                vary(ctx, 1_400.0, 2_000.0) as u64
            } else {
                vary(ctx, 1_400.0, 2_200.0) as u64
            };
            ctx.hold_visual(WalkDirection::Forward, sprint, hold, 300)
                .await;
        }
        if ctx.target_met() {
            break;
        }
        pass += 1;
    }
}
