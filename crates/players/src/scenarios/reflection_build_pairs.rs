use super::*;

/// Mirror motifs in (dx, dy, dz_away) cells — `dz_away` points AWAY from the shared symmetry
/// axis, so the same motif laid by the two partners comes out as a true mirror image. The
/// entries with a `dz_away` component are the visibly chiral ones.
const MOTIFS: [&[(i64, i64, i64)]; 6] = [
    &[(0, 0, 0), (0, 1, 0)],
    &[(0, 0, 0), (0, 1, 0), (0, 2, 0)],
    &[(0, 0, 0), (1, 0, 0)],
    &[(0, 0, 0), (1, 0, 0), (0, 0, 1)],
    &[(0, 0, 0), (1, 0, 0), (1, 1, 0)],
    &[(0, 0, 0), (0, 0, 1), (0, 1, 1)],
];

/// Kit materials; the shared pass hash draws one, so both partners lay the same block.
const MATERIALS: [&str; 6] = [
    "minecraft:stone_bricks",
    "minecraft:oak_planks",
    "minecraft:oak_log",
    "minecraft:cobblestone",
    "minecraft:bricks",
    "minecraft:stone",
];

/// X pitch between neighbouring pair stations along the axis. Four stations (8 bots) plus the
/// ±2-cell site draw and marker drift stay inside the levelled build_arena slab.
const PAIR_PITCH: f64 = 9.0;

/// Near edge of a side's build strip, measured from the axis. Twice this is the closest two
/// partners' cells can face each other, already clear of the 4.5-block peer gap.
const STRIP_NEAR: f64 = 4.5;

/// Depth of the build strip: motif bases land STRIP_NEAR..=STRIP_NEAR+STRIP_DEPTH off the axis.
const STRIP_DEPTH: u64 = 4;

/// Draw in `[lo, hi)` from the per-bot stream, so a number this scenario could have hard-coded
/// varies per bot and per pass and still replays from the seed.
fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

/// splitmix64-style mix of (pair, pass): the shared blueprint stream. Both partners compute the
/// same draw with no communication, which is what keeps the two sides mirror images while their
/// clocks drift apart — geometry is deterministic, only gait/glance rhythm consumes `rng_next`.
fn mix(a: u64, b: u64) -> u64 {
    let mut x = a
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(b.wrapping_mul(0xBF58_476D_1CE4_E5B9))
        .wrapping_add(0x94D0_49BB_1331_11EB);
    x ^= x >> 30;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// Open-loop walk toward `target`: one long key hold sized by distance, then at most one
/// pathfinder correction — the anti-chatter gait radial_trail_build measured its way to.
///
/// The hold floor is a full second, not the 500ms other scenarios use: this scene's legs are
/// short strip hops (2–5 blocks), and at 8 bots the 500–700ms presses they produced measured
/// 36–43% short-press chatter (cap 35%). A slight overshoot costs nothing on the open strip.
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
            let hold = ((dist / speed * 1000.0) as u64).clamp(1_000, 6_000);
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

/// Two partners face each other across a shared symmetry axis and raise mirror-image motifs on
/// their own sides: the blueprint (motif, material, cell) is a pure function of (pair, pass), so
/// the sides mirror without a barrier, and every pass ends with a gaze across the axis at the
/// partner's mirrored cell plus a witness of whatever a neighbour just changed. An odd trailing
/// bot mirrors ITSELF: it builds a motif on one side, then crosses the axis and builds the
/// reflection with its own hands.
pub(super) async fn run(ctx: &mut Ctx) {
    let index = ctx.index();
    let bot_count = ctx.player_count().max(1);
    let anchor = ctx.anchor();
    let axis_z = anchor.1;
    let pairs_total = bot_count.div_ceil(2).max(1);
    let pair = index / 2;
    let solo = bot_count % 2 == 1 && index == bot_count - 1;
    let partner = if solo { None } else { Some(index ^ 1) };
    let px = anchor.0 + (pair as f64 - (pairs_total as f64 - 1.0) / 2.0) * PAIR_PITCH;
    let timeout = ctx.cfg().move_timeout_ms.min(8_000);
    ctx.record(
        TYPE_LOOK,
        &format!("vis-mirror-claim-{index}"),
        &format!("Bot {index} claims pair {pair}'s mirror site on the axis at x={px:.1}."),
        json!({"pair": pair, "partner": partner, "solo": solo, "axis_z": r2(axis_z),
            "station_x": r2(px)}),
    );

    let mut pass = 0u64;
    loop {
        let side: f64 = if solo {
            // The solo bot alternates sides, and `key` repeats every two passes, so passes 2k and
            // 2k+1 lay the SAME motif reflected — self-mirroring with a long axis crossing.
            if pass.is_multiple_of(2) { 1.0 } else { -1.0 }
        } else if index.is_multiple_of(2) {
            1.0
        } else {
            -1.0
        };
        let sdz: i64 = if side > 0.0 { 1 } else { -1 };
        let key = if solo { pass / 2 } else { pass };
        let h = mix(pair as u64, key);
        let motif = MOTIFS[(h % MOTIFS.len() as u64) as usize];
        let material = MATERIALS[((h >> 8) % MATERIALS.len() as u64) as usize];
        let ox = ((h >> 16) % 5) as i64 - 2;
        let depth = STRIP_NEAR + ((h >> 24) % (STRIP_DEPTH + 1)) as f64;
        let cx = px + ox as f64;
        let cz = axis_z + side * depth;

        // Approach a stand-off OUTSIDE the cell (further from the axis), so the motif rises
        // between this bot and its partner and the cross-axis gazes see both.
        let sx = cx + vary(ctx, -1.5, 1.5);
        let sz = axis_z + side * (depth + vary(ctx, 2.0, 3.2));
        let gait = ctx.rng_next();
        match partner {
            // A camera-on-partner travel leg: decoupled from the travel bearing by design.
            Some(p) if gait < 0.2 => {
                ctx.move_to_watching_peer(sx, sz, p, timeout, 1.8).await;
            }
            _ => {
                let sprint = ctx.rng_next() < ctx.cfg().sprint_bout_prob;
                walk_toward(ctx, (sx, sz), sprint).await;
            }
        }

        let feet_y = ctx.position().map(|p| p.1.floor() as i64).unwrap_or(64);
        let height = motif.iter().map(|c| c.1).max().unwrap_or(0) + 1;
        match marker_base_near(ctx, (cx.floor() as i64, feet_y, cz.floor() as i64), height) {
            Some(base) => {
                let blocks: Vec<((i64, i64, i64), String)> = motif
                    .iter()
                    .map(|&(dx, dy, dz_away)| {
                        (
                            (
                                base.x as i64 + dx,
                                base.y as i64 + dy,
                                base.z as i64 + sdz * dz_away,
                            ),
                            material.to_string(),
                        )
                    })
                    .collect();
                let total = blocks.len();
                let (placed, attempted) = ctx
                    .build_blocks(&blocks, |b, _t, _m| {
                        (
                            format!("vis-mirror-place-{index}-p{pass}-{b}"),
                            format!("Lay mirror motif block {}/{total}.", b + 1),
                            json!({"pass": pass, "pair": pair, "side": sdz, "block_index": b}),
                        )
                    })
                    .await;
                ctx.record(
                    TYPE_PLACE,
                    &format!("vis-mirror-motif-{index}-p{pass}"),
                    &format!(
                        "Mirror motif done on side {sdz}: {placed}/{attempted}/{total} blocks."
                    ),
                    json!({"pass": pass, "pair": pair, "side": sdz, "material": material,
                        "motif": (h % MOTIFS.len() as u64), "placed": placed,
                        "attempted": attempted, "expected": total,
                        "cell": {"x": base.x, "y": base.y, "z": base.z}}),
                );
            }
            None => shake_loose(ctx).await,
        }

        // Mirror check: step back outward, read the own motif, then look ACROSS the axis at the
        // partner's mirrored cell — the world-synchronization gaze this scenario exists for.
        // The retreat is the pass's one long leg: 5–8 blocks further out, so the walk is a real
        // held stride rather than the 1–2 block shuffle that fed the 8-bot chatter failure.
        let rx = cx + vary(ctx, -2.0, 2.0);
        let rz = axis_z + side * (depth + vary(ctx, 5.0, 8.0));
        match partner {
            Some(p) if ctx.rng_next() < 0.25 => {
                ctx.move_to_watching_peer(rx, rz, p, timeout, 2.0).await;
            }
            _ => {
                let sprint = ctx.rng_next() < ctx.cfg().sprint_bout_prob;
                walk_toward(ctx, (rx, rz), sprint).await;
            }
        }
        let feet = ctx.position().map(|p| p.1).unwrap_or(64.0);
        ctx.look_default((cx, feet + 1.0, cz)).await;
        sleep_ms(vary(ctx, 600.0, 1_200.0) as u64).await;
        let mirror_z = axis_z - side * depth;
        ctx.look(
            (cx, feet + 1.2, mirror_z),
            ctx.cfg().look_settle_ms.max(600),
        )
        .await;
        sleep_ms(vary(ctx, 900.0, 1_800.0) as u64).await;
        ctx.record(
            TYPE_LOOK,
            &format!("vis-mirror-check-{index}-p{pass}"),
            "Check the mirrored cell across the axis.",
            json!({"pass": pass, "pair": pair, "solo": solo,
                "mirror_cell": {"x": r2(cx), "z": r2(mirror_z)}}),
        );
        if let Some(p) = partner
            && ctx.rng_next() < 0.5
        {
            look_at_peer(ctx, p, ctx.cfg().look_settle_ms.max(500)).await;
            sleep_ms(vary(ctx, 500.0, 1_100.0) as u64).await;
        }
        witness_peer_edits(ctx).await;
        if ctx.rng_next() < 0.25 {
            glance_at_nearest_peer(ctx, None).await;
        }
        if ctx.target_met() {
            break;
        }
        pass += 1;
    }
}
