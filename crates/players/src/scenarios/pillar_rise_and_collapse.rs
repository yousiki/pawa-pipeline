use super::*;

/// The two trades: one rides a pillar up, the other cuts it out from under them.
const RISER: usize = 0;
const TRADES: usize = 2;

/// Draw in `[lo, hi)` from the per-bot stream, so a number this scenario could have hard-coded
/// varies per bot and per pass and still replays from the seed.
fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

/// The trade a bot works this pass. A lone bot alternates: it raises a pillar on one pass and comes
/// back to undercut it on the next, which is the same episode with one actor instead of two.
fn trade_of(index: usize, bot_count: usize, pass: usize) -> usize {
    if bot_count >= TRADES {
        index % TRADES
    } else {
        pass % TRADES
    }
}

/// Nearest peer working the riser trade this pass — the pillar worth cutting.
fn nearest_riser(ctx: &Ctx, pass: usize) -> Option<usize> {
    let own = ctx.position()?;
    let bot_count = ctx.player_count().max(1);
    (0..bot_count)
        .filter(|&j| j != ctx.index() && trade_of(j, bot_count, pass) == RISER)
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

/// Walk off the finished pillar under real controls and wait out the fall.
///
/// A pathfinder cannot route down a one-block-wide column, so a riser that never steps off spends
/// the rest of the episode stranded on top of one. Returns whether the bot actually came down.
async fn step_off(ctx: &Ctx, yaw_deg: f64) -> bool {
    let Some(start) = ctx.position() else {
        return false;
    };
    let yaw = yaw_deg.to_radians();
    let (fx, fz) = (-yaw.sin(), yaw.cos());
    ctx.look(
        (start.0 + fx * 12.0, start.1 + 1.4, start.2 + fz * 12.0),
        ctx.cfg().look_settle_ms.max(600),
    )
    .await;
    ctx.hold_visual(WalkDirection::Forward, false, 900, 0).await;
    let mut previous_y = ctx.position().map(|p| p.1).unwrap_or(start.1);
    let mut stable = 0;
    for _ in 0..80 {
        ctx.bot.wait_ticks(1).await;
        let Some(current) = ctx.position() else {
            continue;
        };
        if ctx.on_ground() && (current.1 - previous_y).abs() < 0.01 {
            stable += 1;
            if stable >= 4 {
                return current.1 < start.1 - 1.5;
            }
        } else {
            stable = 0;
        }
        previous_y = current.1;
    }
    false
}

/// Stack blocks under your own feet and ride the pillar up, then look out from the top and come
/// back down the side. Returns the world column the pillar occupies.
async fn rise(ctx: &mut Ctx, anchor: (f64, f64), pass: usize) -> Option<(i32, i32)> {
    let index = ctx.index();
    let bot_count = ctx.player_count().max(1);
    // Sites sit on a wide ring by index, so two risers are never within reach of each other's
    // column and the undercutters have room to walk in.
    let slot = index as f64 / bot_count as f64 + pass as f64 * 0.17;
    let angle = slot * std::f64::consts::TAU;
    let radius = vary(ctx, 9.0, 14.0);
    let timeout = ctx.cfg().move_timeout_ms.min(18_000);
    let arrived = ctx
        .move_to(
            anchor.0 + angle.cos() * radius,
            anchor.1 + angle.sin() * radius,
            timeout,
            2.5,
        )
        .await;
    let equipped = ctx.equip("minecraft:oak_planks").await;
    let height = vary(ctx, 3.0, 6.0).floor().max(3.0) as i64;
    let column = ctx
        .position()
        .map(|p| (p.0.floor() as i32, p.2.floor() as i32));
    let placed = if equipped {
        ctx.build_tower_underneath(height, "minecraft:oak_planks")
            .await
    } else {
        0
    };
    ctx.record(
        TYPE_PLACE,
        &format!("vis-rise-{index}-p{pass}"),
        "Stack blocks underfoot and ride the pillar up.",
        json!({"pass": pass, "trade": "riser", "arrived": arrived, "equipped": equipped,
            "expected": height, "placed": placed,
            "column": column.map(|c| json!({"x": c.0, "z": c.1}))}),
    );

    // The view from the top is the reason to build it. Sweep, then find whoever is down there.
    let span = vary(ctx, 100.0, 160.0);
    sweep(ctx, span).await;
    if ctx.player_count() > 1 {
        glance_at_nearest_peer(ctx, None).await;
    }

    let outward = vary(ctx, 0.0, 360.0);
    let landed = step_off(ctx, outward).await;
    ctx.record(
        TYPE_MOVE,
        &format!("vis-rise-down-{index}-p{pass}"),
        "Walk off the pillar top and ride the fall down.",
        json!({"pass": pass, "trade": "riser", "heading_deg": r2(outward), "landed": landed,
            "sweep_deg": r2(span)}),
    );
    column
}

/// Walk over to a standing pillar and cut the supports out from under it, then watch the drop from
/// a distance the collapse cannot reach.
async fn undercut(ctx: &mut Ctx, pass: usize, remembered: Option<(i32, i32)>) {
    let index = ctx.index();
    let equipped = ctx.equip("diamond_pickaxe").await;
    let riser = nearest_riser(ctx, pass);
    let column = riser
        .and_then(|peer| ctx.peer_position(peer))
        .map(|p| (p.0.floor() as i32, p.2.floor() as i32))
        .or(remembered);
    let Some((cx, cz)) = column else {
        // Nothing standing to cut: keep the trade honest by walking a leg and reading the ground
        // rather than idling through the pass.
        let bearing = vary(ctx, 0.0, 360.0);
        let distance = vary(ctx, 8.0, 14.0);
        let sprint = ctx.rng_next() < ctx.cfg().sprint_bout_prob;
        walk_bearing(ctx, bearing, distance, sprint).await;
        let span = vary(ctx, 120.0, 170.0);
        sweep(ctx, span).await;
        ctx.record(
            TYPE_LOOK,
            &format!("vis-undercut-search-{index}-p{pass}"),
            "No pillar standing: sweep the arena for one instead of waiting.",
            json!({"pass": pass, "trade": "undercutter", "equipped": equipped,
                "heading_deg": r2(bearing)}),
        );
        return;
    };

    // Stand just outside the column, close enough to reach its lowest cells and far enough that the
    // falling builder does not land on the digger.
    let standoff = vary(ctx, 3.0, 4.0);
    let side = vary(ctx, 0.0, std::f64::consts::TAU);
    let timeout = ctx.cfg().move_timeout_ms.min(16_000);
    let goal = (
        cx as f64 + 0.5 + side.cos() * standoff,
        cz as f64 + 0.5 + side.sin() * standoff,
    );
    let arrived = match riser {
        Some(peer) => {
            ctx.move_to_watching_peer(goal.0, goal.1, peer, timeout, 2.0)
                .await
        }
        None => ctx.move_to(goal.0, goal.1, timeout, 2.0).await,
    };

    let feet_y = ctx.position().map(|p| p.1.floor() as i32).unwrap_or(64);
    let mut attempted = 0;
    let mut dug = 0;
    for dy in [0, 1] {
        if ctx.target_met() {
            break;
        }
        let cell = BlockPos::new(cx, feet_y + dy, cz);
        if block_is_air(ctx, cell) {
            continue;
        }
        attempted += 1;
        if ctx.dig(cell).await {
            dug += 1;
        }
    }
    ctx.record(
        TYPE_MINE,
        &format!("vis-undercut-{index}-p{pass}"),
        "Cut the supports out from under the standing pillar.",
        json!({"pass": pass, "trade": "undercutter", "equipped": equipped, "riser": riser,
            "column": {"x": cx, "z": cz}, "arrived": arrived,
            "attempted": attempted, "dug": dug}),
    );

    // Back off along the line away from the column and watch it come down from the far side.
    let bearing = vary(ctx, 0.0, 360.0);
    let distance = vary(ctx, 7.0, 12.0);
    let sprint = ctx.rng_next() < ctx.cfg().sprint_bout_prob;
    walk_bearing(ctx, bearing, distance, sprint).await;
    match riser {
        Some(peer) => {
            let settle = ctx.cfg().look_settle_ms.max(800);
            look_at_peer(ctx, peer, settle).await;
            sleep_ms(vary(ctx, 1_200.0, 2_400.0) as u64).await;
        }
        None => {
            let span = vary(ctx, 120.0, 170.0);
            sweep(ctx, span).await;
        }
    }
}

/// One crew stacks blocks under its own feet and rides the pillar up; the other walks over, cuts
/// the supports out from under them, and both watch the drop from opposite sides.
pub(crate) async fn run(ctx: &mut Ctx) {
    let index = ctx.index();
    let bot_count = ctx.player_count().max(1);
    let anchor = ctx.anchor();
    let mut last_pillar: Option<(i32, i32)> = None;
    let mut pass = 0usize;
    loop {
        let trade = trade_of(index, bot_count, pass);
        ctx.record(
            TYPE_LOOK,
            &format!("vis-pillar-pass-{index}-p{pass}"),
            &format!(
                "Pass {pass}: bot {index} works as {}.",
                if trade == RISER {
                    "the riser"
                } else {
                    "the undercutter"
                }
            ),
            json!({"pass": pass, "trade": if trade == RISER { "riser" } else { "undercutter" },
                "bot_count": bot_count}),
        );
        if trade == RISER {
            if let Some(column) = rise(ctx, anchor, pass).await {
                last_pillar = Some(column);
            }
        } else {
            undercut(ctx, pass, last_pillar).await;
            last_pillar = None;
        }
        if ctx.target_met() {
            break;
        }
        pass += 1;
    }
}
