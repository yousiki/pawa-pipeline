use super::*;

const TAU: f64 = std::f64::consts::TAU;
const WALK_BPS: f64 = 4.317;
const SPRINT_BPS: f64 = 5.612;

/// Draw in `[lo, hi)` from the per-bot stream, so a number this scenario could have hard-coded
/// varies per bot and per pulse and still replays from the seed.
fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

/// The nearest (or farthest) peer whose position has replicated into THIS process.
fn peer_extreme(ctx: &Ctx, farthest: bool) -> Option<usize> {
    let own = ctx.position()?;
    (0..ctx.player_count())
        .filter(|&j| j != ctx.index())
        .filter_map(|j| {
            ctx.peer_position(j)
                .map(|p| (j, (p.0 - own.0).powi(2) + (p.2 - own.2).powi(2)))
        })
        .reduce(|a, b| if (b.1 > a.1) == farthest { b } else { a })
        .map(|(j, _)| j)
}

/// Keep a moving peer framed for `hold_ms`, re-looking rather than freezing on a stale head.
async fn watch_peer(ctx: &Ctx, peer: usize, hold_ms: u64) {
    let deadline = Instant::now() + Duration::from_millis(hold_ms);
    while Instant::now() < deadline && !ctx.target_met() {
        if look_at_peer(ctx, peer, 350).await.is_none() {
            sleep_ms(300).await;
        }
    }
}

/// A triangle camera sweep of exactly `span_deg`: out to one edge, across, and back.
async fn sweep(ctx: &Ctx, span_deg: f64, pitch_deg: f64) {
    let rate = ctx.cfg().yaw_rate_deg_s.max(1.0);
    for (yaw, pitch) in [
        (span_deg * 0.5, pitch_deg),
        (-span_deg, -pitch_deg),
        (span_deg * 0.5, 0.0),
    ] {
        if ctx.target_met() {
            return;
        }
        let settle = (yaw.abs() / rate * 1000.0) as u64 + 700;
        ctx.drag_view(yaw, pitch, settle).await;
    }
}

/// Face the pulse centre at roughly eye height.
async fn face_centre(ctx: &Ctx, centre: (f64, f64)) {
    if let Some(p) = ctx.position() {
        ctx.look(
            (centre.0, p.1 + 1.0, centre.1),
            ctx.cfg().look_settle_ms.max(500),
        )
        .await;
    }
}

/// Distance from the bot to a 2D point.
fn dist_to(ctx: &Ctx, target: (f64, f64)) -> f64 {
    ctx.position()
        .map(|p| (target.0 - p.0).hypot(target.1 - p.2))
        .unwrap_or(0.0)
}

/// The whole swarm breathes around the anchor: each bot pulses between an inner gathering ring and
/// its own far scatter radius on its own slot spoke, with every dwell, radius, leg style and phase
/// flip drawn per bot — so the group reads as a desynchronized gather–scatter pulse, never a
/// synchronized formation. Leg styles decorrelate camera from travel on purpose: converge while
/// tracking the farthest peer, rush the centre open-loop, scatter BACKWARD with the camera pinned
/// on the collapsing group, sprint out on a free bearing, or sidestep a strafe arc at apogee.
pub(super) async fn run(ctx: &mut Ctx) {
    let index = ctx.index();
    let bot_count = ctx.player_count().max(1);
    let anchor = ctx.anchor();
    let slot_arc = TAU / bot_count as f64;
    let slot = index as f64 * slot_arc;
    // Inner ring wide enough that adjacent slots clear the peer gap even after 2 blocks of wander.
    let inner_base = station_ring(bot_count, 2.0);
    let mut inward = ctx.rng_next() < 0.5; // random start phase: the swarm never breathes in step
    let mut pulse = 0u32;
    let sprint_prob = ctx.cfg().sprint_bout_prob.max(0.3);
    ctx.record(
        TYPE_LOOK,
        &format!("vis-pulse-start-{index}"),
        "Converge–scatter pulse: breathe between the inner ring and a far scatter radius.",
        json!({"slot_deg": r2(slot.to_degrees()), "inner_radius": r2(inner_base),
            "start_phase": if inward { "converge" } else { "scatter" }}),
    );
    while !ctx.target_met() {
        // Target for this half-pulse. Tangential wander is capped near the centre (gap safety)
        // and freer out wide, where adjacent spokes are many blocks apart.
        let (radius, max_jitter, phase) = if inward {
            let r = inner_base + vary(ctx, -1.0, 2.0);
            (r, (0.3 * slot_arc).min(2.0 / r.max(1.0)), "converge")
        } else {
            (inner_base + vary(ctx, 8.0, 16.0), 0.3 * slot_arc, "scatter")
        };
        let ang = slot + vary(ctx, -max_jitter, max_jitter);
        let goal = (anchor.0 + ang.cos() * radius, anchor.1 + ang.sin() * radius);
        let dist = dist_to(ctx, goal);
        let timeout = ctx.cfg().move_timeout_ms.min(12_000);
        let style = (ctx.rng_next() * 3.0) as u32;
        let style_name = match (inward, style) {
            (true, 0) => match peer_extreme(ctx, true) {
                // Track the farthest peer while closing: the whole swarm is converging behind it.
                Some(peer) => {
                    ctx.move_to_watching_peer(goal.0, goal.1, peer, timeout, 2.5)
                        .await;
                    "watch_far_peer"
                }
                None => {
                    travel_to(ctx, goal, TRAVEL_HOP, 2.5).await;
                    "hops"
                }
            },
            (true, 1) => {
                // Rush the centre open-loop, then let the planner finish the approach.
                face_centre(ctx, anchor).await;
                let sprint = ctx.rng_next() < sprint_prob;
                let speed = if sprint { SPRINT_BPS } else { WALK_BPS };
                let hold = ((dist / speed * 1000.0) as u64).clamp(800, 8_000);
                ctx.hold_visual(WalkDirection::Forward, sprint, hold, 0)
                    .await;
                travel_to(ctx, goal, TRAVEL_HOP, 2.5).await;
                "rush_centre"
            }
            (true, _) => {
                travel_to(ctx, goal, TRAVEL_HOP, 2.5).await;
                "hops"
            }
            (false, 0) => {
                // Backpedal out: the body scatters while the camera stays pinned on the group.
                face_centre(ctx, anchor).await;
                let hold = ((dist / WALK_BPS * 1000.0) as u64).clamp(800, 7_000);
                ctx.hold_visual(WalkDirection::Backward, false, hold, 0)
                    .await;
                let own_y = ctx.position().map(|p| p.1).unwrap_or(64.0);
                ctx.move_to_watching_point(
                    goal.0,
                    goal.1,
                    (anchor.0, own_y + 1.0, anchor.1),
                    timeout,
                    2.5,
                )
                .await;
                "backpedal"
            }
            (false, 1) => {
                // Sprint burst out along the spoke, then settle onto the drawn radius.
                if let Some(p) = ctx.position() {
                    ctx.look(
                        (goal.0, p.1 + 1.2, goal.1),
                        ctx.cfg().look_settle_ms.max(500),
                    )
                    .await;
                }
                let sprint = ctx.rng_next() < 0.7;
                let speed = if sprint { SPRINT_BPS } else { WALK_BPS };
                let hold = ((dist / speed * 1000.0) as u64).clamp(800, 8_000);
                ctx.hold_visual(WalkDirection::Forward, sprint, hold, 0)
                    .await;
                travel_to(ctx, goal, TRAVEL_HOP, 2.5).await;
                "sprint_out"
            }
            (false, _) => match peer_extreme(ctx, false) {
                Some(peer) => {
                    ctx.move_to_watching_peer(goal.0, goal.1, peer, timeout, 2.5)
                        .await;
                    "watch_near_peer"
                }
                None => {
                    travel_to(ctx, goal, TRAVEL_HOP, 2.5).await;
                    "hops"
                }
            },
        };
        ctx.record(
            TYPE_MOVE,
            &format!("vis-pulse-leg-{index}-{pulse}"),
            &format!("Pulse {pulse}: {phase} leg ({style_name})."),
            json!({"pulse": pulse, "phase": phase, "style": style_name,
                "radius": r2(radius), "angle_deg": r2(ang.to_degrees()),
                "goal": {"x": r2(goal.0), "z": r2(goal.1)}}),
        );
        if ctx.target_met() {
            break;
        }
        // A leg that ended far short means the planner cannot route to the slot from where the
        // body stands (a dip, a cliff lip, a tree well) — travel_to hops shake themselves loose,
        // but the watching and open-loop styles do not, and an 8-bot run showed one such bot
        // freezing 25 s across stationary beats. Hop blind before pointing the camera anywhere.
        if dist_to(ctx, goal) > 6.0 {
            shake_loose(ctx).await;
        }
        // Camera beat at the pulse extreme, drawn independently of the leg that got here. The
        // strafe arc is apogee-only: at the inner ring a long sidestep could close on a neighbour.
        match (ctx.rng_next() * 4.0) as u32 {
            0 => {
                let span = vary(ctx, 90.0, 175.0);
                let pitch = vary(ctx, -12.0, 12.0);
                sweep(ctx, span, pitch).await;
                ctx.record(
                    TYPE_LOOK,
                    &format!("vis-pulse-sweep-{index}-{pulse}"),
                    "Read the breathing swarm with one camera sweep.",
                    json!({"pulse": pulse, "sweep_deg": r2(span), "pitch_deg": r2(pitch)}),
                );
            }
            1 => {
                // Watch the peer whose motion is most legible from here: the nearest at the
                // gathered extreme, the farthest from out wide.
                if let Some(peer) = peer_extreme(ctx, !inward) {
                    let hold = vary(ctx, 2_500.0, 5_500.0) as u64;
                    watch_peer(ctx, peer, hold).await;
                    ctx.record(
                        TYPE_LOOK,
                        &format!("vis-pulse-watch-{index}-{pulse}"),
                        &format!("Hold bot {peer} in frame at the pulse extreme."),
                        json!({"pulse": pulse, "peer": peer, "hold_ms": hold}),
                    );
                }
            }
            2 if !inward => {
                let dir = if ctx.rng_next() < 0.5 {
                    WalkDirection::Left
                } else {
                    WalkDirection::Right
                };
                face_centre(ctx, anchor).await;
                let hold = vary(ctx, 1_200.0, 2_200.0) as u64;
                ctx.hold_visual(dir, false, hold, 0).await;
                ctx.record(
                    TYPE_MOVE,
                    &format!("vis-pulse-strafe-{index}-{pulse}"),
                    "Sidestep a strafe arc at apogee with the camera on the centre.",
                    json!({"pulse": pulse, "hold_ms": hold,
                        "side": if matches!(dir, WalkDirection::Left) { "left" } else { "right" }}),
                );
            }
            _ => {
                glance_at_nearest_peer(ctx, None).await;
                let nod = vary(ctx, -18.0, 10.0);
                let span = vary(ctx, -60.0, 60.0);
                ctx.drag_view(span, nod, 1_500).await;
            }
        }
        // Rhythm: usually flip phase; sometimes hold the same phase for a beat so the pulse
        // period itself drifts per bot.
        if ctx.rng_next() >= 0.15 {
            inward = !inward;
        }
        pulse += 1;
    }
}
