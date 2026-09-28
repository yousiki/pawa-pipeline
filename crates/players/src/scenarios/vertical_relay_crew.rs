use super::*;

/// The four trades working the site at once.
const TRADES: usize = 4;
const TOWER_BUILDER: usize = 0;
const SUPPORT_REMOVER: usize = 1;
const SHAFT_MINER: usize = 2;

/// Irregularly spaced sites, one per trade, as anchor offsets. Deliberately not a ring: the four
/// trades should read as four separate jobs, not as one formation.
const SITE: [(f64, f64); TRADES] = [(-11.0, -7.0), (10.0, 6.0), (-8.0, 12.0), (13.0, -10.0)];

/// Draw in `[lo, hi)` from the per-bot stream, so a number this scenario could have hard-coded
/// varies per bot and per pass and still replays from the seed.
fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

/// The trade a bot works this pass. Below a full crew the trades rotate every pass, so a
/// short-handed swarm still shows all four jobs over the episode instead of only the first ones.
fn trade_of(index: usize, bot_count: usize, pass: usize) -> usize {
    if bot_count >= TRADES {
        index % TRADES
    } else {
        (index + pass) % TRADES
    }
}

/// Nearest peer working `trade` this pass.
fn nearest_in_trade(ctx: &Ctx, trade: usize, pass: usize) -> Option<usize> {
    let own = ctx.position()?;
    let bot_count = ctx.player_count().max(1);
    (0..bot_count)
        .filter(|&j| j != ctx.index() && trade_of(j, bot_count, pass) == trade)
        .filter_map(|j| {
            ctx.peer_position(j)
                .map(|p| (j, (p.0 - own.0).hypot(p.2 - own.2)))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(j, _)| j)
}

/// The farthest peer whose position has replicated — the other side of a site this wide.
fn farthest_peer(ctx: &Ctx) -> Option<usize> {
    let own = ctx.position()?;
    (0..ctx.player_count())
        .filter(|&j| j != ctx.index())
        .filter_map(|j| {
            ctx.peer_position(j)
                .map(|p| (j, (p.0 - own.0).hypot(p.2 - own.2)))
        })
        .max_by(|a, b| a.1.total_cmp(&b.1))
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

/// Walk off a finished tower under real controls and wait out the fall. A pathfinder cannot route
/// down a one-block-wide column, so a builder that never steps off is stranded for the episode.
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

/// Move onto this trade's site, jittered per bot so two bots of one trade never share a station.
async fn take_site(ctx: &mut Ctx, anchor: (f64, f64), trade: usize, lane: f64) -> bool {
    let (sx, sz) = SITE[trade];
    let jx = vary(ctx, -3.0, 3.0);
    let jz = vary(ctx, -3.0, 3.0);
    let timeout = ctx.cfg().move_timeout_ms.min(18_000);
    ctx.move_to(anchor.0 + sx + jx + lane, anchor.1 + sz + jz, timeout, 2.5)
        .await
}

/// Stack a log tower under your own feet, read the site from the top, and come back down its side.
async fn build_tower(ctx: &mut Ctx, pass: usize) {
    let index = ctx.index();
    let equipped = ctx.equip("minecraft:oak_log").await;
    let height = vary(ctx, 4.0, 7.0).floor().max(4.0) as i64;
    let placed = if equipped {
        ctx.build_tower_underneath(height, "minecraft:oak_log")
            .await
    } else {
        0
    };
    let span = vary(ctx, 80.0, 130.0);
    sweep(ctx, span).await;
    if let Some(peer) = farthest_peer(ctx) {
        let settle = ctx.cfg().look_settle_ms.max(700);
        look_at_peer(ctx, peer, settle).await;
    }
    let outward = vary(ctx, 0.0, 360.0);
    let landed = step_off(ctx, outward).await;
    ctx.record(
        TYPE_PLACE,
        &format!("vis-relay-tower-{index}-p{pass}"),
        "Stack the log tower underfoot, look out from the top, and step off it.",
        json!({"pass": pass, "trade": "tower_builder", "equipped": equipped, "expected": height,
            "placed": placed, "landed": landed, "heading_deg": r2(outward)}),
    );
}

/// Shadow the tower builder and take its lowest supports out once it is up.
async fn remove_supports(ctx: &mut Ctx, pass: usize) {
    let index = ctx.index();
    let equipped = ctx.equip("diamond_pickaxe").await;
    let builder = nearest_in_trade(ctx, TOWER_BUILDER, pass);
    let standoff = vary(ctx, 4.0, 5.5);
    // Follow the builder at a standoff for a few slices, so the removal reads as a decision rather
    // than as two bots that happened to be in one place.
    let mut legs = 0u32;
    if let Some(peer) = builder {
        // Arc around the builder rather than stepping straight at it: an approach leg walks
        // toward exactly what the camera is tracking, which scored 98% camera/travel agreement.
        // A tangential leg keeps the same standoff while the body and the gaze diverge.
        let spin = if ctx.rng_next() < 0.5 { 1.0 } else { -1.0 };
        for _ in 0..3 {
            if ctx.target_met() {
                break;
            }
            let Some(target) = ctx.peer_position(peer) else {
                break;
            };
            let own = ctx.position();
            let mut bearing = match own {
                Some(p) => (p.2 - target.2).atan2(p.0 - target.0),
                None => 0.0,
            };
            bearing += spin * vary(ctx, 0.6, 1.0);
            ctx.move_to_watching_peer(
                target.0 + bearing.cos() * standoff,
                target.2 + bearing.sin() * standoff,
                peer,
                2_500,
                1.5,
            )
            .await;
            legs += 1;
        }
    }

    let column = builder
        .and_then(|peer| ctx.peer_position(peer))
        .map(|p| (p.0.floor() as i32, p.2.floor() as i32));
    let feet_y = ctx.position().map(|p| p.1.floor() as i32).unwrap_or(64);
    let mut attempted = 0u64;
    let mut dug = 0u64;
    if let Some((cx, cz)) = column {
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
    }
    ctx.record(
        TYPE_MINE,
        &format!("vis-relay-cut-{index}-p{pass}"),
        "Shadow the tower builder and cut its lowest supports away.",
        json!({"pass": pass, "trade": "support_remover", "equipped": equipped, "builder": builder,
            "follow_legs": legs, "attempted": attempted, "dug": dug,
            "column": column.map(|c| json!({"x": c.0, "z": c.1}))}),
    );

    let span = vary(ctx, 120.0, 160.0);
    sweep(ctx, span).await;
    let bearing = vary(ctx, 215.0, 255.0);
    let distance = vary(ctx, 12.0, 18.0);
    let sprint = ctx.rng_next() < ctx.cfg().sprint_bout_prob;
    walk_bearing(ctx, bearing, distance, sprint).await;
}

/// Sink a shaft straight down, then patch it back up in stone — which is also how the miner gets
/// out again, so the trade can never end the pass standing at the bottom of its own hole.
async fn sink_and_patch(ctx: &mut Ctx, pass: usize) {
    let index = ctx.index();
    let equipped_tool = ctx.equip("diamond_pickaxe").await;
    let depth = vary(ctx, 2.0, 4.0).floor().max(2.0) as i64;
    let mut sunk = 0i64;
    for _ in 0..depth {
        if ctx.target_met() {
            break;
        }
        let Some(p) = ctx.position() else { break };
        let cell = BlockPos::new(
            p.0.floor() as i32,
            p.1.floor() as i32 - 1,
            p.2.floor() as i32,
        );
        if block_is_air(ctx, cell) {
            break;
        }
        if !ctx.dig(cell).await {
            break;
        }
        sunk += 1;
        // Let the fall into the cleared cell finish before the next face is chosen from live pose.
        sleep_ms(500).await;
    }
    let span = vary(ctx, 140.0, 175.0);
    sweep(ctx, span).await;

    let equipped_block = ctx.equip("minecraft:stone").await;
    let patched = if equipped_block && sunk > 0 {
        ctx.build_tower_underneath(sunk, "minecraft:stone").await
    } else {
        0
    };
    ctx.record(
        TYPE_MINE,
        &format!("vis-relay-shaft-{index}-p{pass}"),
        "Sink a shaft, then patch it back up in stone and climb out on the patch.",
        json!({"pass": pass, "trade": "shaft_miner", "equipped_tool": equipped_tool,
            "equipped_block": equipped_block, "depth": depth, "sunk": sunk,
            "patched": patched, "escaped": patched >= sunk}),
    );
    if ctx.player_count() > 1 {
        glance_at_nearest_peer(ctx, None).await;
    }
}

/// Roam wide between the working sites, circling one of them so the crew is seen from outside.
async fn roam(ctx: &mut Ctx, anchor: (f64, f64), pass: usize) {
    let index = ctx.index();
    let bot_count = ctx.player_count().max(1);
    // A wide random point on this bot's own sector, so scouts of one swarm never converge.
    let sector = (index as f64 + 0.5) / bot_count as f64 * std::f64::consts::TAU;
    let bearing = sector + vary(ctx, -0.6, 0.6) + pass as f64 * 0.7;
    let radius = vary(ctx, 12.0, 19.0);
    let timeout = ctx.cfg().move_timeout_ms.min(22_000);
    let arrived = ctx
        .move_to(
            anchor.0 + bearing.cos() * radius,
            anchor.1 + bearing.sin() * radius,
            timeout,
            3.5,
        )
        .await;
    let span = vary(ctx, 150.0, 180.0);
    sweep(ctx, span).await;

    // Circle whichever trade is working the shaft, in fine steps so it reads as an orbit.
    let peer = nearest_in_trade(ctx, SHAFT_MINER, pass).or_else(|| farthest_peer(ctx));
    let orbit_radius = vary(ctx, 7.0, 10.0);
    let direction = if ctx.rng_next() < 0.5 { 1.0 } else { -1.0 };
    let mut legs = 0u32;
    if let Some(peer) = peer {
        let mut angle = match (ctx.position(), ctx.peer_position(peer)) {
            (Some(own), Some(target)) => (own.2 - target.2).atan2(own.0 - target.0),
            _ => 0.0,
        };
        for _ in 0..6 {
            if ctx.target_met() {
                break;
            }
            let Some(target) = ctx.peer_position(peer) else {
                break;
            };
            angle += direction * 30.0_f64.to_radians();
            ctx.move_to_watching_peer(
                target.0 + angle.cos() * orbit_radius,
                target.2 + angle.sin() * orbit_radius,
                peer,
                2_500,
                1.5,
            )
            .await;
            legs += 1;
        }
    }
    ctx.record(
        TYPE_MOVE,
        &format!("vis-relay-scout-{index}-p{pass}"),
        "Roam wide between the sites and circle one of the working trades.",
        json!({"pass": pass, "trade": "roaming_scout", "arrived": arrived,
            "radius": r2(radius), "orbit_peer": peer, "orbit_radius": r2(orbit_radius),
            "orbit_legs": legs, "direction": direction}),
    );
    if let Some(peer) = farthest_peer(ctx) {
        let settle = ctx.cfg().look_settle_ms.max(800);
        look_at_peer(ctx, peer, settle).await;
    }
    let bearing = vary(ctx, 105.0, 145.0);
    let distance = vary(ctx, 14.0, 20.0);
    let sprint = ctx.rng_next() < ctx.cfg().sprint_bout_prob;
    walk_bearing(ctx, bearing, distance, sprint).await;
}

/// Four trades work irregularly spaced sites at once — one stacks a tower, one cuts its supports
/// away, one sinks a shaft and patches it, and a scout roams wide between them all.
pub(crate) async fn run(ctx: &mut Ctx) {
    let index = ctx.index();
    let bot_count = ctx.player_count().max(1);
    let anchor = ctx.anchor();
    // Every additional full crew works its own set of sites twenty blocks along.
    let lane = (index / TRADES) as f64 * 20.0;
    let mut pass = 0usize;
    loop {
        let trade = trade_of(index, bot_count, pass);
        let arrived = take_site(ctx, anchor, trade, lane).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-relay-site-{index}-p{pass}"),
            &format!("Pass {pass}: bot {index} takes the site for trade {trade}."),
            json!({"pass": pass, "trade": trade, "lane": r2(lane), "arrived": arrived,
                "bot_count": bot_count}),
        );
        match trade {
            TOWER_BUILDER => build_tower(ctx, pass).await,
            SUPPORT_REMOVER => remove_supports(ctx, pass).await,
            SHAFT_MINER => sink_and_patch(ctx, pass).await,
            _ => roam(ctx, anchor, pass).await,
        }
        witness_peer_edits(ctx).await;
        if ctx.target_met() {
            break;
        }
        pass += 1;
    }
}
