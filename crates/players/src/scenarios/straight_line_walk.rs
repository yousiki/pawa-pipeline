use super::*;

/// N bots walk compact parallel lanes along one stable seeded heading. Targets are short segments
/// generated from the live group anchor; the march stops at the first unreachable segment.
pub(super) async fn run(ctx: &mut Ctx) {
    const SEGMENT_LEN: f64 = 4.0;
    const SEGMENT_COUNT: usize = 4;

    let cfg = ctx.cfg().clone();
    let index = ctx.index();
    let start = loop {
        if let Some(position) = ctx.position() {
            break position;
        }
        sleep_ms(100).await;
    };
    let anchor = ctx
        .group_center()
        .map(|p| (p.0, p.2))
        .unwrap_or_else(|| ctx.anchor());
    // Keep one seeded heading for the entire episode. The old implementation treated five
    // independently-random map points as a polyline, so every leg had a different heading and
    // frequently crossed unloaded/blocked terrain.
    let heading = ctx
        .engine()
        .shared
        .native_waypoints
        .first()
        .map(|p| p.1.atan2(p.0))
        .unwrap_or(0.0);
    let (forward_x, forward_z) = (heading.cos(), heading.sin());
    let (side_x, side_z) = (-forward_z, forward_x);
    // Two blocks is enough to make parallel lanes readable without spreading a large swarm over
    // the whole map.
    let slot_spacing = ctx.engine().shared.native_spacing.clamp(1.8, 2.2);
    let lateral = (start.0 - anchor.0) * side_x + (start.2 - anchor.1) * side_z;
    // Each lane starts at the bot's authoritative position. Every later target is that exact point
    // plus a scalar multiple of one immutable forward vector, so no initial lane-correction arc can
    // masquerade as a heading change in the rendered route.
    let lane_start = (start.0, start.2);
    let total_distance = SEGMENT_LEN * SEGMENT_COUNT as f64;
    let lane_end = (
        lane_start.0 + forward_x * total_distance,
        lane_start.1 + forward_z * total_distance,
    );
    let route_focus = (lane_end.0 + forward_x * 5.0, lane_end.1 + forward_z * 5.0);

    if let Some(p) = ctx.position() {
        ctx.look(
            (route_focus.0, p.1 + 0.25, route_focus.1),
            cfg.look_settle_ms.max(700),
        )
        .await;
        ctx.record(
            TYPE_LOOK,
            &format!("vis-line-{index}-start"),
            "Frame the ground-level straight route and its stable heading before marching.",
            json!({"heading_rad": r2(heading), "phase": "start"}),
        );
    }
    sleep_ms(300 + (index as u64) * 180).await;
    let mut stopped_after_failure = false;
    for segment in 1..=SEGMENT_COUNT {
        if ctx.target_met() {
            break;
        }
        let distance = segment as f64 * SEGMENT_LEN;
        let tx = lane_start.0 + forward_x * distance;
        let tz = lane_start.1 + forward_z * distance;
        let arrived = ctx
            .move_to(tx, tz, cfg.move_timeout_ms.min(7000), cfg.goal_radius)
            .await;
        if let Some(p) = ctx.position() {
            ctx.look(
                (route_focus.0, p.1 + 0.25, route_focus.1),
                cfg.look_settle_ms.max(450),
            )
            .await;
        }
        ctx.record(
            TYPE_MOVE,
            &format!("vis-line-{index}-s{segment}"),
            &format!("Walk stable parallel heading segment {segment}."),
            json!({
                "segment_index": segment,
                "segment_length": SEGMENT_LEN,
                "heading_rad": r2(heading),
                "lateral_offset": r2(lateral),
                "slot_spacing": r2(slot_spacing),
                "slot_target": {"x": r2(tx), "z": r2(tz)},
                "arrived": arrived
            }),
        );
        if !arrived {
            stopped_after_failure = true;
            ctx.record(
                TYPE_MOVE,
                &format!("vis-line-{index}-stopped"),
                "Stop extending the straight route after an unreachable segment.",
                json!({"failed_segment": segment, "stopped_expansion": true}),
            );
            break;
        }
    }

    // End the first outward march from a readable peer view. Wait briefly for a parallel walker to
    // finish its last short leg, then keep the rest of the episode active by shuttling both lanes
    // between the same two endpoints instead of idling or extending into terrain beyond the arena.
    let peer = nearest_peer(ctx);
    if let Some(peer) = peer {
        for _ in 0..20 {
            let close = ctx
                .position()
                .zip(ctx.peer_position(peer))
                .is_some_and(|(own, other)| {
                    ((other.0 - own.0).powi(2) + (other.2 - own.2).powi(2)).sqrt() <= 6.0
                });
            if close {
                break;
            }
            sleep_ms(100).await;
        }
        look_at_peer(ctx, peer, cfg.look_settle_ms.max(3200)).await;
        ctx.record(
            TYPE_LOOK,
            &format!("vis-line-{index}-observe"),
            "Hold view on a nearby parallel walker after the straight march.",
            json!({"target_peer": peer, "stopped_after_failure": stopped_after_failure}),
        );
    } else if let Some(p) = ctx.position() {
        ctx.look(
            (anchor.0, p.1 + 0.25, anchor.1),
            cfg.look_settle_ms.max(700),
        )
        .await;
        ctx.record(
            TYPE_LOOK,
            &format!("vis-line-{index}-lookback"),
            "Look back along the straight route to expose the completed displacement.",
            json!({"phase": "lookback", "route_start": {"x": r2(anchor.0), "z": r2(anchor.1)}}),
        );
    }

    let mut returning = true;
    let mut pass = 1u32;
    while !ctx.target_met() {
        let target = if returning { lane_start } else { lane_end };
        let arrived = ctx
            .move_to(
                target.0,
                target.1,
                cfg.move_timeout_ms.min(7000),
                cfg.goal_radius,
            )
            .await;
        if let Some(peer) = nearest_peer(ctx) {
            look_at_peer(ctx, peer, cfg.look_settle_ms.max(3200)).await;
        }
        ctx.record(
            TYPE_MOVE,
            &format!("vis-line-{index}-shuttle-{pass}"),
            if returning {
                "Return along the same straight parallel lane."
            } else {
                "Repeat the outward straight parallel march."
            },
            json!({"phase": "shuttle", "pass": pass,
                "direction": if returning { "return" } else { "outward" },
                "arrived": arrived, "target": {"x": r2(target.0), "z": r2(target.1)}}),
        );
        returning = !returning;
        pass += 1;
        sleep_ms(cfg.action_hold_ms.max(350)).await;
    }
}

fn nearest_peer(ctx: &Ctx) -> Option<usize> {
    let own = ctx.position()?;
    (0..ctx.player_count())
        .filter(|&j| j != ctx.index())
        .filter_map(|j| {
            ctx.peer_position(j).map(|p| {
                let distance2 = (p.0 - own.0).powi(2) + (p.2 - own.2).powi(2);
                (j, distance2)
            })
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(j, _)| j)
}
