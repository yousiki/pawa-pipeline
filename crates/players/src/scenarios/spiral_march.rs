use super::*;

const TAU: f64 = std::f64::consts::TAU;

/// Widest radial excursion any curve may take from its own centre. The centre ring is sized so two
/// neighbours both at full excursion still clear [`MIN_PEER_GAP`].
const R_MAX: f64 = 8.5;
/// Tightest turn of any curve — below this the arc-length step degenerates into pirouettes.
const R_MIN: f64 = 2.5;

const CURVE_NAMES: [&str; 3] = ["breathing_spiral", "petal", "involute"];
const CAMERA_NAMES: [&str; 3] = ["path", "peer", "skyline"];

fn rand_range(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

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

/// Ring of per-bot curve centres: adjacent centres must clear the peer gap even with both
/// neighbours at full radial excursion toward each other. `ring_radius` cannot express that (its
/// floor then scaling overshoots badly at small swarms), so size the chord directly.
fn centre_ring(bot_count: usize) -> f64 {
    let n = bot_count.max(2) as f64;
    let chord = MIN_PEER_GAP + 2.0 * R_MAX;
    (chord / (2.0 * (std::f64::consts::PI / n).sin())).max(R_MAX + 6.0)
}

/// Radius of the marching curve at parameter `theta`, always inside `R_MIN..=r_max`.
fn radius_at(kind: usize, theta: f64, r_max: f64, lobes: f64) -> f64 {
    let span = r_max - R_MIN;
    match kind {
        // Breathing spiral: wind outward for a full turn, back inward for the next.
        0 => {
            let phase = theta.rem_euclid(2.0 * TAU) / (2.0 * TAU);
            let tri = 1.0 - (2.0 * phase - 1.0).abs();
            R_MIN + span * tri
        }
        // Rose petals: sweep out and back `lobes` times per revolution.
        1 => R_MIN + span * (theta * lobes * 0.5).sin().abs(),
        // Involute-ish: unwind outward over 1.5 turns, then cut straight back through the middle.
        _ => {
            let phase = theta.rem_euclid(1.5 * TAU) / (1.5 * TAU);
            R_MIN + span * phase.sqrt()
        }
    }
}

/// Face the world-space direction `(dx, dz)` (need not be normalised).
async fn face_direction(ctx: &Ctx, dx: f64, dz: f64) {
    if let Some(p) = ctx.position() {
        let m = dx.hypot(dz).max(0.001);
        let settle = ctx.cfg().look_settle_ms.max(500);
        ctx.look(
            (p.0 + dx / m * 24.0, p.1 + 1.62, p.2 + dz / m * 24.0),
            settle,
        )
        .await;
    }
}

/// Each bot marches a private parametric curve — breathing spiral, rose petal, or involute — around
/// its own centre on a shared ring, with per-bot chirality, pitch, lobe count, arc speed and start
/// phase, so no two paths ever agree. The camera style switches per segment between following the
/// path, pinning a peer, and panning a skyline point, and the march is punctuated by sprint dashes
/// and hard reversals that re-seat the parameter on wherever the body actually ended up.
pub(crate) async fn run(ctx: &mut Ctx) {
    let index = ctx.index();
    let bot_count = ctx.player_count().max(1);
    let anchor = ctx.anchor();
    let ring = centre_ring(bot_count);
    let slot = index as f64 / bot_count as f64 * TAU;
    let centre = (anchor.0 + slot.cos() * ring, anchor.1 + slot.sin() * ring);

    // The whole curve family is a per-bot draw: kind, spin, extent, lobes, speed, phase.
    let kind = ((ctx.rng_next() * 3.0) as usize).min(2);
    let mut spin = if ctx.rng_next() < 0.5 { 1.0 } else { -1.0 };
    let r_max = rand_range(ctx, 5.5, R_MAX);
    let lobes = 2.0 + (ctx.rng_next() * 3.0).floor();
    let leg_len = rand_range(ctx, 3.0, 5.0);
    let mut theta = ctx.rng_next() * TAU;
    let sprint_prob = ctx.cfg().sprint_bout_prob.clamp(0.05, 0.18);

    ctx.record(
        TYPE_MOVE,
        &format!("vis-spiral-plan-{index}"),
        "Spiral march: walk a private parametric curve around an own centre on a shared ring.",
        json!({
            "curve": CURVE_NAMES[kind],
            "spin": spin,
            "r_max": r2(r_max),
            "lobes": lobes,
            "leg_len": r2(leg_len),
            "centre": {"x": r2(centre.0), "z": r2(centre.1)},
        }),
    );

    // Spawn is at the anchor; the curve centre is out on the ring. Approach hop-wise first.
    travel_to(ctx, centre, TRAVEL_HOP, r_max).await;
    // The curve's home plane. Every terrain probe below is pinned to THIS height, never to the
    // bot's current feet: a per-leg re-anchored guard legitimizes a gradual slope descent two
    // blocks at a time (measured on seed 2026 round 2: p00 walked its waypoints down a ravine
    // slope and finished the episode in the dark at its bottom).
    let mut plane_y = ctx.position().map(|p| p.1).unwrap_or(64.0);

    let mut leg = 0u32;
    let mut seg_left = 0u32;
    let mut camera = 0usize;
    let mut skyline = (centre.0, 80.0, centre.1);
    while !ctx.target_met() {
        // Fallen well below the plane anyway (a mis-planned drop, a shove): climb back toward
        // the centre hop-wise before marching on, and only accept the lower plane when the climb
        // cannot recover the height — keep moving over freezing, but never dance in the dark by
        // default.
        if let Some(p) = ctx.position()
            && p.1 < plane_y - 4.0
        {
            travel_to(ctx, centre, TRAVEL_HOP, 3.0).await;
            if let Some(q) = ctx.position()
                && q.1 < plane_y - 4.0
            {
                plane_y = q.1;
            }
        }
        // A segment holds one camera style for a handful of legs.
        if seg_left == 0 {
            seg_left = 3 + (ctx.rng_next() * 4.0) as u32;
            camera = ((ctx.rng_next() * 3.0) as usize).min(2);
            if camera == 2
                && let Some(p) = ctx.position()
            {
                let bearing = ctx.rng_next() * TAU;
                skyline = (
                    p.0 + bearing.cos() * 60.0,
                    p.1 + rand_range(ctx, 6.0, 14.0),
                    p.2 + bearing.sin() * 60.0,
                );
            }
            ctx.record(
                TYPE_LOOK,
                &format!("vis-spiral-cam-{index}-{leg}"),
                &format!("Switch camera style to {}.", CAMERA_NAMES[camera]),
                json!({"camera": CAMERA_NAMES[camera], "legs": seg_left}),
            );
            if ctx.rng_next() < 0.4 {
                glance_at_nearest_peer(ctx, None).await;
            }
        }
        seg_left -= 1;

        // Advance the parameter by one arc-length step and walk to the new curve point. A curve
        // that crosses a ravine or cliff face must not be followed into it — refuse waypoints
        // with no walkable cell near the curve's HOME plane and march the parameter past the bad
        // sector instead.
        let feet_y = plane_y.floor() as i32;
        let mut goal = None;
        for _ in 0..4 {
            let r_here = radius_at(kind, theta, r_max, lobes).max(3.0);
            theta += spin * (leg_len / r_here);
            let r = radius_at(kind, theta, r_max, lobes);
            let candidate = (centre.0 + theta.cos() * r, centre.1 + theta.sin() * r);
            if let Some(w) = walkable_xz_near(ctx, candidate, feet_y) {
                goal = Some(w);
                break;
            }
        }
        let Some(goal) = goal else {
            // The whole sector ahead is unwalkable from here: walk a probed free bearing instead
            // of standing still (falling back to the blind hop), then re-probe next iteration.
            let mut heading = None;
            for _ in 0..3 {
                let h = ctx.rng_next() * TAU;
                if let Some(p) = ctx.position()
                    && walkable_xz_near(ctx, (p.0 + h.cos() * 5.0, p.2 + h.sin() * 5.0), feet_y)
                        .is_some()
                {
                    heading = Some(h);
                    break;
                }
            }
            if let Some(h) = heading {
                face_direction(ctx, h.cos(), h.sin()).await;
                ctx.hold_visual(WalkDirection::Forward, false, 1_500, 0)
                    .await;
            } else {
                shake_loose(ctx).await;
            }
            leg += 1;
            continue;
        };
        match camera {
            1 if nearest_peer(ctx).is_some() => {
                let peer = nearest_peer(ctx).unwrap();
                ctx.move_to_watching_peer(goal.0, goal.1, peer, 6_000, 1.8)
                    .await;
            }
            2 => {
                ctx.move_to_watching_point(goal.0, goal.1, skyline, 6_000, 1.8)
                    .await;
            }
            _ => {
                ctx.move_to(goal.0, goal.1, 6_000, 1.8).await;
            }
        }
        leg += 1;
        if ctx.target_met() {
            break;
        }

        // Punctuation: an open-loop sprint dash off the curve, or a hard reversal. Both re-seat
        // the parameter on the body's real bearing afterwards so the march resumes from where the
        // bot actually is instead of dragging a stale plan.
        let event = ctx.rng_next();
        if event < sprint_prob {
            // Same terrain rule as the waypoints, probed at TWO distances along the bearing and
            // against the home plane: a 2.4 s sprint covers ~13 blocks, so a single 6-block probe
            // still let a full dash overshoot into the ravine behind it. A bearing whose far
            // probe fails gets only a short dash.
            let mut dash = None;
            for _ in 0..3 {
                let h = ctx.rng_next() * TAU;
                let Some(p) = ctx.position() else { break };
                let probe = |ctx: &Ctx, d: f64| {
                    walkable_xz_near(ctx, (p.0 + h.cos() * d, p.2 + h.sin() * d), feet_y).is_some()
                };
                if !probe(ctx, 4.0) {
                    continue;
                }
                let dash_ms = if probe(ctx, 8.0) && probe(ctx, 12.0) {
                    rand_range(ctx, 1_200.0, 2_400.0)
                } else {
                    rand_range(ctx, 600.0, 850.0)
                };
                dash = Some((h, dash_ms as u64));
                break;
            }
            if let Some((dash_heading, dash_ms)) = dash {
                face_direction(ctx, dash_heading.cos(), dash_heading.sin()).await;
                ctx.hold_visual(WalkDirection::Forward, true, dash_ms, 0)
                    .await;
                ctx.record(
                    TYPE_MOVE,
                    &format!("vis-spiral-dash-{index}-{leg}"),
                    "Sprint dash off the curve before rejoining it.",
                    json!({"heading_deg": r2(dash_heading.to_degrees()), "dash_ms": dash_ms}),
                );
            }
        } else if event < sprint_prob + 0.08 {
            spin = -spin;
            let snap = rand_range(ctx, 60.0, 140.0) * spin;
            let wobble = rand_range(ctx, -8.0, 8.0);
            ctx.drag_view(snap, wobble, 2_500).await;
            ctx.record(
                TYPE_MOVE,
                &format!("vis-spiral-turn-{index}-{leg}"),
                "Hard reversal: flip the curve's chirality with a camera snap.",
                json!({"spin": spin, "snap_deg": r2(snap)}),
            );
        } else {
            continue;
        }
        if let Some(p) = ctx.position() {
            theta = (p.2 - centre.1).atan2(p.0 - centre.0);
        }
    }
}
