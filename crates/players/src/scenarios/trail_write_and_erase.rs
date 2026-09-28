use super::*;

/// Trail materials — the kit's placeable blocks, drawn per placement so one bot's trail is itself
/// multicoloured. Registry ids WITHOUT the `minecraft:` prefix, exactly as `block_id_at` reports
/// them, because the same list is the eraser's "is this a trail block" test.
const MATERIALS: [&str; 6] = [
    "stone_bricks",
    "oak_planks",
    "oak_log",
    "cobblestone",
    "bricks",
    "stone",
];

/// Wander annulus around the anchor. The floor keeps two adjacent sectors' waypoints clear of the
/// peer gap even at eight bots (chord at radius 7 over 45 deg is 5.4 blocks); the ceiling keeps
/// every leg inside the 41x41 `build_arena`.
const LEASH_MIN: f64 = 7.0;
const LEASH_MAX: f64 = 16.0;

/// How far the eraser will travel for a peer's trail block.
const ERASE_MAX_DISTANCE: f64 = 24.0;

/// Own trail cells remembered for the revisit-scrub roll; older cells stay in the world for good.
const TRAIL_MEMORY: usize = 24;

/// The forward-ish key identities a scribble leg may hold, so the walk key and the gaze direction
/// decouple by up to 45 deg without walking away from the waypoint.
const LEG_KEYS: [WalkDirection; 3] = [
    WalkDirection::Forward,
    WalkDirection::ForwardLeft,
    WalkDirection::ForwardRight,
];

/// Draw in `[lo, hi)` from the per-bot stream, so a number this scenario could have hard-coded
/// varies per bot and per pass and still replays from the seed.
fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

/// Whether the cell currently holds a block a trail writer could have placed. The eraser digs
/// nothing else, so the ground plane is never opened and nothing ever darkens.
fn is_trail_block(ctx: &Ctx, cell: BlockPos) -> bool {
    matches!(ctx.block_id_at(cell).as_deref(), Some(id) if MATERIALS.contains(&id))
}

fn nearest_peer(ctx: &Ctx) -> Option<(usize, f64)> {
    let own = ctx.position()?;
    (0..ctx.player_count())
        .filter(|&j| j != ctx.index())
        .filter_map(|j| {
            ctx.peer_position(j)
                .map(|p| (j, (p.0 - own.0).hypot(p.2 - own.2)))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
}

/// True when `cell` is at least the peer gap away from every live peer, so erasing never walks the
/// eraser into a working bot's face.
fn cell_clear_of_peers(ctx: &Ctx, cell: BlockPos) -> bool {
    (0..ctx.player_count())
        .filter(|&j| j != ctx.index())
        .filter_map(|j| ctx.peer_position(j))
        .all(|p| (p.0 - cell.x as f64).hypot(p.2 - cell.z as f64) >= MIN_PEER_GAP)
}

/// Lay `stack` trail blocks (1 = a stone, 2 = a short cairn) on the found base, recording each
/// real placement and remembering the cells for the revisit-scrub roll.
async fn place_trail(
    ctx: &mut Ctx,
    desired: (i64, i64, i64),
    material: &str,
    stack: i64,
    round: u32,
    trail: &mut Vec<BlockPos>,
) -> u64 {
    let index = ctx.index();
    let Some(base) = marker_base_near(ctx, desired, stack) else {
        return 0;
    };
    if !ctx.equip(material).await {
        return 0;
    }
    let mut placed = 0u64;
    for layer in 0..stack {
        let cell = base.up(layer as i32);
        let ok = ctx.place_block(cell, material).await;
        ctx.record(
            TYPE_PLACE,
            &format!("vis-trail-place-{index}-r{round}-{layer}"),
            &format!("Scribble trail block (layer {} of {stack}).", layer + 1),
            json!({"round": round, "placed": ok, "block": material, "layer": layer,
                "block_position": {"x": cell.x, "y": cell.y, "z": cell.z}}),
        );
        if !ok {
            break;
        }
        placed += 1;
        trail.push(cell);
    }
    placed
}

/// Every bot snakes around its own sector of the arena leaving a multicoloured block trail behind
/// it — and randomly turns eraser: it reads the swarm's shared edit log, hunts down the newest
/// peer trail block in range, and mines it back out. A bot also revisits and scrubs cells of its
/// own trail, so the world is continuously written, witnessed, and unwritten from every viewpoint.
/// Deliberately meaningless and dense: the point is the distribution of walk keys, camera moves,
/// placements, and removals — not a structure a viewer could name. Zero mobs; only kit blocks are
/// ever placed, and only kit blocks are ever dug, so the ground plane stays intact and lit.
pub(crate) async fn run(ctx: &mut Ctx) {
    let index = ctx.index();
    let bot_count = ctx.player_count().max(1);
    let anchor = ctx.anchor();
    let sprint_prob = ctx.cfg().sprint_bout_prob;
    let sector = index as f64 / bot_count as f64 * std::f64::consts::TAU;
    let mut trail: Vec<BlockPos> = Vec::new();
    let mut round = 0u32;
    let mut stalled = false;
    ctx.record(
        TYPE_LOOK,
        &format!("vis-trail-init-{index}"),
        "Trail writer: scribble a block trail around an own sector; randomly erase peers' and own trail blocks.",
        json!({"sector_deg": r2(sector.to_degrees()), "leash": [LEASH_MIN, LEASH_MAX],
            "materials": MATERIALS}),
    );
    while !ctx.target_met() {
        // --- one wander leg toward a waypoint in this bot's (slowly rotating) sector -----------
        let leg_start = ctx.position();
        let mut bearing = sector + vary(ctx, -0.55, 0.55) + round as f64 * 0.21;
        let mut radius = vary(ctx, LEASH_MIN, LEASH_MAX);
        // A peer inside the gap overrides the draw: spend this leg opening distance instead.
        let crowded = nearest_peer(ctx).filter(|(_, gap)| *gap < MIN_PEER_GAP + 0.5);
        if let (Some((peer, _)), Some(own)) = (crowded, leg_start)
            && let Some(p) = ctx.peer_position(peer)
        {
            let (ux, uz) = away_unit(Some(own), p);
            bearing = uz.atan2(ux);
            radius = ((own.0 + ux * 7.0 - anchor.0).hypot(own.2 + uz * 7.0 - anchor.1))
                .clamp(LEASH_MIN, LEASH_MAX);
        }
        let wx = anchor.0 + bearing.cos() * radius;
        let wz = anchor.1 + bearing.sin() * radius;
        let timeout = ctx.cfg().move_timeout_ms.min(8_000);
        let open_loop = !stalled && ctx.rng_next() < 0.45;
        if open_loop {
            // Face the waypoint, then hold a drawn forward-ish key with an optional jump cadence.
            if let Some(p) = ctx.position() {
                ctx.look((wx, p.1 + 1.4, wz), ctx.cfg().look_settle_ms.max(500))
                    .await;
            }
            let dir = LEG_KEYS[(ctx.rng_next() * LEG_KEYS.len() as f64) as usize % LEG_KEYS.len()];
            let sprint = ctx.rng_next() < sprint_prob;
            let jump_period = if ctx.rng_next() < 0.3 {
                vary(ctx, 700.0, 2_000.0) as u64
            } else {
                0
            };
            let hold = vary(ctx, 1_400.0, 3_600.0) as u64;
            stalled = !ctx.hold_visual(dir, sprint, hold, jump_period).await;
        } else {
            // Pathfinder leg — also the recovery from an open-loop leg that walked into something.
            ctx.move_to(wx, wz, timeout, 2.0).await;
            stalled = false;
        }

        // --- scribble: drop 1-2 trail blocks a couple of cells back along the walked line -------
        if ctx.rng_next() < 0.85
            && let (Some(a), Some(b)) = (leg_start, ctx.position())
        {
            let (dx, dz) = (b.0 - a.0, b.2 - a.2);
            let walked = dx.hypot(dz);
            if walked >= 2.0 {
                let (ux, uz) = (dx / walked, dz / walked);
                let desired = (
                    (b.0 - ux * 2.5).floor() as i64,
                    b.1.floor() as i64,
                    (b.2 - uz * 2.5).floor() as i64,
                );
                let stack = if ctx.rng_next() < 0.3 { 2 } else { 1 };
                let material =
                    MATERIALS[(ctx.rng_next() * MATERIALS.len() as f64) as usize % MATERIALS.len()];
                place_trail(ctx, desired, material, stack, round, &mut trail).await;
            }
        }

        // --- eraser roll: hunt down the newest ripe PEER edit and mine it back out --------------
        if ctx.rng_next() < 0.4 {
            let min_age = Duration::from_millis(1_500 + (ctx.rng_next() * 4_000.0) as u64);
            if let Some(edit) = ctx.take_witnessable_edit(min_age, ERASE_MAX_DISTANCE) {
                let cell = BlockPos::new(edit.x as i32, edit.y as i32, edit.z as i32);
                if cell_clear_of_peers(ctx, cell) {
                    travel_to(
                        ctx,
                        (edit.x as f64 + 0.5, edit.z as f64 + 0.5),
                        TRAVEL_HOP,
                        2.5,
                    )
                    .await;
                    let mut scrubbed = 0u32;
                    if is_trail_block(ctx, cell) {
                        let equipped = ctx.equip("diamond_pickaxe").await;
                        if equipped && ctx.dig(cell).await {
                            scrubbed += 1;
                        }
                        // A cairn's second stone comes down with the first.
                        if is_trail_block(ctx, cell.up(1)) && ctx.dig(cell.up(1)).await {
                            scrubbed += 1;
                        }
                    }
                    ctx.record(
                        TYPE_MINE,
                        &format!("vis-trail-erase-{index}-r{round}"),
                        &format!("Erase peer bot {}'s trail block.", edit.by),
                        json!({"round": round, "editor": edit.by, "scrubbed": scrubbed,
                            "block_position": {"x": cell.x, "y": cell.y, "z": cell.z}}),
                    );
                }
            }
        }

        // --- revisit-scrub roll: walk back and unwrite a cell of this bot's OWN trail -----------
        if trail.len() >= 6 && ctx.rng_next() < 0.25 {
            let pick = (ctx.rng_next() * trail.len() as f64) as usize % trail.len();
            let cell = trail.swap_remove(pick);
            travel_to(
                ctx,
                (cell.x as f64 + 0.5, cell.z as f64 + 0.5),
                TRAVEL_HOP,
                2.5,
            )
            .await;
            let scrubbed = if is_trail_block(ctx, cell) {
                ctx.equip("diamond_pickaxe").await && ctx.dig(cell).await
            } else {
                false
            };
            ctx.record(
                TYPE_MINE,
                &format!("vis-trail-scrub-{index}-r{round}"),
                "Revisit and scrub a cell of this bot's own trail.",
                json!({"round": round, "scrubbed": scrubbed,
                    "block_position": {"x": cell.x, "y": cell.y, "z": cell.z}}),
            );
        }
        if trail.len() > TRAIL_MEMORY {
            let excess = trail.len() - TRAIL_MEMORY;
            trail.drain(0..excess);
        }

        // --- camera roll + the world-synchronization beat ---------------------------------------
        if ctx.rng_next() < 0.3 {
            let dyaw = vary(ctx, -140.0, 140.0);
            let dpitch = vary(ctx, -25.0, 25.0);
            let rate = ctx.cfg().yaw_rate_deg_s.max(1.0);
            let settle = (dyaw.abs() / rate * 1000.0) as u64 + 600;
            ctx.drag_view(dyaw, dpitch, settle).await;
        }
        witness_peer_edits(ctx).await;
        round += 1;
    }
}
