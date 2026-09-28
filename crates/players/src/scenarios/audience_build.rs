use super::*;

/// Draw in `[lo, hi)` from the per-bot stream.
fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

/// Spend the rest of the round as a LIVING spectator: drift between nearby vantage points on the
/// observation arc (a few blocks at a time, pausing between legs) while the view rotates through
/// the builder, the structure, and free sweeps. Replaces the frozen wait-for-phase sleep that user
/// review flagged (#11/#12): a watcher should shuffle and look around, not stand like a statue.
async fn mill_about(
    ctx: &mut Ctx,
    round: i64,
    round_ms: f64,
    centre: (f64, f64, f64),
    builder: usize,
    radius: f64,
) {
    let index = ctx.index();
    let mut base_angle = match ctx.position() {
        Some(p) => (p.2 - centre.2).atan2(p.0 - centre.0),
        None => 0.0,
    };
    let mut leg = 0u32;
    while ctx.engine().shared.phase_at(round_ms) == round && !ctx.target_met() {
        sleep_ms(vary(ctx, 800.0, 2200.0) as u64).await;
        if ctx.engine().shared.phase_at(round_ms) != round || ctx.target_met() {
            break;
        }
        let roll = ctx.rng_next();
        if roll < 0.35 {
            // View-only beat: turn to something else without moving the body.
            let hold = vary(ctx, 500.0, 1500.0) as u64;
            if ctx.rng_next() < 0.5 {
                look_at_peer(ctx, builder, hold).await;
            } else {
                ctx.look(centre, hold).await;
            }
        } else {
            // Shuffle to a fresh vantage point on the same arc, then re-acquire the scene from
            // there. The swing has a floor because `move_to` treats anything inside 1.5 blocks as
            // already arrived: the original +-0.4 rad drew targets it returned from without the bot
            // taking a step, which is how the audience went on standing still after #11/#12.
            let swing = vary(ctx, 0.5, 1.2) * if ctx.rng_next() < 0.5 { 1.0 } else { -1.0 };
            base_angle += swing;
            let angle = base_angle;
            let dist = (radius + vary(ctx, -1.0, 2.0)).max(4.5);
            let to = (centre.0 + angle.cos() * dist, centre.2 + angle.sin() * dist);
            ctx.move_to(to.0, to.1, 6000, 1.5).await;
            let hold = vary(ctx, 400.0, 1200.0) as u64;
            if ctx.rng_next() < 0.5 {
                look_at_peer(ctx, builder, hold).await;
            }
            ctx.look(centre, hold).await;
        }
        leg += 1;
        if leg == 1 {
            ctx.record(
                TYPE_MOVE,
                &format!("vis-audience-mill-{index}-r{round}"),
                "Watch on the move: drift between vantage points while the round plays out.",
                json!({"round": round, "builder": builder}),
            );
        }
    }
}

/// One builder erects a small seeded structure with genuine survival placement while the other
/// bots watch from a shifting arc of vantage points behind it. Each bot performs its build role
/// once per shared round, then spends the remainder of the round as a moving spectator.
pub(super) async fn run(ctx: &mut Ctx) {
    let cfg = ctx.cfg().clone();
    let index = ctx.index();
    let bot_count = ctx.player_count().max(1);
    let materials = [
        "minecraft:oak_planks",
        "minecraft:stone_bricks",
        "minecraft:cobblestone",
        "minecraft:bricks",
    ];
    let anchor = ctx.anchor();
    let mut last_round: i64 = -1;

    while !ctx.target_met() {
        let (round, builder, stage, pattern, round_ms, audience_radius) = {
            let shared = &ctx.engine().shared;
            let round = shared.phase_at(shared.round_ms);
            (
                round,
                shared.builder_order[(round as usize) % bot_count],
                shift(
                    shared.stages[(round as usize) % shared.stages.len().max(1)],
                    anchor,
                ),
                shared.patterns[(round as usize) % shared.patterns.len().max(1)].clone(),
                shared.round_ms,
                shared.arc_radius.clamp(5.0, 7.0),
            )
        };

        // A completed role waits for the phase change. In particular, audience members never
        // pathfind to (or record) their seat repeatedly while the same round is still active.
        if round == last_round {
            sleep_ms(500).await;
            continue;
        }
        last_round = round;

        let role = if index == builder {
            "builder"
        } else {
            "audience"
        };
        ctx.record(
            TYPE_LOOK,
            &format!("vis-build-round-{index}-r{round}"),
            &format!(
                "Build round {round}: bot {builder} builds a {pattern} at ({}, {}).",
                stage.0, stage.1
            ),
            json!({"round": round, "builder": builder, "pattern": pattern,
                "stage": {"x": stage.0, "z": stage.1}, "role": role}),
        );

        let preferred_x = stage.0.round() as i64;
        let preferred_z = stage.1.round() as i64;
        let planned_centre = pattern_centre(&pattern, preferred_x, preferred_z);

        if index == builder {
            // Approach from the front, leaving the structure footprint and the audience's rear
            // sightline clear. The short cap preserves enough of the round for actual placement.
            ctx.move_to(
                preferred_x as f64 + 2.5,
                preferred_z as f64 + 2.5,
                cfg.move_timeout_ms.min(10_000),
                cfg.goal_radius,
            )
            .await;
            let stage_base_y = ctx.position().map(|p| p.1).unwrap_or(64.0).floor() as i64;
            let stage_origin =
                flat_build_origin(ctx, &pattern, preferred_x, stage_base_y, preferred_z);
            // If the planned-stage scan fails, rescan around the builder's actual feet/air layer.
            // This catches ordinary terrain where pathfinding stopped short or changed elevation.
            let verified_origin = stage_origin.or_else(|| {
                let feet = ctx.position()?;
                flat_build_origin(
                    ctx,
                    &pattern,
                    feet.0.floor() as i64,
                    feet.1.floor() as i64,
                    feet.2.floor() as i64,
                )
            });
            let material = materials[(round as usize) % materials.len()].to_string();
            let (build_x, base_y, build_z) =
                verified_origin.unwrap_or((preferred_x, stage_base_y, preferred_z));
            let raw_blocks = if verified_origin.is_some() {
                pattern_blocks(&pattern, build_x, base_y, build_z)
            } else {
                Vec::new()
            };
            let total = raw_blocks.len();
            let blocks: Vec<((i64, i64, i64), String)> = raw_blocks
                .iter()
                .copied()
                .map(|block| (block, material.clone()))
                .collect();

            let start_description = if verified_origin.is_some() {
                format!("Builder bot {builder} starts the {pattern} on a checked local footprint.")
            } else {
                format!(
                    "Builder bot {builder} skips the {pattern}: no checked flat footprint was found."
                )
            };
            ctx.record(
                TYPE_LOOK,
                &format!("vis-build-start-{index}-r{round}"),
                &start_description,
                json!({"round": round, "builder": builder, "pattern": pattern,
                    "origin": {"x": build_x, "y": base_y, "z": build_z},
                    "flat_start_found": verified_origin.is_some(),
                    "build_skipped": verified_origin.is_none(), "block": material}),
            );

            let mut placed = 0u64;
            let mut attempted = 0u64;
            if verified_origin.is_some() {
                let (first_ok, first_attempted) = ctx
                    .build_blocks(&blocks, |block_index, _target, _material| {
                        (
                            format!("vis-place-{index}-r{round}-p0-{block_index}"),
                            format!("Place {pattern} block {}/{total}.", block_index + 1),
                            json!({"round": round, "builder": builder, "pattern": pattern,
                                "block_index": block_index, "retry_pass": 0,
                                "method": "real_place"}),
                        )
                    })
                    .await;
                placed += first_ok;
                attempted += first_attempted;

                // Retry only blocks that are still verifiably air, once, and still bottom-up
                // through build_blocks. The cap prevents a failed footprint from consuming the
                // observation phase.
                if ctx.engine().shared.phase_at(round_ms) == round && !ctx.target_met() {
                    let retry: Vec<((i64, i64, i64), String)> = blocks
                        .iter()
                        .filter(|(block, _)| {
                            is_air_id(
                                ctx.block_id_at(BlockPos::new(
                                    block.0 as i32,
                                    block.1 as i32,
                                    block.2 as i32,
                                ))
                                .as_deref(),
                            )
                        })
                        .take(3)
                        .cloned()
                        .collect();
                    if !retry.is_empty() {
                        let retry_total = retry.len();
                        let (retry_ok, retry_attempted) = ctx
                            .build_blocks(&retry, |retry_index, _target, _material| {
                                (
                                    format!("vis-place-{index}-r{round}-p1-{retry_index}"),
                                    format!(
                                        "Retry failed {pattern} block {}/{} once.",
                                        retry_index + 1,
                                        retry_total
                                    ),
                                    json!({"round": round, "builder": builder, "pattern": pattern,
                                        "retry_index": retry_index, "retry_pass": 1,
                                        "method": "real_place"}),
                                )
                            })
                            .await;
                        placed += retry_ok;
                        attempted += retry_attempted;
                    }
                }
            }

            let centre = if raw_blocks.is_empty() {
                (planned_centre.0, base_y as f64 + 1.0, planned_centre.1)
            } else {
                structure_centre(&raw_blocks, base_y as f64 + 1.0)
            };
            let observe_x = centre.0 + 4.0;
            let observe_z = centre.2 + 5.5;
            let arrived = ctx
                .move_to(
                    observe_x,
                    observe_z,
                    cfg.move_timeout_ms.min(10_000),
                    cfg.goal_radius,
                )
                .await;
            let observe_description = if verified_origin.is_some() {
                format!("Builder steps back to observe the attempted {pattern}.")
            } else {
                format!("Builder observes the skipped {pattern} site from a safe distance.")
            };
            ctx.record(
                TYPE_MOVE,
                &format!("vis-builder-observe-{index}-r{round}"),
                &observe_description,
                json!({"round": round, "builder": builder, "arrived": arrived,
                    "placed": placed, "attempted": attempted,
                    "observation": {"x": r2(observe_x), "z": r2(observe_z)}}),
            );
            ctx.look(centre, cfg.look_settle_ms.max(500)).await;
            ctx.record(
                TYPE_LOOK,
                &format!("vis-builder-structure-{index}-r{round}"),
                if verified_origin.is_some() {
                    "Builder observes the structure after stepping back."
                } else {
                    "Builder observes the planned structure site after safely skipping placement."
                },
                json!({"round": round, "builder": builder,
                    "structure_centre": {"x": r2(centre.0), "y": r2(centre.1), "z": r2(centre.2)}}),
            );
            mill_about(ctx, round, round_ms, centre, builder, 6.5).await;
        } else {
            let audience_index = if index < builder { index } else { index - 1 };
            let audience_count = (bot_count - 1).max(1);
            let fraction = (audience_index as f64 + 0.5) / audience_count as f64;
            // Seats span the rear-left to rear-right flank. They are deterministic for the whole
            // round and remain 5–7 blocks from the planned structure centre.
            let angle = -3.0 * std::f64::consts::FRAC_PI_4 + fraction * std::f64::consts::FRAC_PI_2;
            let seat_x = planned_centre.0 + angle.cos() * audience_radius;
            let seat_z = planned_centre.1 + angle.sin() * audience_radius;
            let arrived = ctx
                .move_to(
                    seat_x,
                    seat_z,
                    cfg.move_timeout_ms.min(15_000),
                    cfg.goal_radius,
                )
                .await;
            ctx.record(
                TYPE_MOVE,
                &format!("vis-seat-{index}-r{round}"),
                &format!("Take the fixed rear audience seat for builder bot {builder}."),
                json!({"round": round, "builder": builder,
                    "seat": {"x": r2(seat_x), "z": r2(seat_z)},
                    "radius": r2(audience_radius), "arrived": arrived}),
            );

            let builder_visible = look_at_peer(ctx, builder, cfg.look_settle_ms.max(500)).await;
            ctx.record(
                TYPE_LOOK,
                &format!("vis-audience-builder-{index}-r{round}"),
                &format!("Audience bot {index} watches builder bot {builder}."),
                json!({"round": round, "builder": builder,
                    "builder_visible": builder_visible.is_some()}),
            );
            let own_y = ctx.position().map(|p| p.1).unwrap_or(64.0);
            let structure_target = (planned_centre.0, own_y + 1.2, planned_centre.1);
            let combined_target = builder_visible
                .map(head_of)
                .map(|builder_head| {
                    (
                        (builder_head.0 + structure_target.0) / 2.0,
                        (builder_head.1 + structure_target.1) / 2.0,
                        (builder_head.2 + structure_target.2) / 2.0,
                    )
                })
                .unwrap_or(structure_target);
            ctx.look(combined_target, cfg.look_settle_ms.max(500)).await;
            ctx.record(
                TYPE_LOOK,
                &format!("vis-audience-structure-{index}-r{round}"),
                &format!("Audience bot {index} watches the {pattern} structure centre."),
                json!({"round": round, "builder": builder,
                    "structure_centre": {"x": r2(planned_centre.0),
                        "y": r2(structure_target.1), "z": r2(planned_centre.1)},
                    "combined_target": {"x": r2(combined_target.0),
                        "y": r2(combined_target.1), "z": r2(combined_target.2)}}),
            );
            mill_about(
                ctx,
                round,
                round_ms,
                (planned_centre.0, structure_target.1, planned_centre.1),
                builder,
                audience_radius,
            )
            .await;
        }
    }
}

pub(super) fn is_air_id(id: Option<&str>) -> bool {
    matches!(
        id.and_then(|id| id.rsplit(':').next()),
        Some("air" | "cave_air" | "void_air")
    )
}

/// Find a nearby flat footprint whose intended blocks are air and whose base has solid support.
/// The deterministic offsets keep every attempt bounded and favor the preferred stage position.
fn flat_build_origin(
    ctx: &Ctx,
    pattern: &str,
    preferred_x: i64,
    base_y: i64,
    preferred_z: i64,
) -> Option<(i64, i64, i64)> {
    const OFFSETS: [(i64, i64); 13] = [
        (0, 0),
        (-1, 0),
        (1, 0),
        (0, -1),
        (0, 1),
        (-1, -1),
        (1, -1),
        (-1, 1),
        (1, 1),
        (-2, 0),
        (2, 0),
        (0, -2),
        (0, 2),
    ];
    for (dx, dz) in OFFSETS {
        let x = preferred_x + dx;
        let z = preferred_z + dz;
        let blocks = pattern_blocks(pattern, x, base_y, z);
        let min_x = blocks.iter().map(|b| b.0).min().unwrap_or(x);
        let max_x = blocks.iter().map(|b| b.0).max().unwrap_or(x);
        let min_z = blocks.iter().map(|b| b.2).min().unwrap_or(z);
        let max_z = blocks.iter().map(|b| b.2).max().unwrap_or(z);
        let footprint_clear = (min_x..=max_x).all(|fx| {
            (min_z..=max_z).all(|fz| {
                is_air_id(
                    ctx.block_id_at(BlockPos::new(fx as i32, base_y as i32, fz as i32))
                        .as_deref(),
                ) && !is_air_id(
                    ctx.block_id_at(BlockPos::new(fx as i32, base_y as i32 - 1, fz as i32))
                        .as_deref(),
                )
            })
        });
        let structure_clear = blocks.iter().all(|block| {
            is_air_id(
                ctx.block_id_at(BlockPos::new(
                    block.0 as i32,
                    block.1 as i32,
                    block.2 as i32,
                ))
                .as_deref(),
            )
        });
        if footprint_clear && structure_clear {
            return Some((x, base_y, z));
        }
    }
    None
}

fn pattern_centre(pattern: &str, x: i64, z: i64) -> (f64, f64) {
    let blocks = pattern_blocks(pattern, x, 0, z);
    let min_x = blocks.iter().map(|b| b.0).min().unwrap_or(x);
    let max_x = blocks.iter().map(|b| b.0).max().unwrap_or(x);
    let min_z = blocks.iter().map(|b| b.2).min().unwrap_or(z);
    let max_z = blocks.iter().map(|b| b.2).max().unwrap_or(z);
    (
        (min_x + max_x) as f64 / 2.0 + 0.5,
        (min_z + max_z) as f64 / 2.0 + 0.5,
    )
}

fn structure_centre(blocks: &[(i64, i64, i64)], fallback_y: f64) -> (f64, f64, f64) {
    let Some(first) = blocks.first() else {
        return (0.5, fallback_y, 0.5);
    };
    let min_x = blocks.iter().map(|b| b.0).min().unwrap_or(first.0);
    let max_x = blocks.iter().map(|b| b.0).max().unwrap_or(first.0);
    let min_y = blocks.iter().map(|b| b.1).min().unwrap_or(first.1);
    let max_y = blocks.iter().map(|b| b.1).max().unwrap_or(first.1);
    let min_z = blocks.iter().map(|b| b.2).min().unwrap_or(first.2);
    let max_z = blocks.iter().map(|b| b.2).max().unwrap_or(first.2);
    (
        (min_x + max_x) as f64 / 2.0 + 0.5,
        (min_y + max_y) as f64 / 2.0 + 0.5,
        (min_z + max_z) as f64 / 2.0 + 0.5,
    )
}
