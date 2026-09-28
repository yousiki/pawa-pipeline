use super::*;

const TAU: f64 = std::f64::consts::TAU;

/// Rosette motifs in petal coordinates (radial, up, tangent). Every bot lays the SAME motif at
/// the same petal-relative site on a pass, so the ring of stations grows as N rotated copies of
/// one another — the rotational symmetry is the scene's protagonist.
const MOTIFS: [&[(i64, i64, i64)]; 5] = [
    &[(0, 0, 0), (0, 1, 0)],
    &[(0, 0, 0), (0, 1, 0), (0, 2, 0)],
    &[(0, 0, 0), (1, 0, 0), (1, 1, 0)],
    &[(0, 0, 0), (0, 0, 1)],
    &[(0, 0, 0), (1, 0, 0)],
];

/// Kit materials; the shared pass hash draws one, so all petals match.
const MATERIALS: [&str; 6] = [
    "minecraft:stone_bricks",
    "minecraft:oak_planks",
    "minecraft:oak_log",
    "minecraft:cobblestone",
    "minecraft:bricks",
    "minecraft:stone",
];

/// Station ring cap keeping every petal plus its wander, viewpoints and promenades inside the
/// levelled 41x41 build_arena slab.
const MAX_RING: f64 = 15.5;

/// Tangential wander budget a petal site may use (shrunk by `ring_geometry` when the capped
/// ring's chord gets tight).
const SITE_WANDER: f64 = 2.0;

/// Draw in `[lo, hi)` from the per-bot stream, so a number this scenario could have hard-coded
/// varies per bot and per pass and still replays from the seed.
fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

/// splitmix64-style mix: the shared blueprint stream. Every bot computes the same draw for a
/// pass with no communication, which is what keeps the rosette N-fold symmetric while the bots'
/// clocks drift apart — only gait/glance rhythm consumes `rng_next`.
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

/// Station ring radius plus the tangential wander it can absorb: the wander is recomputed from
/// the capped radius' actual chord, so the 4.5-block peer gap survives whatever `bot_count` does.
fn ring_geometry(bot_count: usize) -> (f64, f64) {
    let n = bot_count.max(2) as f64;
    let radius = station_ring(bot_count, SITE_WANDER).min(MAX_RING);
    let chord = 2.0 * radius * (std::f64::consts::PI / n).sin();
    let wander = ((chord - MIN_PEER_GAP) / 2.0).clamp(0.0, SITE_WANDER);
    (radius, wander)
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

/// N bots hold the N petal slots of a rosette around the anchor and raise identical small tower
/// motifs at rotationally symmetric sites — the blueprint is a pure function of the pass number,
/// so the symmetry needs no barrier. Between motifs each bot steps inward to read its own petal,
/// pans to a neighbour's rotated copy (the symmetry-check gaze), witnesses fresh peer edits, and
/// promenades along its own arc of the ring.
pub(super) async fn run(ctx: &mut Ctx) {
    let index = ctx.index();
    let n = ctx.player_count().max(1);
    let anchor = ctx.anchor();
    let (ring, wander) = ring_geometry(n);
    let theta = index as f64 / n as f64 * TAU;
    let u = (theta.cos(), theta.sin());
    let t = (-theta.sin(), theta.cos());
    // Angular excursion the promenade may use while adjacent bots still clear the peer gap even
    // when both wander toward each other.
    let sector = TAU / n as f64;
    let gap_angle = 2.0 * (MIN_PEER_GAP / (2.0 * ring)).min(1.0).asin();
    let excursion = ((sector - gap_angle) / 2.0).clamp(0.0, sector * 0.35);
    let timeout = ctx.cfg().move_timeout_ms.min(8_000);
    ctx.record(
        TYPE_LOOK,
        &format!("vis-rosette-claim-{index}"),
        &format!(
            "Bot {index} claims rosette petal {index}/{n} at {:.0} deg.",
            theta.to_degrees()
        ),
        json!({"petal": index, "petals": n, "ring": r2(ring), "wander": r2(wander),
            "excursion_deg": r2(excursion.to_degrees())}),
    );

    let mut pass = 0u64;
    loop {
        // Shared blueprint: identical motif, material and petal-relative site for every bot.
        let h = mix(0x0052_05ef, pass);
        let motif = MOTIFS[(h % MOTIFS.len() as u64) as usize];
        let material = MATERIALS[((h >> 8) % MATERIALS.len() as u64) as usize];
        // Radial offset points INWARD: round 1 pushed sites outward from the capped ring and the
        // rendered bot spent whole beats at (and off) the arena slab edge — under the levelled
        // slab's overhang, which is exactly the dark-frame failure the cap exists to prevent.
        let ro = ((h >> 16) % 4) as f64;
        let site_r = ring - ro;
        let to = (((h >> 24) % 401) as f64 / 200.0 - 1.0) * wander;
        let site = (
            anchor.0 + u.0 * site_r + t.0 * to,
            anchor.1 + u.1 * site_r + t.1 * to,
        );

        // Stand tangentially beside the site so the motif rises clear of the hitbox.
        let stand_side = if ctx.rng_next() < 0.5 { 1.0 } else { -1.0 };
        let stand = (
            site.0 + t.0 * stand_side * 2.4,
            site.1 + t.1 * stand_side * 2.4,
        );
        let gait = ctx.rng_next();
        if gait < 0.2 && n > 1 {
            // Travel with the camera pinned on the clockwise neighbour — decoupled by design.
            let peer = (index + 1) % n;
            ctx.move_to_watching_peer(stand.0, stand.1, peer, timeout, 1.8)
                .await;
        } else {
            let sprint = ctx.rng_next() < ctx.cfg().sprint_bout_prob;
            walk_toward(ctx, stand, sprint).await;
        }

        let feet_y = ctx.position().map(|p| p.1.floor() as i64).unwrap_or(64);
        let height = motif.iter().map(|c| c.1).max().unwrap_or(0) + 1;
        match marker_base_near(
            ctx,
            (site.0.floor() as i64, feet_y, site.1.floor() as i64),
            height,
        ) {
            Some(base) => {
                // Map petal coordinates to world cells around the resolved base; deduplicate,
                // because rounding a rotated offset can collapse two cells onto one column.
                let mut cells: Vec<(i64, i64, i64)> = Vec::new();
                for &(dr, dy, dt) in motif {
                    let wx =
                        (base.x as f64 + 0.5 + u.0 * dr as f64 + t.0 * dt as f64).floor() as i64;
                    let wz =
                        (base.z as f64 + 0.5 + u.1 * dr as f64 + t.1 * dt as f64).floor() as i64;
                    let cell = (wx, base.y as i64 + dy, wz);
                    if !cells.contains(&cell) {
                        cells.push(cell);
                    }
                }
                let blocks: Vec<((i64, i64, i64), String)> = cells
                    .into_iter()
                    .map(|c| (c, material.to_string()))
                    .collect();
                let total = blocks.len();
                let (placed, attempted) = ctx
                    .build_blocks(&blocks, |b, _t, _m| {
                        (
                            format!("vis-rosette-place-{index}-p{pass}-{b}"),
                            format!("Lay rosette motif block {}/{total}.", b + 1),
                            json!({"pass": pass, "petal": index, "block_index": b}),
                        )
                    })
                    .await;
                ctx.record(
                    TYPE_PLACE,
                    &format!("vis-rosette-motif-{index}-p{pass}"),
                    &format!("Rosette motif done: {placed}/{attempted}/{total} blocks."),
                    json!({"pass": pass, "petal": index, "material": material,
                        "motif": (h % MOTIFS.len() as u64), "radial_offset": ro,
                        "tangent_offset": r2(to), "placed": placed, "attempted": attempted,
                        "expected": total, "cell": {"x": base.x, "y": base.y, "z": base.z}}),
                );
            }
            None => shake_loose(ctx).await,
        }

        // Step inward, read the own petal, then pan to a neighbour's rotated copy — the
        // symmetry-check gaze this scenario exists for.
        let vr = ring - vary(ctx, 4.5, 6.5);
        let vp = (
            anchor.0 + u.0 * vr + t.0 * to * 0.5,
            anchor.1 + u.1 * vr + t.1 * to * 0.5,
        );
        let feet = ctx.position().map(|p| p.1).unwrap_or(64.0);
        if ctx.rng_next() < 0.25 {
            ctx.move_to_watching_point(vp.0, vp.1, (site.0, feet + 1.0, site.1), timeout, 1.8)
                .await;
        } else {
            let sprint = ctx.rng_next() < ctx.cfg().sprint_bout_prob;
            walk_toward(ctx, vp, sprint).await;
        }
        ctx.look_default((site.0, feet + 1.0, site.1)).await;
        sleep_ms(vary(ctx, 600.0, 1_200.0) as u64).await;
        if n > 1 {
            let j = if ctx.rng_next() < 0.5 {
                (index + 1) % n
            } else {
                (index + n - 1) % n
            };
            let thj = j as f64 / n as f64 * TAU;
            let (uj, tj) = ((thj.cos(), thj.sin()), (-thj.sin(), thj.cos()));
            let nf = (
                anchor.0 + uj.0 * site_r + tj.0 * to,
                feet + 1.2,
                anchor.1 + uj.1 * site_r + tj.1 * to,
            );
            ctx.look(nf, ctx.cfg().look_settle_ms.max(600)).await;
            sleep_ms(vary(ctx, 900.0, 1_800.0) as u64).await;
            ctx.record(
                TYPE_LOOK,
                &format!("vis-rosette-sym-{index}-p{pass}"),
                &format!("Check petal {j}'s rotated copy of this pass's motif."),
                json!({"pass": pass, "petal": index, "checked_petal": j,
                    "focus": {"x": r2(nf.0), "z": r2(nf.2)}}),
            );
        }
        witness_peer_edits(ctx).await;
        if ctx.rng_next() < 0.25 {
            glance_at_nearest_peer(ctx, None).await;
        }

        // Promenade along the own arc of the ring — a long held-key leg, sometimes with the
        // camera pinned on the rosette centre instead of the travel bearing.
        let a = theta + vary(ctx, -excursion, excursion);
        // Promenade stays at or inside the station ring — never outward toward the slab edge.
        let pr = ring + vary(ctx, -2.5, 0.0);
        let dest = (anchor.0 + a.cos() * pr, anchor.1 + a.sin() * pr);
        if ctx.rng_next() < 0.3 {
            let feet = ctx.position().map(|p| p.1).unwrap_or(64.0);
            ctx.move_to_watching_point(
                dest.0,
                dest.1,
                (anchor.0, feet + 1.0, anchor.1),
                timeout,
                2.0,
            )
            .await;
        } else {
            let sprint = ctx.rng_next() < ctx.cfg().sprint_bout_prob;
            walk_toward(ctx, dest, sprint).await;
        }
        if ctx.target_met() {
            break;
        }
        pass += 1;
    }
}
