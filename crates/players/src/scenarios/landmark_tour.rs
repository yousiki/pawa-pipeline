use super::*;

/// The validated episode: bots tour shared landmarks, the first to arrive plants a REAL
/// marker block (kit material via the foundation `place_block` — no `/setblock` chat), later
/// bots navigate to and observe it; everyone periodically faces the live group centre.
pub(super) async fn run(ctx: &mut Ctx) {
    let cfg = ctx.cfg().clone();
    let index = ctx.index();
    let bot_count = ctx.player_count().max(1);
    let round_rotation = ctx.engine().shared.round_rotation_rad;
    let station_for = |lm: &Landmark, round: i64| {
        let angle = (index as f64 / bot_count as f64) * std::f64::consts::TAU
            + (lm.ordinal as f64) * std::f64::consts::FRAC_PI_2
            + (round as f64) * round_rotation;
        // Keep the ring close enough that the marker and other players remain readable in frame.
        let radius = (cfg.formation_radius
            + (((index % 3) as f64) - 1.0) * cfg.lane_spacing * 0.12)
            .clamp(2.5, 4.25);
        (
            lm.x + angle.cos() * radius,
            lm.z + angle.sin() * radius,
            radius,
        )
    };
    sleep_ms(500 + (index as u64) * 250).await;
    let n = cfg.landmarks.len();
    let anchor = ctx.anchor();
    let tour: Vec<Landmark> = (0..n)
        .map(|i| {
            let lm = &cfg.landmarks[(i + index) % n];
            // Shift the origin-relative landmark onto the swarm's spawn anchor.
            Landmark {
                x: lm.x + anchor.0,
                z: lm.z + anchor.1,
                ..lm.clone()
            }
        })
        .collect();
    let mut round: i64 = 0;
    loop {
        for lm in &tour {
            let (sx, sz, radius) = station_for(lm, round);
            let anchor = (lm.x.round() as i64, lm.z.round() as i64);
            let base = json!({
                "landmark": lm.id, "round": round, "target_kind": "formation_station",
                "formation_slot": index, "formation_radius": r2(radius),
                "anchor_position": {"x": anchor.0, "z": anchor.1},
                "station_position": {"x": r2(sx), "z": r2(sz)},
            });
            let mut arrived = ctx.move_default(sx, sz).await;
            let mut actual_station = (sx, sz);
            if !arrived {
                // An obstructed outer slot should not leave a bot staring from far away. Retry at
                // a closer, verified standing cell around the same landmark.
                let feet_y = ctx.position().map(|p| p.1.floor() as i32).unwrap_or(64);
                let dx = sx - lm.x;
                let dz = sz - lm.z;
                let mag = (dx * dx + dz * dz).sqrt().max(0.001);
                let fallback = (lm.x + dx / mag * 2.0, lm.z + dz / mag * 2.0);
                if let Some((fx, fz)) = walkable_xz_near(ctx, fallback, feet_y) {
                    arrived = ctx
                        .move_to(fx, fz, cfg.move_timeout_ms.min(6000), cfg.goal_radius)
                        .await;
                    actual_station = (fx, fz);
                }
            }
            ctx.record(
                TYPE_MOVE,
                &format!("vis-arrive-{}-{}-r{}", index, lm.id, round),
                &format!(
                    "Move to formation slot {} around shared landmark {}.",
                    index, lm.id
                ),
                merged(&base, json!({
                    "arrived": arrived,
                    "actual_station_position": {"x": r2(actual_station.0), "z": r2(actual_station.1)},
                })),
            );
            let own_y = ctx.position().map(|p| p.1).unwrap_or(64.0);
            ctx.look_default((anchor.0 as f64 + 0.5, own_y + 1.2, anchor.1 as f64 + 0.5))
                .await;

            let existing = {
                let built = ctx.engine().shared.built_landmarks.lock();
                built.get(&lm.id).cloned()
            };
            match existing {
                None => {
                    // Select an AIR cell resting on solid terrain. The old anchor/feet-Y target
                    // could already be ground or float above a slope, causing false marker claims.
                    const MARKER: &str = "minecraft:oak_log";
                    const TARGET_OFFSETS: [(i32, i32); 13] = [
                        (0, 0),
                        (1, 0),
                        (-1, 0),
                        (0, 1),
                        (0, -1),
                        (1, 1),
                        (1, -1),
                        (-1, 1),
                        (-1, -1),
                        (2, 0),
                        (-2, 0),
                        (0, 2),
                        (0, -2),
                    ];
                    let feet_y = ctx.position().map(|p| p.1.floor() as i32).unwrap_or(64);
                    let mut marker_target = None;
                    'height: for dy in [0, 1, -1, 2, -2] {
                        for (dx, dz) in TARGET_OFFSETS {
                            let candidate = BlockPos::new(
                                anchor.0 as i32 + dx,
                                feet_y + dy,
                                anchor.1 as i32 + dz,
                            );
                            if block_is_air(ctx, candidate)
                                && block_is_safe_support(ctx, candidate.down(1))
                            {
                                marker_target = Some(candidate);
                                break 'height;
                            }
                        }
                    }
                    let mut placed = false;
                    if let Some(target) = marker_target {
                        if !ctx.target_in_reach(target)
                            || ctx.find_place_reference(target).is_none()
                        {
                            let approach_y =
                                ctx.position().map(|p| p.1.floor() as i32).unwrap_or(feet_y);
                            let mut approach = None;
                            for (dx, dz) in [(2.0, 0.0), (-2.0, 0.0), (0.0, 2.0), (0.0, -2.0)] {
                                if let Some(p) = walkable_xz_near(
                                    ctx,
                                    (target.x as f64 + 0.5 + dx, target.z as f64 + 0.5 + dz),
                                    approach_y,
                                ) {
                                    approach = Some(p);
                                    break;
                                }
                            }
                            if let Some((ax, az)) = approach {
                                ctx.move_to(ax, az, cfg.move_timeout_ms.min(8000), 1.5)
                                    .await;
                            }
                        }
                        placed = ctx.equip(MARKER).await && ctx.place_block(target, MARKER).await;
                        if placed {
                            ctx.engine().shared.built_landmarks.lock().insert(
                                lm.id.clone(),
                                BuiltLandmark {
                                    by: ctx.inner.username.clone(),
                                    x: target.x as i64,
                                    y: target.y as i64,
                                    z: target.z as i64,
                                },
                            );
                        }
                    }
                    ctx.record(
                        TYPE_PLACE,
                        &format!("vis-build-{}-{}", index, lm.id),
                        &format!(
                            "Plant a marker at the shared {} anchor for other players to find.",
                            lm.id
                        ),
                        merged(
                            &base,
                            json!({
                                "block_position": marker_target.map(|p| json!({"x": p.x, "y": p.y, "z": p.z})),
                                "block": MARKER, "placed": placed, "method": "real_place",
                            }),
                        ),
                    );
                    sleep_ms(cfg.action_hold_ms.min(1500)).await;
                    if let Some(target) = marker_target {
                        ctx.look_default((
                            target.x as f64 + 0.5,
                            target.y as f64 + 0.5,
                            target.z as f64 + 0.5,
                        ))
                        .await;
                    }
                }
                Some(existing) => {
                    ctx.look_default((
                        existing.x as f64 + 0.5,
                        existing.y as f64 + 0.5,
                        existing.z as f64 + 0.5,
                    ))
                    .await;
                    ctx.record(
                        TYPE_LOOK,
                        &format!("vis-observe-{}-{}-r{}", index, lm.id, round),
                        &format!("Observe the marker {} planted at {} from this bot's formation slot.", existing.by, lm.id),
                        merged(&base, json!({
                            "left_by": existing.by,
                            "observed_block_position": {"x": existing.x, "y": existing.y, "z": existing.z},
                        })),
                    );
                    sleep_ms(cfg.action_hold_ms.min(1500)).await;
                    ctx.look_default((
                        existing.x as f64 + 0.5,
                        existing.y as f64 + 0.5,
                        existing.z as f64 + 0.5,
                    ))
                    .await;
                }
            }

            if let Some(centre) = ctx.group_center() {
                ctx.look_default((centre.0, centre.1 + 1.4, centre.2)).await;
                ctx.record(
                    TYPE_LOOK,
                    &format!("vis-group-{}-{}-r{}", index, lm.id, round),
                    "Look toward the live group centre to keep other players in view.",
                    merged(&base, json!({
                        "group_center": {"x": r2(centre.0), "y": r2(centre.1), "z": r2(centre.2)},
                    })),
                );
                sleep_ms(cfg.operation_pause_ms).await;
            }
            if round > 0 && ctx.target_met() {
                break;
            }
        }
        round += 1;
        if !ctx.target_met() {
            sleep_ms(cfg.inter_round_still_ms).await;
        }
        if ctx.target_met() {
            break;
        }
    }
    sleep_ms(cfg.inter_round_still_ms).await;
}
