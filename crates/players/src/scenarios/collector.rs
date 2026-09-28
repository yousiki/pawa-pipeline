use super::*;

/// REAL leader/follower ore collecting — faithful port of solaris collector-episode.js. One LEADER
/// (bot index 0) equips a diamond pickaxe and runs mining cycles: scan the world for valuable ore
/// veins (`find_visible_ores` = solaris `findVisibleOres`), pathfind adjacent to each and `dig` it
/// for real; on ore-free terrain (e.g. superflat) FALL BACK to directional/staircase mining — dig a
/// short straight/down-sloping tunnel through the natural terrain (solaris `performDirectional/
/// StaircaseMining`). The other bots are FOLLOWERS: they trail the leader (move toward its live
/// position, solaris `GoalFollow`) and place a `torch` against a nearby wall periodically. Replaces
/// the old scatter-point mutex-claim mechanic with genuine survival digging. N>2: 1 leader + rest
/// followers (by index). Bounded by `MAX_CYCLES` and the episode duration, so it always terminates;
/// every `dig` is timeout-capped and air cells are no-ops, so it never hangs on flat terrain.
pub(super) async fn run(ctx: &mut Ctx) {
    let cfg = ctx.cfg().clone();
    let index = ctx.index();
    // solaris constants (collector-episode.js): cap ores/cycle, scan radius, bounded cycles.
    const MAX_ORES_TO_MINE: usize = 8;
    const ORE_SCAN_RADIUS: i32 = 16;
    const MAX_CYCLES: u32 = 6;
    const TORCH_EVERY_CYCLES: u32 = 3;

    let is_leader = index == 0;
    let equipped = ctx
        .equip(if is_leader {
            "diamond_pickaxe"
        } else {
            "torch"
        })
        .await;
    ctx.record(
        TYPE_LOOK,
        &format!("vis-collect-start-{index}"),
        &format!(
            "Bot {index} starts collector as {} (real digging).",
            if is_leader { "LEADER" } else { "FOLLOWER" }
        ),
        json!({"role": if is_leader { "leader" } else { "follower" }, "equipped": equipped,
            "method": "real_dig"}),
    );

    if is_leader {
        collector_lead(ctx, &cfg, ORE_SCAN_RADIUS, MAX_ORES_TO_MINE, MAX_CYCLES).await;
    } else {
        collector_follow(ctx, &cfg, TORCH_EVERY_CYCLES).await;
    }
    // Hold a meaningful final composition: every bot watches a teammate instead of sweeping after
    // its role has completed. Followers already keep the leader in frame throughout their loop.
    glance_at_nearest_peer(ctx, None).await;
    while !ctx.target_met() {
        glance_at_nearest_peer(ctx, None).await;
        sleep_ms(cfg.inter_round_still_ms.max(500)).await;
    }
}

/// Collector LEADER loop — solaris collector-episode.js `mineAsLeader`/`executeMiningTask`. For each
/// bounded cycle: scan for visible ore veins; if any, pathfind adjacent to each (up to `max_ores`)
/// and `dig` it for real; otherwise FALL BACK to directional/staircase mining (dig a short tunnel
/// through the natural terrain). The shared RNG (intact seeded plan) picks the fallback direction +
/// kind, exactly like solaris `getNextTaskSpec`. Always terminates (bounded cycles / episode time).
async fn collector_lead(
    ctx: &mut Ctx,
    cfg: &Cfg,
    scan_radius: i32,
    max_ores: usize,
    max_cycles: u32,
) {
    let index = ctx.index();
    let mut total_dug: u64 = 0;
    for cycle in 0..max_cycles {
        if ctx.target_met() {
            break;
        }
        // Look for valuable ore veins in range (solaris findVisibleOres).
        let ores = ctx.find_visible_ores(scan_radius, max_ores);
        if !ores.is_empty() {
            ctx.record(
                TYPE_LOOK,
                &format!("vis-collect-veins-{index}-{cycle}"),
                &format!("Leader found {} ore block(s) in cycle {cycle}.", ores.len()),
                json!({"cycle": cycle, "ores_found": ores.len()}),
            );
            for (oi, ore) in ores.iter().enumerate() {
                if ctx.target_met() {
                    break;
                }
                // Pathfind adjacent (solaris GoalBlock), explicitly settle the camera on the ore,
                // then dig it for real. The target/result pair makes each mining action legible.
                let arrived = ctx
                    .move_to(
                        ore.x as f64,
                        ore.z as f64,
                        cfg.move_timeout_ms.min(8000),
                        (cfg.place_reach - 1.0).max(1.5),
                    )
                    .await;
                let centre = ore.center();
                ctx.look((centre.x, centre.y, centre.z), cfg.look_settle_ms.max(500))
                    .await;
                ctx.record(
                    TYPE_LOOK,
                    &format!("vis-collect-target-{index}-{cycle}-{oi}"),
                    &format!(
                        "Leader sights ore at ({}, {}, {}) before mining.",
                        ore.x, ore.y, ore.z
                    ),
                    json!({"phase": "target", "cycle": cycle, "ore_index": oi,
                        "arrived": arrived,
                        "block_position": {"x": ore.x, "y": ore.y, "z": ore.z}}),
                );
                let dug = ctx.dig(*ore).await;
                if dug {
                    total_dug += 1;
                }
                ctx.record(
                    TYPE_MINE,
                    &format!("vis-collect-result-{index}-{cycle}-{oi}"),
                    &format!("Mine ore at ({}, {}, {}).", ore.x, ore.y, ore.z),
                    json!({"phase": "result", "cycle": cycle, "ore_index": oi,
                        "block_position": {"x": ore.x, "y": ore.y, "z": ore.z}, "dug": dug}),
                );
                sleep_ms(cfg.action_hold_ms.max(500)).await;
                ctx.look((centre.x, centre.y, centre.z), cfg.look_settle_ms.max(500))
                    .await;
                ctx.record(
                    TYPE_LOOK,
                    &format!("vis-collect-reveal-{index}-{cycle}-{oi}"),
                    "Hold on the mined ore cell to reveal the resulting cavity.",
                    json!({"phase": "reveal", "cycle": cycle, "ore_index": oi,
                        "block_position": {"x": ore.x, "y": ore.y, "z": ore.z},
                        "block_after": ctx.block_id_at(*ore)}),
                );
            }
        } else {
            // No ore (e.g. superflat) — directional/staircase fallback (solaris getNextTaskSpec):
            // 60% directional, else staircase; random cardinal; 5..=8 blocks.
            let staircase = ctx.rng_next() >= 0.6;
            let dir = (ctx.rng_next() * 4.0).floor() as i64 % 4;
            let dist = 5 + (ctx.rng_next() * 4.0).floor() as i64; // 5..=8
            let (dx, dz) = match dir {
                0 => (1i64, 0i64),
                1 => (-1, 0),
                2 => (0, 1),
                _ => (0, -1),
            };
            let n = collector_dig_run(ctx, cfg, dx, dz, dist, staircase, cycle).await;
            total_dug += n;
        }
        sleep_ms(cfg.operation_pause_ms.max(300)).await;
    }
    ctx.record(
        TYPE_MOVE,
        &format!("vis-collect-leaddone-{index}"),
        &format!("Leader mining complete: dug {total_dug} blocks across {max_cycles} cycles."),
        json!({"dug_blocks": total_dug, "cycles": max_cycles, "method": "real_dig"}),
    );
}

/// Directional / staircase mining fallback (solaris performDirectional/StaircaseMining) using REAL
/// digging: carve a 1×2 tunnel `dist` blocks along (dx, dz); when `staircase`, also step DOWN one Y
/// per block (45° descent). Returns the number of blocks actually broken. Air cells are no-ops, so
/// this is safe on any terrain and bounded by `dist`.
async fn collector_dig_run(
    ctx: &mut Ctx,
    cfg: &Cfg,
    dx: i64,
    dz: i64,
    dist: i64,
    staircase: bool,
    cycle: u32,
) -> u64 {
    let index = ctx.index();
    let Some(start) = ctx.position() else {
        return 0;
    };
    let (sx, sz) = (start.0.floor() as i64, start.2.floor() as i64);
    let mut y = start.1.floor() as i64;
    let mut dug: u64 = 0;
    ctx.record(
        TYPE_LOOK,
        &format!("vis-collect-fallback-{index}-{cycle}"),
        &format!(
            "No ore in range; {} mining {dist} blocks (cycle {cycle}).",
            if staircase {
                "staircase"
            } else {
                "directional"
            }
        ),
        json!({"cycle": cycle, "kind": if staircase { "staircase" } else { "directional" },
            "dir": {"x": dx, "z": dz}, "distance": dist}),
    );
    for step in 1..=dist {
        if ctx.target_met() {
            break;
        }
        if staircase {
            y -= 1; // descend one block per step (45° staircase)
        }
        let bx = sx + dx * step;
        let bz = sz + dz * step;
        // Carve feet + head cell so the bot can advance. Each real dig gets an explicit target and
        // result, followed by a short still and a look back toward the followers.
        for (slot, dy) in [0i64, 1i64].into_iter().enumerate() {
            let cell = BlockPos::new(bx as i32, (y + dy) as i32, bz as i32);
            // Never dig into lava.
            if ctx
                .block_id_at(cell)
                .map(|id| id.contains("lava"))
                .unwrap_or(false)
            {
                continue;
            }
            let centre = cell.center();
            ctx.look((centre.x, centre.y, centre.z), cfg.look_settle_ms.max(400))
                .await;
            ctx.record(
                TYPE_LOOK,
                &format!("vis-collect-fallback-target-{index}-{cycle}-{step}-{slot}"),
                &format!("Leader sights fallback tunnel cell {step}/{dist} before mining."),
                json!({"phase": "target", "cycle": cycle, "step": step, "slot": slot,
                    "block_position": {"x": cell.x, "y": cell.y, "z": cell.z}}),
            );
            let ok = ctx.dig(cell).await;
            let cleared = ok && ctx.block_id_at(cell).map(|id| id == "air").unwrap_or(false);
            if cleared {
                dug += 1;
            }
            ctx.record(
                TYPE_MINE,
                &format!("vis-collect-fallback-result-{index}-{cycle}-{step}-{slot}"),
                &format!("Mine fallback tunnel cell {step}/{dist}."),
                json!({"phase": "result", "cycle": cycle, "step": step, "slot": slot,
                    "block_position": {"x": cell.x, "y": cell.y, "z": cell.z},
                    "dug": ok, "cleared": cleared}),
            );
            sleep_ms(cfg.action_hold_ms.max(350)).await;
            ctx.look(
                (
                    bx as f64 + dx as f64,
                    y as f64 + 1.62,
                    bz as f64 + dz as f64,
                ),
                cfg.look_settle_ms.max(300),
            )
            .await;
        }
        let arrived = ctx
            .move_to(
                bx as f64,
                bz as f64,
                cfg.move_timeout_ms.min(6000),
                cfg.goal_radius,
            )
            .await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-collect-fallback-move-{index}-{cycle}-{step}"),
            &format!("Advance into cleared tunnel cell {step}/{dist}."),
            json!({"cycle": cycle, "step": step, "arrived": arrived,
                "target": {"x": bx, "y": y, "z": bz}}),
        );
        sleep_ms(cfg.action_hold_ms.max(250)).await;
    }
    dug
}

/// Collector FOLLOWER loop — solaris collector-episode.js `followAndPlaceTorches`. Trail the leader
/// (bot 0) by moving toward its live position (solaris `GoalFollow`), placing a `torch` against a
/// nearby wall every few cycles. Bounded by the episode duration; if the leader's position is not
/// yet replicated it simply idles toward the group centre, never hanging.
async fn collector_follow(ctx: &mut Ctx, cfg: &Cfg, torch_every_cycles: u32) {
    let index = ctx.index();
    const FOLLOW_MIN: f64 = 3.0;
    const FOLLOW_MAX: f64 = 5.0;
    const FOLLOW_TARGET: f64 = 4.5;
    let mut cycle: u32 = 0;
    let mut torches: u64 = 0;
    while !ctx.target_met() {
        cycle += 1;
        let leader = ctx.peer_position(0);
        if let Some(lp) = leader {
            // Look at the leader before moving. Only correct when outside the readable 3–5 block
            // band; this prevents the old behavior from crowding the leader at their feet.
            look_at_peer(ctx, 0, cfg.look_settle_ms.max(350)).await;
            if let Some(own) = ctx.position() {
                let vx = own.0 - lp.0;
                let vz = own.2 - lp.2;
                let distance = (vx * vx + vz * vz).sqrt();
                let movement = if distance > FOLLOW_MAX {
                    Some((lp.0, lp.2, FOLLOW_TARGET))
                } else if distance < FOLLOW_MIN {
                    let (ux, uz) = if distance > 0.1 {
                        (vx / distance, vz / distance)
                    } else {
                        let angle = (index as f64) * 2.399_963_229_728_653;
                        (angle.cos(), angle.sin())
                    };
                    Some((
                        lp.0 + ux * FOLLOW_TARGET,
                        lp.2 + uz * FOLLOW_TARGET,
                        cfg.goal_radius,
                    ))
                } else {
                    None
                };
                if let Some((tx, tz, radius)) = movement {
                    let arrived = ctx
                        .move_to(tx, tz, cfg.move_timeout_ms.min(6000), radius)
                        .await;
                    let gap = ctx
                        .position()
                        .map(|p| ((p.0 - lp.0).powi(2) + (p.2 - lp.2).powi(2)).sqrt());
                    ctx.record(
                        TYPE_MOVE,
                        &format!("vis-follow-gap-{index}-{cycle}"),
                        "Follower corrects to the 3–5 block observation band behind the leader.",
                        json!({"role": "follower", "leader": 0, "cycle": cycle,
                            "arrived": arrived, "gap": gap.map(r2),
                            "target": {"x": r2(tx), "z": r2(tz)}}),
                    );
                }
            }

            // Settle first on the leader, then slightly lower on the active mining area. This keeps
            // the work readable without leaving the camera pointed at the follower's own feet.
            look_at_peer(ctx, 0, cfg.look_settle_ms.max(350)).await;
            ctx.look((lp.0, lp.1 + 0.6, lp.2), cfg.look_settle_ms.max(300))
                .await;
            ctx.record(
                TYPE_LOOK,
                &format!("vis-follow-watch-{index}-{cycle}"),
                "Follower watches the leader and active mining point.",
                json!({"phase": "target", "role": "follower", "leader": 0, "cycle": cycle,
                    "target": {"x": r2(lp.0), "y": r2(lp.1 + 0.6), "z": r2(lp.2)}}),
            );

            // Place fewer torches than before, and only while close enough to still observe the
            // leader. Placement remains a genuine survival block interaction.
            let gap = ctx
                .position()
                .map(|p| ((p.0 - lp.0).powi(2) + (p.2 - lp.2).powi(2)).sqrt())
                .unwrap_or(f64::INFINITY);
            if cycle.is_multiple_of(torch_every_cycles.max(1))
                && gap <= FOLLOW_MAX + 0.5
                && ctx.equip("torch").await
                && let Some(p) = ctx.position()
            {
                let toward_leader = (lp.0 - p.0, lp.2 - p.2);
                let distance = (toward_leader.0 * toward_leader.0
                    + toward_leader.1 * toward_leader.1)
                    .sqrt()
                    .max(0.001);
                // Place one block toward the leader, not inside the follower's own feet.
                // From this staging the torch stays in the foreground and the leader can
                // remain in the background of the same target/result composition.
                let target = BlockPos::new(
                    (p.0 + toward_leader.0 / distance * 1.5).floor() as i32,
                    p.1.floor() as i32,
                    (p.2 + toward_leader.1 / distance * 1.5).floor() as i32,
                );
                let tc = target.center();
                ctx.look((tc.x, tc.y, tc.z), cfg.look_settle_ms.max(500))
                    .await;
                ctx.record(
                    TYPE_LOOK,
                    &format!("vis-follow-torch-target-{index}-{cycle}"),
                    "Follower sights the ground torch target between itself and the leader.",
                    json!({"phase": "torch_target", "cycle": cycle,
                                "block_position": {"x": target.x, "y": target.y, "z": target.z}}),
                );
                let placed = ctx.place_ground_torch(target).await;
                if placed {
                    torches += 1;
                }
                ctx.record(
                    TYPE_PLACE,
                    &format!("vis-follow-torch-{index}-{cycle}"),
                    &format!("Follower places torch (cycle {cycle})."),
                    json!({"cycle": cycle,
                                "block_position": {"x": target.x, "y": target.y, "z": target.z},
                                "block": "torch", "placed": placed}),
                );
                sleep_ms(cfg.action_hold_ms.max(500)).await;
                ctx.look((tc.x, tc.y + 0.3, tc.z), cfg.look_settle_ms.max(500))
                    .await;
                ctx.record(
                    TYPE_LOOK,
                    &format!("vis-follow-torch-reveal-{index}-{cycle}"),
                    "Reveal the placed torch with the leader beyond it.",
                    json!({"phase": "torch_reveal", "cycle": cycle, "placed": placed,
                                "block_position": {"x": target.x, "y": target.y, "z": target.z},
                                "block_after": ctx.block_id_at(target)}),
                );
                look_at_peer(ctx, 0, cfg.look_settle_ms.max(350)).await;
            }
        } else if let Some(c) = ctx.group_center() {
            ctx.look((c.0, c.1 + 1.2, c.2), cfg.look_settle_ms.max(300))
                .await;
        }
        sleep_ms(cfg.operation_pause_ms.max(500)).await;
    }
    look_at_peer(ctx, 0, cfg.look_settle_ms.max(500)).await;
    ctx.record(
        TYPE_MOVE,
        &format!("vis-follow-done-{index}"),
        &format!("Follower complete: placed {torches} torches trailing the leader."),
        json!({"torches": torches, "method": "real_follow", "finish_view": "leader"}),
    );
}
