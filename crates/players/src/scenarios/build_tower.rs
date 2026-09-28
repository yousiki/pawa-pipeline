use super::*;

/// Draw in `[lo, hi)` from the per-bot stream, so a number this scenario could have hard-coded
/// varies per bot and per pass and still replays from the seed.
fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

/// How far sideways off its own slot a pass may place its tower. `station_ring` widens the ring to
/// absorb two neighbours wandering toward each other by this much.
const SITE_JITTER: f64 = 2.0;

fn tower_station_is_flat(ctx: &Ctx, feet: BlockPos, height: i64, outward: (f64, f64)) -> bool {
    // The pillar cell must be clear for the full build, while the surrounding 3x3 supplies a level,
    // obstacle-free launch area instead of accepting a one-block ledge on natural terrain.
    let launch_area_clear = (-1..=1).all(|dx| {
        (-1..=1).all(|dz| {
            let cell = BlockPos::new(feet.x + dx, feet.y, feet.z + dz);
            block_is_air(ctx, cell)
                && block_is_air(ctx, cell.up(1))
                && block_is_safe_support(ctx, cell.down(1))
        })
    });
    // The walk-off descent leaves the top at walking speed and drifts two to three blocks outward
    // while falling, PAST the 3x3 ring. The landing lane has to be as level and solid as the launch
    // ring, or a tower raised at a plateau edge drops its builder down whatever slope or water lies
    // beyond — measured twice on seed 2026 as a bot pinned below the arena for the whole episode.
    let landing_lane_clear = [2.0_f64, 2.5, 3.0].iter().all(|d| {
        let cell = BlockPos::new(
            feet.x + (outward.0 * d).round() as i32,
            feet.y,
            feet.z + (outward.1 * d).round() as i32,
        );
        block_is_air(ctx, cell)
            && block_is_air(ctx, cell.up(1))
            && block_is_safe_support(ctx, cell.down(1))
    });
    launch_area_clear
        && landing_lane_clear
        && (0..height.max(1) as i32).all(|dy| block_is_air(ctx, feet.up(dy)))
}

fn current_flat_station(ctx: &Ctx, height: i64, outward: (f64, f64)) -> Option<BlockPos> {
    let p = ctx.position()?;
    let feet = BlockPos::new(p.0.floor() as i32, p.1.floor() as i32, p.2.floor() as i32);
    tower_station_is_flat(ctx, feet, height, outward).then_some(feet)
}

fn nearby_flat_station(ctx: &Ctx, height: i64, outward: (f64, f64)) -> Option<BlockPos> {
    const OFFSETS: [(i32, i32); 21] = [
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
        (2, 1),
        (2, -1),
        (-2, 1),
        (-2, -1),
        (1, 2),
        (-1, 2),
        (1, -2),
        (-1, -2),
    ];
    let p = ctx.position()?;
    let origin = BlockPos::new(p.0.floor() as i32, p.1.floor() as i32, p.2.floor() as i32);
    for dy in [0, 1, -1, 2, -2] {
        for (dx, dz) in OFFSETS {
            let candidate = BlockPos::new(origin.x + dx, origin.y + dy, origin.z + dz);
            if tower_station_is_flat(ctx, candidate, height, outward) {
                return Some(candidate);
            }
        }
    }
    None
}

async fn step_off_tower_and_settle(ctx: &Ctx, outward_x: f64, outward_z: f64) -> bool {
    let Some(start) = ctx.position() else {
        return false;
    };
    // Pathfinder navigation cannot descend a one-block-wide pillar. Walk off the completed top
    // under real controls, then wait for the normal physics fall to finish before retreating.
    // The quantized camera turns at a fixed rate, so a 90°+ turn takes well over the old 600 ms
    // floor — and walking before it settles sends the bot off the top in an UNVERIFIED direction
    // instead of down the checked landing lane. wait_camera returns early once settled.
    ctx.look(
        (
            start.0 + outward_x * 12.0,
            start.1 + 1.4,
            start.2 + outward_z * 12.0,
        ),
        ctx.cfg().look_settle_ms.max(2_500),
    )
    .await;
    // Drive forward as one continuous press, released the instant the feet leave the top. Holding
    // the key through the whole fall air-strafes the bot 10+ blocks past the verified flat launch
    // ring and onto whatever natural terrain lies beyond — measured on seed 2026: down a slope into
    // a pit the pathfinder could never leave, freezing the bot for the rest of the episode.
    ctx.walk_until_airborne(1_500).await;

    let mut previous_y = ctx.position().map(|p| p.1).unwrap_or(start.1);
    let mut stable_ticks = 0;
    for _ in 0..80 {
        ctx.bot.wait_ticks(1).await;
        let Some(current) = ctx.position() else {
            continue;
        };
        if ctx.on_ground() && (current.1 - previous_y).abs() < 0.01 {
            stable_ticks += 1;
            if stable_ticks >= 4 {
                return current.1 < start.1 - 2.0;
            }
        } else {
            stable_ticks = 0;
        }
        previous_y = current.1;
    }
    false
}

/// Shared implementation for the compact and independent tower variants.
pub(super) async fn run(ctx: &mut Ctx, independent: bool) {
    let cfg = ctx.cfg().clone();
    let index = ctx.index();
    let bot_count = ctx.player_count().max(1);
    let (height, material, spacing) = {
        let s = &ctx.engine().shared;
        (
            s.native_tower_height.max(4),
            s.native_block_pattern.clone(),
            s.native_spacing.max(6.0),
        )
    };
    let material = if material.is_empty() {
        "minecraft:oak_planks".to_string()
    } else {
        material
    };
    // Put every bot on its own anchor-relative radial station. A minimum twelve-block radius keeps
    // six-plus-block tower columns and their post-build viewpoints from colliding for two bots,
    // while the angular layout remains N-scalable.
    let anchor = ctx.anchor();
    let ang = (index as f64 / bot_count as f64) * std::f64::consts::TAU;
    // The ring also has to keep adjacent stations `MIN_PEER_GAP` apart once both have taken their
    // per-pass jitter, or a large swarm packs neighbouring towers — and the bots building them —
    // into each other. For small swarms the seeded spacing is already the wider of the two and wins.
    let ring = if independent {
        spacing.max(12.0)
    } else {
        (spacing * 0.75).max(10.0)
    }
    .max(station_ring(bot_count, SITE_JITTER));

    let mut pass = 0usize;
    let mut barren_passes = 0usize;
    // The walk-off exit bearing: inward, back along this bot's own spoke toward the anchor. It is
    // also the direction the landing-lane part of the station check probes.
    let (exit_x, exit_z) = (-ang.cos(), -ang.sin());
    let exit = (exit_x, exit_z);
    loop {
        // A finished tower is left standing, so the next pass has to move: the same column would
        // fail its own air check and the bot would spend the rest of the episode beside one pillar.
        // The site steps out along this bot's own spoke and slides along it, and both draws only
        // ever push the ring outward, so the towers spread without closing on the neighbours'.
        let radius = ring + vary(ctx, 0.0, 5.0);
        let tangent = vary(ctx, -SITE_JITTER, SITE_JITTER);
        let bx = (anchor.0 + ang.cos() * radius - ang.sin() * tangent).round() as i64;
        let bz = (anchor.1 + ang.sin() * radius + ang.cos() * tangent).round() as i64;
        let arrived = ctx
            .move_to(bx as f64 + 0.5, bz as f64 + 0.5, cfg.move_timeout_ms, 1.5)
            .await;
        // `move_to` is intentionally approximate. Always validate the actual landing column; after a
        // failed station move, prefer the bot's current verified flat position. Only then make one
        // bounded nearby recovery move, and re-validate wherever the pathfinder really stopped.
        let mut station_source = if arrived {
            "verified_current_after_station_move"
        } else {
            "verified_current_after_failed_move"
        };
        let mut station = current_flat_station(ctx, height, exit);
        let mut station_recovery_attempted = false;
        if station.is_none()
            && let Some(candidate) = nearby_flat_station(ctx, height, exit)
        {
            station_recovery_attempted = true;
            // The tower rises under the bot's own feet, so the approach has to put the feet in the
            // exact approved column. The station move's 1.5-block stop radius parks the bot one
            // cell off the candidate, the re-verify then runs on the wrong column, and with the
            // landing-lane check valid columns are sparse enough that this near-miss repeated for
            // whole episodes.
            ctx.move_to(
                candidate.x as f64 + 0.5,
                candidate.z as f64 + 0.5,
                cfg.move_timeout_ms.min(6_000),
                0.7,
            )
            .await;
            station = current_flat_station(ctx, height, exit);
            station_source = "nearby_flat_recovery";
        }
        let tower_start = ctx
            .position()
            .unwrap_or((bx as f64 + 0.5, 64.0, bz as f64 + 0.5));
        let tower_x = tower_start.0.floor() as i64;
        let tower_z = tower_start.2.floor() as i64;
        let tower_base_y = tower_start.1.floor();

        let equipped = if station.is_some() {
            ctx.equip(&material).await
        } else {
            false
        };
        ctx.record(
            TYPE_LOOK,
            &format!("vis-tower-start-{index}-p{pass}"),
            &format!(
                "Bot {index} prepares a real {height}-block pillar tower at ({tower_x}, {tower_z})."
            ),
            json!({"pass": pass,
                "station": {"x": tower_x, "y": tower_base_y as i64, "z": tower_z},
                "expected": height, "material": material,
                "independent": independent, "equipped": equipped, "arrived": arrived,
                "flat_station_verified": station.is_some(), "station_source": station_source,
                "station_recovery_attempted": station_recovery_attempted,
                "method": "pillar_jump"}),
        );
        let mut placed = 0;
        let mut recovery_passes = 0;
        if equipped {
            // A failed layer no longer abandons the remaining tower. Restart the honest pillar-jump
            // primitive at the current top at most twice, and stop if a pass makes no real progress.
            for attempt in 0..3 {
                if placed >= height || ctx.target_met() {
                    break;
                }
                if attempt > 0 {
                    recovery_passes += 1;
                    ctx.equip(&material).await;
                    sleep_ms(cfg.action_hold_ms.max(500)).await;
                }
                let gained = ctx.build_tower_underneath(height - placed, &material).await;
                placed += gained;
                if gained == 0 && attempt > 0 {
                    break;
                }
            }
        }

        if placed > 0 {
            barren_passes = 0;
        } else {
            barren_passes += 1;
        }

        // A pathfinder cannot route down the one-block-wide completed pillar. Step off with real
        // movement, settle, then retreat far enough for full-height cues. The exit bearing points
        // INWARD, back toward the anchor: the arena side of an outer-ring tower is the verified
        // flat side, while its outward side is where the scenery ends — measured on seed 2026, an
        // outward retreat walked a builder 20 blocks down past the plateau edge into terrain the
        // planner could never leave. With zero layers placed there is no tower under the bot:
        // stepping off and retreating from nothing is exactly the blind pressing that wedged a
        // badly-landed bot deeper into terrain.
        let tangent_x = -exit_z;
        let tangent_z = exit_x;
        let view_x = tower_x as f64 + 0.5 + exit_x * 9.0 + tangent_x * 3.0;
        let view_z = tower_z as f64 + 0.5 + exit_z * 9.0 + tangent_z * 3.0;
        let tower_mid_y = tower_base_y + (placed as f64 * 0.5).max(0.8);
        let tower_focus = (tower_x as f64 + 0.5, tower_mid_y, tower_z as f64 + 0.5);
        let (landed, retreated) = if placed > 0 {
            let landed = step_off_tower_and_settle(ctx, exit_x, exit_z).await;
            // Retreat with the camera pinned on the fresh tower, not on the path: the walk exists
            // to get full-height cues of the artifact, and a camera that only ever restates the
            // travel direction is exactly what the camera-toward-travel QC gate rejects.
            let retreated = landed
                && ctx
                    .move_to_watching_point(
                        view_x,
                        view_z,
                        tower_focus,
                        cfg.move_timeout_ms.min(10_000),
                        1.5,
                    )
                    .await;
            (landed, retreated)
        } else {
            (false, false)
        };
        ctx.look(tower_focus, cfg.look_settle_ms.max(900)).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-tower-done-{index}-p{pass}"),
            &format!("Real pillar tower placed {placed}/{height} expected layers."),
            json!({"pass": pass, "expected": height, "placed": placed, "height": placed,
                "completed": placed == height, "independent": independent,
                "flat_station_verified": station.is_some(), "station_source": station_source,
                "station_recovery_attempted": station_recovery_attempted,
                "recovery_passes": recovery_passes, "barren_passes": barren_passes,
                "landed_after_build": landed, "retreated": retreated,
                "requested_viewpoint": {"x": r2(view_x), "z": r2(view_z)},
                "actual_viewpoint": ctx.position()
                    .map(|p| json!({"x": r2(p.0), "y": r2(p.1), "z": r2(p.2)})),
                "tower": {"x": tower_x, "z": tower_z}, "method": "pillar_jump"}),
        );

        if barren_passes >= 2 && !ctx.target_met() {
            // Two rounds running with zero layers means the body is somewhere the normal cycle
            // cannot build from (a bad landing below the arena, terrain the planner rejects).
            // Re-running the cycle from the same spot spins "placed 0/9" forever — measured on
            // seed 2026 as 93 s of zero net displacement. Relocate along this bot's own spoke
            // (peer-gap safe) with hop-wise travel, which carries its own shake_loose.
            let before = ctx.position();
            let wander_radius = ring * (0.5 + ctx.rng_next() * 0.9);
            let mut target = (
                anchor.0 + ang.cos() * wander_radius,
                anchor.1 + ang.sin() * wander_radius,
            );
            // A draw that lands next to where the bot already stands makes the whole leg a no-op
            // and the guard never actually relocates anything. The arena centre is always open and
            // always far from a bot pinned out on its ring.
            if let Some(p) = before
                && (target.0 - p.0).hypot(target.1 - p.2) < 8.0
            {
                target = anchor;
            }
            travel_to(ctx, target, TRAVEL_HOP, 3.0).await;
            let moved = match (before, ctx.position()) {
                (Some(a), Some(b)) => (b.0 - a.0).hypot(b.2 - a.2),
                _ => 0.0,
            };
            // Still pinned after a whole travel leg: terrain no ground move can leave, like the
            // measured two-deep pit. The one guaranteed exit is vertical — pillar out of the
            // hole with the build kit, then walk off the stub.
            let escape_pillar = moved < 2.0;
            if escape_pillar {
                ctx.equip(&material).await;
                ctx.build_tower_underneath(2, &material).await;
                step_off_tower_and_settle(ctx, exit_x, exit_z).await;
            }
            ctx.record(
                TYPE_MOVE,
                &format!("vis-tower-relocate-{index}-p{pass}"),
                &format!(
                    "Zero-progress guard relocates bot {index} after {barren_passes} barren passes."
                ),
                json!({"pass": pass, "barren_passes": barren_passes, "moved": r2(moved),
                    "escape_pillar": escape_pillar,
                    "position": ctx.position()
                        .map(|p| json!({"x": r2(p.0), "y": r2(p.1), "z": r2(p.2)}))}),
            );
        } else {
            // Read the rest of the site before walking off to the next one: a sweep whose length is its
            // own draw, then whoever else is building nearby. The camera work here is deliberately not
            // tied to the walk that follows, which starts from wherever the sweep left it.
            let scan_ms = vary(ctx, 1_500.0, 3_500.0) as u64;
            ctx.sweep(scan_ms).await;
            if bot_count > 1 {
                witness_peer_edits(ctx).await;
                glance_at_nearest_peer(ctx, None).await;
            }
            sleep_ms(vary(ctx, 600.0, 1_800.0) as u64).await;
        }

        if ctx.target_met() {
            break;
        }
        pass += 1;
    }
}
