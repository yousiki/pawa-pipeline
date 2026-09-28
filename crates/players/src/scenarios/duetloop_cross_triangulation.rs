//! CATALOG: family=many_player_spatial min_players=2 single_process=true world_edits=false scenery=none far_players=false
//!
//! A cross-triangulation loop over one natural terrain column: one player samples three baseline bins
//! while the other holds a compact pivot, then the phase swaps mover and pivot. The task signal is the
//! pair of target bearings and their difference at each stop, not another synchronized orbit. It covers
//! the revisit problem by repeatedly returning both bodies and both cameras to an already observed target.
//!
//! The baseline samples are closed spokes from home and the pivot uses a shorter perpendicular shuttle,
//! so paired action streams differ in route length, order, and role. All measurements use stationary
//! `look_until_facing`; no `LookAt` is re-issued during travel, avoiding both directions of the
//! camera/travel gate. The 5/8/11-block bins create visibly different parallax. If the sampled column is
//! unloaded or all air, the script records the fallback point and still executes every spoke.

use super::*;

const BASELINES: [f64; 3] = [5.0, 8.0, 11.0];
const TARGET_RANGE: f64 = 12.0;
const HOME_HALF: f64 = 4.5;
const PIVOT_STEP: f64 = 4.0;
const ROLE_MS: f64 = 18_000.0;
const TARGET_TOL_DEG: f64 = 6.0;
const PEER_TOL_DEG: f64 = 8.0;

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

fn natural_focus(
    ctx: &Ctx,
    target: (f64, f64),
    reference_y: f64,
) -> ((f64, f64, f64), Option<String>) {
    let x = target.0.round() as i32;
    let z = target.1.round() as i32;
    let centre = reference_y.round() as i32;
    for y in ((centre - 8)..=(centre + 8)).rev() {
        let cell = BlockPos::new(x, y, z);
        // `block_is_air` deliberately treats an unloaded cell as non-air (safe for placement),
        // but that is not evidence of a natural landmark. Keep probing on `None`; otherwise an
        // unloaded column becomes a bogus target at the first sampled Y and the documented
        // all-air/unloaded fallback is never reached.
        let Some(id) = ctx.block_id_at(cell) else {
            continue;
        };
        if !matches!(
            id.as_str(),
            "air"
                | "cave_air"
                | "void_air"
                | "minecraft:air"
                | "minecraft:cave_air"
                | "minecraft:void_air"
        ) {
            return ((x as f64 + 0.5, y as f64 + 0.5, z as f64 + 0.5), Some(id));
        }
    }
    ((target.0, reference_y + 1.4, target.1), None)
}

async fn face_peer(ctx: &Ctx, peer: Option<usize>, fallback: (f64, f64, f64)) -> bool {
    let target = peer
        .and_then(|j| ctx.peer_position(j))
        .map(head_of)
        .unwrap_or(fallback);
    ctx.look_until_facing(target, PEER_TOL_DEG, 5_000).await
}

fn bearings(ctx: &Ctx, peer: Option<usize>, target: (f64, f64, f64)) -> (Option<f64>, Option<f64>) {
    let Some(own) = ctx.position() else {
        return (None, None);
    };
    let target_yaw = look_angles(head_of(own), target).0;
    let peer_yaw = peer
        .and_then(|j| ctx.peer_position(j))
        .map(|p| look_angles(head_of(own), head_of(p)).0);
    (Some(target_yaw), peer_yaw)
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
        anchor.0 + perp.0 * side * HOME_HALF,
        anchor.1 + perp.1 * side * HOME_HALF,
    );

    let opening = face_peer(ctx, peer, (anchor.0, 64.0, anchor.1)).await;
    travel_to(ctx, home, TRAVEL_HOP, 1.2).await;
    let start = ctx.position().unwrap_or((home.0, 64.0, home.1));
    let target_xz = (
        anchor.0 + axis.0 * TARGET_RANGE,
        anchor.1 + axis.1 * TARGET_RANGE,
    );
    let (target, target_block) = natural_focus(ctx, target_xz, start.1);
    let fallback_target = target_block.is_none();
    ctx.record(
        "observe",
        &format!("vis-duetloop-triangulation-setup-{index}"),
        "Open on the peer and identify the natural column used by both triangulation roles.",
        json!({
            "global_index": global,
            "peer": peer,
            "slot": slot,
            "opening_confirmed": opening,
            "target": {"x": r2(target.0), "y": r2(target.1), "z": r2(target.2)},
            "target_block": target_block,
            "fallback_target": fallback_target,
        }),
    );

    let mut beat = 0u64;
    while !ctx.target_met() {
        let phase = ctx.engine().shared.phase_at(ROLE_MS);
        let moving_baseline = (phase + global as i64).rem_euclid(2) == 0;
        if moving_baseline {
            for sample in 0..BASELINES.len() {
                if ctx.target_met() || ctx.engine().shared.phase_at(ROLE_MS) != phase {
                    break;
                }
                let bin = if slot == 0 {
                    sample
                } else {
                    BASELINES.len() - 1 - sample
                };
                let station = (
                    anchor.0 + perp.0 * side * BASELINES[bin],
                    anchor.1 + perp.1 * side * BASELINES[bin],
                );
                let gap = travel_to(ctx, station, TRAVEL_HOP, 1.2).await;
                let target_faced = ctx.look_until_facing(target, TARGET_TOL_DEG, 5_000).await;
                let (target_yaw, peer_yaw) = bearings(ctx, peer, target);
                let peer_faced = face_peer(ctx, peer, target).await;
                let home_gap = travel_to(ctx, home, TRAVEL_HOP, 1.2).await;
                ctx.record(
                    TYPE_LOOK,
                    &format!("vis-duetloop-triangulation-sample-{index}-{beat}-{sample}"),
                    "Measure the target from one baseline bin, cross-check the peer, and close the spoke.",
                    json!({
                        "phase": phase,
                        "sample": sample,
                        "baseline_bin": bin,
                        "baseline_blocks": BASELINES[bin],
                        "station_gap": r2(gap),
                        "target_faced": target_faced,
                        "target_yaw_deg": target_yaw.map(r2),
                        "peer_yaw_deg": peer_yaw.map(r2),
                        "bearing_separation_deg": target_yaw.zip(peer_yaw).map(|(a, b)| r2(((b - a + 180.0).rem_euclid(360.0) - 180.0).abs())),
                        "peer_faced": peer_faced,
                        "home_gap": r2(home_gap),
                    }),
                );
            }
        } else {
            let pivot = (
                home.0 + axis.0 * PIVOT_STEP * if beat.is_multiple_of(2) { 1.0 } else { -1.0 },
                home.1 + axis.1 * PIVOT_STEP * if beat.is_multiple_of(2) { 1.0 } else { -1.0 },
            );
            let pivot_gap = ctx.move_to(pivot.0, pivot.1, 7_000, 0.8).await;
            let target_faced = ctx.look_until_facing(target, TARGET_TOL_DEG, 5_000).await;
            let (target_yaw, peer_yaw) = bearings(ctx, peer, target);
            let home_ok = ctx.move_to(home.0, home.1, 7_000, 0.8).await;
            ctx.record(
                TYPE_LOOK,
                &format!("vis-duetloop-triangulation-pivot-{index}-{beat}"),
                "Hold the compact pivot measurement while the partner changes baseline.",
                json!({
                    "phase": phase,
                    "pivot_arrived": pivot_gap,
                    "target_faced": target_faced,
                    "target_yaw_deg": target_yaw.map(r2),
                    "peer_yaw_deg": peer_yaw.map(r2),
                    "home_arrived": home_ok,
                }),
            );
        }
        beat += 1;
    }
}
