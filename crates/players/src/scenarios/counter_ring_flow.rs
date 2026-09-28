use super::*;

const TAU: f64 = std::f64::consts::TAU;

/// Radial gap between adjacent rings. Minus both bots' worth of lane jitter this still clears
/// [`MIN_PEER_GAP`], so a cross-ring pass never closes under the corpus floor.
const RING_GAP: f64 = 6.0;

/// Widest radial wobble a leg may add to its ring radius. Bounded so `RING_GAP - 2 * JITTER`
/// stays above the peer gap.
const JITTER: f64 = 0.6;

/// Two bots on adjacent rings, both jittered toward each other, must still clear the peer gap.
const _: () = assert!(RING_GAP - 2.0 * JITTER >= MIN_PEER_GAP - 0.5);

/// Below this 2D distance to the nearest peer a bot sidesteps outward before its next leg.
const CROWD_GAP: f64 = 4.0;

/// Draw in `[lo, hi)` from the per-bot stream, so a number this scenario could have hard-coded
/// varies per bot and per leg and still replays from the seed.
fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

/// How many concentric rings a swarm of `n` splits into.
fn ring_count(n: usize) -> usize {
    if n < 10 { 2 } else { 3 }
}

/// `(ring, slot, members)` for one bot. Interleaved by index (`index % rings`) so any prefix of
/// the swarm still populates every ring — the counter-flow needs traffic in BOTH directions at
/// every size, not an outer ring that only exists past some head count.
fn ring_slot(index: usize, n: usize) -> (usize, usize, usize) {
    let rings = ring_count(n);
    let ring = index % rings;
    let slot = index / rings;
    let members = (n - ring).div_ceil(rings);
    (ring, slot, members)
}

/// Radius of `ring`. The innermost ring is the fullest under interleaving, so sizing it with
/// [`ring_radius`] for its own membership clears the slot chord everywhere; each ring outward
/// adds [`RING_GAP`].
fn radius_of(ring: usize, n: usize) -> f64 {
    let rings = ring_count(n);
    ring_radius(n.div_ceil(rings)) + ring as f64 * RING_GAP
}

/// Nearest live peer on this shard, any ring — the crowd-guard input.
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

/// Nearest peer circulating the OTHER way — a bot on any ring of different parity. That is the
/// oncoming traffic this scenario exists to film.
fn oncoming_peer(ctx: &Ctx) -> Option<(usize, f64)> {
    let own = ctx.position()?;
    let rings = ring_count(ctx.player_count());
    let own_parity = (ctx.index() % rings) % 2;
    (0..ctx.player_count())
        .filter(|&j| j != ctx.index() && (j % rings) % 2 != own_parity)
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

/// The swarm splits into concentric rings around the anchor and adjacent rings circulate in
/// OPPOSITE directions, so every bot keeps meeting cross-ring traffic head-on — a high
/// relative-angular-velocity pass no merged scenario produces (orbits and patrols keep one
/// direction; the carousel reverses everyone at once, never against each other).
///
/// Zero world edits; the flat `build_arena` slab is the stage for the same reason the
/// figure-eight needed it: a fixed circular track on natural terrain films ravine walls.
///
/// No barriers: each bot advances its own angle at its own pace, so rings breathe rather than
/// lock step; a bot that finds its stretch crowded sidesteps outward with one held press. Legs
/// are open-loop tangent holds with periodic pathfinder corrections; cameras rotate between the
/// path ahead, oncoming cross-ring traffic, reads across the circle, and free sweeps.
pub(super) async fn run(ctx: &mut Ctx) {
    let index = ctx.index();
    let n = ctx.player_count().max(1);
    let anchor = ctx.anchor();
    let (ring, slot, members) = ring_slot(index, n);
    let radius = radius_of(ring, n);
    let direction = if ring.is_multiple_of(2) { 1.0 } else { -1.0 };
    let mut angle = slot as f64 / members.max(1) as f64 * TAU + vary(ctx, 0.0, 0.3);
    let settle = ctx.cfg().look_settle_ms.max(400);
    let timeout = ctx.cfg().move_timeout_ms.min(8_000);
    ctx.record(
        TYPE_LOOK,
        &format!("vis-cring-open-{index}"),
        &format!(
            "Counter-ring flow: circulate ring {ring} {} against the neighbouring rings.",
            if direction > 0.0 {
                "counterclockwise"
            } else {
                "clockwise"
            }
        ),
        json!({"ring": ring, "slot": slot, "members": members,
            "radius": r2(radius), "direction": direction}),
    );
    // Walk to the starting slot with the pathfinder; the loop then stays mostly open-loop.
    ctx.move_to(
        anchor.0 + angle.cos() * radius,
        anchor.1 + angle.sin() * radius,
        timeout,
        2.0,
    )
    .await;

    let mut leg = 0u32;
    // Consecutive stall-cut holds against the same obstacle (a peer's body, the arena rim).
    let mut stalls = 0u32;
    while !ctx.target_met() {
        let chord = vary(ctx, 4.5, 6.0);
        angle += direction * chord / radius;
        let r = radius + vary(ctx, -JITTER, JITTER);
        let target = (anchor.0 + angle.cos() * r, anchor.1 + angle.sin() * r);

        // Crowd guard: if the next stretch is packed — an oncoming pass converging on the same
        // arc — sidestep AWAY FROM THE PEER with one held press. Sidestepping outward from the
        // anchor instead let a same-ring convoy pair co-drift into the arena rim and wedge (the
        // orbit_lane_swap 8-bot round measured 30 s frozen runs on exactly that); separating
        // along the pair axis breaks the pair, and a stalled guard hold feeds the escape counter.
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

        // The leg: mostly an open-loop tangent hold; every few legs a pathfinder correction
        // re-pins the bot to its ring, sometimes with the camera on the oncoming traffic.
        let correction = leg % 4 == 3;
        if correction {
            let social = ctx.rng_next() < 0.6;
            if social && let Some((peer, _)) = oncoming_peer(ctx) {
                ctx.move_to_watching_peer(target.0, target.1, peer, timeout.min(5_000), 1.6)
                    .await;
            } else {
                ctx.move_to(target.0, target.1, timeout.min(5_000), 1.6)
                    .await;
            }
        } else if let Some(p) = ctx.position() {
            let dist = (target.0 - p.0).hypot(target.1 - p.2);
            if dist > 1.5 {
                // Face past the waypoint along the walking direction, so consecutive holds read
                // as one smooth arc rather than pivots at every chord.
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
        // Two stall-cut holds running means the arc ahead is blocked from where the body stands.
        // Shake loose, skip a LONG arc past the blockage, and re-pin hop-wise: a single move_to
        // kept replanning from inside a pocket of bodies (8-bot round 3 measured a 20.2 s wedge
        // on the inner ring), while travel_to carries its own sidesteps and progress budget.
        if stalls >= 2 {
            shake_loose(ctx).await;
            angle += direction * 6.0 * chord / radius;
            let ahead = (
                anchor.0 + angle.cos() * radius,
                anchor.1 + angle.sin() * radius,
            );
            travel_to(ctx, ahead, TRAVEL_HOP, 2.5).await;
            stalls = 0;
            ctx.record(
                TYPE_MOVE,
                &format!("vis-cring-detour-{index}-{leg}"),
                "Detour around blocked terrain and rejoin the ring further along.",
                json!({"leg": leg}),
            );
        }
        ctx.record(
            TYPE_MOVE,
            &format!("vis-cring-leg-{index}-{leg}"),
            &format!("Circulate ring {ring} one chord further."),
            json!({"leg": leg, "ring": ring, "angle": r2(angle),
                "target": {"x": r2(target.0), "z": r2(target.1)}}),
        );
        if ctx.target_met() {
            break;
        }

        // Garnish, drawn sparsely so the flow keeps flowing: track the oncoming pass, read the
        // far side of the circle where the counter-stream is coming from, or sweep the arena.
        // Skipped while a peer is inside the crowd gap — a stationary sweep inside a pocket of
        // bodies extends exactly the frozen stretch the escape above just broke out of.
        let crowded = nearest_peer(ctx)
            .map(|(_, d)| d < CROWD_GAP)
            .unwrap_or(false);
        let roll = if crowded { 1.0 } else { ctx.rng_next() };
        if roll < 0.15 {
            if let Some((peer, dist)) = oncoming_peer(ctx) {
                let hold = vary(ctx, 1_200.0, 2_400.0) as u64;
                watch_peer(ctx, peer, hold).await;
                ctx.record(
                    TYPE_LOOK,
                    &format!("vis-cring-oncoming-{index}-{leg}"),
                    &format!("Track oncoming bot {peer} passing on the counter-rotating ring."),
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
                    &format!("vis-cring-cross-read-{index}-{leg}"),
                    "Read the far side of the circle across the centre.",
                    json!({"leg": leg}),
                );
            }
        } else if roll < 0.32 {
            let span = vary(ctx, 100.0, 180.0);
            let pitch = vary(ctx, -8.0, 8.0);
            sweep(ctx, span, pitch).await;
            ctx.record(
                TYPE_LOOK,
                &format!("vis-cring-sweep-{index}-{leg}"),
                "Sweep the rings from the current arc.",
                json!({"leg": leg, "sweep_deg": r2(span)}),
            );
        }
        leg += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::{RING_GAP, radius_of, ring_count, ring_slot};
    use crate::scenarios::common::MIN_PEER_GAP;

    /// The spacing this file promises: at every recorded swarm size, every ring is populated
    /// (counter-flow needs both directions), evenly slotted ring-mates clear the peer gap, and
    /// two bots on adjacent rings still clear it after both jitter toward each other.
    #[test]
    fn rings_are_populated_and_clear_the_peer_gap() {
        for n in 4..=32 {
            let rings = ring_count(n);
            let mut seen = vec![0usize; rings];
            for i in 0..n {
                let (ring, slot, members) = ring_slot(i, n);
                assert!(ring < rings && slot < members, "n={n} i={i}");
                seen[ring] += 1;
                let chord =
                    2.0 * radius_of(ring, n) * (std::f64::consts::PI / members.max(2) as f64).sin();
                assert!(
                    chord >= MIN_PEER_GAP - 1e-9,
                    "n={n} ring={ring} chord={chord}"
                );
            }
            for (ring, count) in seen.iter().enumerate() {
                assert!(*count > 0, "n={n} ring {ring} empty");
                let (_, _, members) = ring_slot(ring, n);
                assert_eq!(*count, members, "n={n} ring={ring}");
            }
            // Adjacent rings counter-rotate and stay a full gap apart.
            for ring in 1..rings {
                let gap = radius_of(ring, n) - radius_of(ring - 1, n);
                assert!(gap >= RING_GAP - 1e-9, "n={n} ring={ring} gap={gap}");
            }
        }
    }
}
