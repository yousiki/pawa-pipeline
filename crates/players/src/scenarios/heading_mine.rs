use super::*;

#[derive(Clone, Copy)]
struct TorchPlacement {
    target: BlockPos,
    placed: bool,
    candidates_checked: u64,
    target_air_rejections: u64,
    head_air_rejections: u64,
    floor_support_rejections: u64,
    supported_candidates: u64,
    occupied_candidates: u64,
    reachable_candidates: u64,
    approach_failures: u64,
    interaction_failures: u64,
    approached: bool,
    player_position: Option<(f64, f64, f64)>,
}

fn carved_supported_cell(ctx: &Ctx, target: BlockPos) -> bool {
    block_is_air(ctx, target)
        && block_is_air(ctx, target.up(1))
        && block_is_safe_support(ctx, target.down(1))
}

fn player_occupies_cell(ctx: &Ctx, target: BlockPos) -> bool {
    ctx.position()
        .map(|p| {
            p.0.floor() as i32 == target.x
                && p.1.floor() as i32 == target.y
                && p.2.floor() as i32 == target.z
        })
        .unwrap_or(true)
}

async fn approach_torch_target(
    ctx: &Ctx,
    carved_feet: &[Option<BlockPos>],
    carved_step: i64,
    target_step: i64,
    target: BlockPos,
) -> bool {
    // Reposition two recorded tunnel cells away from the target. Every approach and target comes
    // from the exact cross-sections this run carved, never from an unrelated cave below them.
    for approach_step in [target_step - 2, target_step + 2] {
        if !(1..=carved_step).contains(&approach_step) {
            continue;
        }
        let Some(approach) = carved_feet.get(approach_step as usize).copied().flatten() else {
            continue;
        };
        if approach == target || !carved_supported_cell(ctx, approach) {
            continue;
        }
        ctx.move_to(
            approach.x as f64 + 0.5,
            approach.z as f64 + 0.5,
            ctx.cfg().move_timeout_ms.min(2_000),
            1.2,
        )
        .await;
        return !player_occupies_cell(ctx, target) && ctx.ground_torch_face_in_reach(target);
    }
    false
}

async fn wait_for_vertical_settle(ctx: &Ctx) -> Option<(f64, f64, f64)> {
    let mut previous = ctx.position()?;
    let mut stable_ticks = 0;
    for _ in 0..80 {
        ctx.bot.wait_ticks(1).await;
        let Some(current) = ctx.position() else {
            continue;
        };
        if (current.1 - previous.1).abs() < 0.01 && ctx.on_ground() {
            stable_ticks += 1;
            if stable_ticks >= 4 {
                return Some(current);
            }
        } else {
            stable_ticks = 0;
        }
        previous = current;
    }
    None
}

async fn wait_for_descent_and_settle(ctx: &Ctx, start_y: f64) -> Option<(f64, f64, f64)> {
    let mut previous = ctx.position()?;
    let mut dropped = false;
    let mut stable_ticks = 0;
    for _ in 0..120 {
        ctx.bot.wait_ticks(1).await;
        let Some(current) = ctx.position() else {
            continue;
        };
        dropped |= current.1 <= start_y - 0.5;
        if dropped && (current.1 - previous.1).abs() < 0.01 && ctx.on_ground() {
            stable_ticks += 1;
            if stable_ticks >= 4 {
                return Some(current);
            }
        } else {
            stable_ticks = 0;
        }
        previous = current;
    }
    None
}

async fn place_trailing_ground_torch(
    ctx: &Ctx,
    carved_feet: &[Option<BlockPos>],
    step: i64,
) -> TorchPlacement {
    let fallback_target = carved_feet
        .get(step.saturating_sub(1) as usize)
        .copied()
        .flatten()
        .or_else(|| {
            ctx.position()
                .map(|p| BlockPos::new(p.0.floor() as i32, p.1.floor() as i32, p.2.floor() as i32))
        })
        .unwrap_or(BlockPos::new(0, 0, 0));
    let mut last_target = fallback_target;
    let mut candidates_checked = 0;
    let mut target_air_rejections = 0;
    let mut head_air_rejections = 0;
    let mut floor_support_rejections = 0;
    let mut supported_candidates = 0;
    let mut occupied_candidates = 0;
    let mut reachable_candidates = 0;
    let mut approach_failures = 0;
    let mut interaction_failures = 0;
    let mut any_approached = false;
    // At most four candidates, one initial packet each, and one repositioned retry per candidate.
    for offset in [2i64, 3, 4, 1] {
        if step <= offset {
            continue;
        }
        let trail_step = step - offset;
        let Some(target) = carved_feet.get(trail_step as usize).copied().flatten() else {
            target_air_rejections += 1;
            continue;
        };
        last_target = target;
        candidates_checked += 1;
        if !block_is_air(ctx, target) {
            target_air_rejections += 1;
            continue;
        }
        if !block_is_air(ctx, target.up(1)) {
            head_air_rejections += 1;
            continue;
        }
        if !block_is_safe_support(ctx, target.down(1)) {
            floor_support_rejections += 1;
            continue;
        }
        supported_candidates += 1;
        if player_occupies_cell(ctx, target) {
            occupied_candidates += 1;
            continue;
        }

        let already_reachable = ctx.ground_torch_face_in_reach(target);
        if already_reachable {
            reachable_candidates += 1;
        }
        let approached = if already_reachable {
            false
        } else {
            let ok = approach_torch_target(ctx, carved_feet, step, trail_step, target).await;
            if !ok {
                approach_failures += 1;
            }
            ok
        };
        any_approached |= approached;
        if (!already_reachable && !approached) || !ctx.equip("torch").await {
            continue;
        }

        let mut placed = ctx.place_ground_torch(target).await
            && ctx.block_id_at(target).as_deref() == Some("torch");
        if !placed {
            interaction_failures += 1;
            // Distance alone does not prove line of sight. Reposition once to a carved cell and
            // retry this same supported target before moving to the next candidate.
            if !approached
                && approach_torch_target(ctx, carved_feet, step, trail_step, target).await
            {
                any_approached = true;
                if ctx.equip("torch").await {
                    placed = ctx.place_ground_torch(target).await
                        && ctx.block_id_at(target).as_deref() == Some("torch");
                    if !placed {
                        interaction_failures += 1;
                    }
                }
            }
        }
        if placed {
            return TorchPlacement {
                target,
                placed: true,
                candidates_checked,
                target_air_rejections,
                head_air_rejections,
                floor_support_rejections,
                supported_candidates,
                occupied_candidates,
                reachable_candidates,
                approach_failures,
                interaction_failures,
                approached: any_approached,
                player_position: ctx.position(),
            };
        }
    }
    TorchPlacement {
        target: last_target,
        placed: false,
        candidates_checked,
        target_air_rejections,
        head_air_rejections,
        floor_support_rejections,
        supported_candidates,
        occupied_candidates,
        reachable_candidates,
        approach_failures,
        interaction_failures,
        approached: any_approached,
        player_position: ctx.position(),
    }
}

/// REAL straight-line mining along a heading — faithful port of solaris mine-episode.js. Each bot
/// equips its diamond pickaxe, digs a couple blocks straight DOWN to get underground (solaris
/// `digDownToUnderground`), then drives its OWN tunnel along its parallel heading lane by genuinely
/// `dig`ging the natural terrain block-by-block (the foundation real `Client::mine`, not a
/// `/setblock` air trail), placing a verified floor `torch` every few blocks (solaris
/// torch-placement interval). A 1-wide × 2-tall tunnel is carved (feet + head cell per step) so the
/// bot can walk into the cleared space. N>2: bots fan out on parallel headings by index, so each
/// digs an independent tunnel. Terminates after `trail_len` steps or when the episode duration is
/// hit — never hangs (every `dig` is timeout-capped; an air cell is a no-op).
pub(super) async fn run(ctx: &mut Ctx) {
    let cfg = ctx.cfg().clone();
    let index = ctx.index();
    let bot_count = ctx.player_count().max(1);
    let (heading0, trail_len, spacing) = {
        let s = &ctx.engine().shared;
        (
            s.native_heading_rad,
            s.native_trail_len.max(4),
            s.native_spacing,
        )
    };
    // solaris constants: dig down 2 to get underground; torch every ~5 tunnel blocks.
    const UNDERGROUND_DEPTH: i64 = 2;
    const TORCH_EVERY: i64 = 5;
    // Each bot offset along a parallel heading lane (rank-stable by index).
    let heading = heading0;
    let lane = (index as f64 - (bot_count as f64 - 1.0) / 2.0) * spacing;
    let start = ctx.position().unwrap_or((0.0, 64.0, 0.0));
    let px = -heading.sin();
    let pz = heading.cos();
    let desired_sx = start.0 + px * lane;
    let desired_sz = start.2 + pz * lane;

    // STEP 1: equip the mining tool (solaris ensureItemInHand(diamond_pickaxe)).
    let equipped = ctx.equip("diamond_pickaxe").await;
    ctx.record(
        TYPE_LOOK,
        &format!("vis-headmine-start-{index}"),
        &format!(
            "Bot {index} real-mines a {trail_len}-block tunnel along heading {}.",
            r4(heading)
        ),
        json!({"heading_rad": r4(heading), "lane_offset": r2(lane), "trail_len": trail_len,
            "tool": "diamond_pickaxe", "equipped": equipped, "underground_depth": UNDERGROUND_DEPTH,
            "method": "real_dig"}),
    );

    // The lane target can be unreachable on steep natural terrain. Anchor all later digging and
    // torch geometry to the position the bot actually reached, never to a remote requested point.
    ctx.move_to(
        desired_sx,
        desired_sz,
        cfg.move_timeout_ms.min(8000),
        cfg.goal_radius,
    )
    .await;
    let tunnel_start = ctx.position().unwrap_or((desired_sx, start.1, desired_sz));
    let sx = tunnel_start.0;
    let sz = tunnel_start.2;

    // Establish the entrance and intended tunnel axis before looking down to excavate. Keeping the
    // aim near eye level makes the heading readable in low-resolution captures.
    let entrance_view = (
        sx + heading.cos() * 4.0,
        tunnel_start.1 + 1.4,
        sz + heading.sin() * 4.0,
    );
    ctx.look(entrance_view, cfg.look_settle_ms.max(600)).await;
    ctx.record(
        TYPE_LOOK,
        &format!("vis-headmine-entrance-{index}"),
        "Frame the tunnel entrance and mining heading before descending.",
        json!({"phase": "entrance", "heading_rad": r4(heading),
            "requested_lane_start": {"x": r2(desired_sx), "z": r2(desired_sz)},
            "actual_tunnel_start": {"x": r2(sx), "z": r2(sz)}}),
    );

    // STEP 2: dig straight DOWN to get underground (solaris digDownToUnderground). Dig the block
    // beneath our feet repeatedly; we fall into each cleared cell. Stop early on lava/void.
    let mut dug_down = 0i64;
    for _ in 0..UNDERGROUND_DEPTH {
        if ctx.target_met() {
            break;
        }
        let Some(p) = wait_for_vertical_settle(ctx).await else {
            break;
        };
        let support_y = p.1.floor() as i32 - 1;
        let primary = BlockPos::new(p.0.floor() as i32, support_y, p.2.floor() as i32);
        let min_x = (p.0 - 0.31).floor() as i32;
        let max_x = (p.0 + 0.31).floor() as i32;
        let min_z = (p.2 - 0.31).floor() as i32;
        let max_z = (p.2 + 0.31).floor() as i32;
        let mut supports = Vec::with_capacity(4);
        for x in min_x..=max_x {
            for z in min_z..=max_z {
                let cell = BlockPos::new(x, support_y, z);
                if cell != primary {
                    supports.push(cell);
                }
            }
        }
        // Remove the cell beneath the player's centre last. Until then it keeps the player stable
        // while edge-overlap support blocks are cleared.
        supports.push(primary);
        let mut dug_cells = 0u64;
        let mut blocked = false;
        for cell in supports.iter().copied() {
            match ctx.block_id_at(cell) {
                Some(id) if id == "air" || id == "cave_air" => continue,
                Some(id) if id.contains("lava") => {
                    blocked = true;
                    break;
                }
                None => {
                    blocked = true;
                    break;
                }
                _ => {}
            }
            let centre = cell.center();
            ctx.look((centre.x, centre.y, centre.z), cfg.look_settle_ms.max(300))
                .await;
            if ctx.dig(cell).await {
                dug_cells += 1;
            }
            if !ctx
                .block_id_at(cell)
                .is_some_and(|id| id == "air" || id == "cave_air")
            {
                blocked = true;
                break;
            }
        }
        // A few stale on-ground ticks can survive after the final support disappears. Require an
        // observed half-block drop before accepting the next depth.
        let clear_below = !blocked
            && supports.iter().all(|cell| {
                ctx.block_id_at(*cell)
                    .is_some_and(|id| id == "air" || id == "cave_air")
            });
        let landed = clear_below && wait_for_descent_and_settle(ctx, p.1).await.is_some();
        if landed {
            dug_down += 1;
        }
        ctx.record(
            TYPE_MINE,
            &format!("vis-headmine-down-{index}-{dug_down}"),
            &format!("Dig down to go underground ({dug_down}/{UNDERGROUND_DEPTH})."),
            json!({"phase": "dig_down",
                "block_position": {"x": primary.x, "y": primary.y, "z": primary.z},
                "support_cells": supports.len(), "dug_cells": dug_cells, "landed": landed}),
        );
        if !landed {
            break;
        }
        sleep_ms(cfg.action_hold_ms.max(400)).await;
    }

    let Some(settled) = wait_for_vertical_settle(ctx).await else {
        ctx.record(
            TYPE_MOVE,
            &format!("vis-headmine-unsettled-{index}"),
            "Abort tunnel geometry because no stable supported feet position was observed.",
            json!({"phase": "descent_unsettled", "dug_down": dug_down}),
        );
        while !ctx.target_met() {
            sleep_ms(cfg.inter_round_still_ms.max(250)).await;
        }
        return;
    };
    // Falling into a natural cave can also shift the player horizontally. Rebase the entire
    // tunnel and torch geometry a second time from the settled authoritative position.
    let sx = settled.0;
    let sz = settled.2;
    // STEP 3: tunnel along the heading by REAL digging. The tunnel runs at the bot's CURRENT
    // (now-underground) Y; carve the feet + head cell at each step so we can walk in.
    let tunnel_y = settled.1.floor() as i64;
    let mut dug: u64 = 0;
    let mut torches: u64 = 0;
    let tunnel_view = (
        sx + heading.cos() * trail_len as f64,
        tunnel_y as f64 + 1.3,
        sz + heading.sin() * trail_len as f64,
    );
    ctx.look(tunnel_view, cfg.look_settle_ms.max(600)).await;
    ctx.record(
        TYPE_LOOK,
        &format!("vis-headmine-tunnel-axis-{index}"),
        "Look along the underground tunnel axis before driving the heading.",
        json!({"phase": "tunnel_axis", "heading_rad": r4(heading)}),
    );
    let mut last_step = 0i64;
    let mut carved_feet = vec![None; trail_len as usize + 1];

    for step in 1..=trail_len {
        if ctx.target_met() {
            break;
        }
        last_step = step;

        let tx = sx + heading.cos() * step as f64;
        let tz = sz + heading.sin() * step as f64;
        let bx = tx.round() as i64;
        let bz = tz.round() as i64;
        // Carve the 1x2 cross-section at this step (feet then head cell). dig() is a no-op on air.
        for (slot, dy) in [("feet", 0i64), ("head", 1i64)] {
            let cell = BlockPos::new(bx as i32, (tunnel_y + dy) as i32, bz as i32);
            let centre = cell.center();
            ctx.look((centre.x, centre.y, centre.z), cfg.look_settle_ms.max(400))
                .await;
            ctx.record(
                TYPE_LOOK,
                &format!("vis-tunnel-target-{index}-{step}-{slot}"),
                &format!("Sight tunnel {slot} block {step} before mining."),
                json!({"phase": "target", "step": step, "slot": slot,
                    "block_position": {"x": cell.x, "y": cell.y, "z": cell.z}}),
            );
            let ok = ctx.dig(cell).await;
            if ok && ctx.block_id_at(cell).map(|id| id == "air").unwrap_or(false) {
                dug += 1;
            }
            ctx.record(
                TYPE_MINE,
                &format!("vis-tunnel-{index}-{step}-{slot}"),
                &format!(
                    "Dig tunnel {slot} block {step} at ({bx}, {}, {bz}).",
                    tunnel_y + dy
                ),
                json!({"phase": "tunnel", "step": step, "slot": slot,
                    "block_position": {"x": cell.x, "y": cell.y, "z": cell.z}, "dug": ok}),
            );
        }
        let feet = BlockPos::new(bx as i32, tunnel_y as i32, bz as i32);
        if block_is_air(ctx, feet) && block_is_air(ctx, feet.up(1)) {
            carved_feet[step as usize] = Some(feet);
        }
        // Advance into the cleared cell.
        ctx.move_to(tx, tz, cfg.move_timeout_ms.min(6000), cfg.goal_radius)
            .await;
        sleep_ms(cfg.action_hold_ms.max(300)).await;
        // STEP 4: place a floor torch periodically. The dedicated primitive clicks the solid block
        // below an already-carved trailing cell, forcing an unambiguous top-face interaction and
        // reports success only when the live world state at that cell is exactly `torch`.
        if step % TORCH_EVERY == 0 {
            if ctx.equip("torch").await {
                let torch = place_trailing_ground_torch(ctx, &carved_feet, step).await;
                if torch.placed {
                    torches += 1;
                }
                if torch.placed {
                    let tc = torch.target.center();
                    ctx.look((tc.x, tc.y, tc.z), cfg.look_settle_ms.max(400))
                        .await;
                }
                ctx.record(
                    TYPE_PLACE,
                    &format!("vis-tunnel-torch-{index}-{step}"),
                    &format!("Place ground torch in tunnel at step {step}."),
                    json!({"phase": "torch", "step": step,
                        "block_position": {"x": torch.target.x, "y": torch.target.y,
                            "z": torch.target.z},
                        "block": "torch", "placed": torch.placed,
                        "world_verified": torch.placed,
                        "candidates_checked": torch.candidates_checked,
                        "target_air_rejections": torch.target_air_rejections,
                        "head_air_rejections": torch.head_air_rejections,
                        "floor_support_rejections": torch.floor_support_rejections,
                        "supported_candidates": torch.supported_candidates,
                        "occupied_candidates": torch.occupied_candidates,
                        "reachable_candidates": torch.reachable_candidates,
                        "approach_failures": torch.approach_failures,
                        "interaction_failures": torch.interaction_failures,
                        "approached": torch.approached,
                        "player_position": torch.player_position.map(|p| json!({"x": r2(p.0), "y": r2(p.1), "z": r2(p.2)})),
                        "placement_face": "up"}),
                );
                let reveal_step = (step - 3).max(0) as f64;
                let reveal = (
                    sx + heading.cos() * reveal_step,
                    tunnel_y as f64 + 1.62,
                    sz + heading.sin() * reveal_step,
                );
                ctx.look(reveal, cfg.look_settle_ms.max(3200)).await;
                ctx.record(
                    TYPE_LOOK,
                    &format!("vis-tunnel-torch-reveal-{index}-{step}"),
                    "Reveal the torch-lit excavated tunnel corridor.",
                    json!({"phase": "torch_reveal", "step": step}),
                );
            }
            ctx.equip("diamond_pickaxe").await;
            sleep_ms(cfg.operation_pause_ms).await;
        }
    }
    // A short tunnel still gets one honest lighting attempt. This is also a recovery path if the
    // scheduled cell was temporarily occupied or its floor chunk had not arrived yet.
    if torches == 0 && last_step > 2 && ctx.equip("torch").await {
        let torch = place_trailing_ground_torch(ctx, &carved_feet, last_step).await;
        if torch.placed {
            torches += 1;
        }
        if torch.placed {
            let tc = torch.target.center();
            ctx.look((tc.x, tc.y, tc.z), cfg.look_settle_ms.max(400))
                .await;
        }
        ctx.record(
            TYPE_PLACE,
            &format!("vis-tunnel-torch-{index}-final"),
            "Place the tunnel's final verified ground torch.",
            json!({"phase": "torch_final", "step": last_step,
                "block_position": {"x": torch.target.x, "y": torch.target.y,
                    "z": torch.target.z},
                "block": "torch", "placed": torch.placed,
                "world_verified": torch.placed,
                "candidates_checked": torch.candidates_checked,
                "target_air_rejections": torch.target_air_rejections,
                "head_air_rejections": torch.head_air_rejections,
                "floor_support_rejections": torch.floor_support_rejections,
                "supported_candidates": torch.supported_candidates,
                "occupied_candidates": torch.occupied_candidates,
                "reachable_candidates": torch.reachable_candidates,
                "approach_failures": torch.approach_failures,
                "interaction_failures": torch.interaction_failures,
                "approached": torch.approached,
                "player_position": torch.player_position.map(|p| json!({"x": r2(p.0), "y": r2(p.1), "z": r2(p.2)})),
                "placement_face": "up"}),
        );
        let reveal = (sx, tunnel_y as f64 + 1.62, sz);
        ctx.look(reveal, cfg.look_settle_ms.max(3200)).await;
        ctx.record(
            TYPE_LOOK,
            &format!("vis-tunnel-torch-reveal-{index}-final"),
            "Reveal the final verified torch inside the excavated corridor.",
            json!({"phase": "torch_reveal", "step": last_step}),
        );
    }
    ctx.equip("diamond_pickaxe").await;

    ctx.record(
        TYPE_MOVE,
        &format!("vis-headmine-done-{index}"),
        &format!(
            "Tunnel pass ended: dug {dug} blocks ({dug_down} down), placed {torches} torches."
        ),
        json!({"dug_blocks": dug, "dug_down": dug_down, "torches": torches,
            "trail_len": trail_len, "completed_steps": last_step, "method": "real_dig"}),
    );
    // Finish at the tunnel entrance and frame the full excavated passage toward its face. This
    // direction keeps the corridor opening, torch, walls, and depth axis visible in both POVs.
    let tunnel_end = (
        sx + heading.cos() * last_step as f64,
        sz + heading.sin() * last_step as f64,
    );
    if last_step > 0 {
        ctx.move_to(sx, sz, cfg.move_timeout_ms.min(4000), cfg.goal_radius)
            .await;
    }
    let entrance = (sx, tunnel_y as f64 + 1.3, sz);
    let face = (tunnel_end.0, tunnel_y as f64 + 1.3, tunnel_end.1);
    ctx.look(face, cfg.look_settle_ms.max(3200)).await;
    ctx.record(
        TYPE_LOOK,
        &format!("vis-headmine-forward-{index}"),
        "Frame the full torch-lit excavated corridor from its entrance toward the tunnel face.",
        json!({"phase": "complete_forward", "torches": torches,
            "target": {"x": r2(tunnel_end.0), "y": tunnel_y + 1, "z": r2(tunnel_end.1)}}),
    );
    let mut returning = false;
    let mut pass = 1u32;
    while !ctx.target_met() {
        let target = if returning { (sx, sz) } else { tunnel_end };
        let arrived = ctx
            .move_to(
                target.0,
                target.1,
                cfg.move_timeout_ms.min(5000),
                cfg.goal_radius,
            )
            .await;
        ctx.look(
            if returning { face } else { entrance },
            cfg.look_settle_ms.max(3200),
        )
        .await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-headmine-shuttle-{index}-{pass}"),
            if returning {
                "Traverse the excavated tunnel back toward its entrance."
            } else {
                "Traverse the excavated tunnel outward toward its face."
            },
            json!({"phase": "tunnel_shuttle", "pass": pass,
                "direction": if returning { "return" } else { "outward" },
                "arrived": arrived}),
        );
        returning = !returning;
        pass += 1;
        sleep_ms(cfg.inter_round_still_ms.max(250)).await;
    }
}
