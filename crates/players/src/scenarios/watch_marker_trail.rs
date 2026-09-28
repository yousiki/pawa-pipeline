//! CATALOG: family=construction min_players=2 single_process=true world_edits=true scenery=none far_players=false
//!
//! One bot lays a trail of markers around a closed circuit, looking back at the last one it laid
//! before walking on to the next; the other trails it and catches up on the fresh markers.
//!
//! The revisit problem. **The trajectory loop** is the circuit: `TRAIL_STATIONS` stations evenly
//! spaced on a ring, walked in order forever, so lap two lays markers alongside lap one's and the
//! layer re-enters ground it prepared a lap ago. **The sightline loop** is the back-and-forth
//! between LAID and UNLAID: at each station the layer places a marker, then turns back to the
//! previous marker to confirm it is still there, then turns forward again to the next site. Over a
//! lap that alternation sweeps a full revolution and returns, and — unlike a scenic pan — every
//! backward look has a concrete, verifiable object in it, one this bot put there itself and can
//! probe with
//! `column_find_block`. That turns "did the model reconstruct the observed region" into a question
//! with a block-level answer.
//!
//! The trailing bot supplies the second view of the same trail. Its `witness_peer_edits` gazes are
//! the world-synchronization beat: A's placement must show up in B's footage a few seconds later, or
//! the corpus never teaches that edits persist across viewpoints.
//!
//! **How this clears the camera/travel independence gate.** The layer's every camera move happens
//! while STANDING — placement, the look back, the look forward — and travel between stations is a
//! plain `move_to` leg with the camera facing travel, the idiom 65 generation-5 production scenarios pass on. A
//! frame only enters the gate's denominator when the camera turned AND the body was travelling
//! (`qc.rs:420`), so a standing turn contributes nothing at all. The trailer runs the same
//! walk-stop-look-walk shape: it closes its band with a plain `move_to` and never
//! touches the camera while travelling, then gazes from a standstill. Note the gate is TWO-SIDED
//! (`agreement()` is `max(p, 1-p)`), so hand-driving the camera away from travel would fail just as
//! surely as driving it toward travel — leaving `CameraIntent::Hold` alone is the only safe answer.

use super::*;

/// Trail material. Must be one of the kit items (`EQUIP_ITEMS`) or `equip` silently equips nothing.
/// Cobblestone rather than the stone bricks the eval landmarks use, so a marker of this family is
/// distinguishable from those in the footage.
const MATERIAL: &str = "minecraft:cobblestone";
/// Stations on the circuit. Six gives a chord of one radius at [`TRAIL_RADIUS`] — a 7-block leg,
/// which is one press of ~1.6 s walking or ~1.2 s sprinting, comfortably over the 0.5 s key-chatter
/// floor — and closes a lap often enough for the second lap to happen inside a 120 s episode.
const TRAIL_STATIONS: usize = 6;
/// Radius of the circuit, in blocks. Small enough that the whole trail is one recognizable place
/// rather than a traverse, and that a 4.65 s audit window's displacement stays a few blocks.
const TRAIL_RADIUS: f64 = 7.0;
/// The band the trailer holds behind the layer. Over [`MIN_PEER_GAP`] with slack, and close enough
/// that a marker being placed is legible in its frame.
const TRAIL_BAND: f64 = 7.0;
/// Slack before the trailer bothers to reposition; below it, hold instead. Chasing a live target
/// precisely is what generates the short corrective presses the key-chatter gate counts.
const BAND_SLACK: f64 = 2.5;
/// How long the layer holds the backward view of the marker it just laid.
const LOOK_BACK_MS: i64 = 3_500;
/// The walking gap between two gazes at the layer while in band. Under one 4.65 s training window,
/// for the same gaze-density reason as [`LOOK_BACK_MS`] — with the difference that the layer MOVES,
/// so falling behind on gazes actually loses it from frame.
const TRAIL_HOLD_MS: i64 = 3_000;
/// Vertical window used when probing whether a previously laid marker is still there. Covers the
/// vertical search `marker_base_near` does when the desired cell is unusable on natural terrain.
const PROBE_SPAN: i32 = 6;
/// Shuttle leg for `hold_facing`, in blocks. 4.0 is the validated value — see its doc comment.
const SHUTTLE_LEG: f64 = 4.0;

/// The bot this one is paired with; see the identical helper in the sibling `watch_*` scripts.
fn partner_of(index: usize, bot_count: usize) -> usize {
    let paired = index ^ 1;
    if paired < bot_count { paired } else { 0 }
}

/// World position of trail station `station`, for a layer whose circuit is rotated by `phase`.
fn station_xz(anchor: (f64, f64), phase: f64, station: usize) -> (f64, f64) {
    let angle = phase + station as f64 / TRAIL_STATIONS as f64 * std::f64::consts::TAU;
    (
        anchor.0 + angle.cos() * TRAIL_RADIUS,
        anchor.1 + angle.sin() * TRAIL_RADIUS,
    )
}

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let bot_count = ctx.player_count().max(1);
    let partner = partner_of(index, bot_count);
    let anchor = ctx.anchor();
    let is_layer = index.is_multiple_of(2);

    if bot_count >= 2 {
        look_at_peer(ctx, partner, 800).await;
    }
    ctx.record(
        TYPE_LOOK,
        &format!("vis-trail-open-{index}"),
        &format!(
            "Open facing partner bot {partner}; role is {}.",
            if is_layer { "layer" } else { "trailer" }
        ),
        json!({
            "role": if is_layer { "layer" } else { "trailer" },
            "partner": partner, "stations": TRAIL_STATIONS, "radius": TRAIL_RADIUS,
        }),
    );

    if is_layer || bot_count < 2 {
        lay_trail(ctx, anchor).await;
    } else {
        trail(ctx, partner).await;
    }
}

/// Walk the circuit laying one marker per station, alternating the gaze between the marker just
/// laid and the site about to be.
async fn lay_trail(ctx: &mut Ctx, anchor: (f64, f64)) {
    let index = ctx.index();
    // Rotate the circuit per bot so two layers in a larger swarm never share a station cell — and
    // so their action streams do not run in lockstep for the pairwise near-clones check.
    let phase = global_index(ctx) as f64 * 0.9;
    let start_y = ctx.position().map(|p| p.1).unwrap_or(64.0);
    // The cell of the marker laid at the previous station: the object every backward look aims at.
    let mut previous: Option<BlockPos> = None;
    let mut station = 0usize;
    while !ctx.target_met() {
        let site = station_xz(anchor, phase, station % TRAIL_STATIONS);
        let arrived = ctx.move_to(site.0, site.1, 8_000, 1.2).await;
        let Some(own) = ctx.position() else {
            sleep_ms(300).await;
            continue;
        };
        // One block, not a pillar: a trail is a sequence of small landmarks, and a single block is
        // cheap enough that a whole lap fits in the kit.
        let desired = (
            own.0.round() as i64,
            own.1.round() as i64,
            own.2.round() as i64,
        );
        let laid = raise_landmark(ctx, desired, 1, MATERIAL).await;
        ctx.record(
            TYPE_PLACE,
            &format!("vis-trail-lay-{index}-{station}"),
            &format!("Lay trail marker at station {}.", station % TRAIL_STATIONS),
            json!({
                "role": "layer", "station": station % TRAIL_STATIONS,
                "lap": station / TRAIL_STATIONS, "arrived": arrived,
                "placed": laid.is_some(),
                "site": {"x": r2(site.0), "z": r2(site.1)},
            }),
        );

        // Look BACK at the previous marker and probe whether it survived. `look_until_facing`
        // rather than `look`, because a wide turn that merely timed out would otherwise be recorded
        // as a confirmed return — the exact defect that made `look` unusable for placements.
        if let Some(cell) = previous {
            let point = (
                cell.x as f64 + 0.5,
                cell.y as f64 + 0.5,
                cell.z as f64 + 0.5,
            );
            let confirmed = ctx.look_until_facing(point, 6.0, 5_000).await;
            let still_there = column_find_block(
                ctx,
                cell.x,
                cell.z,
                (cell.y - PROBE_SPAN, cell.y + PROBE_SPAN),
                MATERIAL,
            );
            let back_yaw = look_angles(head_of(own), point).0;
            ctx.record(
                TYPE_LOOK,
                &format!("vis-trail-back-{index}-{station}"),
                "Look back along the laid trail at the previous marker.",
                json!({
                    "role": "layer", "station": station % TRAIL_STATIONS,
                    "previous": {"x": cell.x, "y": cell.y, "z": cell.z},
                    "still_present": still_there.is_some(),
                    "back_yaw_deg": r2(back_yaw), "facing_confirmed": confirmed,
                }),
            );
            // Hold the BACKWARD bearing as a fixed number while shuttling — the marker is a fixed
            // world point, and tracking a fixed point through a shuttle is the measured 96-100%
            // camera/travel failure. Sampled once, it is just a heading.
            hold_facing(ctx, back_yaw, ctx.elapsed_ms() + LOOK_BACK_MS, SHUTTLE_LEG).await;
        }
        if let Some((base, _)) = laid {
            previous = Some(base);
        }
        if ctx.target_met() {
            break;
        }

        // Then look FORWARD at the next, unlaid site: the other end of the alternation, and the
        // beat that swings the gaze back around the circuit.
        let next = station_xz(anchor, phase, (station + 1) % TRAIL_STATIONS);
        let ahead = (next.0, start_y + 1.0, next.1);
        let confirmed = ctx.look_until_facing(ahead, 8.0, 5_000).await;
        ctx.record(
            TYPE_LOOK,
            &format!("vis-trail-ahead-{index}-{station}"),
            "Turn forward to the next, still unlaid station.",
            json!({
                "role": "layer", "station": station % TRAIL_STATIONS,
                "next": {"x": r2(next.0), "z": r2(next.1)},
                "facing_confirmed": confirmed,
            }),
        );
        station += 1;
    }
}

/// Trail the layer at a readable distance and keep catching up on the markers it leaves behind.
async fn trail(ctx: &mut Ctx, partner: usize) {
    let mut round = 0usize;
    while !ctx.target_met() {
        let (Some(peer), Some(own)) = (ctx.peer_position(partner), ctx.position()) else {
            sleep_ms(300).await;
            continue;
        };
        let gap = (peer.0 - own.0).hypot(peer.2 - own.2);
        if (gap - TRAIL_BAND).abs() > BAND_SLACK {
            // Out of band: walk to a point one band short of the layer, camera untouched. The gaze
            // happens on the next pass, standing.
            let (ux, uz) = away_unit(Some(own), peer);
            let target = (peer.0 + ux * TRAIL_BAND, peer.2 + uz * TRAIL_BAND);
            let arrived = ctx.move_to(target.0, target.1, 6_000, 1.8).await;
            ctx.record(
                TYPE_MOVE,
                &format!("vis-trail-close-{}-r{round}", ctx.index()),
                &format!(
                    "Close on the layer, {} blocks off the band.",
                    r2(gap - TRAIL_BAND)
                ),
                json!({"role": "trailer", "round": round, "partner": partner,
                    "gap": r2(gap), "band": TRAIL_BAND, "arrived": arrived}),
            );
        } else {
            // In band: the standing gaze, then `hold_facing` to cover ground until the next one.
            // `hold_facing` does not hold this bearing past its first leg; the layer
            // stays in frame through the density of these gazes, not through tracking.
            let confirmed = ctx.look_until_facing(head_of(peer), 6.0, 4_000).await;
            let bearing = look_angles(head_of(own), head_of(peer)).0;
            ctx.record(
                TYPE_LOOK,
                &format!("vis-trail-watch-{}-r{round}", ctx.index()),
                &format!("Watch the layer work, at bearing {}.", r2(bearing)),
                json!({"role": "trailer", "round": round, "partner": partner,
                    "gap": r2(gap), "bearing_deg": r2(bearing),
                    "facing_confirmed": confirmed}),
            );
            hold_facing(ctx, bearing, ctx.elapsed_ms() + TRAIL_HOLD_MS, SHUTTLE_LEG).await;
        }
        // Catch up on a marker the layer left behind a few seconds ago. This is the beat that puts
        // A's edit into B's footage, with the edit's age recorded alongside it.
        witness_peer_edits(ctx).await;
        round += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Named explicitly rather than left to the parent's glob, matching how
    // counter_ring_flow / orbit_lane_swap / satellite_escort import it in their tests.
    use crate::scenarios::common::MIN_PEER_GAP;

    /// The circuit has to close — station `TRAIL_STATIONS` must be station 0 again — or the "trail"
    /// is a spiral and the layer never re-enters ground it prepared.
    #[test]
    fn the_circuit_closes_and_its_legs_clear_the_chatter_floor() {
        let anchor = (12.0, -34.0);
        let first = station_xz(anchor, 0.7, 0);
        let wrapped = station_xz(anchor, 0.7, TRAIL_STATIONS);
        assert!((first.0 - wrapped.0).abs() < 1e-6 && (first.1 - wrapped.1).abs() < 1e-6);
        // Adjacent stations must be far enough apart that one leg is a long press. A 4-block leg
        // sprints in ~0.7 s against a 0.5 s floor, so anything at or over 4 is safe.
        let next = station_xz(anchor, 0.7, 1);
        assert!((first.0 - next.0).hypot(first.1 - next.1) >= 4.0);
        // And every station must sit on the ring, at the radius the doc comment claims.
        for station in 0..TRAIL_STATIONS {
            let (x, z) = station_xz(anchor, 0.7, station);
            assert!(((x - anchor.0).hypot(z - anchor.1) - TRAIL_RADIUS).abs() < 1e-9);
        }
    }

    /// The trailing band has to clear the peer gap even at the near edge of its slack.
    #[test]
    fn the_band_clears_the_peer_gap_at_its_near_edge() {
        const { assert!(TRAIL_BAND - BAND_SLACK >= MIN_PEER_GAP) };
    }

    /// Nobody trails itself.
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
