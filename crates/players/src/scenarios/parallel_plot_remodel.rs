use super::*;

/// Block materials a plot pass may draw from — kit blocks only.
const MATERIALS: [&str; 4] = [
    "minecraft:stone_bricks",
    "minecraft:oak_planks",
    "minecraft:cobblestone",
    "minecraft:oak_log",
];

/// Small cluster footprints (dx, dy, dz) relative to the pass cell — deliberately tiny, so one
/// pass is a short burst of placements and the plot fills up as scattered lumps, not one build.
const CLUSTERS: [&[(i64, i64, i64)]; 5] = [
    &[(0, 0, 0), (0, 1, 0)],
    &[(0, 0, 0), (0, 1, 0), (0, 2, 0)],
    &[(0, 0, 0), (1, 0, 0)],
    &[(0, 0, 0), (1, 0, 0), (0, 1, 0)],
    &[(0, 0, 0), (1, 0, 0), (1, 1, 0)],
];

/// Centre-to-centre distance between two plots.
const PLOT_PITCH: f64 = 12.0;

/// How far a work cell may sit from its plot centre. `PLOT_PITCH - 2 * (PLOT_REACH + 0.5)` keeps
/// two neighbours clear of the 4.5-block peer gap even when both wander toward each other.
const PLOT_REACH: f64 = 3.0;

/// Keep at most this many remembered own cells; older ones simply stop being remodel candidates.
const OWN_CELL_MEMORY: usize = 48;

/// Draw in `[lo, hi)` from the per-bot stream, so a number this scenario could have hard-coded
/// varies per bot and per pass and still replays from the seed.
fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

/// Rotate a cluster footprint by a quarter-turn drawn from `r`, so the same shape lands in four
/// orientations across passes.
fn orient(cluster: &[(i64, i64, i64)], r: f64) -> Vec<(i64, i64, i64)> {
    let quarter = (r * 4.0) as usize % 4;
    cluster
        .iter()
        .map(|&(dx, dy, dz)| match quarter {
            0 => (dx, dy, dz),
            1 => (-dz, dy, dx),
            2 => (-dx, dy, -dz),
            _ => (dz, dy, -dx),
        })
        .collect()
}

/// This bot's plot centre on an anchor-centred grid. A grid (not a ring) keeps every plot compact
/// and inside the levelled arena at the swarm sizes we record, and the pitch minus twice the work
/// wander leaves a five-block gap between neighbouring workers.
fn plot_center(anchor: (f64, f64), index: usize, bot_count: usize) -> (f64, f64) {
    let cols = (bot_count.max(1) as f64).sqrt().ceil() as usize;
    let rows = bot_count.max(1).div_ceil(cols);
    let (col, row) = (index % cols, index / cols);
    (
        anchor.0 + (col as f64 - (cols as f64 - 1.0) / 2.0) * PLOT_PITCH,
        anchor.1 + (row as f64 - (rows as f64 - 1.0) / 2.0) * PLOT_PITCH,
    )
}

/// Every bot works its own grid plot at the same time: short bursts of tiny cluster placements at
/// random cells, interleaved with digging back one of its own earlier blocks (remodelling), corner
/// strolls, and witness gazes at whatever a neighbour just changed. Nobody waits on anybody — the
/// only coupling is the witness channel.
pub(super) async fn run(ctx: &mut Ctx) {
    let index = ctx.index();
    let bot_count = ctx.player_count().max(1);
    let anchor = ctx.anchor();
    let (px, pz) = plot_center(anchor, index, bot_count);
    let timeout = ctx.cfg().move_timeout_ms.min(8_000);
    travel_to(ctx, (px, pz), TRAVEL_HOP, 1.5).await;
    ctx.record(
        TYPE_MOVE,
        &format!("vis-plot-claim-{index}"),
        &format!("Bot {index} claims its build plot at ({px:.1}, {pz:.1})."),
        json!({"plot": {"x": r2(px), "z": r2(pz)}, "pitch": PLOT_PITCH, "bot_count": bot_count}),
    );

    let mut own_cells: Vec<(BlockPos, String)> = Vec::new();
    let mut pass = 0usize;
    loop {
        let remodel = own_cells.len() >= 4 && ctx.rng_next() < 0.3;
        if remodel {
            let k = (ctx.rng_next() * own_cells.len() as f64) as usize % own_cells.len();
            let (target, material) = own_cells.remove(k);
            let wanted = material.strip_prefix("minecraft:").unwrap_or(&material);
            if ctx.block_id_at(target).as_deref() == Some(wanted) {
                ctx.move_to(
                    target.x as f64 + 0.5,
                    target.z as f64 + 0.5,
                    timeout,
                    (ctx.cfg().place_reach - 1.0).max(1.5),
                )
                .await;
                ctx.equip("diamond_pickaxe").await;
                let removed = ctx.dig(target).await;
                ctx.record(
                    TYPE_MINE,
                    &format!("vis-plot-remodel-{index}-p{pass}"),
                    &format!("Remodel: dig back an earlier {wanted} block of this bot's plot."),
                    json!({"pass": pass, "removed": removed, "block": material,
                        "block_position": {"x": target.x, "y": target.y, "z": target.z}}),
                );
            }
        } else {
            // A fresh cluster at a random cell of the plot, placed from a stand-off point so the
            // base cell never overlaps the placer's own hitbox.
            let cx = px + vary(ctx, -PLOT_REACH, PLOT_REACH);
            let cz = pz + vary(ctx, -PLOT_REACH, PLOT_REACH);
            let stand = (ctx.rng_next() * std::f64::consts::TAU, 2.2);
            ctx.move_to(
                cx + stand.0.cos() * stand.1,
                cz + stand.0.sin() * stand.1,
                timeout,
                1.5,
            )
            .await;
            let material =
                MATERIALS[(ctx.rng_next() * MATERIALS.len() as f64) as usize % MATERIALS.len()];
            let shape =
                CLUSTERS[(ctx.rng_next() * CLUSTERS.len() as f64) as usize % CLUSTERS.len()];
            let cells = orient(shape, ctx.rng_next());
            let feet_y = ctx.position().map(|p| p.1.floor() as i64).unwrap_or(64);
            let Some(base) =
                marker_base_near(ctx, (cx.floor() as i64, feet_y, cz.floor() as i64), 2)
            else {
                // Unusable ground here; the next pass draws a fresh cell.
                shake_loose(ctx).await;
                pass += 1;
                continue;
            };
            let blocks: Vec<((i64, i64, i64), String)> = cells
                .iter()
                .map(|&(dx, dy, dz)| {
                    (
                        (base.x as i64 + dx, base.y as i64 + dy, base.z as i64 + dz),
                        material.to_string(),
                    )
                })
                .collect();
            let total = blocks.len();
            let (placed, attempted) = ctx
                .build_blocks(&blocks, |b, _t, _m| {
                    (
                        format!("vis-plot-place-{index}-p{pass}-{b}"),
                        format!("Place plot cluster block {}/{total}.", b + 1),
                        json!({"pass": pass, "block_index": b}),
                    )
                })
                .await;
            // Remember which cells actually hold this bot's material — those are the remodel
            // candidates. Probing (rather than trusting the count) survives partial failures.
            let wanted = material.strip_prefix("minecraft:").unwrap_or(material);
            for ((bx, by, bz), _) in &blocks {
                let cell = BlockPos::new(*bx as i32, *by as i32, *bz as i32);
                if ctx.block_id_at(cell).as_deref() == Some(wanted) {
                    own_cells.push((cell, material.to_string()));
                }
            }
            if own_cells.len() > OWN_CELL_MEMORY {
                let overflow = own_cells.len() - OWN_CELL_MEMORY;
                own_cells.drain(..overflow);
            }
            ctx.record(
                TYPE_PLACE,
                &format!("vis-plot-cluster-{index}-p{pass}"),
                &format!("Plot cluster done: {placed}/{attempted}/{total} blocks."),
                json!({"pass": pass, "material": material, "placed": placed,
                    "attempted": attempted, "expected": total,
                    "cell": {"x": base.x, "y": base.y, "z": base.z}}),
            );
        }

        witness_peer_edits(ctx).await;
        if ctx.rng_next() < 0.3 {
            glance_at_nearest_peer(ctx, None).await;
        }
        // A held-key stroll toward a far corner of the plot every pass: placement is all short
        // pathfinder taps, and round 1 measured the short-press share at exactly the 35% chatter
        // cap — one long open-loop hold per pass keeps the ratio healthy. `hold_visual` cuts
        // itself short if the body stops translating, so walking into an own structure is
        // bounded, and the follow-up look reframes the plot.
        let corner_x = px + PLOT_REACH * if ctx.rng_next() < 0.5 { 1.0 } else { -1.0 };
        let corner_z = pz + PLOT_REACH * if ctx.rng_next() < 0.5 { 1.0 } else { -1.0 };
        let sprint = ctx.rng_next() < ctx.cfg().sprint_bout_prob;
        if let Some(p) = ctx.position() {
            let dist = (corner_x - p.0).hypot(corner_z - p.2);
            if dist > 2.0 {
                ctx.look(
                    (corner_x, p.1 + 1.62, corner_z),
                    ctx.cfg().look_settle_ms.max(400),
                )
                .await;
                let speed = if sprint { 5.6 } else { 4.3 };
                let hold = ((dist / speed * 1000.0) as u64).clamp(600, 2_500);
                ctx.hold_visual(WalkDirection::Forward, sprint, hold, 0)
                    .await;
            }
        }
        let feet = ctx.position().map(|p| p.1).unwrap_or(64.0);
        ctx.look_default((px, feet + 1.0, pz)).await;
        sleep_ms(vary(ctx, 400.0, 1_200.0) as u64).await;
        if ctx.target_met() {
            break;
        }
        pass += 1;
    }
}
