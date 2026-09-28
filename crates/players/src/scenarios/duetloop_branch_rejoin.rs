//! CATALOG: family=many_player_spatial min_players=2 single_process=true world_edits=false scenery=none far_players=false
//!
//! One explorer takes a short fork from a junction, visits its tip, and returns through the junction;
//! the other player keeps the junction as a reference and confirms the explorer on rejoin. On the next
//! shared phase they exchange roles. This is the revisit problem's branch-memory signal: the context contains the
//! peer before departure, the branch identity is recorded at the tip, and the same peer is reacquired at
//! the closed endpoint instead of being introduced at the end of a one-way ray.
//!
//! Left and right branches alternate by phase, and the explorer uses two unequal approach legs while
//! the anchor player uses a four-block shuttle, preventing near-clones at N=2. Every target turn is a
//! stopped `look_until_facing`; `move_to` is not asked to watch a moving peer. Natural terrain can make
//! a fork unreachable, so residuals are recorded and the explorer still returns toward the junction.

use super::*;

const JUNCTION_HALF: f64 = 5.0;
const BRANCH_DEPTH: f64 = 6.0;
const BRANCH_SIDE: f64 = 4.0;
const ROLE_MS: f64 = 15_000.0;
const ANCHOR_BEAT_MS: i64 = 4_500;
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
    let perp = (-axis.1, axis.0);
    let side = if slot == 0 { 1.0 } else { -1.0 };
    let home = (
        anchor.0 + axis.0 * side * JUNCTION_HALF,
        anchor.1 + axis.1 * side * JUNCTION_HALF,
    );

    let opening = face_peer(ctx, peer, (anchor.0, 64.0, anchor.1)).await;
    travel_to(ctx, home, TRAVEL_HOP, 1.2).await;
    let start = ctx.position().unwrap_or((home.0, 64.0, home.1));
    let junction = (anchor.0, start.1 + 1.2, anchor.1);
    ctx.record(
        "observe",
        &format!("vis-duetloop-branch-setup-{index}"),
        "Open on the partner and establish the shared junction before the first branch.",
        json!({
            "global_index": global,
            "slot": slot,
            "peer": peer,
            "opening_confirmed": opening,
            "home": {"x": r2(home.0), "z": r2(home.1)},
        }),
    );

    let mut beat = 0u64;
    while !ctx.target_met() {
        let phase = ctx.engine().shared.phase_at(ROLE_MS);
        let explorer = (phase + global as i64).rem_euclid(2) == 0;
        // Branch choice is shared by the pair, not by each bot: global parity distinguishes the
        // explorer and anchor roles, so using it here made them choose opposite forks. Pair index
        // keeps both roles on one branch and gives pair 1 a different branch at N=4.
        let pair = global / 2;
        let branch_sign = if (phase + pair as i64).rem_euclid(2) == 0 {
            1.0
        } else {
            -1.0
        };
        let junction_xz = (anchor.0, anchor.1);
        let fork = (
            junction_xz.0 + perp.0 * branch_sign * BRANCH_SIDE,
            junction_xz.1 + perp.1 * branch_sign * BRANCH_SIDE,
        );
        let tip = (
            fork.0 + axis.0 * BRANCH_DEPTH,
            fork.1 + axis.1 * BRANCH_DEPTH,
        );
        if explorer {
            let fork_gap = travel_to(ctx, fork, TRAVEL_HOP, 1.2).await;
            let fork_faced = ctx.look_until_facing(junction, FACE_TOL_DEG, 5_000).await;
            let tip_gap = travel_to(ctx, tip, TRAVEL_HOP, 1.2).await;
            let tip_faced = ctx.look_until_facing(junction, FACE_TOL_DEG, 5_000).await;
            let junction_gap = travel_to(ctx, junction_xz, TRAVEL_HOP, 1.2).await;
            let junction_reacquired = ctx.look_until_facing(junction, FACE_TOL_DEG, 5_000).await;
            let return_gap = travel_to(ctx, home, TRAVEL_HOP, 1.2).await;
            let peer_confirmed = face_peer(ctx, peer, junction).await;
            ctx.record(
                TYPE_MOVE,
                &format!("vis-duetloop-branch-explore-{index}-{beat}"),
                "Explore one fork, report its tip, and rejoin the anchor at home.",
                json!({
                    "phase": phase,
                    "branch": if branch_sign > 0.0 { "left" } else { "right" },
                    "fork": {"x": r2(fork.0), "z": r2(fork.1)},
                    "tip": {"x": r2(tip.0), "z": r2(tip.1)},
                    "fork_gap": r2(fork_gap),
                    "fork_anchor_faced": fork_faced,
                    "tip_gap": r2(tip_gap),
                    "tip_anchor_faced": tip_faced,
                    "junction_gap": r2(junction_gap),
                    "junction_reacquired": junction_reacquired,
                    "return_gap": r2(return_gap),
                    "peer_rejoined": peer_confirmed,
                }),
            );
        } else {
            let peer_faced = face_peer(ctx, peer, junction).await;
            hold_facing(
                ctx,
                ctx.position()
                    .and_then(|own| {
                        peer.and_then(|j| ctx.peer_position(j))
                            .map(|p| look_angles(head_of(own), head_of(p)).0)
                    })
                    .unwrap_or(0.0),
                ctx.elapsed_ms() + ANCHOR_BEAT_MS,
                SHUTTLE_LEG,
            )
            .await;
            let tip_faced = ctx
                .look_until_facing((tip.0, start.1 + 1.2, tip.1), FACE_TOL_DEG, 5_000)
                .await;
            let home_ok = ctx.move_to(home.0, home.1, 7_000, 1.0).await;
            let reunion = face_peer(ctx, peer, junction).await;
            ctx.record(
                TYPE_LOOK,
                &format!("vis-duetloop-branch-anchor-{index}-{beat}"),
                "Hold the junction reference, then confirm the returning explorer and branch tip.",
                json!({
                    "phase": phase,
                    "branch": if branch_sign > 0.0 { "left" } else { "right" },
                    "peer_initially_faced": peer_faced,
                    "tip_faced": tip_faced,
                    "home_arrived": home_ok,
                    "peer_rejoined": reunion,
                }),
            );
        }
        beat += 1;
    }
}
