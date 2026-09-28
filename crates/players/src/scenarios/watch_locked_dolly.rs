//! CATALOG: family=pursuit min_players=2 single_process=true world_edits=false scenery=none far_players=false
//!
//! A camera operator locks its view on a pacing subject and DOLLIES sideways — strafing left, then
//! the same distance right — so the subject stays dead-centre for the entire travelling shot.
//!
//! The revisit problem. **The trajectory loop** is the strafe pair: `DOLLY_MS` left followed by
//! `DOLLY_MS` right returns the body to where it started, because sideways speed is symmetric in a
//! way forward/backward speed is not. Net displacement per pair is a rounding error while path
//! length keeps growing — the ideal ratio for the audit, which reads exactly that pair of numbers.
//! The subject contributes its own loop, pacing between two posts across the anchor. **The
//! sightline loop** is the tightest in the family: the camera does not leave the subject at all
//! during a dolly, so the ground truth of every window inside one is a region the context frame
//! already contained, re-rendered from a translated viewpoint. That is the one condition under
//! which a pixel metric over a 92-frame prediction is straightforwardly interpretable, and it is
//! also the hardest thing to get from a corpus where the camera faces travel by default.
//!
//! **How this clears the camera/travel independence gate — and the caveat.** The gate only counts a
//! frame when the camera turned AND the body was travelling (`qc.rs:420`), and it is two-sided
//! (`agreement()` is `max(p, 1-p)`), so a starved denominator is the only robust answer — a camera
//! that consistently turns away from travel fails as surely as one that turns toward it. A dolly leg
//! has zero
//! camera turn by construction: `hold_visual` freezes `CameraIntent::LookAt` at the current angle and
//! drives ONE continuous movement key, so every frame of the leg is discarded before the sign test
//! runs. All the camera motion is in the standing re-face between pairs, where there is no travel.
//! The gate is therefore not merely passed but starved of evidence. **Caveat, stated plainly:**
//! `hold_visual` is documented as a calibration primitive and has no episodes of QC evidence behind
//! it in this shape, unlike `hold_facing`. So this script hedges — every `DOLLY_ROUNDS_PER_HOLD`
//! rounds it falls back to one `hold_facing` block — which covers ground with the camera left to
//! `CameraIntent::Hold`, NOT pinned on the bearing it is passed — and
//! a dolly leg that `hold_visual` reports as stalled (it returns `false` when the body stopped
//! translating, which on real terrain means a trunk or a ledge) is followed by a `move_to` re-post
//! rather than another blind press.
//!
//! Key chatter is safe by a wide margin: one dolly leg is a single press of `DOLLY_MS`, over twice
//! the 0.5 s `SHORT_RUN_FRAMES` floor, and the standing re-face between legs gives the movement latch
//! its `min_dwell_ticks` before the direction flips.

use super::*;

/// One dolly leg. 1.4 s of strafe covers roughly 6 blocks at walking speed — a long single press
/// (28 ticks against the 10-tick chatter floor) that still keeps the pair's net displacement near
/// zero and the subject inside the 102.5 deg frame at [`DOLLY_STANDOFF`].
const DOLLY_MS: u64 = 1_400;
/// The distance the operator wants between itself and the subject. Well over [`MIN_PEER_GAP`], and
/// far enough that 6 blocks of sideways travel swings the subject only ~35 deg off centre, so it
/// never leaves the frame mid-dolly.
const DOLLY_STANDOFF: f64 = 9.0;
/// Slack on the standoff before the operator re-posts instead of dollying.
const STANDOFF_SLACK: f64 = 3.0;
/// How many dolly rounds run between fallback `hold_facing` blocks. Three keeps the unproven idiom
/// to roughly three quarters of the operator's movement while guaranteeing the validated hold shape
/// appears several times per episode.
const DOLLY_ROUNDS_PER_HOLD: usize = 3;
/// One fallback hold.
const FALLBACK_HOLD_MS: i64 = 4_000;
/// How far apart the subject's two posts are, in blocks. Long enough to be a real 4+ block leg, short
/// enough that the pacing stays inside one frame of the operator's locked camera.
const PACE_SPAN: f64 = 6.0;
/// Shuttle leg for `hold_facing`, in blocks. 4.0 is the validated value — see its doc comment.
const SHUTTLE_LEG: f64 = 4.0;

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
    let is_subject = index.is_multiple_of(2);

    if bot_count >= 2 {
        look_at_peer(ctx, partner, 800).await;
    }
    ctx.record(
        TYPE_LOOK,
        &format!("vis-dolly-open-{index}"),
        &format!(
            "Open facing partner bot {partner}; role is {}.",
            if is_subject { "subject" } else { "operator" }
        ),
        json!({
            "role": if is_subject { "subject" } else { "operator" },
            "partner": partner, "standoff": DOLLY_STANDOFF, "dolly_ms": DOLLY_MS,
        }),
    );

    if is_subject || bot_count < 2 {
        pace(ctx, anchor, partner, bot_count).await;
    } else {
        operate(ctx, partner).await;
    }
}

/// Pace between two fixed posts across the anchor, glancing at the operator at each turnaround.
async fn pace(ctx: &mut Ctx, anchor: (f64, f64), partner: usize, bot_count: usize) {
    let index = ctx.index();
    // Lay the pacing line on the bearing this bot spawned on, so the first leg starts immediately
    // rather than after a traverse, and rotate it per bot so two subjects never share a line.
    let start = ctx.position().unwrap_or((anchor.0, 64.0, anchor.1));
    let bearing = (start.2 - anchor.1).atan2(start.0 - anchor.0) + global_index(ctx) as f64 * 0.7;
    let half = PACE_SPAN / 2.0;
    let posts = [
        (
            anchor.0 + bearing.cos() * half,
            anchor.1 + bearing.sin() * half,
        ),
        (
            anchor.0 - bearing.cos() * half,
            anchor.1 - bearing.sin() * half,
        ),
    ];
    let mut leg = 0usize;
    while !ctx.target_met() {
        let post = posts[leg % posts.len()];
        let arrived = ctx.move_to(post.0, post.1, 6_000, 1.2).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-dolly-pace-{index}-{leg}"),
            &format!("Pace to post {} of the line.", leg % posts.len()),
            json!({
                "role": "subject", "leg": leg, "post": leg % posts.len(),
                "target": {"x": r2(post.0), "z": r2(post.1)}, "arrived": arrived,
            }),
        );
        // At each turnaround, face the operator. It keeps the operator in the subject's POV, and it
        // is a standing turn back to a direction last seen a leg ago — the subject's own return.
        if bot_count >= 2
            && let Some(peer) = ctx.peer_position(partner)
        {
            let confirmed = ctx.look_until_facing(head_of(peer), 6.0, 4_000).await;
            ctx.record(
                TYPE_LOOK,
                &format!("vis-dolly-glance-{index}-{leg}"),
                &format!("Turnaround: face the operator, bot {partner}."),
                json!({"role": "subject", "leg": leg, "partner": partner,
                        "facing_confirmed": confirmed}),
            );
        }
        leg += 1;
    }
}

/// Lock the camera on the subject and dolly sideways, with a validated hold as the periodic hedge.
async fn operate(ctx: &mut Ctx, partner: usize) {
    let index = ctx.index();
    let mut round = 0usize;
    while !ctx.target_met() {
        let (Some(peer), Some(own)) = (ctx.peer_position(partner), ctx.position()) else {
            sleep_ms(300).await;
            continue;
        };
        let gap = (peer.0 - own.0).hypot(peer.2 - own.2);
        if (gap - DOLLY_STANDOFF).abs() > STANDOFF_SLACK {
            // Off the standoff: re-post before shooting. Plain `move_to`, camera facing travel —
            // the idiom 65 generation-5 production scenarios pass on — because framing is not the job here.
            let (ux, uz) = away_unit(Some(own), peer);
            let target = (peer.0 + ux * DOLLY_STANDOFF, peer.2 + uz * DOLLY_STANDOFF);
            let arrived = ctx.move_to(target.0, target.1, 6_000, 1.5).await;
            ctx.record(
                TYPE_MOVE,
                &format!("vis-dolly-repost-{index}-r{round}"),
                &format!("Re-post to the {} block standoff.", r2(DOLLY_STANDOFF)),
                json!({"role": "operator", "round": round, "partner": partner,
                    "gap": r2(gap), "arrived": arrived}),
            );
            round += 1;
            continue;
        }

        // The standing re-face. Every degree of camera motion this round happens here, with the body
        // still, so none of it can reach the camera/travel gate's denominator. Confirmed rather than
        // deadline-waited, because a dolly aimed off-target is a wasted round.
        let locked = ctx.look_until_facing(head_of(peer), 5.0, 5_000).await;
        ctx.record(
            TYPE_LOOK,
            &format!("vis-dolly-lock-{index}-r{round}"),
            &format!("Lock the camera on bot {partner} before the dolly."),
            json!({"role": "operator", "round": round, "partner": partner,
                "gap": r2(gap), "locked": locked}),
        );

        if !locked || round % DOLLY_ROUNDS_PER_HOLD == DOLLY_ROUNDS_PER_HOLD - 1 {
            // The hedge: a `hold_facing` block on the bearing just sampled. Runs on the validated
            // shape, and also covers the case where the lock did not converge, where a blind strafe
            // would be pointing at nothing.
            let bearing = look_angles(head_of(own), head_of(peer)).0;
            let reason = if locked { "cadence" } else { "lock_failed" };
            ctx.record(
                TYPE_MOVE,
                &format!("vis-dolly-fallback-{index}-r{round}"),
                "Fallback shuttle hold on the locked bearing.",
                json!({"role": "operator", "round": round, "reason": reason,
                    "bearing_deg": r2(bearing)}),
            );
            hold_facing(
                ctx,
                bearing,
                ctx.elapsed_ms() + FALLBACK_HOLD_MS,
                SHUTTLE_LEG,
            )
            .await;
            round += 1;
            continue;
        }

        // The dolly proper: two symmetric strafes with the camera frozen. Left-then-right, or
        // right-then-left on odd rounds, so the operator does not accumulate drift in one direction
        // and so consecutive rounds are not identical action streams.
        let (first, second) = if round.is_multiple_of(2) {
            (WalkDirection::Left, WalkDirection::Right)
        } else {
            (WalkDirection::Right, WalkDirection::Left)
        };
        // `sprint=false`: sprint is forward-effective only, so it would do nothing here except make
        // the two halves asymmetric. `jump_period_ms=0`: a jump during a locked shot is camera shake.
        let out = ctx.hold_visual(first, false, DOLLY_MS, 0).await;
        let back = if out {
            ctx.hold_visual(second, false, DOLLY_MS, 0).await
        } else {
            false
        };
        ctx.record(
            TYPE_MOVE,
            &format!("vis-dolly-shot-{index}-r{round}"),
            "Dolly sideways and back with the camera locked on the subject.",
            json!({
                "role": "operator", "round": round, "partner": partner,
                "dolly_ms": DOLLY_MS,
                "out_completed": out, "back_completed": back,
                "camera_locked_during_travel": true,
            }),
        );
        if !out || !back {
            // A stalled press means the body walked into terrain. Re-posting is the only thing that
            // helps — another blind press from the same spot would stall the same way, which is the
            // failure mode `shake_loose` exists for elsewhere in this crate.
            shake_loose(ctx).await;
        }
        round += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Named explicitly rather than left to the parent's glob, matching how
    // counter_ring_flow / orbit_lane_swap / satellite_escort import it in their tests.
    use crate::scenarios::common::MIN_PEER_GAP;

    /// The two numbers this scenario lives or dies on: the press must clear the key-chatter floor,
    /// and the standoff must clear the peer gap even at the near edge of its slack.
    #[test]
    fn the_dolly_press_and_standoff_are_inside_their_gates() {
        // SHORT_RUN_FRAMES is 10 ticks = 500 ms; anything under it counts as chatter.
        const { assert!(DOLLY_MS >= 1_000) };
        const { assert!(DOLLY_STANDOFF - STANDOFF_SLACK >= MIN_PEER_GAP) };
        // The hedge has to actually fire inside an episode, not once at the very end.
        assert!((2..=4).contains(&DOLLY_ROUNDS_PER_HOLD));
    }

    /// Nobody films itself.
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
