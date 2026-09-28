use super::*;

const OPEN_RADIUS: f64 = 5.0;
const MIN_SEGMENT_DISPLACEMENT: f64 = 0.65;

fn horizontal_distance(a: (f64, f64, f64), b: (f64, f64, f64)) -> f64 {
    ((a.0 - b.0).powi(2) + (a.2 - b.2).powi(2)).sqrt()
}

fn open_column(ctx: &Ctx, x: f64, y: f64, z: f64) -> bool {
    let bx = x.floor() as i32;
    let by = y.floor() as i32;
    let bz = z.floor() as i32;
    let Ok(world) = ctx.bot.world() else {
        return false;
    };
    let world = world.read();
    let Some(ground) = world.get_block_state(BlockPos::new(bx, by - 1, bz)) else {
        return false;
    };
    if ground.is_air() {
        return false;
    }
    (0..=1).all(|dy| {
        world
            .get_block_state(BlockPos::new(bx, by + dy, bz))
            .map(|state| state.is_air())
            .unwrap_or(false)
    })
}

fn near_obstacle(ctx: &Ctx) -> bool {
    let Some(p) = ctx.position() else {
        return true;
    };
    let bx = p.0.floor() as i32;
    let by = p.1.floor() as i32;
    let bz = p.2.floor() as i32;
    let Ok(world) = ctx.bot.world() else {
        return true;
    };
    let world = world.read();
    for (dx, dz) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
        for dy in 0..=1 {
            if world
                .get_block_state(BlockPos::new(bx + dx, by + dy, bz + dz))
                .map(|state| !state.is_air())
                .unwrap_or(true)
            {
                return true;
            }
        }
    }
    false
}

fn most_open_direction(ctx: &Ctx) -> (f64, f64) {
    const DIRECTIONS: [(f64, f64); 8] = [
        (1.0, 0.0),
        (0.707, 0.707),
        (0.0, 1.0),
        (-0.707, 0.707),
        (-1.0, 0.0),
        (-0.707, -0.707),
        (0.0, -1.0),
        (0.707, -0.707),
    ];
    let Some(p) = ctx.position() else {
        return (1.0, 0.0);
    };
    DIRECTIONS
        .into_iter()
        .max_by_key(|(dx, dz)| {
            (2..=16)
                .take_while(|step| {
                    let distance = *step as f64;
                    open_column(ctx, p.0 + dx * distance, p.1, p.2 + dz * distance)
                })
                .count()
        })
        .unwrap_or((1.0, 0.0))
}

async fn level_view(ctx: &Ctx, direction: (f64, f64), distance: f64, settle_ms: u64) {
    let Some(p) = ctx.position() else { return };
    ctx.look(
        (
            p.0 + direction.0 * distance,
            p.1 + 1.62,
            p.2 + direction.1 * distance,
        ),
        settle_ms,
    )
    .await;
}

async fn level_open_view(ctx: &Ctx, distance: f64, settle_ms: u64) {
    level_view(ctx, most_open_direction(ctx), distance, settle_ms).await;
}

async fn recover_after_segment(
    ctx: &mut Ctx,
    center: (f64, f64),
    before: Option<(f64, f64, f64)>,
    segment: &str,
) {
    let after = ctx.position();
    let displacement = match (before, after) {
        (Some(a), Some(b)) => horizontal_distance(a, b),
        _ => 0.0,
    };
    let center_distance = after
        .map(|p| ((p.0 - center.0).powi(2) + (p.2 - center.1).powi(2)).sqrt())
        .unwrap_or(f64::INFINITY);
    let obstacle = near_obstacle(ctx);
    if displacement < MIN_SEGMENT_DISPLACEMENT || obstacle || center_distance > OPEN_RADIUS {
        let arrived = ctx.move_default(center.0, center.1).await;
        level_open_view(ctx, 14.0, 1400).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-calib-recover-{}-{}", ctx.index(), ctx.elapsed_ms()),
            "Return to the open calibration center after a stalled, obstructed, or boundary-reaching segment.",
            json!({
                "phase": "recovery",
                "segment": segment,
                "arrived": arrived,
                "displacement": r2(displacement),
                "center_distance": r2(center_distance),
                "near_obstacle": obstacle,
            }),
        );
    }
}

fn nearby_open_waypoint(ctx: &Ctx, rng: &mut Mulberry32, center: (f64, f64)) -> (f64, f64) {
    let y = ctx.position().map(|p| p.1).unwrap_or(64.0);
    for _ in 0..12 {
        let radius = 2.0 + rng.next() * (OPEN_RADIUS - 2.0);
        let angle = rng.next() * std::f64::consts::TAU;
        let candidate = (
            center.0 + radius * angle.cos(),
            center.1 + radius * angle.sin(),
        );
        if open_column(ctx, candidate.0, y, candidate.1) {
            return candidate;
        }
    }
    center
}

fn has_random_window(ctx: &Ctx) -> bool {
    let duration = ctx.cfg().episode_duration_ms;
    duration > 0 && ctx.elapsed_ms() + 7_500 < duration
}

fn open_center(ctx: &Ctx, preferred: (f64, f64)) -> (f64, f64) {
    let y = ctx.position().map(|p| p.1).unwrap_or(64.0);
    if open_column(ctx, preferred.0, y, preferred.1) {
        return preferred;
    }
    let anchor = ctx.anchor();
    for radius in [1.5, 3.0, 4.5] {
        for step in 0..8 {
            let angle = step as f64 * std::f64::consts::TAU / 8.0;
            let candidate = (
                anchor.0 + radius * angle.cos(),
                anchor.1 + radius * angle.sin(),
            );
            if open_column(ctx, candidate.0, y, candidate.1) {
                return candidate;
            }
        }
    }
    preferred
}

/// CALIBRATION: a slow, fully-enumerated action↔video reference episode (NOT a swarm scenario —
/// every bot runs the SAME script, lightly staggered). It walks each movement key on its own, then
/// the diagonals, then isolated view drags, then real pathfinding to a few random nearby points,
/// then random combinations until the episode ends. Every moving segment is bounded around the
/// spawn anchor and checked for stalls/nearby walls before the next action.
pub(super) async fn run(ctx: &mut Ctx) {
    let index = ctx.index();
    let mut rng = ctx.engine().bot_rng(index);
    sleep_ms((index as u64) * 200).await;

    const HOLD_MS: u64 = 2600;
    const PAUSE_MS: u64 = 900;
    const JUMP_MS: u64 = 1300;
    const DRAG_MS: u64 = 1900;

    let anchor = ctx.anchor();
    let center_angle = (index as f64 / ctx.player_count().max(1) as f64) * std::f64::consts::TAU;
    let center = open_center(
        ctx,
        (
            anchor.0 + center_angle.cos() * 1.5,
            anchor.1 + center_angle.sin() * 1.5,
        ),
    );
    ctx.move_default(center.0, center.1).await;
    level_open_view(ctx, 14.0, 1400).await;

    ctx.record(
        TYPE_LOOK,
        &format!("vis-calib-start-{index}"),
        "Action-semantics calibration: enumerate W/A/S/D, diagonals, view drags, pathfinding, random combos.",
        json!({"phase": "start", "method": "calibration"}),
    );

    // STEP 1 — single movement keys, camera frozen (sprint only on forward, where MC honours it).
    for (key, dir, sprint) in [
        ("W", WalkDirection::Forward, true),
        ("A", WalkDirection::Left, false),
        ("S", WalkDirection::Backward, false),
        ("D", WalkDirection::Right, false),
    ] {
        level_open_view(ctx, 14.0, 700).await;
        if ctx.target_met() {
            break;
        }
        let before = ctx.position();
        ctx.record(
            TYPE_MOVE,
            &format!("vis-calib-{key}-{index}"),
            &format!("Hold {key} for {HOLD_MS}ms with a low-frequency jump pulse."),
            json!({"phase": "single_key", "key": key, "sprint": sprint}),
        );
        ctx.hold_visual(dir, sprint, HOLD_MS, JUMP_MS).await;
        recover_after_segment(ctx, center, before, key).await;
        ctx.still(PAUSE_MS).await;
    }

    // STEP 2 — diagonal combos (W+A, W+D, S+A, S+D).
    for (key, dir) in [
        ("WA", WalkDirection::ForwardLeft),
        ("WD", WalkDirection::ForwardRight),
        ("SA", WalkDirection::BackwardLeft),
        ("SD", WalkDirection::BackwardRight),
    ] {
        level_open_view(ctx, 14.0, 700).await;
        if ctx.target_met() {
            break;
        }
        let before = ctx.position();
        ctx.record(
            TYPE_MOVE,
            &format!("vis-calib-{key}-{index}"),
            &format!("Hold {key} (diagonal) for {HOLD_MS}ms."),
            json!({"phase": "diagonal", "key": key}),
        );
        ctx.hold_visual(dir, key.starts_with('W'), HOLD_MS, JUMP_MS)
            .await;
        recover_after_segment(ctx, center, before, key).await;
        ctx.still(PAUSE_MS).await;
    }

    // STEP 3 — isolated view drags from a standstill: right, left, up, down.
    for (name, dyaw, dpitch) in [
        ("look_right", 80.0_f64, 0.0_f64),
        ("look_left", -80.0, 0.0),
        ("look_up", 0.0, -55.0),
        ("look_down", 0.0, 55.0),
    ] {
        if ctx.target_met() {
            break;
        }
        ctx.drag_view(dyaw, dpitch, DRAG_MS).await;
        ctx.still(350).await;
        ctx.record(
            TYPE_LOOK,
            &format!("vis-calib-{name}-{index}"),
            &format!("Drag the view {name} by ({dyaw:+.0},{dpitch:+.0}) deg, standing still."),
            json!({"phase": "view_drag", "name": name, "d_yaw_deg": dyaw, "d_pitch_deg": dpitch}),
        );
        if near_obstacle(ctx) {
            ctx.move_default(center.0, center.1).await;
            level_open_view(ctx, 14.0, 1400).await;
        }
        ctx.still(PAUSE_MS).await;
        // Reset to a level, terrain-referenced composition after every isolated drag. This keeps
        // the next semantic event from inheriting an intentional sky/floor extreme.
        level_open_view(ctx, 14.0, 900).await;
    }

    // STEP 4 — real pathfinding to open points inside the small anchor-centred calibration area.
    for k in 0..3 {
        if ctx.target_met() {
            break;
        }
        let (tx, tz) = nearby_open_waypoint(ctx, &mut rng, center);
        level_open_view(ctx, 14.0, 600).await;
        let before = ctx.position();
        let arrived = ctx.move_default(tx, tz).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-calib-path-{k}-{index}"),
            &format!("Pathfind to an open nearby point ({tx:.1},{tz:.1})."),
            json!({
                "phase": "pathfind",
                "k": k,
                "target": {"x": r2(tx), "z": r2(tz)},
                "arrived": arrived,
            }),
        );
        recover_after_segment(ctx, center, before, &format!("path-{k}")).await;
        ctx.still(PAUSE_MS).await;
    }

    // STEP 5 — random move + view-drift + jump/sprint combos, recentering after every risky segment.
    let pool = [
        WalkDirection::Forward,
        WalkDirection::ForwardLeft,
        WalkDirection::ForwardRight,
        WalkDirection::Left,
        WalkDirection::Right,
        WalkDirection::Backward,
    ];
    while has_random_window(ctx) {
        if near_obstacle(ctx)
            || ctx
                .position()
                .map(|p| ((p.0 - center.0).powi(2) + (p.2 - center.1).powi(2)).sqrt())
                .unwrap_or(f64::INFINITY)
                > OPEN_RADIUS
        {
            ctx.move_default(center.0, center.1).await;
            level_open_view(ctx, 14.0, 1400).await;
        }
        let dir = pool[(rng.next() * pool.len() as f64) as usize % pool.len()];
        let forwardish = matches!(
            dir,
            WalkDirection::Forward | WalkDirection::ForwardLeft | WalkDirection::ForwardRight
        );
        let sprint = forwardish && rng.next() < 0.5;
        let before = ctx.position();
        level_open_view(ctx, 14.0, 500).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-calib-rand-{index}"),
            "Random move + view-drift + jump/sprint combo.",
            json!({"phase": "random", "sprint": sprint}),
        );
        ctx.drag_view((rng.next() - 0.5) * 90.0, (rng.next() - 0.5) * 24.0, 350)
            .await;
        ctx.hold_visual(dir, sprint, 900, 550).await;
        recover_after_segment(ctx, center, before, "random").await;
    }

    // End from the open center with three explicit, level compositions instead of a wall close-up.
    let arrived = ctx.move_default(center.0, center.1).await;
    ctx.record(
        TYPE_MOVE,
        &format!("vis-calib-finish-center-{index}"),
        "Return to the open calibration center for final framing.",
        json!({"phase": "finish", "arrived": arrived}),
    );
    let final_direction = most_open_direction(ctx);
    for (name, distance) in [("far", 24.0), ("medium", 11.0), ("near", 4.5)] {
        level_view(ctx, final_direction, distance, 1800).await;
        ctx.record(
            TYPE_LOOK,
            &format!("vis-calib-finish-{name}-{index}"),
            &format!("Hold a level {name}-distance final view."),
            json!({"phase": "finish_view", "framing": name, "distance": distance}),
        );
        ctx.still(1100).await;
    }
}
