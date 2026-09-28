//! CATALOG: family=pursuit min_players=2 single_process=true world_edits=false scenery=none far_players=false
//!
//! A follower holds a FIXED WORLD BEARING off a leashed wanderer — the flank shot — and switches
//! flanks on a clock, so the same subject is filmed from two opposite sides of the same ground.
//!
//! The revisit problem. **The trajectory loop** is the subject's leash: every leg that would take
//! it past
//! `LEASH` blocks from the anchor is bent back toward the anchor, so the wander is a bounded
//! re-entrant walk over one patch of terrain rather than a departure, and the follower's path is a
//! translated copy of it. **The sightline loop** is the flank switch: when the offset bearing flips
//! by `FLANK_SWITCH_DEG`, the follower crosses to the other side and the subject is re-framed
//! against terrain the follower was standing on two terms ago. Between switches the subject stays
//! dead-centre, which is the property the generation-5 frustum audit rewarded — `follow_leader` (0.70) and
//! `follow_cam_chain` (0.69) are the two best-scoring scenarios in the whole corpus.
//!
//! The occlusion here is the interesting kind: holding a world-fixed offset means the follower
//! sometimes ends up with a rise, a tree or a water body between it and the subject, and the offset
//! keeps pulling it around until the line reopens. Both the loss and the recovery are recorded.
//!
//! **How this clears the camera/travel independence gate.** Strictly walk-stop-look-walk (brief
//! §3.1). Nothing here tracks a peer while travelling: closing on the station is a plain `move_to`,
//! which owns the camera for the duration and faces it along travel, and every gaze at the subject
//! is a STANDING `look_until_facing` — the body is not moving, so `qc.rs:420` discards those frames
//! before the sign test runs. Re-issuing `LookAt` inside a movement loop would be fighting
//! `CameraIntent::Hold` for the camera, which is the shape measured at 96-100% predictability.
//!
//! Note the gate is TWO-SIDED (`agreement()` is `max(p, 1-p)`), so a camera that consistently turns
//! AWAY from travel fails exactly like one that consistently turns toward it. That is a second
//! reason not to hand-drive the camera during travel at all: `Hold`'s face-travel behaviour is the
//! idiom all 65 generation-5 production scenarios pass on, and any correction layered on top of it is a
//! systematic signal.
//!
//! The subject therefore stays in frame through the DENSITY of the standing gazes, not through
//! tracking: one gaze per in-band round, and a fresh one every time the flank station is reached.

use super::*;

/// How far from the anchor the subject may wander before its next leg is bent back. 12 blocks keeps
/// every 4.65 s audit window's displacement small while leaving room for real terrain features
/// between the pair.
const LEASH: f64 = 12.0;
/// The band the follower holds, in blocks. Must clear [`MIN_PEER_GAP`] by enough that a wander leg
/// toward the camera does not put a face in the frame; 7 minus the 2 blocks of band slack still
/// leaves 5 blocks between the bodies.
const STANDOFF: f64 = 7.0;
/// How long one flank lasts. Six terms fit a 120 s episode, so the subject is filmed from several
/// opposite sides and no single bearing dominates the footage.
const FLANK_TERM_MS: i64 = 20_000;
/// How far the offset bearing jumps at a flank switch. Past the 102.5 deg horizontal FOV on
/// purpose: the two flanks share no background, so the switch is a genuine "turn away and come
/// back" rather than a nudge.
const FLANK_SWITCH_DEG: f64 = 130.0;
/// Slack on the band before the follower bothers to reposition. Below this it holds instead of
/// tracking — precision-chasing a live target is what generates short corrective presses.
const BAND_SLACK: f64 = 2.0;
/// The walking gap between two gazes at the subject while in band. Under one 4.65 s training window
/// so most windows contain a gaze: `hold_facing` releases the camera to `Hold` from its first leg,
/// so gaze density is the only thing that keeps the subject in frame.
const HOLD_MS: i64 = 3_000;
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
        &format!("vis-flank-open-{index}"),
        &format!(
            "Open facing partner bot {partner}; role is {}.",
            if is_subject { "subject" } else { "flanker" }
        ),
        json!({
            "role": if is_subject { "subject" } else { "flanker" },
            "partner": partner, "standoff": STANDOFF, "leash": LEASH,
        }),
    );

    if is_subject || bot_count < 2 {
        wander(ctx, anchor, partner, bot_count).await;
    } else {
        flank(ctx, partner).await;
    }
}

/// Leashed wander: veer, walk a leg, and every third leg look back at the flanker so the follower
/// is in the subject's POV too rather than only behind it.
async fn wander(ctx: &mut Ctx, anchor: (f64, f64), partner: usize, bot_count: usize) {
    let mut heading = ctx.rng_next() * std::f64::consts::TAU;
    let mut leg = 0usize;
    while !ctx.target_met() {
        let Some(own) = ctx.position() else {
            sleep_ms(300).await;
            continue;
        };
        let from_anchor = (own.0 - anchor.0).hypot(own.2 - anchor.1);
        if from_anchor > LEASH {
            // Bend home, with jitter so successive returns do not retrace one line.
            heading = (anchor.1 - own.2).atan2(anchor.0 - own.0) + (ctx.rng_next() - 0.5);
        } else {
            heading += (ctx.rng_next() - 0.5) * 1.6;
        }
        // 4-6 blocks: over the key-chatter floor at sprint speed (a 4-block leg sprints in ~0.7 s,
        // walks in ~1.9 s) and under the leash, so one leg can never escape the patch.
        let distance = 4.0 + ctx.rng_next() * 2.0;
        let target = (
            own.0 + heading.cos() * distance,
            own.2 + heading.sin() * distance,
        );
        let arrived = ctx.move_to(target.0, target.1, 7_000, 1.2).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-flank-wander-{}-{leg}", ctx.index()),
            &format!("Wander {} blocks on a leashed bearing.", r2(distance)),
            json!({
                "role": "subject", "leg": leg,
                "heading_rad": r2(heading), "distance": r2(distance),
                "from_anchor": r2(from_anchor), "arrived": arrived,
            }),
        );
        if bot_count >= 2
            && leg % 3 == 2
            && let Some(peer) = ctx.peer_position(partner)
        {
            let confirmed = ctx.look_until_facing(head_of(peer), 6.0, 4_000).await;
            ctx.record(
                TYPE_LOOK,
                &format!("vis-flank-check-{}-{leg}", ctx.index()),
                &format!("Check where the flanker, bot {partner}, has got to."),
                json!({"role": "subject", "leg": leg, "partner": partner,
                        "facing_confirmed": confirmed}),
            );
        }
        leg += 1;
    }
}

/// Hold a world-fixed offset off the subject, switching flanks each term.
async fn flank(ctx: &mut Ctx, partner: usize) {
    let mut last_term: i64 = -1;
    // The offset bearing, in radians, in WORLD frame. Re-drawn per term and flipped by
    // `FLANK_SWITCH_DEG` so consecutive terms film from sides with no shared background.
    let mut offset = ctx.rng_next() * std::f64::consts::TAU;
    let mut round = 0usize;
    while !ctx.target_met() {
        let term = ctx.elapsed_ms().max(0) / FLANK_TERM_MS;
        if term != last_term {
            if last_term >= 0 {
                let side = if term % 2 == 0 { 1.0 } else { -1.0 };
                offset += FLANK_SWITCH_DEG.to_radians() * side;
            }
            last_term = term;
            let offset_deg = r2(offset.to_degrees());
            ctx.record(
                TYPE_LOOK,
                &format!("vis-flank-switch-{}-t{term}", ctx.index()),
                &format!("Switch to the flank at world bearing {offset_deg}."),
                json!({"role": "flanker", "term": term, "offset_deg": offset_deg,
                    "switch_deg": FLANK_SWITCH_DEG}),
            );
        }
        let (Some(peer), Some(own)) = (ctx.peer_position(partner), ctx.position()) else {
            sleep_ms(300).await;
            continue;
        };
        let station = (
            peer.0 + offset.cos() * STANDOFF,
            peer.2 + offset.sin() * STANDOFF,
        );
        let to_station = (station.0 - own.0).hypot(station.1 - own.2);
        if to_station > BAND_SLACK {
            // Out of band: walk to the station and do not touch the camera on the way. The gaze
            // comes on the NEXT pass, standing, once the body is where it belongs.
            let arrived = ctx.move_to(station.0, station.1, 5_000, 1.8).await;
            ctx.record(
                TYPE_MOVE,
                &format!("vis-flank-close-{}-r{round}", ctx.index()),
                &format!("Close on the flank station, {} blocks off.", r2(to_station)),
                json!({"role": "flanker", "round": round, "partner": partner,
                    "station": {"x": r2(station.0), "z": r2(station.1)},
                    "to_station": r2(to_station), "arrived": arrived}),
            );
        } else {
            // In band: the standing gaze — the only place this script aims the camera. Then
            // `hold_facing` covers ground quietly until the next gaze. It does NOT keep the camera
            // on `bearing`: `move_to` takes the camera over from its first leg. The
            // bearing is passed because the opening turn should start from the subject, and because
            // the recorded number is what makes the gaze auditable.
            let confirmed = ctx.look_until_facing(head_of(peer), 6.0, 4_000).await;
            let bearing = look_angles(head_of(own), head_of(peer)).0;
            ctx.record(
                TYPE_LOOK,
                &format!("vis-flank-hold-{}-r{round}", ctx.index()),
                &format!("On station: hold the subject at bearing {}.", r2(bearing)),
                json!({"role": "flanker", "round": round, "partner": partner,
                    "bearing_deg": r2(bearing),
                    "distance": r2((peer.0 - own.0).hypot(peer.2 - own.2)),
                    "facing_confirmed": confirmed}),
            );
            hold_facing(ctx, bearing, ctx.elapsed_ms() + HOLD_MS, SHUTTLE_LEG).await;
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

    /// The standoff has to clear the peer gap, and the flank switch has to exceed the horizontal
    /// FOV or the two flanks share a background and the switch teaches nothing. Both are tuned
    /// numbers with a reason, so pin the reason.
    #[test]
    fn the_band_clears_the_peer_gap_and_the_switch_clears_the_fov() {
        // Worst case: the follower is a whole slack short of its station and the subject has
        // just walked toward it. That distance still has to clear the peer gap.
        const { assert!(STANDOFF - BAND_SLACK >= MIN_PEER_GAP) };
        const { assert!(BAND_SLACK < STANDOFF - MIN_PEER_GAP) };
        const { assert!(FLANK_SWITCH_DEG > 102.5) };
    }

    /// Nobody flanks itself.
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
