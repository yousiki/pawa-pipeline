use super::*;

/// Crews in the relay: one steps in and stacks its column while the other two hold back and watch
/// it work, then the turn passes along the line.
const CREWS: usize = 3;

/// One material per crew, so the finished wall reads as three distinct hands.
const CREW_MATERIAL: [&str; CREWS] = [
    "minecraft:oak_planks",
    "minecraft:stone_bricks",
    "minecraft:cobblestone",
];

/// Draw in `[lo, hi)` from the per-bot stream, so a number this scenario could have hard-coded
/// varies per bot and per pass and still replays from the seed.
fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

/// The crew a bot works this pass. Below a full crew the assignment rotates every pass, so a
/// short-handed swarm still raises all three columns instead of leaving two of them unbuilt.
fn crew_of(index: usize, bot_count: usize, pass: usize) -> usize {
    if bot_count >= CREWS {
        index % CREWS
    } else {
        (index + pass) % CREWS
    }
}

/// Station for one column. Crews stand 4 blocks apart inside a lane and every additional full crew
/// takes its own 12-block lane, so two bots never share a column whatever the swarm size.
fn column_station(anchor: (f64, f64), crew: usize, lane: usize, wall_z: f64) -> (f64, f64) {
    (
        anchor.0 + (crew as f64 - 1.0) * 4.0 + lane as f64 * 12.0,
        wall_z,
    )
}

/// Nearest peer working `crew` this pass — the bot the watchers are meant to be watching.
fn nearest_in_crew(ctx: &Ctx, crew: usize, pass: usize) -> Option<usize> {
    let own = ctx.position()?;
    let bot_count = ctx.player_count().max(1);
    (0..bot_count)
        .filter(|&j| j != ctx.index() && crew_of(j, bot_count, pass) == crew)
        .filter_map(|j| {
            ctx.peer_position(j)
                .map(|p| (j, (p.0 - own.0).hypot(p.2 - own.2)))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(j, _)| j)
}

/// Triangle camera sweep of `deg` total: out to one edge, across to the other, back to centre.
async fn sweep(ctx: &Ctx, deg: f64) {
    let settle = ((deg.abs() / ctx.cfg().yaw_rate_deg_s.max(1.0) * 1000.0) as u64 + 600).min(4_000);
    for leg in [deg * 0.5, -deg, deg * 0.5] {
        ctx.drag_view(leg, 0.0, settle).await;
    }
}

/// Face a world bearing (Minecraft yaw: 0° is +Z, 90° is -X) and hold forward long enough to cover
/// `blocks`. Open-loop on purpose: no pathfinder goal, so the recorded key bits stay independent of
/// the navigator.
async fn walk_bearing(ctx: &Ctx, yaw_deg: f64, blocks: f64, sprint: bool) {
    let Some(p) = ctx.position() else { return };
    let yaw = yaw_deg.to_radians();
    let (fx, fz) = (-yaw.sin(), yaw.cos());
    ctx.look(
        (p.0 + fx * 32.0, p.1 + 1.62, p.2 + fz * 32.0),
        ctx.cfg().look_settle_ms.max(500),
    )
    .await;
    let speed = if sprint { 5.6 } else { 4.3 };
    let hold = (((blocks / speed) * 1000.0) as u64).clamp(500, 12_000);
    ctx.hold_visual(WalkDirection::Forward, sprint, hold, 0)
        .await;
}

/// This crew's turn: step in to its own column of the shared wall, stack it, then back off down the
/// line so the next crew has a clear approach.
async fn build_column(
    ctx: &mut Ctx,
    anchor: (f64, f64),
    crew: usize,
    lane: usize,
    wall_z: f64,
    pass: usize,
) {
    let index = ctx.index();
    let (sx, sz) = column_station(anchor, crew, lane, wall_z);
    // Stand back from the column so the stack goes up in front of the bot, not inside its hitbox.
    let standoff = vary(ctx, 2.0, 3.0);
    let timeout = ctx.cfg().move_timeout_ms.min(16_000);
    let arrived = ctx.move_to(sx, sz - standoff, timeout, 2.0).await;
    let material = CREW_MATERIAL[crew];
    let equipped = ctx.equip(material).await;
    ctx.look(
        (sx, ctx.position().map(|p| p.1).unwrap_or(64.0) + 1.0, sz),
        600,
    )
    .await;

    let height = vary(ctx, 2.0, 4.0).floor().max(2.0) as i64;
    let feet_y = ctx.position().map(|p| p.1.floor() as i64).unwrap_or(64);
    let base = marker_base_near(ctx, (sx.floor() as i64, feet_y, sz.floor() as i64), height);
    let blocks: Vec<((i64, i64, i64), String)> = base
        .map(|b| {
            (0..height)
                .map(|dy| {
                    (
                        (b.x as i64, b.y as i64 + dy, b.z as i64),
                        material.to_string(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    let (placed, attempted) = ctx
        .build_blocks(&blocks, |i, _target, _material| {
            (
                format!("vis-relay-place-{index}-p{pass}-c{crew}-{i}"),
                format!("Stack column layer {} of the shared wall.", i + 1),
                json!({"crew": crew, "layer": i, "pass": pass}),
            )
        })
        .await;
    ctx.record(
        TYPE_PLACE,
        &format!("vis-relay-column-{index}-p{pass}"),
        &format!("Crew {crew} raises its column of the shared wall."),
        json!({"crew": crew, "lane": lane, "pass": pass, "material": material,
            "station": {"x": r2(sx), "z": r2(sz)}, "arrived": arrived, "equipped": equipped,
            "height": height, "placed": placed, "attempted": attempted,
            "footprint_found": base.is_some()}),
    );

    let bearing = vary(ctx, 160.0, 200.0);
    let distance = vary(ctx, 7.0, 11.0);
    let sprint = ctx.rng_next() < ctx.cfg().sprint_bout_prob;
    walk_bearing(ctx, bearing, distance, sprint).await;
    ctx.record(
        TYPE_MOVE,
        &format!("vis-relay-handoff-{index}-p{pass}"),
        &format!("Crew {crew} backs off the wall and hands the turn on."),
        json!({"crew": crew, "pass": pass, "heading_deg": r2(bearing),
            "distance": r2(distance), "sprint": sprint}),
    );
}

/// Not this crew's turn: take a seat on the observation arc behind the working column and keep the
/// crew that IS working in frame. Watchers travel every turn, so no crew stands still in the loop.
async fn watch_turn(ctx: &mut Ctx, anchor: (f64, f64), turn: usize, wall_z: f64, pass: usize) {
    let index = ctx.index();
    let bot_count = ctx.player_count().max(1);
    let (cx, cz) = column_station(anchor, turn, 0, wall_z);
    // Seats spread over a 150° arc at 9–13 blocks, which keeps adjacent watchers more than four
    // blocks apart even at sixteen bots.
    let slot = (index as f64 + 0.5) / bot_count as f64;
    let angle = (slot - 0.5) * 150.0_f64.to_radians();
    let radius = vary(ctx, 9.0, 13.0);
    let seat_x = cx + angle.sin() * radius;
    let seat_z = cz - angle.cos() * radius;
    let timeout = ctx.cfg().move_timeout_ms.min(14_000);
    let arrived = ctx.move_to(seat_x, seat_z, timeout, 2.5).await;

    let watched = nearest_in_crew(ctx, turn, pass);
    match watched {
        Some(peer) => {
            let settle = ctx.cfg().look_settle_ms.max(700);
            look_at_peer(ctx, peer, settle).await;
            sleep_ms(vary(ctx, 1_200.0, 2_400.0) as u64).await;
        }
        None => {
            // Nobody is working this column — this is the short-crewed case. Read the wall itself
            // rather than staring at an empty station.
            let span = vary(ctx, 90.0, 150.0);
            sweep(ctx, span).await;
        }
    }
    ctx.record(
        TYPE_LOOK,
        &format!("vis-relay-watch-{index}-p{pass}-t{turn}"),
        &format!("Hold back and watch crew {turn} work its column."),
        json!({"watching_crew": turn, "pass": pass, "watched_peer": watched,
            "seat": {"x": r2(seat_x), "z": r2(seat_z)}, "arrived": arrived}),
    );
}

/// Three crews raise one shared wall a column at a time: while one steps in and stacks its column
/// the others hold back on the observation arc and watch it work, then the turn passes along.
pub(crate) async fn run(ctx: &mut Ctx) {
    let index = ctx.index();
    let bot_count = ctx.player_count().max(1);
    let anchor = ctx.anchor();
    let lane = index / CREWS;
    let wall_z = anchor.1 + 6.0 + vary(ctx, -1.5, 1.5);
    let mut pass = 0usize;
    loop {
        let crew = crew_of(index, bot_count, pass);
        ctx.record(
            TYPE_LOOK,
            &format!("vis-relay-pass-{index}-p{pass}"),
            &format!("Relay pass {pass}: bot {index} works the wall as crew {crew}."),
            json!({"pass": pass, "crew": crew, "lane": lane, "crews": CREWS,
                "wall_z": r2(wall_z), "bot_count": bot_count}),
        );
        for turn in 0..CREWS {
            if ctx.target_met() {
                break;
            }
            if turn == crew {
                build_column(ctx, anchor, crew, lane, wall_z, pass).await;
            } else {
                watch_turn(ctx, anchor, turn, wall_z, pass).await;
            }
        }
        if ctx.target_met() {
            break;
        }
        pass += 1;
    }
}
