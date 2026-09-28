use super::*;

const TAU: f64 = std::f64::consts::TAU;

/// Kit materials a visiting hand may lay — one is drawn per visit, so a site's accreted layers
/// read as the sequence of hands that worked it.
const MATERIALS: [&str; 6] = [
    "minecraft:stone_bricks",
    "minecraft:oak_planks",
    "minecraft:oak_log",
    "minecraft:cobblestone",
    "minecraft:bricks",
    "minecraft:stone",
];

/// Whether `id` (as `block_id_at` reports it, no `minecraft:` prefix) is a block some builder
/// hand could have laid. The amend beat digs nothing else, so the arena's smooth_stone floor is
/// never opened and nothing ever darkens.
fn is_kit_block(id: &str) -> bool {
    matches!(
        id,
        "stone_bricks" | "oak_planks" | "oak_log" | "cobblestone" | "bricks" | "stone"
    )
}

/// How far a work cell may wander off its station centre. `ring_geometry` shrinks this at swarm
/// sizes where the capped ring's chord gets tight.
const SITE_WANDER: f64 = 2.5;

/// Station ring cap keeping every site plus its wander and retreat inside the 41x41 build_arena
/// slab (radius 20 around the anchor).
///
/// ponytail: the capped ring holds the 4.5-block peer gap up to ~21 stations; the recorded tiers
/// are 4 and 8 bots. Beyond that, drop the cap and accept off-slab sites.
const MAX_RING: f64 = 15.5;

/// A site column stops stacking this high above the visit's floor; further motifs land on other
/// columns instead, spreading the structure sideways and keeping every cell placeable from the
/// ground.
const MAX_RISE: i64 = 5;

/// Draw in `[lo, hi)` from the per-bot stream, so a number this scenario could have hard-coded
/// varies per bot and per visit and still replays from the seed.
fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

/// Uniform draw in `0..n` from the per-bot stream.
fn pick(ctx: &mut Ctx, n: usize) -> usize {
    ((ctx.rng_next() * n as f64) as usize).min(n.saturating_sub(1))
}

/// Station ring radius plus the per-cell wander it can absorb. The wander is recomputed from the
/// capped radius' actual chord, so the 4.5-block peer gap survives whatever `bot_count` does to
/// the ring.
fn ring_geometry(bot_count: usize) -> (f64, f64) {
    let n = bot_count.max(2) as f64;
    let radius = station_ring(bot_count, SITE_WANDER).min(MAX_RING);
    let chord = 2.0 * radius * (std::f64::consts::PI / n).sin();
    let wander = ((chord - MIN_PEER_GAP) / 2.0).clamp(0.0, SITE_WANDER);
    (radius, wander)
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

/// 2D distance from `(x, z)` to the nearest live peer.
fn nearest_peer_dist(ctx: &Ctx, x: f64, z: f64) -> f64 {
    (0..ctx.player_count())
        .filter(|&j| j != ctx.index())
        .filter_map(|j| ctx.peer_position(j))
        .map(|p| (p.0 - x).hypot(p.2 - z))
        .fold(f64::INFINITY, f64::min)
}

/// Highest air-over-solid cell of the column around `ref_y`: the arena floor on a fresh cell, or
/// the top of whatever earlier hands stacked there — which is what makes a later visit CONTINUE
/// the inherited structure instead of grinding buried cells.
fn column_top_y(ctx: &Ctx, x: i64, z: i64, ref_y: i64) -> i64 {
    for y in ((ref_y - 8)..=(ref_y + 12)).rev() {
        let pos = BlockPos::new(x as i32, y as i32, z as i32);
        if block_is_air(ctx, pos) && block_is_safe_support(ctx, pos.down(1)) {
            return y;
        }
    }
    ref_y
}

/// One placement: world cell plus the material to put there — `Ctx::build_blocks`'s row type.
type Placement = ((i64, i64, i64), String);

/// A small self-supporting motif: 1–3 columns strung along the site's tangent `axis`, each
/// grounded on its OWN column top, so a course laid across an uneven inherited structure follows
/// it like a terrain-hugging row. Columns already at the rise cap contribute nothing, which is
/// what turns tall sites sideways instead of out of placement reach.
fn motif_blocks(
    ctx: &mut Ctx,
    centre: (i64, i64),
    axis: (i64, i64),
    floor_y: i64,
    material: &str,
) -> (Vec<Placement>, &'static str) {
    let (plan, kind): (Vec<(i64, i64)>, &'static str) = match pick(ctx, 4) {
        0 => (vec![(-1, 1), (0, 1), (1, 1)], "course"),
        1 => (vec![(0, 2 + pick(ctx, 2) as i64)], "post"),
        2 => (vec![(0, 1), (1, 2)], "steps"),
        _ => (vec![(-1, 2), (0, 1)], "bracket"),
    };
    // A drawn flip lands the same motif in both directions along the tangent.
    let flip: i64 = if ctx.rng_next() < 0.5 { 1 } else { -1 };
    let mut blocks = Vec::new();
    for (offset, want) in plan {
        let (x, z) = (
            centre.0 + axis.0 * offset * flip,
            centre.1 + axis.1 * offset * flip,
        );
        let ground = column_top_y(ctx, x, z, floor_y);
        let allowed = (floor_y + MAX_RISE - ground).max(0);
        for dy in 0..want.min(allowed) {
            blocks.push(((x, ground + dy, z), material.to_string()));
        }
    }
    (blocks, kind)
}

/// Walk the ring chord to the next station: long open-loop bearing holds with a drawn veer
/// (sometimes sprinting), an occasional leg spent watching the nearest peer, and one closing
/// pathfinder correction. Long holds on purpose — hop-wise travel here reads as key chatter
/// (first-batch lesson).
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
    ctx.move_to(target.0, target.1, timeout, 1.8).await;
}

/// Dig one block an earlier hand laid near the site and patch the hole in this visit's material —
/// the continuation semantics made visible: a new hand does not just extend the inherited
/// structure, it amends it. Only kit blocks are ever dug (see [`is_kit_block`]).
async fn amend(ctx: &mut Ctx, site: (f64, f64), wander: f64, floor_y: i64, material: &str) {
    let timeout = ctx.cfg().move_timeout_ms.min(6_000);
    let reach = (ctx.cfg().place_reach - 1.0).max(1.5);
    for _probe in 0..4 {
        let x = (site.0 + vary(ctx, -wander - 0.5, wander + 0.5)).floor() as i64;
        let z = (site.1 + vary(ctx, -wander - 0.5, wander + 0.5)).floor() as i64;
        let found = (floor_y..=(floor_y + MAX_RISE + 1)).rev().find(|&y| {
            ctx.block_id_at(BlockPos::new(x as i32, y as i32, z as i32))
                .as_deref()
                .is_some_and(is_kit_block)
        });
        let Some(y) = found else { continue };
        let target = BlockPos::new(x as i32, y as i32, z as i32);
        if !ctx.target_in_reach(target) {
            ctx.move_to(x as f64 + 0.5, z as f64 + 0.5, timeout, reach)
                .await;
        }
        ctx.equip("diamond_pickaxe").await;
        let dug = ctx.dig(target).await;
        ctx.record(
            TYPE_MINE,
            &format!("vis-handoff-amend-dig-{}-{}", ctx.index(), ctx.elapsed_ms()),
            "Amend: dig back one block an earlier hand laid at this site.",
            json!({"dug": dug,
                "block_position": {"x": target.x, "y": target.y, "z": target.z}}),
        );
        if dug && ctx.rng_next() < 0.8 && ctx.equip(material).await {
            let ok = ctx.place_block(target, material).await;
            ctx.record(
                TYPE_PLACE,
                &format!(
                    "vis-handoff-amend-patch-{}-{}",
                    ctx.index(),
                    ctx.elapsed_ms()
                ),
                "Amend: patch the dug cell in this visit's own material.",
                json!({"placed": ok, "block": material,
                    "block_position": {"x": target.x, "y": target.y, "z": target.z}}),
            );
        }
        return;
    }
}

/// Nobody owns a site: every bot opens a structure at one ring station, works it for a short
/// stint, then HANDS IT OFF — walks the chord to the next station around the ring and continues
/// whatever the previous hand left standing there, in its own drawn material. Each site accretes
/// a layered multi-hand structure while the whole crew migrates around the ring; witness gazes
/// and a dig-and-patch amend beat tie the hands together across viewpoints.
///
/// No barriers: stint lengths are per-bot draws, so the rotation phases drift freely, and a bot
/// arriving at a still-occupied site works a free cell of it or strolls the rim instead of
/// queueing. Continuation is mechanical, not negotiated — motif columns ground themselves on the
/// inherited structure's own tops (`column_top_y`), so later visits stack until the rise cap and
/// then spread sideways.
pub(super) async fn run(ctx: &mut Ctx) {
    let index = ctx.index();
    let n = ctx.player_count().max(1);
    let anchor = ctx.anchor();
    let (radius, wander) = ring_geometry(n);
    let timeout = ctx.cfg().move_timeout_ms.min(8_000);
    let settle = ctx.cfg().look_settle_ms.max(400);
    let mut shift = 0usize;
    let mut visit = 0usize;
    while !ctx.target_met() {
        let station = (index + shift) % n;
        let ang = station as f64 / n as f64 * TAU;
        let (rx, rz) = (ang.cos(), ang.sin());
        let (sx, sz) = (anchor.0 + rx * radius, anchor.1 + rz * radius);
        // Approach on the inward side of the station; the ring interior is always open floor.
        migrate(ctx, (sx - rx * 3.0, sz - rz * 3.0), timeout).await;
        let material = MATERIALS[pick(ctx, MATERIALS.len())];
        // Cardinal axis closest to the station's tangent: motifs grow sideways along the ring,
        // never radially, so migration chords and the inward approach stay walkable.
        let axis = if rz.abs() >= rx.abs() {
            ((-rz.signum()) as i64, 0)
        } else {
            (0, rx.signum() as i64)
        };
        let floor_y = ctx.position().map(|p| p.1).unwrap_or(64.0).floor() as i64;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-handoff-claim-{index}-v{visit}"),
            &format!(
                "Bot {index} takes over site {station} and continues its structure in {material}."
            ),
            json!({"visit": visit, "station": station, "material": material,
                "site": {"x": r2(sx), "z": r2(sz)}, "ring_radius": r2(radius)}),
        );

        let passes = 1 + pick(ctx, 2);
        for pass in 0..passes {
            if ctx.target_met() {
                break;
            }
            // A free work cell of the site: redrawn away from any peer still packing up here.
            let mut cell = None;
            for _try in 0..3 {
                let c = (
                    sx + vary(ctx, -wander, wander),
                    sz + vary(ctx, -wander, wander),
                );
                if nearest_peer_dist(ctx, c.0, c.1) >= MIN_PEER_GAP {
                    cell = Some(c);
                    break;
                }
            }
            let Some((cx, cz)) = cell else {
                // Site still occupied — stroll the rim tangentially instead of queueing, and
                // spend the beat witnessing instead of building.
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
                continue;
            };
            // Stand off the cell at a drawn polar angle, so the base cell never overlaps the
            // placer's own hitbox.
            let stand = ctx.rng_next() * TAU;
            ctx.move_to(cx + stand.cos() * 2.4, cz + stand.sin() * 2.4, timeout, 1.5)
                .await;
            let (blocks, kind) = motif_blocks(
                ctx,
                (cx.floor() as i64, cz.floor() as i64),
                axis,
                floor_y,
                material,
            );
            if blocks.is_empty() {
                // Every column here is already at the rise cap; the next pass draws a fresh cell.
                shake_loose(ctx).await;
                continue;
            }
            let total = blocks.len();
            let (placed, attempted) = ctx
                .build_blocks(&blocks, |b, _t, _m| {
                    (
                        format!("vis-handoff-place-{index}-v{visit}-p{pass}-{b}"),
                        format!(
                            "Continue site {station}: place {kind} block {}/{total}.",
                            b + 1
                        ),
                        json!({"visit": visit, "pass": pass, "station": station,
                            "motif": kind, "block_index": b}),
                    )
                })
                .await;
            ctx.record(
                TYPE_PLACE,
                &format!("vis-handoff-motif-{index}-v{visit}-p{pass}"),
                &format!("Site {station} {kind} done: {placed}/{attempted}/{total} blocks."),
                json!({"visit": visit, "pass": pass, "station": station, "motif": kind,
                    "placed": placed, "attempted": attempted, "expected": total}),
            );
            witness_peer_edits(ctx).await;
            if ctx.rng_next() < 0.35 {
                glance_at_nearest_peer(ctx, None).await;
            }
            if ctx.rng_next() < 0.4 {
                amend(ctx, (sx, sz), wander, floor_y, material).await;
            }
            // One sustained press per pass keeps the move-key runs long; placement itself is all
            // short pathfinder nudges (first-batch chatter lesson). Tangential with a slight
            // inward pull, so the walk stays on open slab beside the structure.
            let side = if ctx.rng_next() < 0.5 { 1.0 } else { -1.0 };
            if let Some(p) = ctx.position() {
                let (dx, dz) = (-rz * side - rx * 0.3, rx * side - rz * 0.3);
                ctx.look((p.0 + dx * 18.0, p.1 + 1.3, p.2 + dz * 18.0), settle)
                    .await;
            }
            let hold = vary(ctx, 1_100.0, 2_000.0) as u64;
            ctx.hold_visual(WalkDirection::Forward, false, hold, 300)
                .await;
        }

        // Read the site once from the inward side — the handoff's "as left" shot — then move on
        // to the next station around the ring.
        let focus = (sx, floor_y as f64 + 2.0, sz);
        let retreat = vary(ctx, 4.0, 6.0);
        ctx.move_to_watching_point(sx - rx * retreat, sz - rz * retreat, focus, timeout, 2.0)
            .await;
        ctx.look_default(focus).await;
        sleep_ms(vary(ctx, 500.0, 1_200.0) as u64).await;
        witness_peer_edits(ctx).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-handoff-pass-on-{index}-v{visit}"),
            &format!("Bot {index} hands site {station} on and migrates to the next station."),
            json!({"visit": visit, "station": station}),
        );
        shift += 1;
        visit += 1;
    }
}
