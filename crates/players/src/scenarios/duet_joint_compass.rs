//! CATALOG: family=group_motion min_players=2 single_process=true world_edits=false scenery=none far_players=false
//!
//! Two players calibrate bearings together: a caller chooses one of four terrain directions, both stop
//! and look at the same distant point, then each turns back to the partner to verify that the other's
//! body is still on the expected side of the shared axis. This covers the revisit problem's common-attention and
//! re-acquisition faces. The opening view is the peer, before station travel; every compass observation
//! is stationary and confirmed with `look_until_facing`.
//!
//! The paths are closed spokes from home and back. Different radii, opposite spoke order, alternating
//! caller/listener roles, and per-role dwell ranges keep pair agreement below the near-clone ceiling.
//! Natural terrain may block a spoke; `travel_to` then returns a non-zero gap that is recorded, while
//! the gaze calibration still runs from the reachable stop rather than silently claiming arrival.

use super::*;

const RADII: [f64; 2] = [4.5, 7.0];
const HOME_HALF: f64 = 4.8;
const LOOK_DISTANCE: f64 = 28.0;
const ROUND_MS: f64 = 6_500.0;
const PEER_TOL_DEG: f64 = 8.0;
const TARGET_TOL_DEG: f64 = 6.0;

fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

fn axis(ctx: &Ctx) -> (f64, f64) {
    let (x, z) = ctx
        .engine()
        .shared
        .native_waypoints
        .first()
        .copied()
        .unwrap_or((1.0, 0.0));
    let d = x.hypot(z).max(0.001);
    // Keep roles aligned within each pair, but rotate pair workspaces for N=4+.
    let angle = ctx.index().saturating_div(2) as f64 * 0.83;
    let (sin, cos) = angle.sin_cos();
    (x / d * cos - z / d * sin, x / d * sin + z / d * cos)
}

fn ring_point(origin: (f64, f64), yaw_deg: f64, radius: f64) -> (f64, f64) {
    let a = yaw_deg.to_radians();
    (origin.0 - a.sin() * radius, origin.1 + a.cos() * radius)
}

async fn face_peer(ctx: &Ctx, peer: Option<usize>, fallback: (f64, f64, f64)) -> bool {
    let target = peer
        .and_then(|j| ctx.peer_position(j))
        .map(head_of)
        .unwrap_or(fallback);
    ctx.look_until_facing(target, PEER_TOL_DEG, 4_000).await
}

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let peer = Some(index ^ 1).filter(|&j| j < ctx.player_count());
    let slot = global % 2;
    let anchor = ctx.anchor();
    let (ax, az) = axis(ctx);
    let base_yaw = (-ax).atan2(az).to_degrees();
    let side = if slot == 0 { 1.0 } else { -1.0 };
    let home = (
        anchor.0 + ax * side * HOME_HALF,
        anchor.1 + az * side * HOME_HALF,
    );

    let opening = face_peer(ctx, peer, (anchor.0, 64.0, anchor.1)).await;
    travel_to(ctx, home, TRAVEL_HOP, 1.3).await;
    let start = ctx.position().unwrap_or((home.0, 64.0, home.1));
    ctx.record("observe", &format!("vis-duet-compass-setup-{index}"), "Open on the peer, then claim a different-radius calibration station.", json!({
        "global_index": global, "role": if slot == 0 { "bearing_caller" } else { "bearing_checker" },
        "peer": peer, "opening_confirmed": opening, "radius": RADII[slot],
        "home": {"x": r2(home.0), "z": r2(home.1)},
    }));

    let mut beat = 0u64;
    let mut last_round = i64::MIN;
    while !ctx.target_met() {
        let round = ctx.engine().shared.phase_at(ROUND_MS);
        let calling = (round + global as i64).rem_euclid(2) == 0;
        if round != last_round {
            last_round = round;
            ctx.record(
                TYPE_LOOK,
                &format!("vis-duet-compass-round-{index}-{round}"),
                if calling {
                    "Call the next shared terrain bearing."
                } else {
                    "Check the caller against the same terrain bearing."
                },
                json!({"round": round, "calling": calling}),
            );
        }

        // Opposite order is intentional: the two bots share the target but do not issue the same turn
        // sequence on the same frames, which is the near-clones distinction this scenario relies on.
        let step = if slot == 0 { beat % 4 } else { 3 - beat % 4 };
        let yaw = base_yaw + step as f64 * 90.0;
        let spoke = ring_point(home, yaw, RADII[slot]);
        let spoke_gap = travel_to(ctx, spoke, TRAVEL_HOP, 1.2).await;
        let eye_y = ctx.position().map(|p| p.1 + 1.4).unwrap_or(start.1 + 1.4);
        let look_xz = ring_point(anchor, yaw, LOOK_DISTANCE);
        let shared_target = (look_xz.0, eye_y, look_xz.1);
        let target_confirmed = ctx
            .look_until_facing(shared_target, TARGET_TOL_DEG, 5_000)
            .await;
        sleep_ms(vary(
            ctx,
            if calling { 500.0 } else { 850.0 },
            if calling { 900.0 } else { 1_350.0 },
        ) as u64)
        .await;
        let peer_confirmed = face_peer(ctx, peer, (anchor.0, eye_y, anchor.1)).await;
        let home_gap = travel_to(ctx, home, TRAVEL_HOP, 1.2).await;
        let home_peer_confirmed = face_peer(ctx, peer, (anchor.0, eye_y, anchor.1)).await;
        ctx.record(TYPE_LOOK, &format!("vis-duet-compass-bearing-{index}-{beat}"), "Share one terrain bearing, verify the peer, and return home.", json!({
            "beat": beat, "round": round, "calling": calling, "bearing_deg": r2(yaw),
            "spoke_gap": r2(spoke_gap), "target_confirmed": target_confirmed,
            "peer_confirmed": peer_confirmed, "home_gap": r2(home_gap), "home_peer_confirmed": home_peer_confirmed,
            "shared_target": {"x": r2(shared_target.0), "y": r2(shared_target.1), "z": r2(shared_target.2)},
        }));
        beat += 1;
    }
}
