//! CATALOG: family=group_motion min_players=2 single_process=true world_edits=true scenery=none far_players=false

use super::*;

/// A teammate is eclipsed and reappears — the occluded subject is a PLAYER, not scenery.
///
/// The revisit problem, this face of it: in an earlier generation-5 collection, 22.4% of
/// multi-player windows have a peer walking into frame from nowhere, which asks the model to invent
/// a player the context frame never showed. The inverse case — a peer that WAS in the opening
/// frame, disappears behind something, and comes back to the same place — does not occur in the
/// corpus at all, and it is the one where the ground truth is an observed region and a pixel metric
/// therefore means something.
///
/// The eclipse is symmetric and both sides record it: the eclipser walks between a station on the
/// screen's axis (hidden) and one well off it (visible), while the beacon stands its ground, tracks
/// the peer with `look_until_facing`, and probes its own sight line each pass. So a single episode
/// yields the event from the disappearing side and the watching side simultaneously — something a
/// two-player hard-coded pipeline cannot stage and our N=1..32 gets for free.
///
/// The occluder is BUILT rather than found. Terrain relief would be more natural, but a peer eclipse
/// needs the shadow to fall on a known line between two specific bodies, and only a wall the beacon
/// raises itself guarantees that on any terrain we spawn into.
const MATERIAL: &str = "minecraft:stone_bricks";

/// Screen geometry. At 2 blocks from the beacon's eye a 2-tall wall spans roughly -39 deg to
/// +11 deg vertically, which covers a player's 1.8-block body at any distance beyond a few blocks;
/// pushed further out the same wall stops reaching eye level. Three lanes give a +/-37 deg shadow,
/// so the eclipser has a ~9-block-wide corridor to hide in at the eclipse radius instead of a
/// knife-edge the arrival slop would miss.
const SCREEN_DISTANCE: f64 = 2.0;
const SCREEN_HEIGHT: i64 = 2;
const SCREEN_LANES: [i64; 3] = [-1, 0, 1];

/// How far out the eclipser orbits. Comfortably past `MIN_PEER_GAP`, and short enough that the
/// station-to-station chord stays a walk rather than a journey.
const ECLIPSE_RADIUS: f64 = 12.0;

/// Angle off the screen's axis for the visible station. It has to exceed the screen's ~37 deg
/// shadow half-angle with margin, and 0.9 rad (51.6 deg) also makes the chord between the two
/// stations ~10 blocks — several full-length legs, well clear of the key-chatter floor.
const SHOW_ANGLE_RAD: f64 = 0.9;

/// One shuttle leg, in blocks. 4 clears the key-chatter floor at sprint speed and matches
/// Solaris's 3-4 block random-walk primitive; see `hold_facing`.
const SHUTTLE_LEG: f64 = 4.0;

/// A steady shot on the aimed bearing before the shuttle starts.
///
/// `hold_facing` hands the camera to `CameraIntent::Hold` the moment its first leg begins, and
/// `Hold` faces the direction of travel — so an aimed bearing survives only while the body is
/// standing. Without a standing beat the recovery would be recorded during a shuttle that is
/// swinging the camera through a fresh random heading every leg, and no training window would ever
/// contain a readable view of it. Four seconds is just under one 93-frame window at 20 fps, so a
/// whole window fits inside a steady shot, and it is far inside the 20 s of stillness the frozen
/// gate allows.
const AIMED_DWELL_MS: u64 = 4_000;

/// Holds, staggered per bot. Beacon and eclipser already run different code, so the near-clones
/// check is safe at N=2; the stagger covers swarms with several eclipsers.
const STATION_HOLD_MS: i64 = 8_000;
const BEACON_HOLD_MS: i64 = 6_000;
const HOLD_STAGGER_MS: i64 = 800;

/// Whether the straight sight line from `eye` to `target` is interrupted by a solid block. Copied
/// per-file on purpose, the way `wade_column_detour` copied `ridge_valley_circuit`'s surface probe,
/// so no scenario file depends on a sibling scenario module.
///
/// `block_is_safe_support` is false for an unloaded chunk, which is the honest reading: an unknown
/// cell must not be reported as a wall.
fn los_blocked(ctx: &Ctx, eye: (f64, f64, f64), target: (f64, f64, f64)) -> bool {
    let (dx, dy, dz) = (target.0 - eye.0, target.1 - eye.1, target.2 - eye.2);
    let span = (dx * dx + dy * dy + dz * dz).sqrt();
    let steps = (span * 2.0).ceil() as i32;
    (1..steps).any(|step| {
        let t = step as f64 / steps as f64;
        block_is_safe_support(
            ctx,
            BlockPos::new(
                (eye.0 + dx * t).floor() as i32,
                (eye.1 + dy * t).floor() as i32,
                (eye.2 + dz * t).floor() as i32,
            ),
        )
    })
}

/// The eclipse axis, agreed by every bot in one adjacent pair without a message: the direction from
/// that pair's beacon toward its eclipser. `single_process` guarantees local indices are the whole swarm, so
/// every bot reads the same two bodies and derives the same axis the beacon builds its screen on.
/// Where the eclipse is measured from, and along which direction: the beacon's own position paired
/// with the unit vector pointing from it toward the first peer.
type EclipseAxis = ((f64, f64, f64), (f64, f64));

fn eclipse_axis(ctx: &Ctx, beacon_index: usize, first_index: usize) -> Option<EclipseAxis> {
    let beacon = ctx.peer_position(beacon_index)?;
    let first = ctx.peer_position(first_index)?;
    let (dx, dz) = (first.0 - beacon.0, first.2 - beacon.2);
    let span = dx.hypot(dz);
    if span < 1.0 {
        return None;
    }
    Some((beacon, (dx / span, dz / span)))
}

/// Beacon: hold the ground the screen was built on, keep the peer's bearing, and record whether the
/// peer is currently behind the screen. This is the WATCHING side of the eclipse, and it does no
/// travelling of its own beyond `hold_facing`'s shuttle — a beacon that wandered would move the
/// shadow it is supposed to anchor.
async fn run_beacon(ctx: &mut Ctx, watched: usize) {
    let index = ctx.index();
    let global = global_index(ctx);
    let mut cycle = 0usize;
    while !ctx.target_met() {
        let Some(peer) = ctx.peer_position(watched) else {
            // No peer geometry yet: shuffle rather than stand, so the frozen gate never sees a
            // stationary beacon while the swarm is still connecting.
            hold_facing(ctx, 0.0, ctx.elapsed_ms() + 3_000, SHUTTLE_LEG).await;
            continue;
        };
        let focus = head_of(peer);
        let arrived = ctx.look_until_facing(focus, 6.0, 5_000).await;
        let eye = ctx.position().map(head_of);
        let clear = eye.is_some_and(|e| !los_blocked(ctx, e, focus));
        let peer_yaw = eye.map(|e| look_angles(e, focus).0).unwrap_or(0.0);
        let range = eye
            .map(|e| (peer.0 - e.0).hypot(peer.2 - e.2))
            .unwrap_or(0.0);
        ctx.record(
            "observe",
            &format!("vis-occl-eclipse-watch-{index}-{cycle}"),
            "Watch the teammate across the screen: record whether it is currently eclipsed.",
            json!({
                "cycle": cycle,
                "role": "beacon",
                "watched_peer": watched,
                "camera_arrived": arrived,
                "peer_visible": clear,
                "peer_yaw_deg": r2(peer_yaw),
                "peer_range": r2(range),
                "elapsed_ms": ctx.elapsed_ms(),
            }),
        );
        // A steady shot before the shuttle takes the camera: `hold_facing`'s first leg hands it to
        // `CameraIntent::Hold`, which faces travel, so this is the only stretch that actually holds
        // the aimed bearing.
        ctx.still(AIMED_DWELL_MS).await;
        hold_facing(
            ctx,
            peer_yaw,
            ctx.elapsed_ms() + BEACON_HOLD_MS + global as i64 * HOLD_STAGGER_MS,
            SHUTTLE_LEG,
        )
        .await;
        cycle += 1;
    }
}

/// Eclipser: walk between the station inside the screen's shadow and the one clear of it, facing
/// the beacon at both. The loop is the trajectory; the bearing to the beacon changes by only the
/// station angle, so the recovery is scored on a view that barely turned.
async fn run_eclipser(ctx: &mut Ctx, beacon_index: usize, first_index: usize) {
    let index = ctx.index();
    let global = global_index(ctx);
    // Several eclipsers would otherwise stack on one pair of stations and violate `MIN_PEER_GAP`;
    // pushing each one further out keeps them on the same shadow corridor without colliding.
    let radius = ECLIPSE_RADIUS;
    let mut cycle = 0usize;
    while !ctx.target_met() {
        let Some((origin, axis)) = eclipse_axis(ctx, beacon_index, first_index) else {
            hold_facing(ctx, 0.0, ctx.elapsed_ms() + 3_000, SHUTTLE_LEG).await;
            continue;
        };
        let axis_rad = axis.1.atan2(axis.0);
        // Alternate which side of the axis the visible station sits on, so the episode is not one
        // arc walked over and over.
        let side = if (cycle + global).is_multiple_of(2) {
            1.0
        } else {
            -1.0
        };
        let show_rad = axis_rad + SHOW_ANGLE_RAD * side;
        let stations = [
            (
                "show",
                (
                    origin.0 + show_rad.cos() * radius,
                    origin.2 + show_rad.sin() * radius,
                ),
            ),
            (
                "hide",
                (
                    origin.0 + axis_rad.cos() * radius,
                    origin.2 + axis_rad.sin() * radius,
                ),
            ),
        ];
        for (phase, station) in stations {
            if ctx.target_met() {
                return;
            }
            travel_to(ctx, station, TRAVEL_HOP, 2.0).await;
            let Some(beacon) = ctx.peer_position(beacon_index) else {
                continue;
            };
            let focus = head_of(beacon);
            // Face the beacon at BOTH stations: at "show" this is the recovery being scored, at
            // "hide" it is the honest framing of the screen that took the peer away.
            let arrived = ctx.look_until_facing(focus, 6.0, 5_000).await;
            let eye = ctx.position().map(head_of);
            let clear = eye.is_some_and(|e| !los_blocked(ctx, e, focus));
            let beacon_yaw = eye.map(|e| look_angles(e, focus).0).unwrap_or(0.0);
            ctx.record(
                if phase == "hide" {
                    TYPE_MOVE
                } else {
                    "observe"
                },
                &format!("vis-occl-eclipse-{phase}-{index}-{cycle}"),
                "Move between the screen's shadow and the clear station, facing the beacon.",
                json!({
                    "cycle": cycle,
                    "role": "eclipser",
                    "phase": phase,
                    "is_recovery": phase == "show" && cycle > 0,
                    "camera_arrived": arrived,
                    "beacon_visible": clear,
                    "beacon_yaw_deg": r2(beacon_yaw),
                    "station_radius": r2(radius),
                    "side": side,
                    "elapsed_ms": ctx.elapsed_ms(),
                }),
            );
            // A steady shot before the shuttle takes the camera: `hold_facing`'s first leg hands it to
            // `CameraIntent::Hold`, which faces travel, so this is the only stretch that actually holds
            // the aimed bearing.
            ctx.still(AIMED_DWELL_MS).await;
            hold_facing(
                ctx,
                beacon_yaw,
                ctx.elapsed_ms() + STATION_HOLD_MS + global as i64 * HOLD_STAGGER_MS,
                SHUTTLE_LEG,
            )
            .await;
            // The hold shuttles four blocks off station, which at the shadow's edge can change the
            // answer. Record the flicker rather than claim the phase held throughout.
            let after = ctx
                .position()
                .map(head_of)
                .is_some_and(|e| !los_blocked(ctx, e, focus));
            ctx.record(
                "observe",
                &format!("vis-occl-eclipse-{phase}-end-{index}-{cycle}"),
                "Re-probe the peer's sight line at the end of the station hold.",
                json!({
                    "cycle": cycle,
                    "phase": phase,
                    "visible_on_arrival": clear,
                    "visible_at_hold_end": after,
                    "flickered": clear != after,
                }),
            );
        }
        cycle += 1;
    }
}

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    // Each adjacent pair owns one eclipse: slot 0 is its beacon and slot 1 walks the shadow. This
    // preserves the N=2 behavior while making N=4+ a set of independent, non-overlapping pair
    // experiments instead of sending every extra bot to bot 0's screen.
    let pair_base = (index / 2) * 2;
    let is_beacon = index == pair_base;
    let partner = if is_beacon { pair_base + 1 } else { pair_base };

    // Open facing the partner, before any block goes down: hard rule 5 wants the other player in
    // the first frame, and in this scenario that player IS the subject.
    if let Some(peer) = ctx.peer_position(partner) {
        ctx.look_until_facing(head_of(peer), 8.0, 5_000).await;
    }

    let axis = eclipse_axis(ctx, pair_base, pair_base + 1);
    let mut lanes_up = 0u32;
    if is_beacon && let Some((origin, unit)) = axis {
        let perp = (-unit.1, unit.0);
        for lane in SCREEN_LANES {
            let cell = (
                (origin.0 + unit.0 * SCREEN_DISTANCE + perp.0 * lane as f64).round() as i64,
                origin.1.round() as i64,
                (origin.2 + unit.1 * SCREEN_DISTANCE + perp.1 * lane as f64).round() as i64,
            );
            if raise_landmark(ctx, cell, SCREEN_HEIGHT, MATERIAL)
                .await
                .is_some()
            {
                lanes_up += 1;
            }
        }
    }
    ctx.record(
        "observe",
        &format!("vis-occl-eclipse-setup-{index}"),
        "Take up the eclipse roles: one bot anchors the screen, the rest walk through its shadow.",
        json!({
            "global_index": global,
            "role": if is_beacon { "beacon" } else { "eclipser" },
            "partner": partner,
            "screen_lanes_placed": lanes_up,
            "axis_resolved": axis.is_some(),
            "bot_count": ctx.player_count(),
        }),
    );

    if is_beacon {
        run_beacon(ctx, partner).await;
    } else {
        run_eclipser(ctx, partner, index).await;
    }
}
