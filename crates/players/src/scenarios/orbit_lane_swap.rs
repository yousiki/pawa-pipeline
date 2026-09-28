use super::*;

const TAU: f64 = std::f64::consts::TAU;

/// Radial gap between the two lanes. Minus both bots' worth of lane jitter this still clears
/// [`MIN_PEER_GAP`], so an overtake across the lane gap never closes under the corpus floor.
const LANE_GAP: f64 = 6.0;

/// Widest radial wobble a leg may add to its lane radius.
const JITTER: f64 = 0.6;

/// Two bots on opposite lanes, both jittered toward each other, must still clear the peer gap.
const _: () = assert!(LANE_GAP - 2.0 * JITTER >= MIN_PEER_GAP - 0.5);

/// Below this 2D distance to the nearest peer a bot sidesteps outward before its next leg.
const CROWD_GAP: f64 = 4.0;

/// Clearance a lane-change target must have from every peer before the migration starts, so a
/// bot never merges into a body already holding that stretch of the other lane.
const MERGE_CLEAR: f64 = 5.0;

/// Draw in `[lo, hi)` from the per-bot stream, so a number this scenario could have hard-coded
/// varies per bot and per leg and still replays from the seed.
fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

/// Radius of `lane`. The inner lane is sized for the WHOLE swarm, so even the worst case — every
/// bot migrated into one lane on evenly spread slots — still clears the peer-gap chord.
fn lane_radius(lane: usize, n: usize) -> f64 {
    ring_radius(n) + lane as f64 * LANE_GAP
}

/// Nearest live peer on this shard — the crowd-guard input.
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

/// Whether no peer stands within `clear` blocks of the 2D point — the merge check.
fn point_clear(ctx: &Ctx, point: (f64, f64), clear: f64) -> bool {
    (0..ctx.player_count())
        .filter(|&j| j != ctx.index())
        .filter_map(|j| ctx.peer_position(j))
        .all(|q| (q.0 - point.0).hypot(q.2 - point.1) >= clear)
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

/// Every bot circulates one of two concentric lanes around the anchor — SAME direction, same
/// tangential speed, so the inner lane's angular rate is higher and inner traffic continuously
/// overtakes outer traffic — and on a private random schedule each bot changes lanes: a radial
/// merge across the gap that swaps its radius and angular rate. The motion signature is highway
/// lane-changing bent into a circle. Distinct from every merged rotation scene: rings/patrols
/// keep fixed radii for a whole term, ring_slot_exchange crosses THROUGH the centre in discrete
/// rounds, counter-flow passes head-on — here traffic is one-way and the event is the merge.
///
/// Zero world edits; the flat `build_arena` slab is the stage (fixed circular track).
///
/// No barriers: angles advance on each bot's own step counter, merge timers are per-bot draws,
/// and a blocked merge just re-arms instead of waiting on anyone. Legs are open-loop tangent
/// holds with periodic pathfinder corrections; cameras rotate between the path ahead, the
/// traffic in the other lane, mirror-checks over the shoulder before merging, and free sweeps.
pub(super) async fn run(ctx: &mut Ctx) {
    let index = ctx.index();
    let n = ctx.player_count().max(1);
    let anchor = ctx.anchor();
    let mut lane = index % 2;
    let mut angle = index as f64 / n as f64 * TAU + vary(ctx, 0.0, 0.3);
    let mut next_merge_ms = ctx.elapsed_ms() + vary(ctx, 8_000.0, 24_000.0) as i64;
    let settle = ctx.cfg().look_settle_ms.max(400);
    let timeout = ctx.cfg().move_timeout_ms.min(8_000);
    ctx.record(
        TYPE_LOOK,
        &format!("vis-lane-open-{index}"),
        &format!("Orbit lane swap: circulate lane {lane} and merge across on a private timer."),
        json!({"lane": lane, "inner_radius": r2(lane_radius(0, n)),
            "outer_radius": r2(lane_radius(1, n))}),
    );
    let start_r = lane_radius(lane, n);
    ctx.move_to(
        anchor.0 + angle.cos() * start_r,
        anchor.1 + angle.sin() * start_r,
        timeout,
        2.0,
    )
    .await;

    let mut leg = 0u32;
    let mut stalls = 0u32;
    while !ctx.target_met() {
        let radius = lane_radius(lane, n);
        let chord = vary(ctx, 4.5, 6.0);
        angle += chord / radius;
        let r = radius + vary(ctx, -JITTER, JITTER);
        let target = (anchor.0 + angle.cos() * r, anchor.1 + angle.sin() * r);

        // Crowd guard: if the next stretch is packed (an overtake converging on the same arc),
        // sidestep AWAY FROM THE PEER with one held press. Round-1 8-bot sidestepped outward
        // from the anchor instead, so a same-speed convoy pair co-drifted leg after leg into
        // the arena rim and wedged there (30.2 s and 24.1 s frozen runs); separating along the
        // pair axis breaks the convoy, and a stalled guard hold now feeds the escape counter.
        if let (Some(own), Some((peer, dist))) = (ctx.position(), nearest_peer(ctx))
            && dist < CROWD_GAP
            && let Some(pp) = ctx.peer_position(peer)
        {
            let (ux, uz) = away_unit(Some(own), pp);
            ctx.look((own.0 + ux * 12.0, own.1 + 1.3, own.2 + uz * 12.0), settle)
                .await;
            let hold = vary(ctx, 900.0, 1_300.0) as u64;
            if !ctx
                .hold_visual(WalkDirection::Forward, false, hold, 0)
                .await
            {
                stalls += 1;
            }
        }

        // Merge beat: timer fired, the slot across the gap is clear — mirror-check the other
        // lane, then cut across radially with the camera on the traffic being merged into.
        if ctx.elapsed_ms() >= next_merge_ms {
            let to_lane = lane ^ 1;
            let to_r = lane_radius(to_lane, n);
            let merge = (anchor.0 + angle.cos() * to_r, anchor.1 + angle.sin() * to_r);
            if point_clear(ctx, merge, MERGE_CLEAR) {
                // Over-the-shoulder mirror check at the lane being entered.
                ctx.look_default((
                    merge.0,
                    ctx.position().map(|p| p.1).unwrap_or(64.0) + 1.0,
                    merge.1,
                ))
                .await;
                sleep_ms(vary(ctx, 300.0, 800.0) as u64).await;
                let social = ctx.rng_next() < 0.5;
                if social && let Some((peer, _)) = nearest_peer(ctx) {
                    ctx.move_to_watching_peer(merge.0, merge.1, peer, timeout.min(6_000), 1.6)
                        .await;
                } else {
                    ctx.move_to(merge.0, merge.1, timeout.min(6_000), 1.6).await;
                }
                lane = to_lane;
                next_merge_ms = ctx.elapsed_ms() + vary(ctx, 12_000.0, 28_000.0) as i64;
                ctx.record(
                    TYPE_MOVE,
                    &format!("vis-lane-merge-{index}-{leg}"),
                    &format!("Merge across the gap into lane {lane}."),
                    json!({"leg": leg, "lane": lane, "radius": r2(to_r)}),
                );
                leg += 1;
                continue;
            }
            // Occupied: re-arm shortly and keep circulating rather than waiting on the peer.
            next_merge_ms = ctx.elapsed_ms() + vary(ctx, 3_000.0, 6_000.0) as i64;
        }

        // The leg: mostly an open-loop tangent hold; every few legs a pathfinder correction
        // re-pins the bot to its lane, sometimes with the camera on the other lane's traffic.
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
        if stalls >= 2 {
            shake_loose(ctx).await;
            angle += 3.0 * chord / radius;
            let ahead = (
                anchor.0 + angle.cos() * radius,
                anchor.1 + angle.sin() * radius,
            );
            ctx.move_to(ahead.0, ahead.1, timeout.min(6_000), 2.5).await;
            stalls = 0;
            ctx.record(
                TYPE_MOVE,
                &format!("vis-lane-detour-{index}-{leg}"),
                "Detour around blocked terrain and rejoin the lane further along.",
                json!({"leg": leg}),
            );
        }
        ctx.record(
            TYPE_MOVE,
            &format!("vis-lane-leg-{index}-{leg}"),
            &format!("Circulate lane {lane} one chord further."),
            json!({"leg": leg, "lane": lane, "angle": r2(angle),
                "target": {"x": r2(target.0), "z": r2(target.1)}}),
        );
        if ctx.target_met() {
            break;
        }

        // Garnish: track the other lane's traffic, glance at the stretch of lane being overtaken
        // (or overtaking), or sweep the whole two-lane ring.
        let roll = ctx.rng_next();
        if roll < 0.15 {
            if let Some((peer, dist)) = nearest_peer(ctx) {
                let hold = vary(ctx, 1_200.0, 2_400.0) as u64;
                watch_peer(ctx, peer, hold).await;
                ctx.record(
                    TYPE_LOOK,
                    &format!("vis-lane-traffic-{index}-{leg}"),
                    &format!("Track bot {peer} in the passing traffic."),
                    json!({"leg": leg, "peer": peer, "distance": r2(dist), "hold_ms": hold}),
                );
            }
        } else if roll < 0.25 {
            if let Some(p) = ctx.position() {
                let far = (anchor.0 - (p.0 - anchor.0), anchor.1 - (p.2 - anchor.1));
                ctx.look_default((far.0, p.1 + 1.0, far.1)).await;
                sleep_ms(vary(ctx, 500.0, 1_100.0) as u64).await;
                ctx.record(
                    TYPE_LOOK,
                    &format!("vis-lane-cross-read-{index}-{leg}"),
                    "Read the far side of the two-lane ring.",
                    json!({"leg": leg}),
                );
            }
        } else if roll < 0.32 {
            let span = vary(ctx, 100.0, 180.0);
            let pitch = vary(ctx, -8.0, 8.0);
            sweep(ctx, span, pitch).await;
            ctx.record(
                TYPE_LOOK,
                &format!("vis-lane-sweep-{index}-{leg}"),
                "Sweep the two-lane ring from the current arc.",
                json!({"leg": leg, "sweep_deg": r2(span)}),
            );
        }
        leg += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::{LANE_GAP, lane_radius};
    use crate::scenarios::common::MIN_PEER_GAP;

    /// The spacing this file promises: even with every bot merged into one lane on evenly spread
    /// slots the chord clears the peer gap (the inner lane is sized for the whole swarm), and two
    /// bots on opposite lanes still clear it after both jitter toward each other.
    #[test]
    fn lanes_clear_the_peer_gap_at_every_swarm_size() {
        for n in 3..=32 {
            let chord = 2.0 * lane_radius(0, n) * (std::f64::consts::PI / n.max(2) as f64).sin();
            assert!(chord >= MIN_PEER_GAP - 1e-9, "n={n} chord={chord}");
            let gap = lane_radius(1, n) - lane_radius(0, n);
            assert!(gap >= LANE_GAP - 1e-9, "n={n} gap={gap}");
        }
    }
}
