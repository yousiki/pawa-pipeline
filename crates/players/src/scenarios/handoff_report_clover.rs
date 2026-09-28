//! CATALOG: family=group_motion min_players=2 single_process=true world_edits=false scenery=none far_players=false
//!
//! A two-role report loop: one player runs three closed petals from a home endpoint while the other
//! keeps the endpoint as a moving sentry; the shared phase then swaps the roles. This targets issue
//! #41's missing peer grounding and one-way travel at once: every petal returns to the same endpoint,
//! and every report ends with a stationary, confirmed gaze back to the partner.
//!
//! The courier's three bearings and the sentry's shuttle are deliberately different action grammars,
//! so N=2 never becomes near-clones. All scripted turns happen after travel with
//! `look_until_facing`; `move_to` is allowed to own the camera during a leg, and no moving target is
//! tracked. Six-block petals clear the 0.5 s key floor. On hostile terrain a failed petal is recorded,
//! the return is still attempted, and the next bearing keeps the bot moving.

use super::*;

const PETALS: usize = 3;
const PETAL_RADIUS: f64 = 6.0;
const HOME_HALF: f64 = 5.0;
const ROLE_MS: f64 = 18_000.0;
const SENTRY_BEAT_MS: i64 = 5_000;
const SHUTTLE_LEG: f64 = 4.0;
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
    let side = if slot == 0 { 1.0 } else { -1.0 };
    let home = (
        anchor.0 + axis.0 * side * HOME_HALF,
        anchor.1 + axis.1 * side * HOME_HALF,
    );

    let opening = face_peer(ctx, peer, (anchor.0, 64.0, anchor.1)).await;
    let home_gap = travel_to(ctx, home, TRAVEL_HOP, 1.2).await;
    let start = ctx.position().unwrap_or((home.0, 64.0, home.1));
    let endpoint = (home.0, start.1 + 1.4, home.1);
    ctx.record(
        "observe",
        &format!("vis-handoff-clover-setup-{index}"),
        "Open on the partner and claim one report endpoint.",
        json!({
            "global_index": global,
            "peer": peer,
            "slot": slot,
            "home": {"x": r2(home.0), "z": r2(home.1)},
            "home_gap": r2(home_gap),
            "opening_confirmed": opening,
        }),
    );

    let mut beat = 0u64;
    while !ctx.target_met() {
        let phase = ctx.engine().shared.phase_at(ROLE_MS);
        let courier = (phase + global as i64).rem_euclid(2) == 0;
        if courier {
            for petal in 0..PETALS {
                if ctx.target_met() || ctx.engine().shared.phase_at(ROLE_MS) != phase {
                    break;
                }
                let angle = (global as f64 * 37.0 + petal as f64 * 120.0).to_radians();
                let tip = (
                    home.0 + angle.cos() * PETAL_RADIUS,
                    home.1 + angle.sin() * PETAL_RADIUS,
                );
                let out = ctx.move_to(tip.0, tip.1, 7_000, 1.0).await;
                let tip_peer = face_peer(ctx, peer, endpoint).await;
                let back = ctx.move_to(home.0, home.1, 7_000, 1.0).await;
                let report = face_peer(ctx, peer, endpoint).await;
                ctx.record(
                    TYPE_MOVE,
                    &format!("vis-handoff-clover-report-{index}-{beat}-{petal}"),
                    "Close one courier petal and deliver its observation to the sentry.",
                    json!({
                        "phase": phase,
                        "petal": petal,
                        "tip": {"x": r2(tip.0), "z": r2(tip.1)},
                        "out_arrived": out,
                        "tip_peer_confirmed": tip_peer,
                        "home_arrived": back,
                        "report_peer_confirmed": report,
                    }),
                );
            }
        } else {
            let peer_faced = face_peer(ctx, peer, endpoint).await;
            let bearing = ctx
                .position()
                .and_then(|own| {
                    peer.and_then(|j| ctx.peer_position(j))
                        .map(|p| look_angles(head_of(own), head_of(p)).0)
                })
                .unwrap_or(if slot == 0 { 90.0 } else { -90.0 });
            hold_facing(ctx, bearing, ctx.elapsed_ms() + SENTRY_BEAT_MS, SHUTTLE_LEG).await;
            let closed = ctx.move_to(home.0, home.1, 7_000, 1.0).await;
            ctx.record(
                TYPE_LOOK,
                &format!("vis-handoff-clover-sentry-{index}-{beat}"),
                "Keep the report endpoint alive, then return to it for the role handoff.",
                json!({
                    "phase": phase,
                    "peer_confirmed": peer_faced,
                    "watch_bearing_deg": r2(bearing),
                    "home_arrived": closed,
                }),
            );
        }
        beat += 1;
    }
}
