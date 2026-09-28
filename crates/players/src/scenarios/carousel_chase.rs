use super::*;

const TAU: f64 = std::f64::consts::TAU;

/// Soft leash around the swarm anchor. A chase that drifts past the prepared ground ends in
/// ravines and cave mouths — the same failure `no_contact_pair_chase` measured before it grew one.
const LEASH: f64 = 26.0;

const WALK_BPS: f64 = 4.317;
const SPRINT_BPS: f64 = 5.612;

/// Ground-probe stride ahead of a blind run. A `hold_visual` leg never consults the pathfinder, so
/// a pit deeper than two blocks a few strides ahead must truncate the leg before the body falls in.
const PROBE: f64 = 1.5;

/// Wall-clock cadence on which the whole carousel reverses. Every bot derives the direction from
/// its own episode clock, so the swarm flips near-simultaneously without ever sharing a barrier.
const FLIP_PERIOD_MS: i64 = 24_000;

/// Inside this gap to the quarry the chaser breaks off sideways instead of closing further — the
/// carousel is a pursuit ring, not a clinch, and the peer-gap rule holds through it.
const BREAK_OFF: f64 = MIN_PEER_GAP + 0.5;

/// A chase burst that displaced the body less than this is a stall, not a run.
const HOLD_MIN_TRAVEL: f64 = 1.0;

fn rand_range(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

fn gap(a: (f64, f64, f64), b: (f64, f64, f64)) -> f64 {
    (a.0 - b.0).hypot(a.2 - b.2)
}

/// How far along `heading_rad` the ground keeps holding the bot up (one or two blocks of drop is a
/// slope and stays allowed; deeper is a pit). Same probe `no_contact_pair_chase` runs before its
/// blind legs — that scenario measured the alternative as half an episode of black cave frames.
fn safe_run_distance(ctx: &Ctx, heading_rad: f64, want: f64) -> f64 {
    let Some(p) = ctx.position() else { return 0.0 };
    let feet_y = p.1.floor() as i32;
    let mut safe = 0.0;
    while safe + PROBE <= want {
        let step = safe + PROBE;
        let cell = BlockPos::new(
            (p.0 - heading_rad.sin() * step).floor() as i32,
            feet_y,
            (p.2 + heading_rad.cos() * step).floor() as i32,
        );
        if !block_is_safe_support(ctx, cell.down(1)) && !block_is_safe_support(ctx, cell.down(2)) {
            break;
        }
        safe = step;
    }
    safe
}

/// One long-press run toward a world bearing: face it, then hold Forward as a single press for as
/// long as the (probe-truncated) distance takes. Jump pulses climb the one-block steps natural
/// terrain puts in a straight line. Returns the hold length, 0 when the ground refused the run.
async fn run_heading(ctx: &Ctx, heading_rad: f64, distance: f64, sprint: bool) -> u64 {
    let distance = safe_run_distance(ctx, heading_rad, distance);
    if distance < PROBE {
        return 0;
    }
    let speed = if sprint { SPRINT_BPS } else { WALK_BPS };
    let hold = ((distance / speed * 1000.0) as u64).clamp(600, 6_000);
    if let Some(p) = ctx.position() {
        let settle = ctx.cfg().look_settle_ms.max(500);
        ctx.look(
            (
                p.0 - heading_rad.sin() * 32.0,
                p.1 + 1.62,
                p.2 + heading_rad.cos() * 32.0,
            ),
            settle,
        )
        .await;
    }
    ctx.hold_visual(WalkDirection::Forward, sprint, hold, 850)
        .await;
    hold
}

/// Heading (radians, `-sin/cos` convention shared with the probe) from `own` toward `to`.
fn heading_toward(own: (f64, f64, f64), to: (f64, f64, f64)) -> f64 {
    let d = gap(own, to).max(0.01);
    let (ux, uz) = ((to.0 - own.0) / d, (to.2 - own.2) / d);
    (-ux).atan2(uz)
}

/// Every bot chases its next ring neighbour while its previous neighbour chases it, and the whole
/// carousel reverses direction on a shared wall-clock cadence — so each bot is chaser and quarry at
/// once, and twice a minute the entire swarm does a coordinated about-face.
///
/// The chase itself is long-press runs at the quarry (one `hold_visual` per burst, never a hail of
/// short repositioning presses — the chatter failure that retired `mob_chase`), broken up by flank
/// arcs that circle the quarry when the gap is already right, break-off veers that keep the peer
/// gap when cyclic pursuit tries to collapse the ring, and over-the-shoulder glances at the threat
/// behind. Every distance, bearing and hold is a per-round draw.
pub(crate) async fn run(ctx: &mut Ctx) {
    let index = ctx.index();
    let n = ctx.player_count().max(1);
    let anchor = ctx.anchor();
    let sprint_prob = ctx.cfg().sprint_bout_prob;
    let move_timeout = ctx.cfg().move_timeout_ms.min(8_000);
    // Per-bot flip lag, so the about-face ripples through the ring instead of snapping in sync.
    let flip_jitter = (ctx.rng_next() * 2_500.0) as i64;

    // Fan out to a ring slot first: the carousel needs its spacing before anyone starts running.
    let radius = ring_radius(n);
    let slot = index as f64 / n as f64 * TAU;
    let start = (
        anchor.0 + slot.cos() * radius,
        anchor.1 + slot.sin() * radius,
    );
    ctx.move_to(start.0, start.1, move_timeout, 2.5).await;
    ctx.record(
        TYPE_LOOK,
        &format!("vis-carousel-init-{index}"),
        &format!("Carousel chase: bot {index} takes ring slot {index} of {n}."),
        json!({"slot": index, "ring_radius": r2(radius),
            "flip_period_ms": FLIP_PERIOD_MS, "flip_jitter_ms": flip_jitter}),
    );

    let mut round = 0u32;
    let mut stalls = 0u32;
    let mut was_forward: Option<bool> = None;
    while !ctx.target_met() {
        // Solo fallback (below the catalog's min_players, but never crash): patrol the ring alone.
        if n < 2 {
            let angle = ctx.rng_next() * TAU;
            let r = rand_range(ctx, radius - 2.0, radius + 2.0);
            ctx.move_to(
                anchor.0 + angle.cos() * r,
                anchor.1 + angle.sin() * r,
                move_timeout,
                2.5,
            )
            .await;
            round += 1;
            continue;
        }

        let forward = ((ctx.elapsed_ms() + flip_jitter) / FLIP_PERIOD_MS) % 2 == 0;
        let quarry = if forward {
            (index + 1) % n
        } else {
            (index + n - 1) % n
        };
        let threat = if forward {
            (index + n - 1) % n
        } else {
            (index + 1) % n
        };
        if was_forward.is_some_and(|prev| prev != forward) {
            // The about-face beat: hold the turn toward the new quarry so the reversal reads.
            if let Some(p) = look_at_peer(ctx, quarry, 900).await {
                ctx.record(
                    TYPE_LOOK,
                    &format!("vis-carousel-flip-{index}-{round}"),
                    &format!("Carousel reverses: bot {index} about-faces onto bot {quarry}."),
                    json!({"round": round, "forward": forward, "quarry": quarry,
                        "quarry_distance": r2(ctx.position().map(|o| gap(o, p)).unwrap_or(0.0))}),
                );
            }
        }
        was_forward = Some(forward);

        let (Some(own), Some(peer)) = (ctx.position(), ctx.peer_position(quarry)) else {
            sleep_ms(250).await;
            continue;
        };

        // Leash before anything else: a chase that left the prepared ground walks back first.
        if gap(own, (anchor.0, 0.0, anchor.1)) > LEASH {
            let home = (
                anchor.0 + rand_range(ctx, -5.0, 5.0),
                anchor.1 + rand_range(ctx, -5.0, 5.0),
            );
            let arrived = ctx.move_to(home.0, home.1, move_timeout, 3.0).await;
            ctx.record(
                TYPE_MOVE,
                &format!("vis-carousel-leash-{index}-{round}"),
                "Walk back inside the arena leash before rejoining the carousel.",
                json!({"round": round, "arrived": arrived}),
            );
            round += 1;
            continue;
        }

        let dist = gap(own, peer);
        let far_line = rand_range(ctx, 7.0, 10.0);
        if dist > far_line {
            // Close in one long press aimed at the quarry (run_heading faces the bearing itself).
            let heading = heading_toward(own, peer);
            let want = (dist - 5.5).clamp(3.0, 13.0);
            let sprint = ctx.rng_next() < sprint_prob;
            let hold = run_heading(ctx, heading, want, sprint).await;
            if hold == 0 {
                // The straight line crosses a pit: let the planner route around it instead.
                ctx.move_to(peer.0, peer.2, 3_500, 5.0).await;
                stalls = 0;
            } else if ctx
                .position()
                .is_some_and(|now| gap(now, own) < HOLD_MIN_TRAVEL)
            {
                stalls += 1;
                if stalls >= 2 {
                    shake_loose(ctx).await;
                    stalls = 0;
                }
            } else {
                stalls = 0;
            }
            look_at_peer(ctx, quarry, 200).await;
            let refreshed = match (ctx.position(), ctx.peer_position(quarry)) {
                (Some(a), Some(b)) => gap(a, b),
                _ => f64::INFINITY,
            };
            ctx.record(
                TYPE_MOVE,
                &format!("vis-carousel-chase-{index}-{round}"),
                &format!("Run at bot {quarry}, now {} blocks ahead.", r2(refreshed)),
                json!({"round": round, "role": "chaser", "quarry": quarry, "forward": forward,
                    "sprint": sprint, "hold_ms": hold, "distance": r2(refreshed)}),
            );
        } else if dist < BREAK_OFF {
            // Cyclic pursuit wants the ring to collapse; the break-off veer refuses. Bear away
            // from the quarry plus a drawn slant so the veer curves instead of backing straight up.
            let away = heading_toward(peer, own);
            let slant = rand_range(ctx, 0.5, 1.3) * if ctx.rng_next() < 0.5 { 1.0 } else { -1.0 };
            let sprint = ctx.rng_next() < sprint_prob;
            let want = rand_range(ctx, 5.0, 9.0);
            let hold = run_heading(ctx, away + slant, want, sprint).await;
            if hold == 0 {
                let (ux, uz) = away_unit(Some(own), peer);
                ctx.move_to(own.0 + ux * want, own.2 + uz * want, 3_500, 2.0)
                    .await;
            }
            look_at_peer(ctx, quarry, 300).await;
            ctx.record(
                TYPE_MOVE,
                &format!("vis-carousel-veer-{index}-{round}"),
                &format!("Break off bot {quarry} to keep the ring open."),
                json!({"round": round, "role": "chaser", "quarry": quarry,
                    "slant_rad": r2(slant), "requested_distance": r2(want), "hold_ms": hold}),
            );
        } else {
            // The gap is already right: hold it and circle, camera pinned on the quarry.
            let arc = rand_range(ctx, 30.0, 55.0).to_radians()
                * if ctx.rng_next() < 0.5 { 1.0 } else { -1.0 };
            let ring = dist.clamp(5.0, 9.0);
            let angle = (own.2 - peer.2).atan2(own.0 - peer.0) + arc;
            ctx.move_to_watching_peer(
                peer.0 + angle.cos() * ring,
                peer.2 + angle.sin() * ring,
                quarry,
                3_000,
                1.5,
            )
            .await;
            ctx.record(
                TYPE_MOVE,
                &format!("vis-carousel-flank-{index}-{round}"),
                &format!("Flank bot {quarry} along a {}-block arc.", r2(ring)),
                json!({"round": round, "role": "chaser", "quarry": quarry,
                    "arc_rad": r2(arc), "ring": r2(ring)}),
            );
        }

        // Over-the-shoulder check on the bot chasing THIS one — the carousel's second thread.
        if !ctx.target_met() && ctx.rng_next() < 0.35 {
            let settle = rand_range(ctx, 400.0, 900.0) as u64;
            if let Some(p) = look_at_peer(ctx, threat, settle).await {
                ctx.record(
                    TYPE_LOOK,
                    &format!("vis-carousel-shoulder-{index}-{round}"),
                    &format!("Glance over the shoulder at pursuing bot {threat}."),
                    json!({"round": round, "threat": threat,
                        "threat_distance": r2(ctx.position().map(|o| gap(o, p)).unwrap_or(0.0)),
                        "settle_ms": settle}),
                );
            }
        }
        round += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ring topology must name a real, distinct quarry and threat at every swarm size, and the
    /// reversal must swap exactly those two roles.
    #[test]
    fn quarry_and_threat_are_distinct_neighbours_and_swap_on_reversal() {
        for n in 3..=9 {
            for index in 0..n {
                let quarry_fwd = (index + 1) % n;
                let threat_fwd = (index + n - 1) % n;
                assert_ne!(quarry_fwd, index);
                assert_ne!(threat_fwd, index);
                assert_ne!(quarry_fwd, threat_fwd, "n={n} index={index}");
                // Reversed direction: the roles swap.
                let quarry_rev = (index + n - 1) % n;
                assert_eq!(quarry_rev, threat_fwd);
            }
        }
    }

    /// Every bot derives the direction from its own clock, so the schedule itself must alternate
    /// on exact period boundaries — that shared pure function is the only synchronization there is.
    #[test]
    fn the_flip_direction_alternates_on_period_boundaries() {
        let forward = |t: i64| (t / FLIP_PERIOD_MS) % 2 == 0;
        assert!(forward(0));
        assert!(forward(FLIP_PERIOD_MS - 1));
        assert!(!forward(FLIP_PERIOD_MS));
        assert!(!forward(2 * FLIP_PERIOD_MS - 1));
        assert!(forward(2 * FLIP_PERIOD_MS));
        // A 120 s episode spans five direction epochs, i.e. four reversals.
        assert_eq!(120_000 / FLIP_PERIOD_MS, 5);
    }
}
