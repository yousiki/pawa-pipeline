use super::*;

pub(super) async fn run(ctx: &mut Ctx) {
    let cfg = ctx.cfg().clone();
    let index = ctx.index();
    let global = global_index(ctx);
    let anchor = ctx.anchor();
    // Settle before reading the world: at episode start the bot is still falling onto its spread
    // position and the surrounding chunks may not be loaded, so an immediate placement probe sees
    // neither support nor air and the mark is lost for the whole episode.
    for _ in 0..40 {
        if ctx.on_ground() {
            break;
        }
        ctx.bot.wait_ticks(1).await;
    }
    ctx.pause().await;
    let start = ctx.position().unwrap_or((anchor.0, 64.0, anchor.1));
    let start_distance_to_anchor = (anchor.0 - start.0).hypot(anchor.1 - start.2);
    ctx.record(
        "observe",
        &format!("vis-depart_and_revisit-start-{index}"),
        "Observe the starting location before marking, departing, and revisiting it.",
        json!({
            "global_index": global,
            "role": "revisitor",
            "shard_index": cfg.shard_index,
            "shard_count": cfg.shard_count,
            "far_players": cfg.far_players,
            "start_distance_to_anchor": start_distance_to_anchor,
            "single_process_violated": cfg.shard_count > 1,
        }),
    );

    let (waypoints, tag_distance, native_round_ms, material) = {
        let shared = &ctx.engine().shared;
        (
            shared.native_waypoints.clone(),
            shared.tag_distance,
            shared.native_round_ms,
            if shared.native_block_pattern.is_empty() {
                "minecraft:stone_bricks".to_string()
            } else {
                shared.native_block_pattern.clone()
            },
        )
    };
    let desired = (
        start.0.round() as i64 + 1,
        start.1.round() as i64,
        start.2.round() as i64,
    );
    let base = marker_base_near(ctx, desired, 1);
    let cell = base.unwrap_or(BlockPos::new(
        desired.0 as i32,
        desired.1 as i32,
        desired.2 as i32,
    ));
    let equipped = ctx.equip(&material).await;
    let placed = equipped && base.is_some() && ctx.place_block(cell, &material).await;
    ctx.record(
        "place_block",
        &format!("vis-revisit-mark-{index}"),
        "Place a persistent marker at the departure location.",
        json!({
            "cell": {"x": cell.x, "y": cell.y, "z": cell.z},
            "desired_cell": {"x": desired.0, "y": desired.1, "z": desired.2},
            "block": material,
            "placed": placed,
            "equipped": equipped,
            "resolved_placeable_cell": base.is_some(),
        }),
    );

    let offset = waypoints
        .get(global % waypoints.len().max(1))
        .copied()
        .unwrap_or((0.0, 0.0));
    let target = (anchor.0 + offset.0, anchor.1 + offset.1);
    let away_distance = travel_to(ctx, target, TRAVEL_HOP, cfg.goal_radius.max(2.0)).await;
    let dx = target.0 - cell.x as f64;
    let dz = target.1 - cell.z as f64;
    let length = dx.hypot(dz).max(0.001);
    let heading = (dx / length, dz / length);
    // Dwell as a LIVING wait: shuffle between nearby cells around the away-point while facing
    // back along the travel heading. Standing the dwell out was a 26-58 s frozen stretch per pov.
    let dwell_deadline = ctx.elapsed_ms() + native_round_ms.round() as i64;
    let mut drift_angle = ctx.rng_next() * std::f64::consts::TAU;
    while !ctx.target_met() && ctx.elapsed_ms() < dwell_deadline {
        drift_angle += (0.7 + ctx.rng_next()) * if ctx.rng_next() < 0.5 { 1.0 } else { -1.0 };
        let reach = 2.5 + ctx.rng_next() * 2.0;
        ctx.move_to(
            target.0 + drift_angle.cos() * reach,
            target.1 + drift_angle.sin() * reach,
            5_000,
            1.5,
        )
        .await;
        if let Some(position) = ctx.position() {
            ctx.look_default((
                position.0 + heading.0 * 16.0,
                position.1 + 1.4,
                position.2 + heading.1 * 16.0,
            ))
            .await;
        }
        witness_peer_edits(ctx).await;
        sleep_ms(600 + (ctx.rng_next() * 900.0) as u64).await;
    }
    ctx.record(
        "observe",
        &format!("vis-revisit-away-{index}"),
        "Dwell away from the marked departure location.",
        json!({"target": {"x": target.0, "z": target.1}, "final_distance": away_distance}),
    );

    let revisit_distance = travel_to(
        ctx,
        (cell.x as f64, cell.z as f64),
        TRAVEL_HOP,
        tag_distance,
    )
    .await;
    // Probe the exact placed cell first, then a bounded window over the same column: the bot can be
    // shoved off its mark, and a miss must be distinguishable from a genuinely reverted world.
    let exact = ctx.block_id_at(cell);
    let expected = material.strip_prefix("minecraft:").unwrap_or(&material);
    let found = column_find_block(ctx, cell.x, cell.z, (cell.y - 3, cell.y + 3), &material);
    ctx.record(
        "observe",
        &format!("vis-revisit-observe-{index}"),
        "Observe whether the departure marker persisted after the long excursion.",
        json!({
            "expected": material,
            "observed": exact,
            "observed_exact": exact.as_deref() == Some(expected),
            "observed_y": found,
            "persisted": found.is_some(),
            "placed": placed,
            "elapsed_ms": ctx.elapsed_ms(),
            "final_distance": revisit_distance,
        }),
    );

    // Keep the marker in frame from a slow drift around it — the terminal stare was the other
    // designed statue in this script.
    let focus = (
        cell.x as f64 + 0.5,
        cell.y as f64 + 0.5,
        cell.z as f64 + 0.5,
    );
    let mut orbit_angle = ctx.rng_next() * std::f64::consts::TAU;
    while !ctx.target_met() {
        orbit_angle += (0.5 + ctx.rng_next() * 0.7) * if ctx.rng_next() < 0.5 { 1.0 } else { -1.0 };
        let reach = 4.5 + ctx.rng_next() * 2.0;
        ctx.move_to(
            focus.0 + orbit_angle.cos() * reach,
            focus.2 + orbit_angle.sin() * reach,
            5_000,
            1.5,
        )
        .await;
        ctx.look_default(focus).await;
        witness_peer_edits(ctx).await;
        sleep_ms(600 + (ctx.rng_next() * 900.0) as u64).await;
    }
}
