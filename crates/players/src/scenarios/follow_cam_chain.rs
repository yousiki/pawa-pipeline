use super::*;

const TAU: f64 = std::f64::consts::TAU;

/// How long one bot holds the wander lead before the role steps to the next index. Five terms fit
/// a 120 s episode, so a four-bot chain rotates the lead through every hand.
const LEAD_TERM_MS: i64 = 24_000;

/// Soft leash on the wandering head, so the chain never drags itself off the prepared ground into
/// ravines — the same failure `no_contact_pair_chase` measured before it grew one.
const LEASH: f64 = 24.0;

/// The trailing band the camera bots hold: never closer than the peer gap plus margin, re-drawn
/// per lead term inside this range.
const GAP_MIN: f64 = 6.0;
const GAP_MAX: f64 = 9.0;

/// Draw in `[lo, hi)` from the per-bot stream, so a number this scenario could have hard-coded
/// varies per bot and per leg and still replays from the seed.
fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

fn gap2(a: (f64, f64, f64), b: (f64, f64, f64)) -> f64 {
    (a.0 - b.0).hypot(a.2 - b.2)
}

/// The bot currently holding the wander lead, derived from each bot's own episode clock — the
/// carousel_chase recipe: a pure clock function, so the role hands over near-simultaneously
/// without any shared barrier.
fn leader_at(elapsed_ms: i64, n: usize) -> usize {
    ((elapsed_ms.max(0) / LEAD_TERM_MS) as usize) % n.max(1)
}

/// A follow-cam chain: every bot keeps its chain predecessor on camera and in a trailing band
/// while the current head wanders — so bot i films bot i-1, is filmed by bot i+1, and the whole
/// swarm is one moving line of camera operators. The head role steps down the chain on a shared
/// clock cadence. Pursuit family on purpose: a tracking shot that stares at its subject while
/// travelling is correct semantics here.
pub(crate) async fn run(ctx: &mut Ctx) {
    let index = ctx.index();
    let n = ctx.player_count().max(1);
    let anchor = ctx.anchor();
    let target = (index + n - 1) % n;
    let clock_jitter = (ctx.rng_next() * 2_000.0) as i64;
    let move_timeout = ctx.cfg().move_timeout_ms.min(8_000);

    // Fan out to a spaced ring slot first, so the chain starts over the peer gap.
    let r0 = ring_radius(n);
    let slot = index as f64 / n as f64 * TAU;
    ctx.move_to(
        anchor.0 + slot.cos() * r0,
        anchor.1 + slot.sin() * r0,
        move_timeout,
        2.5,
    )
    .await;
    ctx.record(
        TYPE_LOOK,
        &format!("vis-chain-init-{index}"),
        &format!("Follow-cam chain: bot {index} films bot {target}."),
        json!({"target": target, "lead_term_ms": LEAD_TERM_MS,
            "clock_jitter_ms": clock_jitter}),
    );

    let mut heading = ctx.rng_next() * TAU;
    let mut follow_gap = vary(ctx, GAP_MIN, GAP_MAX);
    let mut last_leader: Option<usize> = None;
    // Legs without displacement in a row. On natural terrain a wedged bot re-issues unreachable
    // tracking goals from the same spot: the 8-bot review measured two POVs accruing 20-28 s
    // frozen runs that way before this watchdog existed.
    let mut stalled = 0u32;
    let mut round = 0u32;
    while !ctx.target_met() {
        let leader = leader_at(ctx.elapsed_ms() + clock_jitter, n);
        if last_leader != Some(leader) {
            // Handover beat: re-draw the trailing distance and face the new head so the role
            // change reads on camera before the term's legs begin.
            follow_gap = vary(ctx, GAP_MIN, GAP_MAX);
            if n >= 2 && leader != index {
                look_at_peer(ctx, leader, 700).await;
            }
            ctx.record(
                TYPE_LOOK,
                &format!("vis-chain-lead-change-{index}-r{round}"),
                &format!("Lead hands over: bot {leader} now heads the chain."),
                json!({"round": round, "leader": leader, "follow_gap": r2(follow_gap)}),
            );
            last_leader = Some(leader);
        }
        let before = ctx.position();
        if n < 2 || leader == index {
            lead_leg(ctx, anchor, n, &mut heading, round).await;
        } else {
            follow_leg(ctx, target, follow_gap, round).await;
        }
        let moved = match (before, ctx.position()) {
            (Some(a), Some(b)) => (a.0 - b.0).hypot(a.2 - b.2) >= 1.2,
            _ => true,
        };
        stalled = if moved { 0 } else { stalled + 1 };
        if stalled >= 2 {
            // Two legs without moving: blind-hop loose, then walk toward the anchor's open
            // ground — no goal issued from the wedged spot was going to solve itself.
            shake_loose(ctx).await;
            if let Some(p) = ctx.position() {
                let toward = (anchor.0 - p.0, anchor.1 - p.2);
                let mag = toward.0.hypot(toward.1).max(0.01);
                let d = vary(ctx, 5.0, 8.0);
                ctx.move_to(
                    p.0 + toward.0 / mag * d,
                    p.2 + toward.1 / mag * d,
                    6_000,
                    2.0,
                )
                .await;
            }
            stalled = 0;
        }
        round += 1;
    }
}

/// One wander leg of the chain's head: veer the bearing (bend hard for home once the leash
/// tightens), walk a drawn distance, and sometimes check the camera bot trailing behind.
async fn lead_leg(ctx: &mut Ctx, anchor: (f64, f64), n: usize, heading: &mut f64, round: u32) {
    let index = ctx.index();
    let Some(own) = ctx.position() else {
        sleep_ms(300).await;
        return;
    };
    let leash_dist = (own.0 - anchor.0).hypot(own.2 - anchor.1);
    if leash_dist > LEASH {
        *heading = (anchor.1 - own.2).atan2(anchor.0 - own.0) + vary(ctx, -0.5, 0.5);
    } else {
        *heading += vary(ctx, -1.0, 1.0);
    }
    let d = vary(ctx, 6.0, 11.0);
    let to = (own.0 + heading.cos() * d, own.2 + heading.sin() * d);
    let arrived = ctx.move_to(to.0, to.1, 7_000, 2.0).await;
    ctx.record(
        TYPE_MOVE,
        &format!("vis-chain-lead-{index}-r{round}"),
        &format!(
            "Head the chain: wander {} blocks on a fresh bearing.",
            r2(d)
        ),
        json!({"round": round, "role": "lead", "heading_rad": r2(*heading),
            "distance": r2(d), "leash_dist": r2(leash_dist), "arrived": arrived}),
    );
    // Over-the-shoulder: the head is the one bot whose camera owes nothing to a subject, so it
    // spends some beats checking the operator trailing it and some reading the terrain ahead.
    if n >= 2 && ctx.rng_next() < 0.45 {
        let trailing = (index + 1) % n;
        let settle = vary(ctx, 400.0, 900.0) as u64;
        if let Some(p) = look_at_peer(ctx, trailing, settle).await {
            ctx.record(
                TYPE_LOOK,
                &format!("vis-chain-shoulder-{index}-r{round}"),
                &format!("Glance back at trailing camera bot {trailing}."),
                json!({"round": round, "trailing": trailing,
                    "trailing_distance": r2(ctx.position().map(|o| gap2(o, p)).unwrap_or(0.0))}),
            );
        }
    } else if ctx.rng_next() < 0.35 {
        ctx.sweep_default().await;
    }
}

/// One tracking leg of a camera bot: trail toward the band when far, back off when the frame gets
/// too tight, and dolly on an arc when the distance is already right — the camera pinned on the
/// subject through all three.
async fn follow_leg(ctx: &mut Ctx, target: usize, follow_gap: f64, round: u32) {
    let index = ctx.index();
    let (Some(own), Some(peer)) = (ctx.position(), ctx.peer_position(target)) else {
        sleep_ms(300).await;
        return;
    };
    let d = gap2(own, peer);
    if d > follow_gap + 2.5 {
        // Trail: close toward a point one gap short of the subject, camera rolling on them.
        let (ux, uz) = away_unit(Some(own), peer);
        let to = (
            peer.0 + ux * follow_gap * 0.9,
            peer.2 + uz * follow_gap * 0.9,
        );
        let arrived = ctx
            .move_to_watching_peer(to.0, to.1, target, 6_000, 2.0)
            .await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-chain-trail-{index}-r{round}"),
            &format!(
                "Trail bot {target}, closing to the {} block band.",
                r2(follow_gap)
            ),
            json!({"round": round, "role": "camera", "target": target,
                "distance": r2(d), "arrived": arrived}),
        );
    } else if d < MIN_PEER_GAP + 0.5 {
        // Too tight for the frame: back straight off along the subject axis, still rolling.
        let (ux, uz) = away_unit(Some(own), peer);
        let back = vary(ctx, 3.0, 5.0);
        ctx.move_to_watching_peer(own.0 + ux * back, own.2 + uz * back, target, 5_000, 1.5)
            .await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-chain-backoff-{index}-r{round}"),
            &format!("Back off bot {target} to reopen the frame."),
            json!({"round": round, "role": "camera", "target": target, "distance": r2(d),
                "backoff": r2(back)}),
        );
    } else {
        // In the band: dolly along an arc around the subject so the shot angle keeps changing
        // even when the subject pauses — and the operator never freezes waiting for them.
        let arc =
            vary(ctx, 30.0, 55.0).to_radians() * if ctx.rng_next() < 0.5 { 1.0 } else { -1.0 };
        let ring = d.clamp(5.0, follow_gap);
        let ang = (own.2 - peer.2).atan2(own.0 - peer.0) + arc;
        let arrived = ctx
            .move_to_watching_peer(
                peer.0 + ang.cos() * ring,
                peer.2 + ang.sin() * ring,
                target,
                5_000,
                1.5,
            )
            .await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-chain-dolly-{index}-r{round}"),
            &format!("Dolly around bot {target} on a {} block arc.", r2(ring)),
            json!({"round": round, "role": "camera", "target": target, "arc_rad": r2(arc),
                "ring": r2(ring), "arrived": arrived}),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every bot must film a real, distinct predecessor, and the lead must visit every index over
    /// consecutive terms — the chain has no other synchronization to fall back on.
    #[test]
    fn the_chain_names_distinct_targets_and_the_lead_rotates_through_everyone() {
        for n in 2..=9 {
            for index in 0..n {
                let target = (index + n - 1) % n;
                assert_ne!(target, index, "n={n} index={index}");
            }
            let mut seen = vec![false; n];
            for term in 0..n {
                seen[leader_at(term as i64 * LEAD_TERM_MS, n)] = true;
            }
            assert!(seen.iter().all(|&s| s), "n={n}: lead skipped an index");
        }
    }

    /// The lead term must hand over on exact boundaries and give a 120 s episode several terms.
    #[test]
    fn lead_terms_change_on_boundaries_and_repeat_within_an_episode() {
        assert_eq!(leader_at(0, 4), 0);
        assert_eq!(leader_at(LEAD_TERM_MS - 1, 4), 0);
        assert_eq!(leader_at(LEAD_TERM_MS, 4), 1);
        const {
            assert!(120_000 / LEAD_TERM_MS >= 4);
        }
    }
}
