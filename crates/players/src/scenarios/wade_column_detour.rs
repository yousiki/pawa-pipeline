use super::*;

/// One march phase: the column walks out along the bearing for this long, then turns and walks
/// back, the old tail leading the return leg. Two phases fit a 120 s episode with slack.
const PHASE_MS: i64 = 40_000;

/// Following distance the chain tries to hold. Above the peer gap, below losing sight.
const FOLLOW_GAP: f64 = 6.0;

/// Perpendicular detour offsets tried, in order, when the drawn waypoint lands on water.
const DETOURS: [f64; 5] = [0.0, 4.0, -4.0, 8.0, -8.0];

/// Draw in `[lo, hi)` from the per-bot stream.
fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

/// Feet altitude of the open-sky LAND surface in column `(x, z)`, or `None` when the column is
/// water, lava, canopy, or unresolvable — the march never targets those. Adapted from
/// `ridge_valley_circuit::skylit_feet_y`: scanning from above makes the first support the topmost,
/// so a waypoint can never resolve to a cave floor.
fn land_feet_y(ctx: &Ctx, x: i32, z: i32, y_hint: i32) -> Option<i32> {
    let mut fluff = 0u32;
    for y in ((y_hint - 12)..=(y_hint + 12)).rev() {
        let cell = BlockPos::new(x, y, z);
        let id = ctx.block_id_at(cell)?;
        if id.contains("water") || id.contains("lava") {
            return None;
        }
        if block_is_safe_support(ctx, cell) {
            if id.contains("leaves") || id.ends_with("log") {
                return None;
            }
            return Some(y + 1);
        }
        if !block_is_air(ctx, cell) {
            fluff += 1;
            if fluff > 3 {
                return None;
            }
        }
    }
    None
}

/// Resolve `want` to the nearest dry column: the point itself, else perpendicular detour offsets
/// along `(px, pz)` — the shoreline skirt. `None` means everything here is water.
fn dry_target(
    ctx: &Ctx,
    want: (f64, f64),
    perp: (f64, f64),
    y_hint: i32,
) -> Option<(f64, f64, i32)> {
    DETOURS.iter().find_map(|&off| {
        let x = want.0 + perp.0 * off;
        let z = want.1 + perp.1 * off;
        land_feet_y(ctx, x.floor() as i32, z.floor() as i32, y_hint).map(|y| (x, z, y))
    })
}

/// Whether the bot is currently standing in water (wading deeper than intended).
fn in_water(ctx: &Ctx) -> bool {
    ctx.position()
        .and_then(|p| {
            ctx.block_id_at(BlockPos::new(
                p.0.floor() as i32,
                p.1.floor() as i32,
                p.2.floor() as i32,
            ))
        })
        .map(|id| id.contains("water"))
        .unwrap_or(false)
}

/// A column of bots marches a fixed cross-country bearing in single file — out along the line for
/// one phase, back the next, the old tail leading the return — and every marcher probes the
/// ground ahead: waypoints that land on water are skirted with perpendicular detour offsets, so
/// the column bends around ponds and rivers instead of soaking in them (shallow shoreline splashes
/// are welcome; targets never are). The head weaves, sprints uphill legs, and stops to look back
/// down the column; followers hold their spacing, walk legs with the camera pinned on the bot
/// ahead, and shuffle sideways rather than freeze when the column compresses.
pub(crate) async fn run(ctx: &mut Ctx) {
    let index = ctx.index();
    let n = ctx.player_count().max(1);
    // The march axis is shared, deterministic, and seed-varied: every bot derives the same
    // bearing without any coordination.
    let bearing = (ctx.engine().seed % 360) as f64;
    let rad = bearing.to_radians();
    let (ux, uz) = (rad.cos(), rad.sin());
    let (px, pz) = (-uz, ux); // perpendicular, for weave and detours
    let sprint_prob = ctx.cfg().sprint_bout_prob;

    ctx.record(
        TYPE_LOOK,
        &format!("vis-wade-init-{index}"),
        "Column march: single file along a fixed bearing, bending around water, reversing each phase.",
        json!({"bearing_deg": r2(bearing), "phase_ms": PHASE_MS, "column_length": n}),
    );

    let mut leg = 0u32;
    // (position, elapsed_ms) of the last time the body actually displaced — the follower's
    // stand-timer; reset at the bottom of every loop iteration.
    let mut stall_anchor = (ctx.position().unwrap_or((0.0, 0.0, 0.0)), ctx.elapsed_ms());
    while !ctx.target_met() {
        let outbound = (ctx.elapsed_ms() / PHASE_MS) % 2 == 0;
        let d = if outbound { 1.0 } else { -1.0 };
        let head = if outbound { 0 } else { n - 1 };
        let Some(own) = ctx.position() else {
            sleep_ms(200).await;
            continue;
        };
        let y_hint = own.1.floor() as i32;

        if index == head {
            // --- head: draw the next waypoint along the bearing, skirt water, pick a gait ---
            let stride = vary(ctx, 8.0, 14.0);
            let weave = vary(ctx, -3.5, 3.5);
            let want = (
                own.0 + ux * d * stride + px * weave,
                own.2 + uz * d * stride + pz * weave,
            );
            match dry_target(ctx, want, (px, pz), y_hint) {
                Some((tx, tz, ty)) => {
                    let detoured = (tx - want.0).hypot(tz - want.1) > 1.0;
                    let climb = ty - y_hint;
                    let gait = ctx.rng_next();
                    if gait < 0.35 && n > 1 {
                        // Walk while keeping the column behind in frame.
                        let follower = if outbound { 1 } else { n - 2 };
                        ctx.move_to_watching_peer(tx, tz, follower, 12_000, 2.5)
                            .await;
                    } else if gait < 0.6 {
                        // Open-loop bout toward the waypoint, then close with the planner.
                        ctx.look((tx, own.1 + 1.4, tz), ctx.cfg().look_settle_ms.max(500))
                            .await;
                        let sprint =
                            ctx.rng_next() < sprint_prob.max(if climb > 1 { 0.7 } else { 0.0 });
                        let hold = vary(ctx, 1_200.0, 2_800.0) as u64;
                        ctx.hold_visual(WalkDirection::Forward, sprint, hold, 0)
                            .await;
                        travel_to(ctx, (tx, tz), TRAVEL_HOP, 2.5).await;
                    } else {
                        travel_to(ctx, (tx, tz), TRAVEL_HOP, 2.5).await;
                    }
                    ctx.record(
                        TYPE_MOVE,
                        &format!("vis-wade-head-{index}-{leg}"),
                        &format!(
                            "Lead the column {}: stride {} blocks{}.",
                            if outbound { "outbound" } else { "back" },
                            r2(stride),
                            if detoured {
                                ", detouring around water"
                            } else {
                                ""
                            }
                        ),
                        json!({"leg": leg, "outbound": outbound, "detoured": detoured,
                            "climb": climb, "target": {"x": r2(tx), "z": r2(tz)}}),
                    );
                }
                None => {
                    // Water everywhere ahead: read the shoreline, then sidestep along it.
                    ctx.look(
                        (want.0, own.1 + 0.2, want.1),
                        ctx.cfg().look_settle_ms.max(700),
                    )
                    .await;
                    sleep_ms(vary(ctx, 600.0, 1_400.0) as u64).await;
                    let side = if ctx.rng_next() < 0.5 { 1.0 } else { -1.0 };
                    let hop = vary(ctx, 4.0, 8.0);
                    travel_to(
                        ctx,
                        (own.0 + px * side * hop, own.2 + pz * side * hop),
                        TRAVEL_HOP,
                        2.5,
                    )
                    .await;
                    ctx.record(
                        TYPE_MOVE,
                        &format!("vis-wade-blocked-{index}-{leg}"),
                        "Water across the whole front: study it, then work along the shore.",
                        json!({"leg": leg, "front": {"x": r2(want.0), "z": r2(want.1)}}),
                    );
                }
            }
            // Look back down the column — the head's signature reverse-angle beat.
            if ctx.rng_next() < 0.35 && n > 1 {
                let follower = if outbound { 1 } else { n - 2 };
                let settle = vary(ctx, 700.0, 1_600.0) as u64;
                look_at_peer(ctx, follower, settle).await;
                sleep_ms(vary(ctx, 400.0, 1_000.0) as u64).await;
            }
        } else {
            // --- follower: hold station behind the neighbour ahead in the current direction ---
            let ahead = if outbound { index - 1 } else { index + 1 };
            match ctx.peer_position(ahead) {
                Some(pp) => {
                    let dist = (pp.0 - own.0).hypot(pp.2 - own.2);
                    // Hysteresis: chase only once the gap has opened well past FOLLOW_GAP, so a
                    // catch-up leg is always several blocks — round 1 failed key chatter (64% of
                    // presses under 0.5 s) on exactly the short corrective closes this replaces.
                    if dist < FOLLOW_GAP + 3.0 {
                        // In station: watch the column; any body motion is a HELD perpendicular
                        // walk bout (one press well over the chatter bar), forced after 8 s of
                        // standing so the frozen gate never gets close either.
                        let stood_ms = ctx.elapsed_ms() - stall_anchor.1;
                        if stood_ms > 8_000 || ctx.rng_next() < 0.3 {
                            let mut side = if ctx.rng_next() < 0.5 { 1.0 } else { -1.0 };
                            let probe = (own.0 + px * side * 3.0, own.2 + pz * side * 3.0);
                            if land_feet_y(
                                ctx,
                                probe.0.floor() as i32,
                                probe.1.floor() as i32,
                                y_hint,
                            )
                            .is_none()
                            {
                                side = -side; // shoreline on that flank: bout the other way
                            }
                            ctx.look(
                                (
                                    own.0 + px * side * 12.0,
                                    own.1 + 1.4,
                                    own.2 + pz * side * 12.0,
                                ),
                                ctx.cfg().look_settle_ms.max(400),
                            )
                            .await;
                            let bout = vary(ctx, 600.0, 1_100.0) as u64;
                            ctx.hold_visual(WalkDirection::Forward, false, bout, 0)
                                .await;
                        }
                        if ctx.rng_next() < 0.6 {
                            look_at_peer(ctx, ahead, 400).await;
                        } else {
                            glance_at_nearest_peer(ctx, Some(ahead)).await;
                        }
                        sleep_ms(vary(ctx, 300.0, 900.0) as u64).await;
                    } else {
                        // Close up to FOLLOW_GAP short of the bot ahead, on dry ground.
                        let (dx, dz) = ((pp.0 - own.0) / dist, (pp.2 - own.2) / dist);
                        let want = (pp.0 - dx * FOLLOW_GAP * 0.8, pp.2 - dz * FOLLOW_GAP * 0.8);
                        let target = dry_target(ctx, want, (px, pz), y_hint)
                            .map(|(x, z, _)| (x, z))
                            .unwrap_or(want);
                        if ctx.rng_next() < 0.45 {
                            ctx.move_to_watching_peer(target.0, target.1, ahead, 10_000, 2.0)
                                .await;
                        } else {
                            travel_to(ctx, target, TRAVEL_HOP, 2.0).await;
                        }
                    }
                    if leg.is_multiple_of(4) {
                        ctx.record(
                            TYPE_MOVE,
                            &format!("vis-wade-follow-{index}-{leg}"),
                            &format!("Hold station in the column behind bot {ahead}."),
                            json!({"leg": leg, "ahead": ahead, "outbound": outbound,
                                "gap": r2(dist)}),
                        );
                    }
                }
                None => sleep_ms(300).await,
            }
        }

        // Anyone who ended a leg actually standing in water wades straight back out sideways.
        if in_water(ctx) {
            let out = DETOURS.iter().skip(1).find_map(|&off| {
                let x = own.0 + px * off;
                let z = own.2 + pz * off;
                land_feet_y(ctx, x.floor() as i32, z.floor() as i32, y_hint).map(|_| (x, z))
            });
            if let Some(out) = out {
                travel_to(ctx, out, TRAVEL_HOP, 2.0).await;
                ctx.record(
                    TYPE_MOVE,
                    &format!("vis-wade-out-{index}-{leg}"),
                    "Wade back out of the water onto the nearest dry ground.",
                    json!({"leg": leg, "target": {"x": r2(out.0), "z": r2(out.1)}}),
                );
            }
        }
        if ctx.rng_next() < 0.15 {
            glance_at_nearest_peer(ctx, None).await;
        }
        if let Some(p) = ctx.position() {
            let moved = (p.0 - stall_anchor.0.0).hypot(p.2 - stall_anchor.0.2)
                + (p.1 - stall_anchor.0.1).abs();
            if moved >= 1.5 {
                stall_anchor = (p, ctx.elapsed_ms());
            }
        }
        leg += 1;
    }
}
