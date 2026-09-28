//! CATALOG: family=construction min_players=2 single_process=true world_edits=true scenery=none far_players=false
//!
//! One player becomes a temporary shutter in the other player's sightline to a small landmark, then
//! steps aside and the shared phase swaps shutter and viewer. Unlike a wall occlusion, the cause is a
//! known player whose body was already in the opening frame. This addresses the revisit problem's learnable
//! disappearance/reappearance case while keeping both trajectory halves closed at their home marks.
//!
//! The shutter runs gate-to-clear-to-home; the viewer uses unequal left/right pinned-camera legs, so
//! their action streams cannot be near-clones. Every landmark or peer re-acquisition is stationary and
//! confirmed. The 4.2-block clear offset moves a body well outside the roughly one-block sight tube;
//! 1300-1500 ms visual legs clear key chatter. `hold_visual` failures are recorded and followed by one
//! pathfinder recovery, so natural terrain shortens a beat without creating a statue.

use super::*;

const MATERIAL: &str = "minecraft:bricks";
const LANDMARK_HEIGHT: i64 = 3;
const HOME_HALF: f64 = 6.0;
const CLEAR_OFFSET: f64 = 4.2;
const ROLE_MS: f64 = 11_000.0;
const FACE_TOL_DEG: f64 = 8.0;
const VIEW_LEGS_MS: [u64; 2] = [1_300, 1_500];

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

fn body_on_sightline(
    viewer: (f64, f64, f64),
    body: (f64, f64, f64),
    subject: (f64, f64, f64),
) -> bool {
    let eye = head_of(viewer);
    let line = (subject.0 - eye.0, subject.2 - eye.2);
    let len2 = line.0 * line.0 + line.1 * line.1;
    if len2 < 0.01 {
        return false;
    }
    let rel = (body.0 - eye.0, body.2 - eye.2);
    let t = (rel.0 * line.0 + rel.1 * line.1) / len2;
    let nearest = (eye.0 + line.0 * t, eye.2 + line.1 * t);
    let ray_y = eye.1 + (subject.1 - eye.1) * t;
    t > 0.05
        && t < 0.95
        && (body.0 - nearest.0).hypot(body.2 - nearest.1) < 0.9
        // `position()` is the feet position; accept the player's physical body, not just its XZ
        // shadow. The ray starts at the viewer's eye, so height is tested in the same frame. This prevents a label from claiming occlusion when the ray passes above them.
        && ray_y >= body.1 - 0.2
        && ray_y <= body.1 + 1.9
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

    let opening = face_peer(ctx, peer, (anchor.0, 64.0, anchor.1)).await;
    let raised = if slot == 0 {
        travel_to(ctx, anchor, TRAVEL_HOP, 2.5).await;
        let y = ctx.position().map(|p| p.1).unwrap_or(64.0).round() as i64;
        raise_landmark(
            ctx,
            (anchor.0.round() as i64, y, anchor.1.round() as i64),
            LANDMARK_HEIGHT,
            MATERIAL,
        )
        .await
    } else {
        None
    };
    let home = (
        anchor.0 - axis.0 * HOME_HALF + perp.0 * side * 2.6,
        anchor.1 - axis.1 * HOME_HALF + perp.1 * side * 2.6,
    );
    travel_to(ctx, home, TRAVEL_HOP, 1.2).await;
    let start = ctx.position().unwrap_or((home.0, 64.0, home.1));
    let marker_cell = raised.map(|(base, _)| base).or_else(|| {
        area_find_block(
            ctx,
            (anchor.0.round() as i32, anchor.1.round() as i32),
            (
                start.1.round() as i32 - 2,
                start.1.round() as i32 + LANDMARK_HEIGHT as i32 + 2,
            ),
            MATERIAL,
        )
    });
    let focus = raised
        .map(|(_, p)| p)
        .or_else(|| {
            marker_cell.map(|base| {
                (
                    base.x as f64 + 0.5,
                    base.y as f64 + LANDMARK_HEIGHT as f64 - 0.5,
                    base.z as f64 + 0.5,
                )
            })
        })
        .unwrap_or((anchor.0, start.1 + LANDMARK_HEIGHT as f64 - 0.5, anchor.1));
    ctx.record(
        "observe",
        &format!("vis-duetloop-shutter-setup-{index}"),
        "Open on the partner, then frame the landmark that the partner will briefly eclipse.",
        json!({
            "global_index": global,
            "peer": peer,
            "slot": slot,
            "opening_confirmed": opening,
            "landmark_placed_by_this_bot": raised.is_some(),
            "landmark_cell": marker_cell.map(|cell| json!({"x": cell.x, "y": cell.y, "z": cell.z})),
            "home": {"x": r2(home.0), "z": r2(home.1)},
        }),
    );

    let mut beat = 0u64;
    while !ctx.target_met() {
        let phase = ctx.engine().shared.phase_at(ROLE_MS);
        let shutter = (phase + global as i64).rem_euclid(2) == 0;
        if shutter {
            let viewer = peer.and_then(|j| ctx.peer_position(j));
            let gate = viewer
                .map(|p| (p.0 + (focus.0 - p.0) * 0.62, p.2 + (focus.2 - p.2) * 0.62))
                .unwrap_or((anchor.0 - axis.0 * 2.0, anchor.1 - axis.1 * 2.0));
            let clear = (
                gate.0 + perp.0 * side * CLEAR_OFFSET,
                gate.1 + perp.1 * side * CLEAR_OFFSET,
            );
            let gate_ok = ctx.move_to(gate.0, gate.1, 7_000, 0.8).await;
            let labelled = ctx
                .position()
                .zip(viewer)
                .is_some_and(|(body, viewer)| body_on_sightline(viewer, body, focus));
            let viewer_faced = face_peer(ctx, peer, focus).await;
            let clear_ok = ctx.move_to(clear.0, clear.1, 7_000, 0.8).await;
            let released = ctx.look_until_facing(focus, FACE_TOL_DEG, 5_000).await;
            let home_ok = ctx.move_to(home.0, home.1, 7_000, 1.0).await;
            ctx.record(
                TYPE_MOVE,
                &format!("vis-duetloop-shutter-pass-{index}-{beat}"),
                "Enter the landmark sightline, step clear, and return home for the role swap.",
                json!({
                    "phase": phase,
                    "gate_arrived": gate_ok,
                    "body_on_sightline": labelled,
                    "viewer_reacquired": viewer_faced,
                    "clear_arrived": clear_ok,
                    "landmark_released": released,
                    "home_arrived": home_ok,
                }),
            );
        } else {
            let landmark_faced = ctx.look_until_facing(focus, FACE_TOL_DEG, 5_000).await;
            let first = if slot == 0 {
                WalkDirection::Left
            } else {
                WalkDirection::Right
            };
            let second = if slot == 0 {
                WalkDirection::Right
            } else {
                WalkDirection::Left
            };
            let ran_first = ctx.hold_visual(first, false, VIEW_LEGS_MS[slot], 0).await;
            let reacquired = ctx.look_until_facing(focus, FACE_TOL_DEG, 5_000).await;
            let ran_second = ctx
                .hold_visual(second, false, VIEW_LEGS_MS[1 - slot], 0)
                .await;
            if !(ran_first && ran_second) {
                ctx.move_to(home.0, home.1, 6_000, 0.8).await;
            }
            let peer_now = peer.and_then(|j| ctx.peer_position(j));
            let occluded = ctx
                .position()
                .zip(peer_now)
                .is_some_and(|(viewer, body)| body_on_sightline(viewer, body, focus));
            ctx.record(
                TYPE_LOOK,
                &format!("vis-duetloop-shutter-watch-{index}-{beat}"),
                "Hold the landmark view while the partner blocks and releases it.",
                json!({
                    "phase": phase,
                    "landmark_faced": landmark_faced,
                    "landmark_reacquired": reacquired,
                    "first_visual_leg_completed": ran_first,
                    "second_visual_leg_completed": ran_second,
                    "peer_currently_on_sightline": occluded,
                }),
            );
        }
        beat += 1;
    }
}
