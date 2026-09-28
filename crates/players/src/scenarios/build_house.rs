use super::*;

/// Each bot builds its own compact, visually complete house skeleton near the swarm anchor with
/// genuine survival placement. Four two-block corner posts support a complete four-sided roof
/// outline; natural ground is the floor and the open sides keep every placement reachable and the
/// finished structure readable within the 60-second capture budget.
pub(super) async fn run(ctx: &mut Ctx) {
    const SIZE: i64 = 4;

    let cfg = ctx.cfg().clone();
    let index = ctx.index();
    let bot_count = ctx.player_count().max(1);
    let post_mat = {
        let material = ctx.engine().shared.native_block_pattern.clone();
        if material.is_empty() {
            "minecraft:oak_planks".to_string()
        } else {
            material
        }
    };
    let roof_mat = "minecraft:oak_log".to_string();

    // Keep neighbouring 4x4 stations at least seven blocks apart around the real world anchor.
    // The circumference-based radius preserves that spacing as N grows.
    let anchor = ctx.anchor();
    let station_gap: f64 = 7.0;
    let base_radius = station_gap.max((bot_count as f64 * station_gap) / std::f64::consts::TAU);
    let ang = (index as f64 / bot_count as f64) * std::f64::consts::TAU;

    // The skeleton takes ~35 s at four bots, so one house cannot carry a 120 s episode: each
    // round moves the station outward along the bot's own bearing and builds the next house
    // there. Same-round neighbours keep their angular gap and other rounds sit on other radii,
    // so stations never collide; the walk between stations is itself footage.
    let mut round: i64 = 0;
    while !ctx.target_met() {
        let ring_radius = base_radius + round as f64 * (SIZE as f64 + 6.0);
        let x0 = anchor.0.round() as i64 + (ang.cos() * ring_radius).round() as i64;
        let z0 = anchor.1.round() as i64 + (ang.sin() * ring_radius).round() as i64;
        let x1 = x0 + SIZE - 1;
        let z1 = z0 + SIZE - 1;
        let door_x = x0 + SIZE / 2;
        build_house_round(
            ctx,
            &cfg,
            index,
            round,
            (x0, x1, z0, z1),
            door_x,
            &post_mat,
            &roof_mat,
        )
        .await;
        round += 1;
    }
}

#[allow(clippy::too_many_arguments)]
async fn build_house_round(
    ctx: &mut Ctx,
    cfg: &Cfg,
    index: usize,
    round: i64,
    bounds: (i64, i64, i64, i64),
    door_x: i64,
    post_mat: &str,
    roof_mat: &str,
) {
    const SIZE: i64 = 4;
    const HEIGHT: i64 = 2;
    let (x0, x1, z0, z1) = bounds;
    let index = format!("{index}-r{round}");
    let index = index.as_str();
    let post_mat = post_mat.to_string();
    let roof_mat = roof_mat.to_string();
    let anchor = ctx.anchor();

    // Start outside the open front. Bound this setup move so it cannot consume the build budget.
    ctx.move_to(
        door_x as f64 + 0.5,
        z0 as f64 - 2.5,
        cfg.move_timeout_ms.min(8000),
        1.5,
    )
    .await;
    let base_y = ctx.position().map(|p| p.1).unwrap_or(64.0).floor() as i64;
    ctx.record(
        TYPE_LOOK,
        &format!("vis-house-start-{index}"),
        &format!(
            "Bot {index} builds a compact {SIZE}x{SIZE} house skeleton with {HEIGHT}-block corner posts at ({x0}, {z0})."
        ),
        json!({"origin": {"x": x0, "z": z0}, "anchor": {"x": r2(anchor.0), "z": r2(anchor.1)},
            "house_size": SIZE, "post_height": HEIGHT, "posts": post_mat,
            "floor": "natural_ground", "roof": roof_mat, "method": "real_place"}),
    );

    // Four two-block corner posts: enough vertical form to read as a house while keeping every
    // block reachable from outside and directly supported bottom-up.
    let mut post_blocks: Vec<((i64, i64, i64), String)> = Vec::new();
    for dy in 0..HEIGHT {
        let y = base_y + dy;
        for (x, z) in [(x0, z0), (x0, z1), (x1, z0), (x1, z1)] {
            post_blocks.push(((x, y, z), post_mat.clone()));
        }
    }
    let post_expected = post_blocks.len();
    let (post_placed, post_attempted) =
        build_house_phase(ctx, index, "posts", &post_blocks, (x0, x1, z0, z1)).await;
    record_house_phase(
        ctx,
        index,
        "posts",
        post_expected,
        post_attempted,
        post_placed,
    );

    // A compact flat roof outline reads clearly from low-resolution third-person capture while
    // avoiding the sixteen extra placements of a solid slab. It sits directly above layer two.
    let roof_y = base_y + HEIGHT;
    let mut roof_blocks: Vec<((i64, i64, i64), String)> = Vec::new();
    for x in x0..=x1 {
        for z in z0..=z1 {
            if x == x0 || x == x1 || z == z0 || z == z1 {
                roof_blocks.push(((x, roof_y, z), roof_mat.clone()));
            }
        }
    }
    let roof_expected = roof_blocks.len();
    let (roof_placed, roof_attempted) =
        build_house_phase(ctx, index, "roof", &roof_blocks, (x0, x1, z0, z1)).await;
    record_house_phase(
        ctx,
        index,
        "roof",
        roof_expected,
        roof_attempted,
        roof_placed,
    );

    // Natural ground is the floor. Do not spend the 60-second budget "placing" into already-solid
    // terrain, which `place_block` would correctly treat as filled but would not be a real attempt.
    let floor_expected: usize = 0;
    let floor_attempted: u64 = 0;
    let floor_placed: u64 = 0;
    record_house_phase(
        ctx,
        index,
        "floor",
        floor_expected,
        floor_attempted,
        floor_placed,
    );
    let complete = post_placed as usize == post_expected && roof_placed as usize == roof_expected;
    let description = if complete {
        format!(
            "House skeleton complete: posts {post_placed}/{post_attempted}/{post_expected}, roof {roof_placed}/{roof_attempted}/{roof_expected}, floor {floor_placed}/{floor_attempted}/{floor_expected} (placed/attempted/expected)."
        )
    } else {
        format!(
            "House skeleton incomplete: posts {post_placed}/{post_attempted}/{post_expected}, roof {roof_placed}/{roof_attempted}/{roof_expected}, floor {floor_placed}/{floor_attempted}/{floor_expected} (placed/attempted/expected)."
        )
    };
    ctx.record(
        TYPE_MOVE,
        &format!("vis-house-done-{index}"),
        &description,
        json!({"complete": complete,
            "posts": {"expected": post_expected, "attempted": post_attempted, "placed": post_placed},
            "roof": {"expected": roof_expected, "attempted": roof_attempted, "placed": roof_placed},
            "floor": {"expected": floor_expected, "attempted": floor_attempted, "placed": floor_placed}}),
    );

    // Admire from 5.5 blocks south of the facade — briefly. The next round's station move is the
    // scene's continuation; standing here until the clock ran out was an 86 s statue per pov.
    let center_x = (x0 + x1) as f64 / 2.0 + 0.5;
    let center_z = (z0 + z1) as f64 / 2.0 + 0.5;
    ctx.move_to(center_x, center_z - 5.5, cfg.move_timeout_ms.min(8000), 1.5)
        .await;
    let house_focus = (center_x, base_y as f64 + 1.4, center_z);
    let admire_until = ctx.elapsed_ms() + 5_000;
    while !ctx.target_met() && ctx.elapsed_ms() < admire_until {
        ctx.look_default(house_focus).await;
        witness_peer_edits(ctx).await;
        glance_at_nearest_peer(ctx, None).await;
        sleep_ms(cfg.inter_round_still_ms.min(1_500)).await;
    }
}

/// Fast real-placement phase for the time-bounded house-skeleton episode. `Ctx::place_block`
/// already holds long enough for interaction and verification; omitting `build_blocks`' additional
/// per-block presentation delay lets all designed post/roof blocks fit in a 60-second episode.
async fn build_house_phase(
    ctx: &mut Ctx,
    index: &str,
    phase: &str,
    blocks: &[((i64, i64, i64), String)],
    bounds: (i64, i64, i64, i64),
) -> (u64, u64) {
    let expected = blocks.len();
    let mut attempted = 0;
    let mut placed = vec![false; expected];

    for pass in 0..=3 {
        for (block_index, ((x, y, z), material)) in blocks.iter().enumerate() {
            if ctx.target_met() {
                break;
            }
            if placed[block_index] {
                continue;
            }
            let target = BlockPos::new(*x as i32, *y as i32, *z as i32);
            let expected_id = material.strip_prefix("minecraft:").unwrap_or(material);
            if ctx.block_id_at(target).as_deref() == Some(expected_id) {
                placed[block_index] = true;
                continue;
            }
            let (x0, x1, z0, z1) = bounds;
            let outward = if *z == z0 {
                (*x as f64 + 0.5, *z as f64 - 3.0)
            } else if *z == z1 {
                (*x as f64 + 0.5, *z as f64 + 3.0)
            } else if *x == x0 {
                (*x as f64 - 3.0, *z as f64 + 0.5)
            } else {
                debug_assert_eq!(*x, x1);
                (*x as f64 + 3.0, *z as f64 + 0.5)
            };
            if pass > 0 || !ctx.target_in_reach(target) {
                let cfg = ctx.cfg();
                let approach = if pass > 0 {
                    let feet_y = ctx
                        .position()
                        .map(|p| p.1.floor() as i32)
                        .unwrap_or(*y as i32);
                    walkable_xz_near(ctx, outward, feet_y).unwrap_or(outward)
                } else {
                    (*x as f64, *z as f64)
                };
                ctx.move_to(approach.0, approach.1, cfg.move_timeout_ms.min(5000), 1.5)
                    .await;
            }
            if !ctx.equip(material).await {
                continue;
            }
            attempted += 1;
            let ok = ctx.place_block(target, material).await;
            placed[block_index] = ok;
            let description = if pass == 0 {
                format!("Place house {phase} block {}/{expected}.", block_index + 1)
            } else {
                format!(
                    "Retry house {phase} block {}/{expected} (pass {pass}).",
                    block_index + 1
                )
            };
            ctx.record(
                TYPE_PLACE,
                &format!("vis-house-{phase}-{index}-{block_index}-p{pass}"),
                &description,
                json!({"phase": phase, "block_index": block_index, "expected": expected,
                    "retry_pass": pass, "placed": ok, "block": material,
                    "block_position": {"x": target.x, "y": target.y, "z": target.z}}),
            );
        }
        if placed.iter().all(|ok| *ok) || ctx.target_met() {
            break;
        }
    }
    (
        placed.into_iter().filter(|ok| *ok).count() as u64,
        attempted,
    )
}

fn record_house_phase(
    ctx: &Ctx,
    index: &str,
    phase: &str,
    expected: usize,
    attempted: u64,
    placed: u64,
) {
    ctx.record(
        TYPE_MOVE,
        &format!("vis-house-{phase}-done-{index}"),
        &format!(
            "House {phase} phase: expected {expected}, attempted {attempted}, placed {placed}."
        ),
        json!({"phase": phase, "expected": expected, "attempted": attempted, "placed": placed}),
    );
}
