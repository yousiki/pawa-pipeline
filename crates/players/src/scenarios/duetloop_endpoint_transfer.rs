//! CATALOG: family=group_motion min_players=2 single_process=true world_edits=false scenery=none far_players=false
//!
//! A role handoff at a closed-loop endpoint: the active player walks an asymmetric dogleg and returns
//! to its home mark, while the receiving player performs a shorter side shuttle and keeps that endpoint
//! in view. The shared phase swaps them. This targets the revisit problem's peer-in-context failure with a clear
//! causal beat: the return gaze is taken only after the active player has come back to the endpoint.
//!
//! The dogleg has four unequal legs and the receiver has two pinned-camera legs, so paired action streams
//! are structurally different rather than jittered clones. Turns use stopped `look_until_facing`; travel
//! is never paired with repeated `LookAt`. The 4-6 block legs clear key chatter while every route closes
//! at home. If terrain rejects a corner, `travel_to` leaves a measurable residual and the next corner is
//! still attempted, so the handoff does not stall or claim a false arrival.

use super::*;

const HOME_HALF: f64 = 5.0;
const CORNER_A: f64 = 4.0;
const CORNER_B: f64 = 6.5;
const ROLE_MS: f64 = 13_000.0;
const RECEIVER_LEG_MS: [u64; 2] = [1_300, 1_500];
const FACE_TOL_DEG: f64 = 8.0;

fn shared_axis(ctx: &Ctx) -> (f64, f64) {
    let v = ctx
        .engine()
        .shared
        .native_waypoints
        .first()
        .copied()
        .unwrap_or((1.0, 0.0));
    let d = v.0.hypot(v.1).max(0.001);
    // Pair roles remain slot 0/1, but each pair gets its own rotated axis at N=4+ so pairs do
    // not converge on the same homes, screens or workstations. N=2 keeps the original axis.
    let angle = ctx.index().saturating_div(2) as f64 * 0.83;
    let (sin, cos) = angle.sin_cos();
    (v.0 / d * cos - v.1 / d * sin, v.0 / d * sin + v.1 / d * cos)
}

async fn face_peer(ctx: &Ctx, peer: Option<usize>, fallback: (f64, f64, f64)) -> bool {
    let target = peer
        .and_then(|j| ctx.peer_position(j))
        .map(head_of)
        .unwrap_or(fallback);
    ctx.look_until_facing(target, FACE_TOL_DEG, 5_000).await
}

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let slot = global % 2;
    let peer = Some(index ^ 1).filter(|&j| j < ctx.player_count());
    let anchor = ctx.anchor();
    let axis = shared_axis(ctx);
    let perp = (-axis.1, axis.0);
    let side = if slot == 0 { 1.0 } else { -1.0 };
    let home = (
        anchor.0 + axis.0 * side * HOME_HALF,
        anchor.1 + axis.1 * side * HOME_HALF,
    );
    let endpoint = (anchor.0, anchor.1);
    let opening = face_peer(ctx, peer, (anchor.0, 64.0, anchor.1)).await;
    travel_to(ctx, home, TRAVEL_HOP, 1.2).await;
    let start = ctx.position().unwrap_or((home.0, 64.0, home.1));
    let focus = (endpoint.0, start.1 + 1.4, endpoint.1);
    ctx.record(
        "observe",
        &format!("vis-duetloop-endpoint-setup-{index}"),
        "Open on the partner before assigning the closed-loop endpoint handoff.",
        json!({
            "global_index": global,
            "slot": slot,
            "peer": peer,
            "opening_confirmed": opening,
            "home": {"x": r2(home.0), "z": r2(home.1)},
            "endpoint": {"x": r2(endpoint.0), "z": r2(endpoint.1)},
        }),
    );

    let mut beat = 0u64;
    while !ctx.target_met() {
        let phase = ctx.engine().shared.phase_at(ROLE_MS);
        let active = (phase + global as i64).rem_euclid(2) == 0;
        if active {
            let corners = [
                (
                    home.0 + perp.0 * side * CORNER_A,
                    home.1 + perp.1 * side * CORNER_A,
                ),
                (
                    endpoint.0 + perp.0 * side * CORNER_B,
                    endpoint.1 + perp.1 * side * CORNER_B,
                ),
                (
                    endpoint.0 - perp.0 * side * CORNER_B,
                    endpoint.1 - perp.1 * side * CORNER_B,
                ),
                home,
            ];
            let mut residuals = Vec::new();
            for (leg, target) in corners.into_iter().enumerate() {
                if ctx.target_met() || ctx.engine().shared.phase_at(ROLE_MS) != phase {
                    break;
                }
                let distance = travel_to(ctx, target, TRAVEL_HOP, 1.2).await;
                let reacquired = face_peer(ctx, peer, focus).await;
                residuals.push(json!({
                    "leg": leg,
                    "target": {"x": r2(target.0), "z": r2(target.1)},
                    "residual": r2(distance),
                    "peer_reacquired": reacquired,
                }));
            }
            let endpoint_gap = travel_to(ctx, endpoint, TRAVEL_HOP, 1.2).await;
            let endpoint_faced = face_peer(ctx, peer, focus).await;
            // The endpoint is the handoff observation, not the route's terminal point. Close the
            // active player's route back at the same home mark before the next role phase starts.
            let home_gap = travel_to(ctx, home, TRAVEL_HOP, 1.2).await;
            let home_returned = face_peer(ctx, peer, focus).await;
            ctx.record(
                TYPE_MOVE,
                &format!("vis-duetloop-endpoint-active-{index}-{beat}"),
                "Visit the handoff endpoint, then close the unequal dogleg back at home.",
                json!({"phase": phase, "legs": residuals, "endpoint_gap": r2(endpoint_gap),
                    "endpoint_faced": endpoint_faced, "endpoint_return": endpoint_gap <= 1.2,
                    "home_gap": r2(home_gap), "home_returned": home_returned}),
            );
        } else {
            let initial = ctx.look_until_facing(focus, FACE_TOL_DEG, 5_000).await;
            let first = if slot == 0 {
                WalkDirection::Left
            } else {
                WalkDirection::Right
            };
            let second = if slot == 0 {
                WalkDirection::Backward
            } else {
                WalkDirection::Forward
            };
            let ran_first = ctx
                .hold_visual(first, false, RECEIVER_LEG_MS[slot], 0)
                .await;
            let at_endpoint = ctx.look_until_facing(focus, FACE_TOL_DEG, 5_000).await;
            let ran_second = ctx
                .hold_visual(second, false, RECEIVER_LEG_MS[1 - slot], 0)
                .await;
            let home_ok = ctx.move_to(home.0, home.1, 7_000, 1.0).await;
            let peer_at_home = face_peer(ctx, peer, focus).await;
            ctx.record(
                TYPE_LOOK,
                &format!("vis-duetloop-endpoint-receiver-{index}-{beat}"),
                "Receive the returned player at the endpoint, then close the receiver loop.",
                json!({
                    "phase": phase,
                    "initial_peer_confirmed": initial,
                    "endpoint_peer_confirmed": at_endpoint,
                    "first_leg_completed": ran_first,
                    "second_leg_completed": ran_second,
                    "home_arrived": home_ok,
                    "peer_at_home": peer_at_home,
                }),
            );
        }
        beat += 1;
    }
}
