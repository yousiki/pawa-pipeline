use super::*;

fn distance_to_anchor(ctx: &Ctx) -> f64 {
    let anchor = ctx.anchor();
    ctx.position()
        .map(|(x, _, z)| (anchor.0 - x).hypot(anchor.1 - z))
        .unwrap_or(f64::INFINITY)
}

fn nearest_peer_distance(ctx: &Ctx) -> Option<f64> {
    let own = ctx.position()?;
    (0..ctx.player_count())
        .filter(|&j| j != ctx.index())
        .filter_map(|j| ctx.peer_position(j))
        .map(|p| (p.0 - own.0).hypot(p.2 - own.2))
        .min_by(f64::total_cmp)
}

fn group_focus(ctx: &Ctx) -> (f64, f64, f64) {
    ctx.group_center().unwrap_or_else(|| {
        let own_y = ctx.position().map(|p| p.1).unwrap_or(64.0);
        let anchor = ctx.anchor();
        (anchor.0, own_y, anchor.1)
    })
}

pub(super) async fn run(ctx: &mut Ctx, with_edits: bool) {
    let cfg = ctx.cfg().clone();
    let index = ctx.index();
    let global = global_index(ctx);
    let traveler = is_far_traveler(ctx);
    let builder = !traveler && global == 0;
    let role = if traveler {
        "traveler"
    } else if builder && with_edits {
        "builder"
    } else {
        "group"
    };
    let scenario_name = if with_edits {
        "rendezvous_after_edits"
    } else {
        "cross_region_rendezvous"
    };
    let start_distance_to_anchor = distance_to_anchor(ctx);
    ctx.record(
        "observe",
        &format!("vis-{scenario_name}-start-{index}"),
        "Observe the initial cross-region player layout.",
        json!({
            "global_index": global,
            "role": role,
            "shard_index": cfg.shard_index,
            "shard_count": cfg.shard_count,
            "far_players": cfg.far_players,
            "start_distance_to_anchor": start_distance_to_anchor,
            "single_process_violated": cfg.shard_count > 1,
        }),
    );

    let anchor = ctx.anchor();
    let (tag_distance, waypoints, material, height) = {
        let shared = &ctx.engine().shared;
        (
            shared.tag_distance,
            shared.native_waypoints.clone(),
            if shared.native_block_pattern.is_empty() {
                "minecraft:stone_bricks".to_string()
            } else {
                shared.native_block_pattern.clone()
            },
            shared.native_tower_height.max(1),
        )
    };
    let mx = anchor.0.round() as i64 + 2;
    let mz = anchor.1.round() as i64;

    if builder && with_edits {
        let feet_y = ctx.position().map(|p| p.1.round() as i64).unwrap_or(64);
        let base = marker_base_near(ctx, (mx, feet_y, mz), height);
        let (placed, attempted) = match base {
            Some(base) => {
                let blocks = (0..height)
                    .map(|dy| {
                        (
                            (base.x as i64, base.y as i64 + dy, base.z as i64),
                            material.clone(),
                        )
                    })
                    .collect::<Vec<_>>();
                ctx.build_blocks(&blocks, |block_index, _target, _material| {
                    (
                        format!("vis-rendezvous-marker-block-{block_index}"),
                        "Place one block in the rendezvous marker column.".to_string(),
                        json!({"role": "builder"}),
                    )
                })
                .await
            }
            None => (0, 0),
        };
        ctx.record(
            "place_block",
            "vis-rendezvous-marker-0",
            "Build the persistent marker column before the traveler arrives.",
            json!({
                "block": material,
                "cell": base.map(|base| json!({"x": base.x, "y": base.y, "z": base.z})),
                "desired_column": {"x": mx, "z": mz},
                "height": height,
                "placed": placed,
                "attempted": attempted,
                "resolved_placeable_cell": base.is_some(),
            }),
        );
    }

    if !traveler {
        let mut round = 0usize;
        while !ctx.target_met() {
            let offset = waypoints
                .get((global + round) % waypoints.len().max(1))
                .copied()
                .unwrap_or((0.0, 0.0));
            ctx.move_default(anchor.0 + offset.0, anchor.1 + offset.1)
                .await;
            ctx.look_default(group_focus(ctx)).await;
            witness_peer_edits(ctx).await;
            glance_at_nearest_peer(ctx, None).await;
            ctx.pause().await;
            ctx.record(
                "observe",
                &format!("vis-rendezvous-group-{index}-r{round}"),
                "Move within the anchor group while waiting for the distant traveler.",
                json!({"nearest_peer_distance": nearest_peer_distance(ctx)}),
            );
            round += 1;
        }
        return;
    }

    ctx.record(
        "observe",
        &format!("vis-rendezvous-depart-{index}"),
        "Begin the long traversal from the far region to the anchor group.",
        json!({"start_distance_to_anchor": start_distance_to_anchor}),
    );
    let final_dist = travel_to(ctx, anchor, TRAVEL_HOP, tag_distance).await;
    ctx.record(
        "observe",
        &format!("vis-rendezvous-arrive-{index}"),
        "Finish the cross-region traversal at the anchor group.",
        json!({
            "final_distance": final_dist,
            "arrived": final_dist <= tag_distance,
            "tag_distance": tag_distance,
            "elapsed_ms": ctx.elapsed_ms(),
        }),
    );

    if with_edits {
        let own_y = ctx.position().map(|p| p.1.round() as i32).unwrap_or(64);
        // The builder resolves its own placeable base, which may sit off the desired column, so
        // search the same neighbourhood the placer was allowed to use rather than one guessed cell.
        let found = area_find_block(
            ctx,
            (mx as i32, mz as i32),
            (own_y - 6, own_y + 8),
            &material,
        );
        ctx.record(
            "observe",
            &format!("vis-rendezvous-observe-edit-{index}"),
            "Observe whether the anchor-group marker persisted during the traversal.",
            json!({
                "expected": material,
                "observed": found.map(|cell| ctx.block_id_at(cell)),
                "observed_cell": found.map(|cell| json!({"x": cell.x, "y": cell.y, "z": cell.z})),
                "persisted": found.is_some(),
                "desired_column": {"x": mx, "z": mz},
                "search_y_range": [own_y - 6, own_y + 8],
            }),
        );
    }

    ctx.look_default(group_focus(ctx)).await;
    let mut hold_round = 0usize;
    while !ctx.target_met() {
        if hold_round.is_multiple_of(2) {
            glance_at_nearest_peer(ctx, None).await;
        }
        ctx.pause().await;
        hold_round += 1;
    }
}
