//! CATALOG: family=pursuit min_players=2 single_process=true world_edits=false scenery=none far_players=false
//!
//! A courier walks a closed circuit; an observer stays on one post and keeps re-facing it.
//!
//! The revisit problem, both halves. **The trajectory loop** is the courier's: a hexagonal circuit
//! whose every node is `CIRCUIT_RADIUS` from the anchor, so it passes through the same six cells
//! lap after lap and its net displacement is bounded by the diameter no matter how long the episode
//! runs. **The sightline loop** is the observer's, and it is the reason this scenario exists: with
//! the courier on a closed circuit, the bearing from a FIXED post traverses a full revolution and
//! comes back to where it started, once per lap. Nothing in the script says "turn back" — the
//! geometry says it, which is what makes the return unforced.
//!
//! Occlusion comes from the terrain between post and circuit (`scenery=none`, so real world gen):
//! on any ground that is not a table the courier disappears behind a rise on part of every lap and
//! reappears at a predictable place. That is the recoverable kind of occlusion — the observer has
//! already seen both the courier and the ridge it went behind.
//!
//! **How this clears the camera/travel independence gate.** Walk-stop-look-walk, nothing else. Each
//! round is a STANDING `look_until_facing` (the body is not travelling, so `qc.rs:420` discards
//! those frames outright) followed by `hold_facing`. Re-issuing `LookAt` at the courier inside a
//! movement loop would be fighting `CameraIntent::Hold` for the camera, which is the shape measured
//! at 96-100% predictability.
//!
//! **What `hold_facing` actually does here.** It does NOT keep the camera on the bearing it is
//! given: `move_to` overwrites `camera_intent` with `CameraIntent::Hold` from the first leg, and
//! `Hold` faces travel while moving. Its job is to cover ground quietly between gazes —
//! which is exactly what is wanted, because the gate is TWO-SIDED (`agreement()` is `max(p, 1-p)`),
//! so any hand-driven camera correction during travel is a systematic signal whichever way it
//! points. The subject stays in frame through the DENSITY of the standing gazes instead: one every
//! `WATCH_MS`, which is under one 4.65 s training window, so a window that
//! contains no gaze at all is the exception.
//!
//! The courier itself moves on plain `move_to` legs, the idiom 65 generation-5 production scenarios pass on.

use super::*;

/// Nodes of the courier's circuit. Six gives a 6-block chord at `CIRCUIT_RADIUS` — one leg is then
/// a ~1.4 s walk / ~1.1 s sprint, comfortably over the 0.5 s key-chatter floor, while a whole lap
/// is short enough to close several times inside a 120 s episode.
const CIRCUIT_NODES: usize = 6;
/// Radius of the circuit, in blocks. Sets both the chord above and the ceiling on the courier's
/// net displacement (2 x 6 = 12 blocks worst case, and ~6 for the 4.65 s windows the audit reads).
const CIRCUIT_RADIUS: f64 = 6.0;
/// Where the observer posts up, as a distance from the anchor. The clearance that matters is not
/// post-to-circle but WORST-CASE body-to-body: `hold_facing` shuttles the observer up to
/// [`SHUTTLE_LEG`] blocks off its post on a drawn heading, so the courier's near pass can close by
/// that much on top. 15 - 6 - 4 = 5.0 blocks still clears [`MIN_PEER_GAP`]; the obvious 11 would
/// have left 1.0, which is a face filling the frame.
const POST_RADIUS: f64 = 15.0;
/// One observation round: how long the observer walks between two gazes at the courier. Kept under
/// the 4.65 s training window so most windows contain a gaze, and over `hold_facing`'s ~2.2 s leg
/// budget so each round still covers a full 4-block leg (frozen reads position, not intent).
const WATCH_MS: i64 = 3_000;
/// Shuttle leg for `hold_facing`, in blocks. 4.0 is the validated value — see its doc comment.
const SHUTTLE_LEG: f64 = 4.0;

/// The bot this one is paired with. At N=2 — the swarm size collected first — bot 0 and
/// bot 1 are each other's partner. A larger swarm pairs by index parity and sends an unmatched odd
/// bot back to bot 0, so nobody ends up watching itself.
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
    // Even bots courier, odd bots observe. Parity rather than a shared election so the roles need
    // no cross-process handshake and are stable for the whole episode.
    let is_courier = index.is_multiple_of(2);

    // Opening beat, before either role starts: face the partner. The swarm spawns inside
    // `spread_range` of the anchor, so this is the one moment both bodies are guaranteed close, and
    // it is what puts a teammate in the FIRST frame of every window that starts here.
    if bot_count >= 2 {
        look_at_peer(ctx, partner, 800).await;
    }
    ctx.record(
        TYPE_LOOK,
        &format!("vis-courier-open-{index}"),
        &format!(
            "Open facing partner bot {partner}; role is {}.",
            if is_courier { "courier" } else { "observer" }
        ),
        json!({
            "role": if is_courier { "courier" } else { "observer" },
            "partner": partner,
            "circuit_radius": CIRCUIT_RADIUS,
            "post_radius": POST_RADIUS,
        }),
    );

    if is_courier {
        courier(ctx, anchor, partner, bot_count).await;
    } else {
        observer(ctx, anchor, partner).await;
    }
}

/// Walk the closed circuit, lap after lap, glancing back at the observer once per lap.
async fn courier(ctx: &mut Ctx, anchor: (f64, f64), partner: usize, bot_count: usize) {
    // Phase the circuit by global index so two couriers in a larger swarm are never on the same
    // node at the same time — the pairwise near-clones check reads exactly that kind of lockstep.
    let phase = global_index(ctx) as f64 / CIRCUIT_NODES as f64 * std::f64::consts::TAU;
    let mut step = 0usize;
    while !ctx.target_met() {
        let angle = phase + step as f64 / CIRCUIT_NODES as f64 * std::f64::consts::TAU;
        let node = (
            anchor.0 + angle.cos() * CIRCUIT_RADIUS,
            anchor.1 + angle.sin() * CIRCUIT_RADIUS,
        );
        // Arrival radius well under the 6-block chord: a tolerance near the leg length lets the
        // planner declare victory without walking, which is how the first eval hold recorded zero
        // moving frames.
        let arrived = ctx.move_to(node.0, node.1, 7_000, 1.2).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-courier-node-{}-{step}", ctx.index()),
            &format!("Courier walks to circuit node {}.", step % CIRCUIT_NODES),
            json!({
                "role": "courier",
                "node": step % CIRCUIT_NODES,
                "lap": step / CIRCUIT_NODES,
                "target": {"x": r2(node.0), "z": r2(node.1)},
                "arrived": arrived,
            }),
        );
        // Once per lap, look back at the observer. This is the courier's own return: its camera has
        // been facing travel all the way round the circuit, so the glance is a scripted turn back to
        // a direction it last saw a lap ago, and it keeps the observer in the courier's POV too.
        if bot_count >= 2
            && step % CIRCUIT_NODES == CIRCUIT_NODES - 1
            && let Some(peer) = ctx.peer_position(partner)
        {
            let held = ctx.look_until_facing(head_of(peer), 6.0, 4_000).await;
            ctx.record(
                TYPE_LOOK,
                &format!("vis-courier-lookback-{}-{step}", ctx.index()),
                &format!("Courier closes the lap looking back at observer bot {partner}."),
                json!({
                    "role": "courier",
                    "lap": step / CIRCUIT_NODES,
                    "partner": partner,
                    "facing_confirmed": held,
                }),
            );
        }
        step += 1;
    }
}

/// Hold one post and re-face the courier once per round; the bearing does the looping.
async fn observer(ctx: &mut Ctx, anchor: (f64, f64), partner: usize) {
    // Post on the bearing this bot already spawned on, pushed out to `POST_RADIUS`. Keeping the
    // spawn bearing makes the approach a few blocks instead of a traverse across the circuit, so
    // the observation starts while the courier is still on its first lap.
    let start = ctx.position().unwrap_or((anchor.0, 64.0, anchor.1));
    let (ux, uz) = away_unit(Some(start), (anchor.0, start.1, anchor.1));
    let post = (anchor.0 + ux * POST_RADIUS, anchor.1 + uz * POST_RADIUS);
    let reached = travel_to(ctx, post, TRAVEL_HOP, 2.0).await;
    ctx.record(
        TYPE_MOVE,
        &format!("vis-courier-post-{}", ctx.index()),
        "Observer takes its post outside the courier's circuit.",
        json!({
            "role": "observer",
            "post": {"x": r2(post.0), "z": r2(post.1)},
            "residual": r2(reached),
        }),
    );

    let mut round = 0usize;
    while !ctx.target_met() {
        let Some(peer) = ctx.peer_position(partner) else {
            // A peer position can be missing for a few ticks after a chunk reload. Waiting beats
            // guessing a bearing, and the wait is far shorter than the 20 s frozen run that fails.
            sleep_ms(300).await;
            continue;
        };
        let Some(own) = ctx.position() else {
            sleep_ms(300).await;
            continue;
        };
        // STANDING turn: the whole camera motion of this round happens here, with the body still,
        // so it contributes no frames at all to the camera/travel gate's denominator.
        let facing = ctx.look_until_facing(head_of(peer), 6.0, 4_000).await;
        let bearing = look_angles(head_of(own), head_of(peer)).0;
        let distance = (peer.0 - own.0).hypot(peer.2 - own.2);
        ctx.record(
            TYPE_LOOK,
            &format!("vis-courier-watch-{}-r{round}", ctx.index()),
            &format!(
                "Observer re-faces courier bot {partner} at bearing {}.",
                r2(bearing)
            ),
            json!({
                "role": "observer",
                "round": round,
                "partner": partner,
                "bearing_deg": r2(bearing),
                "distance": r2(distance),
                "facing_confirmed": facing,
            }),
        );
        // Then cover ground until the next gaze. `hold_facing` hands the camera to `Hold` from its
        // first leg, so this is not "keep looking at the courier" — that is what the next round's
        // standing gaze is for.
        hold_facing(ctx, bearing, ctx.elapsed_ms() + WATCH_MS, SHUTTLE_LEG).await;
        round += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Named explicitly rather than left to the parent's glob, matching how
    // counter_ring_flow / orbit_lane_swap / satellite_escort import it in their tests.
    use crate::scenarios::common::MIN_PEER_GAP;

    /// Nobody may watch itself, and the parity pairing must be symmetric at N=2 — the swarm
    /// size collected first, and the size this script has to be complete at.
    #[test]
    fn partners_are_distinct_and_symmetric_at_two_bots() {
        assert_eq!(partner_of(0, 2), 1);
        assert_eq!(partner_of(1, 2), 0);
        for bots in 2..=16 {
            for index in 0..bots {
                assert_ne!(partner_of(index, bots), index, "bots={bots} index={index}");
                assert!(partner_of(index, bots) < bots);
            }
        }
    }

    /// The post has to sit outside the circuit by more than the peer gap PLUS the shuttle leg: the
    /// observer does not stand on its post, it paces up to a leg away from it, and the courier is
    /// coming the other way. Pinned because all three numbers are tuned independently.
    #[test]
    fn the_post_clears_the_circuit_even_at_the_far_end_of_a_shuttle() {
        const { assert!(POST_RADIUS - CIRCUIT_RADIUS - SHUTTLE_LEG >= MIN_PEER_GAP) };
    }
}
