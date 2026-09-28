use super::*;

fn peer_metrics(ctx: &Ctx, tag_distance: f64) -> (Option<f64>, usize) {
    let Some(own) = ctx.position() else {
        return (None, 0);
    };
    let mut nearest: Option<f64> = None;
    let mut peers_within_tag = 0;
    for j in 0..ctx.player_count() {
        if j == ctx.index() {
            continue;
        }
        let Some(peer) = ctx.peer_position(j) else {
            continue;
        };
        let distance = (peer.0 - own.0).hypot(peer.2 - own.2);
        if distance <= tag_distance {
            peers_within_tag += 1;
        }
        if nearest.map(|best| distance < best).unwrap_or(true) {
            nearest = Some(distance);
        }
    }
    (nearest, peers_within_tag)
}

pub(super) async fn run(ctx: &mut Ctx) {
    let cfg = ctx.cfg().clone();
    let index = ctx.index();
    let global = global_index(ctx);
    let anchor = ctx.anchor();
    let start_distance_to_anchor = ctx
        .position()
        .map(|p| (anchor.0 - p.0).hypot(anchor.1 - p.2))
        .unwrap_or(f64::INFINITY);
    ctx.record(
        "observe",
        &format!("vis-crowded_interaction-start-{index}"),
        "Observe the initial dense many-player layout.",
        json!({
            "global_index": global,
            "role": "participant",
            "shard_index": cfg.shard_index,
            "shard_count": cfg.shard_count,
            "far_players": cfg.far_players,
            "start_distance_to_anchor": start_distance_to_anchor,
            "single_process_violated": cfg.shard_count > 1,
        }),
    );

    let (waypoints, tag_distance) = {
        let shared = &ctx.engine().shared;
        (shared.native_waypoints.clone(), shared.tag_distance)
    };
    let mut round = 0usize;
    while !ctx.target_met() {
        let offset = waypoints
            .get((global + round) % waypoints.len().max(1))
            .copied()
            .unwrap_or((0.0, 0.0));
        ctx.move_default(anchor.0 + offset.0, anchor.1 + offset.1)
            .await;
        glance_at_nearest_peer(ctx, None).await;
        if (round + 1).is_multiple_of(3) {
            ctx.sweep_default().await;
        }
        ctx.pause().await;
        let (nearest_peer_distance, peers_within_tag) = peer_metrics(ctx, tag_distance);
        ctx.record(
            "observe",
            &format!("vis-crowd-{index}-r{round}"),
            "Move through the dense group and observe nearby peers.",
            json!({
                "nearest_peer_distance": nearest_peer_distance,
                "peers_within_tag": peers_within_tag,
            }),
        );
        round += 1;
    }
}
