//! Generation-7 pilot dispatch.
//!
//! The pilot is intentionally a catalog-level fork of the smallest existing, reviewed shapes:
//! interaction, pair visibility, and surface-grounded revisits. Keeping the handlers shared means a
//! v7 episode tests the new manifest/data split without creating twelve copies of controller logic;
//! the names remain distinct so each target can be audited and expanded independently later.

use super::*;

pub(super) async fn run(ctx: &mut Ctx) {
    match ctx.engine().name.as_str() {
        "v7_interact_cairn"
        | "v7_interact_ferry"
        | "v7_interact_layers"
        | "v7_interact_handoff" => super::duet_anchor_calibration::run(ctx).await,
        "v7_covis_rail" | "v7_covis_baseline" | "v7_covis_exchange" | "v7_covis_shared_column" => {
            run_covis(ctx).await
        }
        "v7_ground_crest" | "v7_ground_bank" | "v7_ground_shore" | "v7_ground_revisit" => {
            run_ground(ctx).await
        }
        _ => unreachable!("generation-7 scenario missing from v7 dispatch"),
    }
}

async fn run_covis(ctx: &mut Ctx) {
    let peer = nearest_peer(ctx).and_then(|j| ctx.peer_position(j).map(head_of));
    run_pinned_motion(ctx, peer).await;
}

async fn run_ground(ctx: &mut Ctx) {
    let peer = nearest_peer(ctx).and_then(|j| ctx.peer_position(j).map(head_of));
    let anchor = ctx.anchor();
    let subject = peer.or_else(|| ctx.position().map(|p| (anchor.0, p.1 + 1.4, anchor.1)));
    run_pinned_motion(ctx, subject).await;
}

/// Keep the camera on one locally observed subject while the body takes long, bounded bouts. Unlike
/// `move_to`, `hold_visual` does not replace the camera intent, so this gives training windows a stable
/// visual reference without sending a bot through an unbounded terrain route.
async fn run_pinned_motion(ctx: &mut Ctx, subject: Option<(f64, f64, f64)>) {
    let deadline = ctx.cfg().episode_duration_ms;
    let anchor = ctx.anchor();
    let mut forward = global_index(ctx).is_multiple_of(2);
    let mut bout = 0u64;
    while !ctx.target_met() {
        let Some(position) = ctx.position() else {
            sleep_ms(200).await;
            continue;
        };
        let target = subject.unwrap_or((anchor.0, position.1 + 1.4, anchor.1));
        let bearing = look_angles(head_of(position), target).0;
        ctx.look_default(point_at_yaw(head_of(position), bearing, 24.0))
            .await;
        let hold_ms = 1_700 + (global_index(ctx) % 3) as u64 * 180;
        let until = (ctx.elapsed_ms() + hold_ms as i64).min(deadline);
        let direction = if forward {
            WalkDirection::Forward
        } else {
            WalkDirection::Backward
        };
        let sprint = forward && (bout + global_index(ctx) as u64).is_multiple_of(3);
        if until > ctx.elapsed_ms() {
            let _ = ctx
                .hold_visual(direction, sprint, (until - ctx.elapsed_ms()) as u64, 0)
                .await;
        }
        bout += 1;
        forward = !forward;
        if ctx.position().is_some_and(|p| p.1 < position.1 - 3.0) {
            let safe = walkable_xz_near(ctx, (position.0, position.2), position.1.floor() as i32);
            if let Some((x, z)) = safe {
                let _ = ctx.move_to(x, z, 5_000, 0.8).await;
            }
        }
    }
}

fn nearest_peer(ctx: &Ctx) -> Option<usize> {
    let own = ctx.position()?;
    (0..ctx.player_count())
        .filter(|&j| j != ctx.index())
        .filter_map(|j| {
            ctx.peer_position(j)
                .map(|p| (j, (p.0 - own.0).hypot(p.2 - own.2)))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(j, _)| j)
}
