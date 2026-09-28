use super::*;

const TAU: f64 = std::f64::consts::TAU;

/// Lateral lane a bot keeps off the shared track, by index parity — two streams of traffic that
/// pass beside each other at the waist instead of through the same cells.
const LANE: f64 = 1.2;

/// Below this 2D distance to the nearest peer a bot sidesteps outward before walking its next
/// leg, so a phase pile-up dissolves instead of compacting.
const CROWD_GAP: f64 = 4.0;

/// Draw in `[lo, hi)` from the per-bot stream, so a number this scenario could have hard-coded
/// varies per bot and per leg and still replays from the seed.
fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

/// Lobe radius sized so the evenly staggered swarm keeps ~6 blocks of arc between neighbours on
/// the 4-pi-r track, floored so a small swarm still draws a readable eight.
fn lobe_radius(bot_count: usize) -> f64 {
    (6.0 * bot_count.max(1) as f64 / (2.0 * TAU)).max(8.0)
}

/// The shared figure-eight track, sampled at `step` of `2 * steps_per_lobe` positions: lobe 0 is
/// walked counterclockwise, lobe 1 clockwise, and the two meet at the anchor — the waist every
/// bot threads twice per lap. `axis` orients the eight; the lobe centres sit `radius` out on
/// either side along it. Pure geometry, identical across bots — the stagger comes from each
/// bot's own phase.
fn track_point(
    anchor: (f64, f64),
    axis: f64,
    radius: f64,
    steps_per_lobe: usize,
    step: usize,
) -> (f64, f64) {
    let (ax, az) = (axis.cos(), axis.sin());
    let k = steps_per_lobe.max(1);
    let (lobe, i) = (step / k % 2, step % k);
    let phase = i as f64 / k as f64 * TAU;
    // In the axis-rotated frame: lobe 0 (centre -r along the axis) is walked CCW from the waist
    // (theta = phase; theta 0 points back at the anchor), lobe 1 (centre +r) CW from the waist
    // (theta = pi - phase). Opposite turn directions join the lobes with a continuous tangent —
    // a smooth eight, not two circles on a string.
    let (sign, theta) = if lobe == 0 {
        (-1.0, phase)
    } else {
        (1.0, std::f64::consts::PI - phase)
    };
    let (cx, cz) = (anchor.0 + ax * radius * sign, anchor.1 + az * radius * sign);
    let (tc, ts) = (theta.cos(), theta.sin());
    (
        cx + (ax * tc - az * ts) * radius,
        cz + (az * tc + ax * ts) * radius,
    )
}

/// Nearest live peer's index and 2D distance on this shard's roster.
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

/// Keep a moving peer framed for `hold_ms`, re-looking rather than freezing on a stale head.
async fn watch_peer(ctx: &Ctx, peer: usize, hold_ms: u64) {
    let deadline = Instant::now() + Duration::from_millis(hold_ms);
    while Instant::now() < deadline && !ctx.target_met() {
        if look_at_peer(ctx, peer, 350).await.is_none() {
            sleep_ms(300).await;
        }
    }
}

/// Triangle camera sweep of `span_deg` total: out to one edge, across to the other, back.
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

/// Every bot walks the SAME figure-eight track around the anchor, staggered evenly along it: two
/// tangent lobes walked with opposite turning directions, meeting at a shared waist that each
/// bot threads twice per lap while cross-lobe traffic passes beside it on its own lane. The
/// motion signature — smooth curvature that flips sign every half lap, plus a periodic dense
/// crossing point — matches no existing scenario: rings never self-intersect, orbits keep one
/// chirality, lanes never curve.
///
/// Zero world edits; the flat `build_arena` slab is the stage (round 2 on natural terrain passed
/// QC numerically but filmed ravine walls — the fixed track needs the open floor, and the eight's
/// full extent at the recorded swarm sizes stays inside the slab's radius-20 clearing).
///
/// No barriers: each bot advances its own step counter at its own pace, so the stagger breathes;
/// a bot that finds its next stretch crowded sidesteps outward and re-joins rather than queueing.
/// Legs are open-loop bearing holds along the local tangent with occasional pathfinder
/// corrections; cameras alternate between the path ahead, the peer traffic, and cross-waist
/// reads of the opposite lobe.
pub(super) async fn run(ctx: &mut Ctx) {
    let index = ctx.index();
    let n = ctx.player_count().max(1);
    let anchor = ctx.anchor();
    let radius = lobe_radius(n);
    // Legs of ~6 blocks: chord = 2 pi r / steps.
    let steps_per_lobe = ((TAU * radius) / 6.0).ceil() as usize;
    let track_len = steps_per_lobe * 2;
    // The eight's orientation must be identical across bots and `rng_next` is a per-bot stream,
    // so it is derived from the shared anchor coordinates instead of drawn.
    let axis = ((anchor.0 * 0.37 + anchor.1 * 0.61).sin() * TAU).rem_euclid(TAU);
    // Fixed per-bot lane keeps opposing waist traffic in two streams.
    let lane = if index.is_multiple_of(2) { LANE } else { -LANE };
    let mut step = index * track_len / n;
    let settle = ctx.cfg().look_settle_ms.max(400);
    let timeout = ctx.cfg().move_timeout_ms.min(8_000);
    ctx.record(
        TYPE_LOOK,
        &format!("vis-eight-open-{index}"),
        "Figure-eight weave: walk the shared two-lobe track, threading the waist each half lap.",
        json!({"lobe_radius": r2(radius), "track_steps": track_len,
            "start_step": step, "lane": r2(lane), "axis_deg": r2(axis.to_degrees())}),
    );
    // Walk to the starting slot with the pathfinder; the loop then stays open-loop.
    let start = track_point(anchor, axis, radius, steps_per_lobe, step % track_len);
    ctx.move_to(start.0, start.1, timeout, 2.0).await;

    let mut leg = 0u32;
    // Consecutive stall-cut holds against the same obstacle (a peer's body, the arena rim).
    // Round 1 on natural terrain measured a bot shoving one cliff for an 18.3 s frozen run and a
    // 5.2 s stuck total because nothing ever routed it AROUND the blockage.
    let mut stalls = 0u32;
    while !ctx.target_met() {
        step = (step + 1) % track_len;
        let raw = track_point(anchor, axis, radius, steps_per_lobe, step);
        // Push the waypoint outward from the nearer lobe centre by the bot's lane, so opposing
        // streams pass beside each other at the waist.
        let lobe = step / steps_per_lobe % 2;
        let sign = if lobe == 0 { -1.0 } else { 1.0 };
        let centre = (
            anchor.0 + axis.cos() * radius * sign,
            anchor.1 + axis.sin() * radius * sign,
        );
        let d = (raw.0 - centre.0).hypot(raw.1 - centre.1).max(0.1);
        let jitter = vary(ctx, -0.6, 0.6);
        let target = (
            raw.0 + (raw.0 - centre.0) / d * (lane + jitter),
            raw.1 + (raw.1 - centre.1) / d * (lane + jitter),
        );

        // Crowd guard: if the next stretch is packed, sidestep outward with ONE HELD press and
        // let the gap open. The first 8-bot round did this hop with the pathfinder, and those
        // 2.5-block goals recorded as sub-0.5 s taps — one POV crossed the 35% chatter cap on
        // them alone (40.3%), since the dense waist triggers the guard constantly at that size.
        if let (Some(own), Some((_, dist))) = (ctx.position(), nearest_peer(ctx))
            && dist < CROWD_GAP
        {
            let (ox, oz) = (own.0 - anchor.0, own.2 - anchor.1);
            let om = ox.hypot(oz).max(0.1);
            ctx.look(
                (own.0 + ox / om * 12.0, own.1 + 1.3, own.2 + oz / om * 12.0),
                settle,
            )
            .await;
            let hold = vary(ctx, 900.0, 1_300.0) as u64;
            ctx.hold_visual(WalkDirection::Forward, false, hold, 0)
                .await;
        }

        // The leg itself: mostly an open-loop tangent hold; every few legs a pathfinder
        // correction re-pins the bot to the track, sometimes with the camera on a peer.
        let correction = leg % 4 == 3;
        if correction {
            let social = ctx.rng_next() < 0.6;
            if social && let Some((peer, _)) = nearest_peer(ctx) {
                ctx.move_to_watching_peer(target.0, target.1, peer, timeout.min(5_000), 1.6)
                    .await;
            } else {
                ctx.move_to(target.0, target.1, timeout.min(5_000), 1.6)
                    .await;
            }
        } else if let Some(p) = ctx.position() {
            let dist = (target.0 - p.0).hypot(target.1 - p.2);
            if dist > 1.5 {
                // Face a point past the waypoint along the walking direction, so consecutive
                // holds read as one smooth curve rather than pivots at every chord.
                let (ux, uz) = ((target.0 - p.0) / dist, (target.1 - p.2) / dist);
                ctx.look((p.0 + ux * 16.0, p.1 + 1.3, p.2 + uz * 16.0), settle)
                    .await;
                let sprint = ctx.rng_next() < ctx.cfg().sprint_bout_prob * 0.6;
                let speed = if sprint { 5.6 } else { 4.3 };
                let hold = ((dist / speed * 1000.0) as u64).clamp(800, 2_600);
                if ctx
                    .hold_visual(WalkDirection::Forward, sprint, hold, 400)
                    .await
                {
                    stalls = 0;
                } else {
                    stalls += 1;
                }
            }
        }
        // Escape hatch: two stall-cut holds running means the track ahead is blocked from where
        // the body stands. Shake loose, skip a few steps past the blockage, and let the
        // pathfinder re-pin — grinding the same cliff is what the round-1 stuck failure was.
        if stalls >= 2 {
            shake_loose(ctx).await;
            step = (step + 3) % track_len;
            let ahead = track_point(anchor, axis, radius, steps_per_lobe, step);
            ctx.move_to(ahead.0, ahead.1, timeout.min(6_000), 2.5).await;
            stalls = 0;
            ctx.record(
                TYPE_MOVE,
                &format!("vis-eight-detour-{index}-{leg}"),
                "Detour around blocked terrain and rejoin the track further along.",
                json!({"leg": leg, "rejoin_step": step}),
            );
        }
        ctx.record(
            TYPE_MOVE,
            &format!("vis-eight-leg-{index}-{leg}"),
            &format!(
                "Walk track step {step}/{track_len} of the figure-eight ({}).",
                if lobe == 0 { "left lobe" } else { "right lobe" }
            ),
            json!({"leg": leg, "step": step, "lobe": lobe,
                "target": {"x": r2(target.0), "z": r2(target.1)}}),
        );
        if ctx.target_met() {
            break;
        }

        // Garnish, drawn sparsely so the flow keeps flowing: read the opposite lobe across the
        // waist, track passing traffic, or sweep the whole eight from the lobe's far rim.
        let roll = ctx.rng_next();
        if roll < 0.14 {
            let feet = ctx.position().map(|p| p.1).unwrap_or(64.0);
            let far = (
                anchor.0 - (ctx.position().map(|p| p.0).unwrap_or(anchor.0) - anchor.0),
                anchor.1 - (ctx.position().map(|p| p.2).unwrap_or(anchor.1) - anchor.1),
            );
            ctx.look_default((far.0, feet + 1.0, far.1)).await;
            sleep_ms(vary(ctx, 500.0, 1_100.0) as u64).await;
            ctx.record(
                TYPE_LOOK,
                &format!("vis-eight-cross-read-{index}-{leg}"),
                "Read the opposite lobe across the waist.",
                json!({"leg": leg}),
            );
        } else if roll < 0.24 {
            if let Some((peer, _)) = nearest_peer(ctx) {
                let hold = vary(ctx, 1_200.0, 2_400.0) as u64;
                watch_peer(ctx, peer, hold).await;
                ctx.record(
                    TYPE_LOOK,
                    &format!("vis-eight-traffic-{index}-{leg}"),
                    &format!("Track bot {peer} passing on the weave."),
                    json!({"leg": leg, "peer": peer, "hold_ms": hold}),
                );
            }
        } else if roll < 0.31 {
            let span = vary(ctx, 100.0, 180.0);
            let pitch = vary(ctx, -8.0, 8.0);
            sweep(ctx, span, pitch).await;
            ctx.record(
                TYPE_LOOK,
                &format!("vis-eight-sweep-{index}-{leg}"),
                "Sweep the whole eight from the rim.",
                json!({"leg": leg, "sweep_deg": r2(span)}),
            );
        }
        leg += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::track_point;

    /// The bug this pins: a wrong lobe-1 parameterization starts that lobe at the FAR side of
    /// its circle, turning the eight into two circles joined by a 2r jump. Both lobes must enter
    /// and leave at the waist (the anchor), every sample must sit on its lobe's circle, and
    /// consecutive samples must stay one short chord apart — including across the two seams.
    #[test]
    fn track_is_a_closed_eight_with_short_chords() {
        let anchor = (100.0, -40.0);
        let (axis, radius, k) = (0.83_f64, 9.0, 10_usize);
        let total = 2 * k;
        let near = |a: (f64, f64), b: (f64, f64)| (a.0 - b.0).hypot(a.1 - b.1) < 1e-9;
        // Both lobes start at the waist.
        assert!(near(track_point(anchor, axis, radius, k, 0), anchor));
        assert!(near(track_point(anchor, axis, radius, k, k), anchor));
        let max_chord = 2.0 * radius * (std::f64::consts::PI / k as f64).sin() + 1e-9;
        for step in 0..total {
            let p = track_point(anchor, axis, radius, k, step);
            // On one of the two lobe circles.
            let (ax, az) = (axis.cos(), axis.sin());
            let d0 = (p.0 - (anchor.0 - ax * radius)).hypot(p.1 - (anchor.1 - az * radius));
            let d1 = (p.0 - (anchor.0 + ax * radius)).hypot(p.1 - (anchor.1 + az * radius));
            assert!(
                (d0 - radius).abs() < 1e-9 || (d1 - radius).abs() < 1e-9,
                "step {step} off both lobes: {d0} {d1}"
            );
            // One short chord to the next sample, wrapping across both seams.
            let q = track_point(anchor, axis, radius, k, (step + 1) % total);
            let chord = (q.0 - p.0).hypot(q.1 - p.1);
            assert!(
                chord <= max_chord,
                "step {step}: chord {chord} > {max_chord}"
            );
        }
    }
}
