use super::*;

const TAU: f64 = std::f64::consts::TAU;

/// Compact shapes a pass may raise on a plot — the small end of the `pattern_blocks` vocabulary,
/// so one build is a short placement burst (3–7 blocks) and a raze never faces a fortress.
const KINDS: [&str; 3] = ["pillar", "cross", "wall"];

/// Materials a pass may build in, drawn per pass so successive occupants of the same plot leave
/// visibly different structures. Registry ids WITHOUT the `minecraft:` prefix, exactly as
/// `block_id_at` reports them, because the same list is the razer's "did a bot lay this" test.
const MATERIALS: [&str; 6] = [
    "stone_bricks",
    "oak_planks",
    "oak_log",
    "cobblestone",
    "bricks",
    "stone",
];

/// How far sideways off the plot centre a build may land, so a rebuilt plot is not a pixel-exact
/// overwrite of the razed one. Absorbed by the ring geometry below.
const PLOT_WANDER: f64 = 1.5;

/// Half-extent of the raze scan box around the plot centre: covers the wander plus the widest
/// shape's footprint (wall/cross reach 1 block off their origin).
const RAZE_REACH: i32 = 3;

/// Blocks a raze may take down in one visit. A cap, not a target: the predecessor's shape is
/// 3–7 blocks, so one visit normally clears it whole, and a plot that somehow accreted more is
/// worn down across visits instead of one bot grinding a quarry.
const RAZE_MAX: usize = 8;

/// Station ring cap keeping every plot — plus its wander and retreat — inside the 41x41
/// `build_arena` slab (radius 20 around the anchor).
const MAX_RING: f64 = 15.5;

/// Draw in `[lo, hi)` from the per-bot stream, so a number this scenario could have hard-coded
/// varies per bot and per pass and still replays from the seed.
fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

/// Uniform draw in `0..n` from the per-bot stream.
fn pick(ctx: &mut Ctx, n: usize) -> usize {
    ((ctx.rng_next() * n as f64) as usize).min(n.saturating_sub(1))
}

/// Plot ring radius plus the wander it can absorb, recomputed from the capped radius' actual
/// chord so the 4.5-block peer gap survives whatever `bot_count` does to the ring.
fn ring_geometry(bot_count: usize) -> (f64, f64) {
    let n = bot_count.max(2) as f64;
    let radius = station_ring(bot_count, PLOT_WANDER).min(MAX_RING);
    let chord = 2.0 * radius * (std::f64::consts::PI / n).sin();
    let wander = ((chord - MIN_PEER_GAP) / 2.0).clamp(0.0, PLOT_WANDER);
    (radius, wander)
}

/// Whether the cell currently holds a block some pass could have laid. The razer digs nothing
/// else, so the arena floor is never opened and nothing ever darkens.
fn is_plot_block(ctx: &Ctx, cell: BlockPos) -> bool {
    matches!(ctx.block_id_at(cell).as_deref(), Some(id) if MATERIALS.contains(&id))
}

/// 2D distance from `(x, z)` to the nearest live peer.
fn nearest_peer_dist(ctx: &Ctx, x: f64, z: f64) -> f64 {
    (0..ctx.player_count())
        .filter(|&j| j != ctx.index())
        .filter_map(|j| ctx.peer_position(j))
        .map(|p| (p.0 - x).hypot(p.2 - z))
        .fold(f64::INFINITY, f64::min)
}

/// Nearest live peer's index on this shard's roster, for camera-on-peer travel legs.
fn nearest_peer(ctx: &Ctx) -> Option<usize> {
    let own = ctx.position()?;
    (0..ctx.player_count())
        .filter(|&j| j != ctx.index())
        .filter_map(|j| {
            ctx.peer_position(j)
                .map(|p| (j, (p.0 - own.0).powi(2) + (p.2 - own.2).powi(2)))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(j, _)| j)
}

/// Ground level of the site column: the highest air-over-solid cell in a band around `ref_y`.
/// Falls back to `ref_y` when the column is sheer or unloaded.
fn site_ground_y(ctx: &Ctx, sx: i64, sz: i64, ref_y: i64) -> i64 {
    for y in ((ref_y - 8)..=(ref_y + 12)).rev() {
        let pos = BlockPos::new(sx as i32, y as i32, sz as i32);
        if block_is_air(ctx, pos) && block_is_safe_support(ctx, pos.down(1)) {
            return y;
        }
    }
    ref_y
}

/// Walk the ring chord to the next plot: long open-loop bearing holds with a drawn veer, an
/// occasional leg spent watching the nearest peer, then one closing pathfinder correction. Long
/// holds on purpose — hop-wise travel here reads as key chatter (first-batch lesson).
async fn migrate(ctx: &mut Ctx, target: (f64, f64), timeout: u64) {
    let settle = ctx.cfg().look_settle_ms.max(400);
    for _leg in 0..3 {
        if ctx.target_met() {
            return;
        }
        let Some(p) = ctx.position() else { return };
        let dist = (target.0 - p.0).hypot(target.1 - p.2);
        if dist < 3.5 {
            break;
        }
        if ctx.rng_next() < 0.3 {
            // One social leg: close part of the chord with the camera pinned on a peer, so the
            // gaze decouples from the travel bearing.
            if let Some(peer) = nearest_peer(ctx) {
                let frac = 0.4 + ctx.rng_next() * 0.3;
                ctx.move_to_watching_peer(
                    p.0 + (target.0 - p.0) * frac,
                    p.2 + (target.1 - p.2) * frac,
                    peer,
                    timeout,
                    2.0,
                )
                .await;
                continue;
            }
        }
        let veer = (ctx.rng_next() - 0.5) * 0.6;
        let (ux, uz) = ((target.0 - p.0) / dist, (target.1 - p.2) / dist);
        let (dx, dz) = (
            ux * veer.cos() - uz * veer.sin(),
            ux * veer.sin() + uz * veer.cos(),
        );
        ctx.look((p.0 + dx * 24.0, p.1 + 1.3, p.2 + dz * 24.0), settle)
            .await;
        let sprint = ctx.rng_next() < ctx.cfg().sprint_bout_prob;
        let speed = if sprint { 5.6 } else { 4.3 };
        let hold = ((dist.min(14.0) / speed * 1000.0) as u64).clamp(900, 3_200);
        ctx.hold_visual(WalkDirection::Forward, sprint, hold, 300)
            .await;
    }
    // A closing correction from a couple of blocks out is a sub-half-second key press, and at
    // eight bots those pile up into key chatter (measured: p07 38.9% on the 8-bot round).
    // Close enough is close enough — the work loop re-navigates per cell anyway.
    if ctx
        .position()
        .map(|p| (target.0 - p.0).hypot(target.1 - p.2) > 2.2)
        .unwrap_or(false)
    {
        ctx.move_to(target.0, target.1, timeout, 1.8).await;
    }
}

/// Every standing plot block in the scan box, tallest first, so the raze pulls a structure down
/// from its top the way it went up from its base and never mines out a supported cell's footing
/// while standing on it.
fn standing_plot_blocks(ctx: &Ctx, centre: (i64, i64), floor_y: i64) -> Vec<BlockPos> {
    let mut cells = Vec::new();
    for dx in -RAZE_REACH..=RAZE_REACH {
        for dz in -RAZE_REACH..=RAZE_REACH {
            for y in floor_y..=(floor_y + 5) {
                let cell = BlockPos::new(centre.0 as i32 + dx, y as i32, centre.1 as i32 + dz);
                if is_plot_block(ctx, cell) {
                    cells.push(cell);
                }
            }
        }
    }
    cells.sort_by_key(|c| std::cmp::Reverse(c.y));
    cells
}

/// Tear down whatever the previous occupant left on this plot: dig its kit blocks top-down,
/// stepping into reach per cell, under the [`RAZE_MAX`] cap. Returns how many came down.
async fn raze(ctx: &mut Ctx, centre: (i64, i64), floor_y: i64, pass: usize) -> usize {
    let cells = standing_plot_blocks(ctx, centre, floor_y);
    if cells.is_empty() {
        return 0;
    }
    let index = ctx.index();
    let timeout = ctx.cfg().move_timeout_ms.min(6_000);
    let reach = (ctx.cfg().place_reach - 1.0).max(1.5);
    ctx.equip("diamond_pickaxe").await;
    let mut razed = 0usize;
    for cell in cells.into_iter().take(RAZE_MAX) {
        if ctx.target_met() {
            break;
        }
        // The scan ran before the first swing; a cell whose tower already collapsed is air now.
        if !is_plot_block(ctx, cell) {
            continue;
        }
        if !ctx.target_in_reach(cell) {
            ctx.move_to(cell.x as f64 + 0.5, cell.z as f64 + 0.5, timeout, reach)
                .await;
        }
        let dug = ctx.dig(cell).await;
        if dug {
            razed += 1;
        }
        ctx.record(
            TYPE_MINE,
            &format!("vis-raze-{index}-p{pass}-{razed}"),
            "Raze: pull down one block of the previous occupant's structure.",
            json!({"pass": pass, "dug": dug,
                "block_position": {"x": cell.x, "y": cell.y, "z": cell.z}}),
        );
    }
    razed
}

/// N plots on a ring, and every pass the whole crew rotates one plot around it: each bot walks the
/// chord to its neighbour's plot, RAZES the structure that neighbour just built there, and raises
/// its own small shape on the cleared ground — so every structure in the world is built, briefly
/// witnessed, and torn down by a different hand, over and over. The build–demolish alternation is
/// the point: the corpus gets both edit directions on the same cells from multiple viewpoints.
/// Zero mobs; only kit blocks are placed and only kit blocks are ever dug, so the arena floor
/// stays intact and lit. No barriers: passes are self-paced, and a plot still occupied by a
/// straggler is strolled past (rim walk + witness) instead of queued on.
pub(super) async fn run(ctx: &mut Ctx) {
    let index = ctx.index();
    let n = ctx.player_count().max(1);
    let anchor = ctx.anchor();
    let (radius, wander) = ring_geometry(n);
    let timeout = ctx.cfg().move_timeout_ms.min(8_000);
    let settle = ctx.cfg().look_settle_ms.max(400);
    ctx.record(
        TYPE_LOOK,
        &format!("vis-swapraze-init-{index}"),
        "Plot swap-raze: rotate around the plot ring, tearing down the predecessor's structure and building anew.",
        json!({"plots": n, "ring_radius": r2(radius), "wander": r2(wander),
            "kinds": KINDS, "materials": MATERIALS}),
    );
    let mut pass = 0usize;
    while !ctx.target_met() {
        let plot = (index + pass) % n;
        let ang = plot as f64 / n as f64 * TAU;
        let (rx, rz) = (ang.cos(), ang.sin());
        let (px, pz) = (anchor.0 + rx * radius, anchor.1 + rz * radius);
        // Approach on the inward side; the ring interior is always open floor.
        migrate(ctx, (px - rx * 3.0, pz - rz * 3.0), timeout).await;

        // A straggler still packing up here: stroll the rim tangentially and spend the beat
        // witnessing instead of shoving into their face. The plot comes back around next pass.
        if nearest_peer_dist(ctx, px, pz) < MIN_PEER_GAP {
            if let Some(p) = ctx.position() {
                let side = if ctx.rng_next() < 0.5 { 1.0 } else { -1.0 };
                ctx.look(
                    (p.0 - rz * 16.0 * side, p.1 + 1.3, p.2 + rx * 16.0 * side),
                    settle,
                )
                .await;
                ctx.hold_visual(WalkDirection::Forward, false, 1_600, 300)
                    .await;
            }
            witness_peer_edits(ctx).await;
            pass += 1;
            continue;
        }

        let floor_y = ctx.position().map(|p| p.1).unwrap_or(64.0).floor() as i64;
        let razed = raze(ctx, (px.floor() as i64, pz.floor() as i64), floor_y, pass).await;

        // Rebuild on the cleared ground: a fresh shape in a fresh material, slid off the exact
        // centre by the wander so the new structure is not a pixel-exact overwrite.
        let kind = KINDS[pick(ctx, KINDS.len())];
        let material = format!("minecraft:{}", MATERIALS[pick(ctx, MATERIALS.len())]);
        let sx = (px + vary(ctx, -wander, wander)).floor() as i64;
        let sz = (pz + vary(ctx, -wander, wander)).floor() as i64;
        let base_y = site_ground_y(ctx, sx, sz, floor_y);
        // Stand off the origin so no cell overlaps the placer's hitbox — but only when the body
        // is not already at a workable stand-off: a 1–2-block correction is exactly the short
        // key press the 8-bot chatter gate measures.
        let own_gap = ctx
            .position()
            .map(|p| (sx as f64 + 0.5 - p.0).hypot(sz as f64 + 0.5 - p.2))
            .unwrap_or(0.0);
        if !(1.6..=3.2).contains(&own_gap) {
            let stand = ctx.rng_next() * TAU;
            ctx.move_to(
                sx as f64 + 0.5 + stand.cos() * 2.4,
                sz as f64 + 0.5 + stand.sin() * 2.4,
                timeout,
                1.5,
            )
            .await;
        }
        let total = pattern_blocks(kind, sx, base_y, sz).len();
        let blocks: Vec<((i64, i64, i64), String)> = pattern_blocks(kind, sx, base_y, sz)
            .into_iter()
            .map(|b| (b, material.clone()))
            .collect();
        let (placed, attempted) = ctx
            .build_blocks(&blocks, |b, _t, _m| {
                (
                    format!("vis-swapraze-place-{index}-p{pass}-{b}"),
                    format!("Rebuild the razed plot: {kind} block {}/{total}.", b + 1),
                    json!({"pass": pass, "kind": kind, "block_index": b}),
                )
            })
            .await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-swapraze-plot-{index}-p{pass}"),
            &format!(
                "Plot {plot} turned over: razed {razed} predecessor blocks, \
                 rebuilt a {kind} ({placed}/{attempted}/{total} placed/attempted/expected)."
            ),
            json!({"pass": pass, "plot": plot, "razed": razed, "kind": kind,
                "material": material, "placed": placed, "attempted": attempted,
                "expected": total, "site": {"x": sx, "z": sz}}),
        );

        // Retreat inward under one LONG open-loop hold (not a pathfinder press — the first-batch
        // chatter fix), read the fresh structure, then spend the loop boundary on the
        // world-synchronization beats.
        let retreat = vary(ctx, 4.5, 6.5);
        if let Some(p) = ctx.position() {
            let (tx, tz) = (px - rx * retreat, pz - rz * retreat);
            let d = (tx - p.0).hypot(tz - p.2);
            ctx.look(
                (p.0 + (tx - p.0) * 8.0, p.1 + 1.3, p.2 + (tz - p.2) * 8.0),
                settle,
            )
            .await;
            let hold = ((d / 4.3 * 1000.0) as u64).clamp(1_100, 2_800);
            ctx.hold_visual(WalkDirection::Forward, false, hold, 0)
                .await;
        }
        ctx.look_default((sx as f64 + 0.5, base_y as f64 + 1.2, sz as f64 + 0.5))
            .await;
        sleep_ms(vary(ctx, 700.0, 1_800.0) as u64).await;
        witness_peer_edits(ctx).await;
        glance_at_nearest_peer(ctx, None).await;
        pass += 1;
    }
}
