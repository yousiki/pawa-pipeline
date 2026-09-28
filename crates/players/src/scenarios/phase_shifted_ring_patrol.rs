use super::*;

const TAU: f64 = std::f64::consts::TAU;
const PHASES: usize = 3;
const PHASE_NAMES: [&str; PHASES] = ["phase_a", "phase_b", "phase_c"];

fn rand_range(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

/// The nearest (or farthest) peer whose position has replicated into THIS process. Peer geometry
/// only exists for the bots this shard hosts, so the index is local.
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

/// A triangle camera sweep of exactly `span_deg`, out to one edge, across, and back.
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

/// Circle a peer in ~20° slices while facing it. Fine slices read as an orbit rather than repeated
/// teleports across the circle.
async fn orbit(ctx: &Ctx, peer: usize, radius: f64, arc_deg: f64, direction: f64) -> u32 {
    let slices = (arc_deg / 20.0).ceil().max(1.0) as u32;
    let step = (arc_deg / slices as f64).to_radians() * direction;
    let mut angle = match (ctx.position(), ctx.peer_position(peer)) {
        (Some(own), Some(p)) => (own.2 - p.2).atan2(own.0 - p.0),
        _ => 0.0,
    };
    let mut legs = 0;
    for _ in 0..slices {
        if ctx.target_met() {
            break;
        }
        let Some(p) = ctx.peer_position(peer) else {
            look_at_peer(ctx, peer, 250).await;
            sleep_ms(200).await;
            continue;
        };
        angle += step;
        ctx.move_to_watching_peer(
            p.0 + angle.cos() * radius,
            p.2 + angle.sin() * radius,
            peer,
            2_500,
            1.5,
        )
        .await;
        legs += 1;
    }
    legs
}

/// Walk a world bearing open-loop: face it first (`hold_visual` freezes the camera where it finds
/// it), then hold forward for as long as the distance takes at the speed actually travelled.
async fn walk_heading(ctx: &Ctx, heading_deg: f64, distance: f64, sprint: bool) -> u64 {
    const WALK_BPS: f64 = 4.317;
    const SPRINT_BPS: f64 = 5.612;
    let speed = if sprint { SPRINT_BPS } else { WALK_BPS };
    let hold = ((distance / speed * 1000.0) as u64).clamp(400, 12_000);
    if let Some(p) = ctx.position() {
        let yaw = heading_deg.to_radians();
        let settle = ctx.cfg().look_settle_ms.max(500);
        ctx.look(
            (p.0 - yaw.sin() * 32.0, p.1 + 1.62, p.2 + yaw.cos() * 32.0),
            settle,
        )
        .await;
    }
    ctx.hold_visual(WalkDirection::Forward, sprint, hold, 0)
        .await;
    hold
}

/// Three phase groups walk the same wide ring around the anchor at different radii and different
/// points on the circle, so peers stay in frame the whole time without ever marching in step.
///
/// Every station, arc, sweep span and leg length is a per-bot draw rather than the literal the
/// JSON pinned, and each bot advances its own arc by its own step — which is what keeps the phases
/// drifting apart instead of locking into a shared rhythm. The garnish after each station is drawn
/// INDEPENDENTLY of the leg that preceded it, so the camera action a viewer sees is never a
/// restatement of where the body just walked.
pub(crate) async fn run(ctx: &mut Ctx) {
    let index = ctx.index();
    let bot_count = ctx.player_count().max(1);
    let anchor = ctx.anchor();
    let phase = index % PHASES;
    let sprint_prob = ctx.cfg().sprint_bout_prob;
    let move_timeout = ctx.cfg().move_timeout_ms.min(14_000);

    // Own slot on the ring plus a private band. Adjacent slots belong to DIFFERENT phases and
    // therefore sit on different radii, so the peer gap holds radially as well as angularly.
    let band = ring_radius(bot_count) + phase as f64 * rand_range(ctx, 2.5, 4.0);
    let slot_arc = TAU / bot_count as f64;
    let mut angle = index as f64 * slot_arc + rand_range(ctx, -0.3, 0.3) * slot_arc;
    let direction = if ctx.rng_next() < 0.5 { 1.0 } else { -1.0 };

    ctx.record(
        TYPE_LOOK,
        &format!("vis-ring-patrol-{index}"),
        "Phase-shifted ring patrol: walk one band of a shared ring, out of step with the others.",
        json!({
            "role": PHASE_NAMES[phase],
            "phase": phase,
            "band_radius": r2(band),
            "direction": direction,
            "slot_deg": r2(angle.to_degrees()),
        }),
    );

    let mut leg = 0u32;
    while !ctx.target_met() {
        angle += direction * rand_range(ctx, 0.6, 1.5);
        let radius = band + rand_range(ctx, -1.5, 1.5);
        let scatter_dir = ctx.rng_next() * TAU;
        let scatter = rand_range(ctx, 0.0, 1.8);
        let goal = (
            anchor.0 + angle.cos() * radius + scatter_dir.cos() * scatter,
            anchor.1 + angle.sin() * radius + scatter_dir.sin() * scatter,
        );
        let arrived = ctx.move_to(goal.0, goal.1, move_timeout, 2.5).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-ring-station-{index}-{leg}"),
            &format!(
                "Patrol to ring station {leg} on the {} band.",
                PHASE_NAMES[phase]
            ),
            json!({
                "role": PHASE_NAMES[phase],
                "leg": leg,
                "station": {"x": r2(goal.0), "z": r2(goal.1)},
                "radius": r2(radius),
                "angle_deg": r2(angle.to_degrees()),
                "arrived": arrived,
            }),
        );
        if ctx.target_met() {
            break;
        }

        match (ctx.rng_next() * 4.0) as u32 {
            0 => {
                if let Some(peer) = peer_extreme(ctx, false) {
                    let radius = rand_range(ctx, 5.0, 9.5);
                    let arc = rand_range(ctx, 90.0, 240.0);
                    let spin = if ctx.rng_next() < 0.5 { 1.0 } else { -1.0 };
                    let legs = orbit(ctx, peer, radius, arc, spin).await;
                    ctx.record(
                        TYPE_MOVE,
                        &format!("vis-ring-orbit-{index}-{leg}"),
                        &format!("Orbit bot {peer} without leaving the patrol band."),
                        json!({"peer": peer, "radius": r2(radius), "arc_deg": r2(arc),
                            "direction": spin, "legs": legs}),
                    );
                }
            }
            1 => {
                let span = rand_range(ctx, 60.0, 175.0);
                let pitch = rand_range(ctx, -14.0, 14.0);
                sweep(ctx, span, pitch).await;
                ctx.record(
                    TYPE_LOOK,
                    &format!("vis-ring-sweep-{index}-{leg}"),
                    "Sweep the camera across the ring from a patrol station.",
                    json!({"sweep_deg": r2(span), "pitch_deg": r2(pitch)}),
                );
            }
            2 => {
                let farthest = ctx.rng_next() < 0.5;
                let hold = rand_range(ctx, 3_000.0, 6_500.0) as u64;
                if let Some(peer) = peer_extreme(ctx, farthest) {
                    watch_peer(ctx, peer, hold).await;
                    ctx.record(
                        TYPE_LOOK,
                        &format!("vis-ring-watch-{index}-{leg}"),
                        &format!("Hold bot {peer} in frame from across the ring."),
                        json!({"peer": peer, "farthest": farthest, "hold_ms": hold}),
                    );
                }
            }
            _ => {
                let heading = ctx.rng_next() * 360.0;
                let distance = rand_range(ctx, 8.0, 18.0);
                let sprint = ctx.rng_next() < sprint_prob;
                let hold = walk_heading(ctx, heading, distance, sprint).await;
                ctx.record(
                    TYPE_MOVE,
                    &format!("vis-ring-cut-{index}-{leg}"),
                    "Cut off the ring on a free bearing before rejoining the band.",
                    json!({"heading_deg": r2(heading), "requested_distance": r2(distance),
                        "sprint": sprint, "hold_ms": hold}),
                );
            }
        }
        leg += 1;
    }
}
