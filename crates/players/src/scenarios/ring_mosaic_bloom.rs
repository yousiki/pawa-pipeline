use super::*;

const TAU: f64 = std::f64::consts::TAU;

/// Materials the mosaic's rings are tiled in. Registry ids WITHOUT the `minecraft:` prefix,
/// exactly as `block_id_at` reports them, because the same list is the fade's "is this a mosaic
/// tile" test. Ring `r` of cycle `c` takes entry `(c + r) % 6`, so every cycle re-tints.
const MATERIALS: [&str; 6] = [
    "stone_bricks",
    "oak_planks",
    "oak_log",
    "cobblestone",
    "bricks",
    "stone",
];

/// Cap on tiles laid or lifted in one act, so a pass is a short readable burst.
const ACT_MAX: usize = 9;

/// Station ring cap keeping every mosaic — plus the tiler's perimeter walk and retreat — inside
/// the 41x41 `build_arena` slab (radius 20 around the anchor).
const MAX_RING: f64 = 15.5;

/// Draw in `[lo, hi)` from the per-bot stream, so a number this scenario could have hard-coded
/// varies per bot and per pass and still replays from the seed.
fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

/// Station radius plus the mosaic pattern radius the capped ring's chord can absorb.
///
/// A tiler works up to `pattern_r + 1.5` blocks off its own centre (the outermost ring plus the
/// stand-off), so two neighbours cost twice that out of the chord.
///
/// ponytail: pattern_r 2 holds the 4.5-block peer gap up to 8 stations and pattern_r 1 to ~10;
/// the recorded tiers are 4 and 8 bots. Beyond that, grow MAX_RING and accept off-slab stations.
fn ring_geometry(bot_count: usize) -> (f64, i32) {
    let n = bot_count.max(2) as f64;
    let radius = station_ring(bot_count, 3.5).min(MAX_RING);
    let chord = 2.0 * radius * (std::f64::consts::PI / n).sin();
    let slack = (chord - MIN_PEER_GAP) / 2.0 - 1.5;
    let pattern_r = if slack >= 2.0 { 2 } else { 1 };
    (radius, pattern_r)
}

/// The cells of concentric square ring `r` around `centre`, ordered walking around the perimeter
/// (by angle), so tiling them reads as a lap of the ring rather than a zig-zag.
fn ring_cells(centre: (i64, i64), r: i32) -> Vec<(i64, i64)> {
    if r == 0 {
        return vec![centre];
    }
    let mut cells: Vec<(i64, i64)> = Vec::new();
    for dx in -r..=r {
        for dz in -r..=r {
            if dx.abs().max(dz.abs()) == r {
                cells.push((centre.0 + dx as i64, centre.1 + dz as i64));
            }
        }
    }
    cells.sort_by(|a, b| {
        let aa = ((a.1 - centre.1) as f64).atan2((a.0 - centre.0) as f64);
        let ab = ((b.1 - centre.1) as f64).atan2((b.0 - centre.0) as f64);
        aa.total_cmp(&ab)
    });
    cells
}

/// The acts of one bloom-and-fade cycle: rings are laid centre-out (the pattern rises out of the
/// ground layer by layer), then lifted in the same order (the OLDEST ring fades first, and for a
/// beat the mosaic is a bare halo before it vanishes too). Ring 2's sixteen cells are split into
/// two half-laps to respect [`ACT_MAX`].
#[derive(Clone)]
struct Act {
    ring: i32,
    erase: bool,
    cells: Vec<(i64, i64)>,
}

fn cycle_acts(centre: (i64, i64), pattern_r: i32) -> Vec<Act> {
    let mut lays: Vec<Act> = Vec::new();
    for r in 0..=pattern_r {
        let cells = ring_cells(centre, r);
        if r == 1 {
            // Ring 1 rides with the centre cell: nine tiles, one act.
            let mut joined = ring_cells(centre, 0);
            joined.extend(cells);
            lays.pop();
            lays.push(Act {
                ring: 1,
                erase: false,
                cells: joined,
            });
        } else if cells.len() > ACT_MAX {
            let half = cells.len() / 2;
            lays.push(Act {
                ring: r,
                erase: false,
                cells: cells[..half].to_vec(),
            });
            lays.push(Act {
                ring: r,
                erase: false,
                cells: cells[half..].to_vec(),
            });
        } else {
            lays.push(Act {
                ring: r,
                erase: false,
                cells,
            });
        }
    }
    let mut acts = lays.clone();
    acts.extend(lays.into_iter().map(|act| Act { erase: true, ..act }));
    acts
}

/// Ground cell a tile goes into on this column: the highest air-over-solid cell in a small band
/// around `ref_y`. `None` when the column is sheer or unloaded — the tile is skipped honestly
/// instead of grinding a doomed placement.
fn tile_cell(ctx: &Ctx, x: i64, z: i64, ref_y: i64) -> Option<BlockPos> {
    for y in ((ref_y - 3)..=(ref_y + 3)).rev() {
        let pos = BlockPos::new(x as i32, y as i32, z as i32);
        if block_is_air(ctx, pos) && block_is_safe_support(ctx, pos.down(1)) {
            return Some(pos);
        }
    }
    None
}

/// Whether the cell holds a tile some cycle laid. The fade digs nothing else, so the arena floor
/// is never opened.
fn is_mosaic_tile(ctx: &Ctx, cell: BlockPos) -> bool {
    matches!(ctx.block_id_at(cell).as_deref(), Some(id) if MATERIALS.contains(&id))
}

/// One interlude between acts: back off to a drawn viewing distance, read the half-built pattern,
/// spend the world-synchronization beats, then stroll a tangential leg so the loop never idles on
/// one spot.
async fn interlude(ctx: &mut Ctx, centre: (f64, f64), floor_y: f64) {
    let timeout = ctx.cfg().move_timeout_ms.min(6_000);
    let ang = ctx.rng_next() * TAU;
    let dist = vary(ctx, 4.5, 7.0);
    ctx.move_to_watching_point(
        centre.0 + ang.cos() * dist,
        centre.1 + ang.sin() * dist,
        (centre.0, floor_y + 0.5, centre.1),
        timeout,
        1.8,
    )
    .await;
    sleep_ms(vary(ctx, 600.0, 1_500.0) as u64).await;
    witness_peer_edits(ctx).await;
    glance_at_nearest_peer(ctx, None).await;
    // Tangential stroll: real walk keys between placement bursts.
    if let Some(p) = ctx.position() {
        let settle = ctx.cfg().look_settle_ms.max(400);
        let side = if ctx.rng_next() < 0.5 { 1.0 } else { -1.0 };
        ctx.look(
            (
                p.0 - ang.sin() * 16.0 * side,
                p.1 + 1.3,
                p.2 + ang.cos() * 16.0 * side,
            ),
            settle,
        )
        .await;
        let sprint = ctx.rng_next() < ctx.cfg().sprint_bout_prob;
        let hold = vary(ctx, 1_200.0, 2_600.0) as u64;
        ctx.hold_visual(WalkDirection::Forward, sprint, hold, 300)
            .await;
    }
}

/// Every bot tiles a flat mosaic of concentric square rings into the ground at its own station:
/// the pattern BLOOMS ring by ring out of the floor (each ring its own material and its own lap
/// of the perimeter), stands complete for a beat, then FADES in laying order — oldest ring lifted
/// first — until the ground is bare and the next cycle re-blooms it re-centred and re-tinted.
/// The subject is the edit rhythm itself: a ground figure that emerges layer by layer and is
/// unwritten the same way, witnessed from every station around the arena. Zero mobs; tiles sit ON
/// the arena floor and only tiles are ever dug, so the ground plane stays intact and lit.
pub(super) async fn run(ctx: &mut Ctx) {
    let index = ctx.index();
    let n = ctx.player_count().max(1);
    let anchor = ctx.anchor();
    let (radius, pattern_r) = ring_geometry(n);
    let ang = index as f64 / n as f64 * TAU;
    let station = (anchor.0 + ang.cos() * radius, anchor.1 + ang.sin() * radius);
    let timeout = ctx.cfg().move_timeout_ms.min(8_000);
    ctx.record(
        TYPE_LOOK,
        &format!("vis-mosaic-init-{index}"),
        "Ring mosaic bloom: tile a concentric ground pattern ring by ring, then lift it oldest ring first.",
        json!({"station": {"x": r2(station.0), "z": r2(station.1)}, "pattern_r": pattern_r,
            "ring_radius": r2(radius), "materials": MATERIALS}),
    );
    ctx.move_to(station.0, station.1, ctx.cfg().move_timeout_ms, 2.0)
        .await;
    let mut cycle = 0usize;
    'cycles: while !ctx.target_met() {
        // Re-centre each cycle: a small drawn slide keeps successive blooms from pixel-exact
        // overwrites without leaving the station's chord slack.
        let centre = (
            (station.0 + vary(ctx, -1.5, 1.5)).floor() as i64,
            (station.1 + vary(ctx, -1.5, 1.5)).floor() as i64,
        );
        let centre_f = (centre.0 as f64 + 0.5, centre.1 as f64 + 0.5);
        let floor_y = ctx.position().map(|p| p.1).unwrap_or(64.0).floor() as i64;
        // Standing tiles per ring, pushed by the lay acts and drained by the fades.
        let mut laid: Vec<Vec<BlockPos>> = vec![Vec::new(); (pattern_r + 1) as usize];
        for (a, act) in cycle_acts(centre, pattern_r).into_iter().enumerate() {
            if ctx.target_met() {
                break 'cycles;
            }
            let material = MATERIALS[(cycle + act.ring as usize) % MATERIALS.len()];
            let mut done = 0usize;
            if act.erase {
                ctx.equip("diamond_pickaxe").await;
                // Drain this ring's remembered tiles that fall inside this act's cells.
                let ledger = &mut laid[act.ring as usize];
                let (mine, keep): (Vec<BlockPos>, Vec<BlockPos>) = ledger
                    .drain(..)
                    .partition(|cell| act.cells.contains(&(cell.x as i64, cell.z as i64)));
                *ledger = keep;
                for cell in mine {
                    if ctx.target_met() {
                        break;
                    }
                    if !is_mosaic_tile(ctx, cell) {
                        continue;
                    }
                    if !ctx.target_in_reach(cell) {
                        ctx.move_to(cell.x as f64 + 0.5, cell.z as f64 + 0.5, timeout, 2.0)
                            .await;
                    }
                    if ctx.dig(cell).await {
                        done += 1;
                    }
                    ctx.record(
                        TYPE_MINE,
                        &format!("vis-mosaic-lift-{index}-c{cycle}-a{a}-{done}"),
                        &format!(
                            "Fade: lift a ring-{} tile back out of the mosaic.",
                            act.ring
                        ),
                        json!({"cycle": cycle, "ring": act.ring,
                            "block_position": {"x": cell.x, "y": cell.y, "z": cell.z}}),
                    );
                }
            } else {
                let full = format!("minecraft:{material}");
                if !ctx.equip(&full).await {
                    continue;
                }
                for (i, &(x, z)) in act.cells.iter().enumerate() {
                    if ctx.target_met() {
                        break;
                    }
                    let Some(cell) = tile_cell(ctx, x, z, floor_y) else {
                        continue;
                    };
                    // Never tile the cell under our own feet — vanilla rejects it and three
                    // retries grind for nothing. Step one cell outward first.
                    if let Some(p) = ctx.position()
                        && (p.0.floor() as i64, p.2.floor() as i64) == (x, z)
                    {
                        let out = ((z - centre.1) as f64).atan2((x - centre.0) as f64);
                        ctx.move_to(
                            x as f64 + 0.5 + out.cos() * 1.6,
                            z as f64 + 0.5 + out.sin() * 1.6,
                            timeout,
                            1.5,
                        )
                        .await;
                    }
                    if !ctx.target_in_reach(cell) {
                        ctx.move_to(cell.x as f64 + 0.5, cell.z as f64 + 0.5, timeout, 2.0)
                            .await;
                    }
                    let ok = ctx.place_block(cell, &full).await;
                    ctx.record(
                        TYPE_PLACE,
                        &format!("vis-mosaic-tile-{index}-c{cycle}-a{a}-{i}"),
                        &format!(
                            "Bloom: tile {}/{} of mosaic ring {}.",
                            i + 1,
                            act.cells.len(),
                            act.ring
                        ),
                        json!({"cycle": cycle, "ring": act.ring, "placed": ok, "block": full,
                            "block_position": {"x": cell.x, "y": cell.y, "z": cell.z}}),
                    );
                    if ok {
                        laid[act.ring as usize].push(cell);
                        done += 1;
                    }
                }
            }
            ctx.record(
                TYPE_MOVE,
                &format!("vis-mosaic-act-{index}-c{cycle}-a{a}"),
                &format!(
                    "Mosaic act {}: {} {} tiles of ring {}.",
                    a,
                    if act.erase { "lifted" } else { "laid" },
                    done,
                    act.ring
                ),
                json!({"cycle": cycle, "act": a, "ring": act.ring, "erase": act.erase,
                    "tiles": done, "centre": {"x": centre.0, "z": centre.1}}),
            );
            interlude(ctx, centre_f, floor_y as f64).await;
        }
        cycle += 1;
    }
}
