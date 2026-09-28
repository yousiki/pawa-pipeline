//! CATALOG: family=group_motion min_players=2 single_process=true world_edits=false scenery=none far_players=false
//!
//! Two bots stand on opposite sides of a ring and walk the same arc back and forth, each keeping the
//! other in frame across the middle. Mutual observation: neither is a subject, both are.
//!
//! The revisit problem. **The trajectory loop** is the arc: the slot angle is a TRIANGLE WAVE of
//! the episode clock, so each bot sweeps `ARC_SWEEP_DEG` of the ring one way and then exactly the
//! same arc back, returning to the slot it started from every `SWEEP_PERIOD_MS`. Path length grows
//! without bound while net displacement is capped by the arc's chord. **The sightline loop** rides
//! on it: the bearing across the ring to the antipodal partner sweeps with the arc and comes back,
//! and because both bots are on the same wave the pair stays antipodal — so each one's return is
//! corroborated by a second POV of the same moment from the opposite side. That mutual
//! corroboration is something no single-observer scenario in the corpus provides.
//!
//! The ring crosses real terrain, so the line between the two bots is broken by whatever sits near
//! the anchor and reopens as the arc carries them past it — occlusion that both POVs record.
//!
//! **How this clears the camera/travel independence gate.** A constant-sign mutual orbit is a
//! guaranteed failure: if both bots circle one way while tracking each other, the camera turns one
//! way forever and travel sits at a constant 90 deg off it, so the sign relation never flips and the
//! agreement pins at 100%. This script breaks that in two ways at once. First, the arc REVERSES
//! every half period, so any sign relation that did build up is inverted twice a minute. Second, and
//! decisively, the camera is never driven by the partner's position while the body travels
//! (walk-stop-look-walk): each round is an arc leg on plain `move_to` (camera untouched,
//! the idiom 65 generation-5 production scenarios pass on), then a STANDING `look_until_facing` across the ring,
//! whose frames `qc.rs:420` discards because the body is still. The mutual framing therefore comes
//! from the density of those standing gazes, not from tracking — `hold_facing` hands the camera to
//! `Hold` from its first leg and does not keep it on the partner. And because `agreement()` is
//! `max(p, 1-p)`, a hand-driven camera that consistently pointed AWAY from travel would fail
//! identically; leaving `Hold` alone is the only safe answer.
//!
//! Near-clones: the two bots ride the same wave, which is what keeps them antipodal, so they are
//! desynchronized on everything else instead — each gets its own ring radius (hence its own arc
//! chord and therefore its own leg durations), its own hold length, and its own small phase offset.

use super::*;

/// Total arc each bot sweeps, in degrees of the ring. Past the 102.5 deg horizontal FOV so the
/// sightline genuinely leaves and re-enters the region it started on rather than jittering inside it.
const ARC_SWEEP_DEG: f64 = 150.0;
/// One out-and-back sweep. 48 s gives two and a half complete loops in a 120 s episode, and makes a
/// single round's arc step 40-50 deg, whose chord at these radii is a 5-7 block leg — over the 0.5 s
/// key-chatter floor even at sprint speed.
const SWEEP_PERIOD_MS: i64 = 48_000;
/// Extra ring radius the odd bot gets, in blocks. Same wave, different circumference: different
/// chord per round, different leg duration, different key-press pattern. That is what keeps the two
/// action streams apart for the pairwise near-clones check without breaking the antipodal geometry.
const RADIUS_SPREAD: f64 = 3.0;
/// Per-bot phase offset of the wave, as a fraction of the period. Small on purpose: it desynchronizes
/// the reversals without letting the angular gap close far from antipodal (a 6% offset at
/// [`ARC_SWEEP_DEG`] moves the gap by under 40 deg, and the pair is 16+ blocks apart at 140 deg).
const PHASE_FRACTION: f64 = 0.06;
/// One mutual-observation hold, in ms, for the even bot; the odd bot gets [`HOLD_SPREAD_MS`] more.
const HOLD_MS: i64 = 3_000;
/// See [`HOLD_MS`]: the two bots must not change beat on the same tick.
const HOLD_SPREAD_MS: i64 = 1_500;
/// Shuttle leg for `hold_facing`, in blocks. 4.0 is the validated value — see its doc comment.
const SHUTTLE_LEG: f64 = 4.0;

/// Where in the sweep the ring slot sits at `elapsed_ms`, in degrees off the bot's base slot.
///
/// A triangle wave, not a saw and not an accumulator. A saw would teleport the slot back at the
/// period boundary; an accumulated step would let rounding drift the two bots out of antipodal over
/// a long episode. A closed-form function of the clock has neither problem, and both bots evaluate
/// the same one, so they stay opposite each other for free.
fn sweep_offset_deg(elapsed_ms: i64) -> f64 {
    let phase = (elapsed_ms.rem_euclid(SWEEP_PERIOD_MS)) as f64 / SWEEP_PERIOD_MS as f64;
    let triangle = if phase < 0.5 {
        4.0 * phase - 1.0
    } else {
        3.0 - 4.0 * phase
    };
    triangle * ARC_SWEEP_DEG / 2.0
}

/// The bot this one is paired with; see the identical helper in the sibling `watch_*` scripts.
fn partner_of(index: usize, bot_count: usize) -> usize {
    let paired = index ^ 1;
    if paired < bot_count { paired } else { 0 }
}

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let bot_count = ctx.player_count().max(1);
    let partner = partner_of(index, bot_count);
    let anchor = ctx.anchor();
    let global = global_index(ctx);
    // `ring_radius` is the radius at which evenly spaced slots still clear `MIN_PEER_GAP`; at N=2 it
    // floors at 8, putting the antipodal pair 16 blocks apart — a readable full-body framing.
    let outer = !global.is_multiple_of(2);
    let radius = ring_radius(bot_count) + if outer { RADIUS_SPREAD } else { 0.0 };
    let hold_ms = HOLD_MS + if outer { HOLD_SPREAD_MS } else { 0 };
    let phase_ms = (global as f64 * PHASE_FRACTION * SWEEP_PERIOD_MS as f64) as i64;
    // Base slot: evenly spaced around the ring, so in the first two-player tier the two bots are exactly antipodal.
    let base_deg = index as f64 / bot_count as f64 * 360.0;

    if bot_count >= 2 {
        look_at_peer(ctx, partner, 800).await;
    }
    ctx.record(
        TYPE_LOOK,
        &format!("vis-arc-open-{index}"),
        &format!(
            "Open facing bot {partner}; take the ring slot at {} deg.",
            r2(base_deg)
        ),
        json!({
            "partner": partner, "base_deg": r2(base_deg), "radius": r2(radius),
            "hold_ms": hold_ms, "phase_ms": phase_ms,
            "arc_sweep_deg": ARC_SWEEP_DEG, "sweep_period_ms": SWEEP_PERIOD_MS,
        }),
    );

    let mut round = 0usize;
    while !ctx.target_met() {
        let offset = sweep_offset_deg(ctx.elapsed_ms() + phase_ms);
        let angle = (base_deg + offset).to_radians();
        let slot = (
            anchor.0 + angle.cos() * radius,
            anchor.1 + angle.sin() * radius,
        );
        // Arrival tolerance far under the arc chord: a tolerance near the leg length lets the
        // planner declare arrival without the body taking a step, which is how the first eval hold
        // recorded zero moving frames and a 31.7 s frozen stretch.
        let arrived = ctx.move_to(slot.0, slot.1, 7_000, 1.2).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-arc-step-{index}-r{round}"),
            &format!("Walk the ring to {} deg off the base slot.", r2(offset)),
            json!({
                "round": round, "offset_deg": r2(offset),
                "slot": {"x": r2(slot.0), "z": r2(slot.1)},
                "arrived": arrived,
            }),
        );
        if ctx.target_met() {
            break;
        }

        let (Some(peer), Some(own)) = (ctx.peer_position(partner), ctx.position()) else {
            sleep_ms(300).await;
            round += 1;
            continue;
        };
        // Standing turn across the ring onto the partner, then hold that one bearing. Sampled once:
        // re-aiming at the partner through the hold, while both bodies keep moving along the ring,
        // is precisely the constant-sign coupling the camera/travel gate is built to catch.
        let confirmed = ctx.look_until_facing(head_of(peer), 6.0, 4_000).await;
        let bearing = look_angles(head_of(own), head_of(peer)).0;
        ctx.record(
            TYPE_LOOK,
            &format!("vis-arc-mutual-{index}-r{round}"),
            &format!("Look across the ring at bot {partner}; they are looking back."),
            json!({
                "round": round, "partner": partner, "bearing_deg": r2(bearing),
                "distance": r2((peer.0 - own.0).hypot(peer.2 - own.2)),
                "facing_confirmed": confirmed,
            }),
        );
        hold_facing(ctx, bearing, ctx.elapsed_ms() + hold_ms, SHUTTLE_LEG).await;
        round += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Named explicitly rather than left to the parent's glob, matching how
    // counter_ring_flow / orbit_lane_swap / satellite_escort import it in their tests.
    use crate::scenarios::common::MIN_PEER_GAP;

    /// The wave is the mechanism: it has to stay inside its amplitude, come back to zero at the
    /// period boundary, and actually reverse in the middle. Getting any of those wrong turns the
    /// scenario into the constant-sign orbit the camera/travel gate fails.
    #[test]
    fn the_sweep_returns_to_its_start_and_reverses_once_per_period() {
        assert!((sweep_offset_deg(0) + ARC_SWEEP_DEG / 2.0).abs() < 1e-9);
        assert!((sweep_offset_deg(SWEEP_PERIOD_MS / 2) - ARC_SWEEP_DEG / 2.0).abs() < 1e-9);
        assert!((sweep_offset_deg(SWEEP_PERIOD_MS) + ARC_SWEEP_DEG / 2.0).abs() < 1e-9);
        for step in 0..400 {
            let t = step * SWEEP_PERIOD_MS / 100;
            assert!(
                sweep_offset_deg(t).abs() <= ARC_SWEEP_DEG / 2.0 + 1e-9,
                "t={t}"
            );
        }
        // Rising for the first half, falling for the second: one reversal per period, not zero.
        let quarter = sweep_offset_deg(SWEEP_PERIOD_MS / 4);
        let three_quarters = sweep_offset_deg(3 * SWEEP_PERIOD_MS / 4);
        assert!(quarter > sweep_offset_deg(0));
        assert!(three_quarters < sweep_offset_deg(SWEEP_PERIOD_MS / 2));
        // The sweep must exceed the horizontal FOV, or the sightline never leaves what it saw.
        const { assert!(ARC_SWEEP_DEG > 102.5) };
    }

    /// The pair must stay far from each other despite the per-bot phase offset, or the "mutual"
    /// framing becomes a face filling the frame.
    #[test]
    fn the_phase_offset_keeps_the_pair_near_antipodal() {
        let radius = 8.0_f64;
        let mut worst = f64::INFINITY;
        for step in 0..200 {
            let t = step * SWEEP_PERIOD_MS / 50;
            let a = sweep_offset_deg(t);
            let b = sweep_offset_deg(t + (PHASE_FRACTION * SWEEP_PERIOD_MS as f64) as i64) + 180.0;
            let gap = ((b - a).to_radians() / 2.0).sin().abs() * 2.0 * radius;
            worst = worst.min(gap);
        }
        assert!(worst >= MIN_PEER_GAP, "closest approach {worst} blocks");
    }

    /// Nobody watches itself.
    #[test]
    fn partners_are_distinct() {
        for bots in 2..=16 {
            for index in 0..bots {
                assert_ne!(partner_of(index, bots), index, "bots={bots} index={index}");
                assert!(partner_of(index, bots) < bots);
            }
        }
    }
}
