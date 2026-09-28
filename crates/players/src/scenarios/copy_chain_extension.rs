use super::*;

/// Segment variants: footprints whose presence bitmask over the three probe cells
/// ((0,1,0), (1,0,0), (1,1,0)) relative to the base is unique, so a downstream copier can READ
/// which variant its upstream neighbour built by probing four blocks — genuine observation of
/// the world, no shared state.
const VARIANTS: [&[(i64, i64, i64)]; 5] = [
    &[(0, 0, 0)],
    &[(0, 0, 0), (0, 1, 0)],
    &[(0, 0, 0), (1, 0, 0)],
    &[(0, 0, 0), (1, 0, 0), (1, 1, 0)],
    &[(0, 0, 0), (0, 1, 0), (1, 0, 0)],
];

/// Kit block ids as `block_id_at` reports them (no namespace), index-aligned with `MATERIALS`.
const KIT_IDS: [&str; 6] = [
    "stone_bricks",
    "oak_planks",
    "oak_log",
    "cobblestone",
    "bricks",
    "stone",
];

const MATERIALS: [&str; 6] = [
    "minecraft:stone_bricks",
    "minecraft:oak_planks",
    "minecraft:oak_log",
    "minecraft:cobblestone",
    "minecraft:bricks",
    "minecraft:stone",
];

/// Segment grid along X: `SEGMENTS` cells `SEG_PITCH` apart, spanning the levelled arena.
const SEG_PITCH: i64 = 3;
const SEGMENTS: i64 = 11;
const CHAIN_HALF_SPAN: f64 = 15.0;

/// How many segments the copy source lags behind the copier's own cursor. Everyone starts at
/// segment 0 at the same time, so reading the segment the upstream bot is building RIGHT NOW is
/// a coin flip; reading two segments back is almost always a finished shape. The pattern thus
/// propagates across lanes as a diagonal wave, shifted `COPY_LAG` per lane.
const COPY_LAG: i64 = 2;

/// Z pitch between lanes: wide when few lanes, compressed (never below the peer gap plus slack)
/// so eight lanes still fit the levelled arena.
fn lane_pitch(n: usize) -> f64 {
    (36.0 / (n.max(2) - 1) as f64).clamp(5.0, 7.0)
}

/// Draw in `[lo, hi)` from the per-bot stream, so a number this scenario could have hard-coded
/// varies per bot and per pass and still replays from the seed.
fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

/// Uniform draw in `0..n` from the per-bot stream.
fn pick(ctx: &mut Ctx, n: usize) -> usize {
    ((ctx.rng_next() * n as f64) as usize).min(n.saturating_sub(1))
}

/// World X of the base cell of segment `s`.
fn seg_x(anchor_x: f64, s: i64) -> i64 {
    (anchor_x - CHAIN_HALF_SPAN).floor() as i64 + s * SEG_PITCH
}

/// Kit-material index of the block at `pos`, or None when it is not a block a builder laid.
fn kit_at(ctx: &Ctx, pos: BlockPos) -> Option<usize> {
    ctx.block_id_at(pos)
        .as_deref()
        .and_then(|id| KIT_IDS.iter().position(|k| *k == id))
}

/// Read the upstream lane's segment at column (x, z): find its base (the LOWEST kit block in the
/// column, scanned around own feet height — later storeys stack above and must not confuse the
/// read), then classify the variant from the three-probe bitmask. `None` means "not built yet or
/// unreadable", and the copier invents instead of waiting — the decoupling rule.
fn read_segment(ctx: &Ctx, x: i64, z: i64, feet_y: i64) -> Option<(usize, usize)> {
    let base_y = (feet_y - 5..=feet_y + 5)
        .find(|&y| kit_at(ctx, BlockPos::new(x as i32, y as i32, z as i32)).is_some())?;
    let material = kit_at(ctx, BlockPos::new(x as i32, base_y as i32, z as i32))?;
    let probes = [(0i64, 1i64, 0i64), (1, 0, 0), (1, 1, 0)];
    let mut mask = 0usize;
    for (bit, &(dx, dy, dz)) in probes.iter().enumerate() {
        let cell = BlockPos::new((x + dx) as i32, (base_y + dy) as i32, (z + dz) as i32);
        if kit_at(ctx, cell).is_some() {
            mask |= 1 << bit;
        }
    }
    let variant = match mask {
        0b000 => 0,
        0b001 => 1,
        0b010 => 2,
        0b110 => 3,
        0b011 => 4,
        _ => return None,
    };
    Some((variant, material))
}

/// Open-loop walk toward `target`: one long key hold sized by distance, then at most one
/// pathfinder correction — the anti-chatter gait radial_trail_build measured its way to.
async fn walk_toward(ctx: &mut Ctx, target: (f64, f64), sprint: bool) {
    if let Some(p) = ctx.position() {
        let dist = (target.0 - p.0).hypot(target.1 - p.2);
        if dist > 1.5 {
            ctx.look(
                (target.0, p.1 + 1.62, target.1),
                ctx.cfg().look_settle_ms.max(400),
            )
            .await;
            let speed = if sprint { 5.6 } else { 4.3 };
            let hold = ((dist / speed * 1000.0) as u64).clamp(500, 6_000);
            ctx.hold_visual(WalkDirection::Forward, sprint, hold, 0)
                .await;
        }
    }
    let close = ctx
        .position()
        .map(|q| (target.0 - q.0).hypot(target.1 - q.2) <= 2.5)
        .unwrap_or(false);
    if !close {
        let timeout = ctx.cfg().move_timeout_ms.min(8_000);
        ctx.move_to(target.0, target.1, timeout, 1.8).await;
    }
}

/// Copy chain along parallel lanes: each bot advances its own lane segment by segment, and before
/// laying each one it turns to its upstream neighbour's lane, READS the shape that neighbour
/// built a couple of segments back (block probes — real observation), reproduces it in its own
/// lane and moves on; when nothing is readable yet it invents a variant instead of waiting.
/// Finished rows wrap into the next row one cell toward the arena centre, so the chain keeps
/// extending for the whole episode.
pub(super) async fn run(ctx: &mut Ctx) {
    let index = ctx.index();
    let n = ctx.player_count().max(1);
    let anchor = ctx.anchor();
    let pitch = lane_pitch(n);
    let lane_z = anchor.1 + (index as f64 - (n as f64 - 1.0) / 2.0) * pitch;
    let upstream = (index + n - 1) % n;
    let up_z = anchor.1 + (upstream as f64 - (n as f64 - 1.0) / 2.0) * pitch;
    // Later rows grow toward the arena centre so the outermost lanes never push blocks off-slab.
    let dz_dir: i64 = if lane_z <= anchor.1 { 1 } else { -1 };
    let timeout = ctx.cfg().move_timeout_ms.min(8_000);
    ctx.record(
        TYPE_LOOK,
        &format!("vis-chain-claim-{index}"),
        &format!("Bot {index} claims chain lane {index} (copies bot {upstream})."),
        json!({"lane": index, "upstream": upstream, "lane_z": r2(lane_z),
            "pitch": r2(pitch), "copy_lag": COPY_LAG}),
    );

    let mut row = 0i64;
    'rows: loop {
        let dz = (row.min(2)) * dz_dir;
        // Two segments per stop: one stand-off serves both, so travel legs are ~6-block held
        // walks rather than 3-block pathfinder taps (key-chatter economics).
        let mut s = 0i64;
        while s < SEGMENTS {
            if ctx.target_met() {
                break 'rows;
            }
            let x0 = seg_x(anchor.0, s);
            let stand = (
                x0 as f64 + 1.5 + vary(ctx, -0.6, 0.6),
                lane_z - dz_dir as f64 * (2.0 + vary(ctx, 0.0, 1.0)),
            );
            let gait = ctx.rng_next();
            if gait < 0.15 && n > 1 {
                ctx.move_to_watching_peer(stand.0, stand.1, upstream, timeout, 1.8)
                    .await;
            } else if gait < 0.35 && s > 0 {
                // Camera pinned on the own previous segment while the body advances.
                let fx = seg_x(anchor.0, s - 1) as f64 + 0.5;
                let feet = ctx.position().map(|p| p.1).unwrap_or(64.0);
                ctx.move_to_watching_point(
                    stand.0,
                    stand.1,
                    (fx, feet + 1.0, lane_z + dz as f64),
                    timeout,
                    1.8,
                )
                .await;
            } else {
                let sprint = ctx.rng_next() < ctx.cfg().sprint_bout_prob;
                walk_toward(ctx, stand, sprint).await;
            }

            for seg in [s, s + 1] {
                if seg >= SEGMENTS || ctx.target_met() {
                    break;
                }
                let x = seg_x(anchor.0, seg);
                let feet_y = ctx.position().map(|p| p.1.floor() as i64).unwrap_or(64);
                // OBSERVE: turn to the upstream lane and read the shape built there a couple of
                // segments back, before deciding what to lay here.
                let source = seg - COPY_LAG;
                let read = if source >= 0 {
                    let sx = seg_x(anchor.0, source);
                    ctx.look(
                        (sx as f64 + 0.5, feet_y as f64 + 1.0, up_z),
                        ctx.cfg().look_settle_ms.max(500),
                    )
                    .await;
                    sleep_ms(vary(ctx, 500.0, 1_200.0) as u64).await;
                    read_segment(ctx, sx, up_z.floor() as i64, feet_y)
                } else {
                    None
                };
                let copied = read.is_some();
                let (variant, mat_i) = match read {
                    Some(v) => v,
                    None => (pick(ctx, VARIANTS.len()), pick(ctx, MATERIALS.len())),
                };
                ctx.record(
                    TYPE_LOOK,
                    &format!("vis-chain-read-{index}-r{row}-s{seg}"),
                    &if copied {
                        format!("Copy bot {upstream}'s segment {source} into own segment {seg}.")
                    } else {
                        format!("Nothing readable upstream; invent segment {seg}.")
                    },
                    json!({"row": row, "segment": seg, "source_segment": source,
                        "upstream": upstream, "copied": copied, "variant": variant}),
                );
                let material = MATERIALS[mat_i];
                let cells = VARIANTS[variant];
                let height = cells.iter().map(|c| c.1).max().unwrap_or(0) + 1;
                match marker_base_near(ctx, (x, feet_y, lane_z.floor() as i64 + dz), height) {
                    Some(base) => {
                        let blocks: Vec<((i64, i64, i64), String)> = cells
                            .iter()
                            .map(|&(bx, by, bz)| {
                                (
                                    (base.x as i64 + bx, base.y as i64 + by, base.z as i64 + bz),
                                    material.to_string(),
                                )
                            })
                            .collect();
                        let total = blocks.len();
                        let (placed, attempted) = ctx
                            .build_blocks(&blocks, |b, _t, _m| {
                                (
                                    format!("vis-chain-place-{index}-r{row}-s{seg}-{b}"),
                                    format!("Lay chain segment block {}/{total}.", b + 1),
                                    json!({"row": row, "segment": seg, "block_index": b,
                                        "copied": copied}),
                                )
                            })
                            .await;
                        ctx.record(
                            TYPE_PLACE,
                            &format!("vis-chain-seg-{index}-r{row}-s{seg}"),
                            &format!(
                                "Chain segment {seg} done ({}): {placed}/{attempted}/{total} \
                                 blocks.",
                                if copied { "copied" } else { "invented" }
                            ),
                            json!({"row": row, "segment": seg, "copied": copied,
                                "variant": variant, "material": material, "placed": placed,
                                "attempted": attempted, "expected": total,
                                "cell": {"x": base.x, "y": base.y, "z": base.z}}),
                        );
                    }
                    None => shake_loose(ctx).await,
                }
            }
            witness_peer_edits(ctx).await;
            if ctx.rng_next() < 0.25 {
                glance_at_nearest_peer(ctx, None).await;
            }
            s += 2;
        }
        if ctx.target_met() {
            break;
        }

        // Row done: walk the lane back — first half as one long held leg, second half with an
        // own earlier segment held in frame (persistence beat) — then start the next row.
        let mid = (
            anchor.0,
            lane_z - dz_dir as f64 * (2.0 + vary(ctx, 0.0, 1.0)),
        );
        let sprint = ctx.rng_next() < ctx.cfg().sprint_bout_prob;
        walk_toward(ctx, mid, sprint).await;
        let review = seg_x(anchor.0, pick(ctx, SEGMENTS as usize) as i64);
        let feet = ctx.position().map(|p| p.1).unwrap_or(64.0);
        ctx.move_to_watching_point(
            seg_x(anchor.0, 0) as f64 + 0.5,
            lane_z - dz_dir as f64 * 2.0,
            (review as f64 + 0.5, feet + 1.0, lane_z + dz as f64),
            timeout,
            2.0,
        )
        .await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-chain-wrap-{index}-r{row}"),
            &format!("Row {row} complete; walk the lane back and start the next row."),
            json!({"row": row, "reviewed_x": review}),
        );
        row += 1;
    }
}
