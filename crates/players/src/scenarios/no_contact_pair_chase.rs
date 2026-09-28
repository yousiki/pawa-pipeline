use super::*;

/// How far a runner is allowed to draw its next open-ground goal from the anchor. Without it a
/// curving flight leaves the combat arena and the chase stops being recordable. 32 blocks was wide
/// enough to reach the ravines and cave mouths outside the coordinator's prepared arena, which is
/// where the flights that ended in black frames went.
const ARENA_LEASH: f64 = 26.0;

const WALK_BPS: f64 = 4.317;
const SPRINT_BPS: f64 = 5.612;

/// Sampling stride when probing the ground ahead of a blind run. Coarser than a block, but a bot
/// cannot fall through a hole narrower than itself, and it is also the shortest leg worth walking.
const PROBE: f64 = 1.5;

fn rand_range(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

/// This bot's opposite number. Pairs are adjacent local indices, so a swarm of six is three chases
/// rather than five bots converging on bot 0. An odd bot out doubles up on the last pair.
fn partner(index: usize, bot_count: usize) -> Option<usize> {
    if bot_count < 2 {
        return None;
    }
    let mate = if index.is_multiple_of(2) {
        index + 1
    } else {
        index - 1
    };
    Some(if mate < bot_count {
        mate
    } else {
        index.saturating_sub(1)
    })
}

/// Clamp a goal into the arena around `anchor`.
fn leashed(goal: (f64, f64), anchor: (f64, f64)) -> (f64, f64) {
    let out = (goal.0 - anchor.0).hypot(goal.1 - anchor.1);
    if out <= ARENA_LEASH {
        return goal;
    }
    (
        anchor.0 + (goal.0 - anchor.0) * ARENA_LEASH / out,
        anchor.1 + (goal.1 - anchor.1) * ARENA_LEASH / out,
    )
}

/// Snap a drawn goal onto a cell the bot could actually stand on, or reject it outright.
fn standable(ctx: &Ctx, goal: (f64, f64)) -> Option<(f64, f64)> {
    let feet_y = ctx.position()?.1.floor() as i32;
    walkable_xz_near(ctx, goal, feet_y)
}

/// How far along `heading_deg` the bot can run before the ground stops holding it up. A blind
/// `hold_visual` leg never consults the pathfinder, so a cave mouth four blocks ahead is a fall into
/// the dark — which is how this scenario spent the back half of an episode on black frames. One
/// block of drop is a normal slope and stays allowed; deeper is a pit.
fn safe_run_distance(ctx: &Ctx, heading_deg: f64, want: f64) -> f64 {
    let Some(p) = ctx.position() else { return 0.0 };
    let yaw = heading_deg.to_radians();
    let feet_y = p.1.floor() as i32;
    let mut safe = 0.0;
    while safe + PROBE <= want {
        let step = safe + PROBE;
        let cell = BlockPos::new(
            (p.0 - yaw.sin() * step).floor() as i32,
            feet_y,
            (p.2 + yaw.cos() * step).floor() as i32,
        );
        if !block_is_safe_support(ctx, cell.down(1)) && !block_is_safe_support(ctx, cell.down(2)) {
            break;
        }
        safe = step;
    }
    safe
}

async fn walk_heading(ctx: &Ctx, heading_deg: f64, distance: f64, sprint: bool) -> u64 {
    let distance = safe_run_distance(ctx, heading_deg, distance);
    if distance < PROBE {
        return 0;
    }
    let speed = if sprint { SPRINT_BPS } else { WALK_BPS };
    let hold = ((distance / speed * 1000.0) as u64).clamp(500, 10_000);
    if let Some(p) = ctx.position() {
        let yaw = heading_deg.to_radians();
        let settle = ctx.cfg().look_settle_ms.max(500);
        ctx.look(
            (p.0 - yaw.sin() * 32.0, p.1 + 1.62, p.2 + yaw.cos() * 32.0),
            settle,
        )
        .await;
    }
    ctx.hold_visual(WalkDirection::Forward, sprint, hold, 0)
        .await;
    hold
}

async fn sweep(ctx: &Ctx, span_deg: f64, pitch_deg: f64) {
    let rate = ctx.cfg().yaw_rate_deg_s.max(1.0);
    for (yaw, pitch) in [
        (span_deg * 0.5, pitch_deg),
        (-span_deg, -pitch_deg),
        (span_deg * 0.5, 0.0),
    ] {
        if ctx.target_met() {
            return;
        }
        let settle = (yaw.abs() / rate * 1000.0) as u64 + 700;
        ctx.drag_view(yaw, pitch, settle).await;
    }
}

/// Circle a peer at arm's length in ~20° slices, facing it the whole way.
async fn orbit(ctx: &Ctx, peer: usize, radius: f64, arc_deg: f64, direction: f64) -> u32 {
    let slices = (arc_deg / 20.0).ceil().max(1.0) as u32;
    let step = (arc_deg / slices as f64).to_radians() * direction;
    let mut angle = match (ctx.position(), ctx.peer_position(peer)) {
        (Some(own), Some(p)) => (own.2 - p.2).atan2(own.0 - p.0),
        _ => 0.0,
    };
    let mut legs = 0;
    for _ in 0..slices {
        if ctx.target_met() {
            break;
        }
        let Some(p) = ctx.peer_position(peer) else {
            look_at_peer(ctx, peer, 250).await;
            sleep_ms(200).await;
            continue;
        };
        angle += step;
        ctx.move_to_watching_peer(
            p.0 + angle.cos() * radius,
            p.2 + angle.sin() * radius,
            peer,
            2_500,
            1.5,
        )
        .await;
        legs += 1;
    }
    legs
}

/// Close on the mate and swing at melee range under the real attack cooldown, for at most
/// `swings` hits or `hold_ms` of wall clock. Returns `(swings issued, swings that reached a tracked
/// entity)` — the same honest split the pvp scenario records.
async fn press_attack(ctx: &Ctx, mate: usize, swings: u64, hold_ms: u64) -> (u64, u64) {
    let deadline = Instant::now() + Duration::from_millis(hold_ms);
    let username = ctx.peer_username(mate).unwrap_or_default();
    let mut issued = 0u64;
    let mut real = 0u64;
    let mut last = Instant::now() - Duration::from_millis(ATTACK_COOLDOWN_MS);
    while issued < swings && Instant::now() < deadline && !ctx.target_met() {
        let (Some(own), Some(peer)) = (ctx.position(), ctx.peer_position(mate)) else {
            sleep_ms(COMBAT_TICK_MS).await;
            continue;
        };
        if (peer.0 - own.0).hypot(peer.2 - own.2) > MELEE_RANGE {
            let slice = deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .min(2_500) as u64;
            ctx.move_to(peer.0, peer.2, slice.max(400), (MELEE_RANGE - 1.0).max(1.5))
                .await;
            look_at_peer(ctx, mate, 120).await;
            continue;
        }
        look_at_peer(ctx, mate, 120).await;
        if last.elapsed() < Duration::from_millis(ATTACK_COOLDOWN_MS) {
            sleep_ms(COMBAT_TICK_MS).await;
            continue;
        }
        real += u64::from(ctx.attack_peer(&username));
        ctx.set_pending("attack");
        issued += 1;
        last = Instant::now();
    }
    (issued, real)
}

/// Keep away from the mate for `hold_ms`, glancing back so the pursuer stays in frame instead of
/// running blind.
async fn shake_off(ctx: &Ctx, mate: usize, keep: f64, hold_ms: u64) -> u32 {
    let deadline = Instant::now() + Duration::from_millis(hold_ms);
    let mut legs = 0;
    while Instant::now() < deadline && !ctx.target_met() {
        let Some(peer) = ctx.peer_position(mate) else {
            sleep_ms(250).await;
            continue;
        };
        let own = ctx.position();
        let gap = own.map(|p| (peer.0 - p.0).hypot(peer.2 - p.2));
        if gap.is_some_and(|d| d >= keep) {
            look_at_peer(ctx, mate, 300).await;
            sleep_ms(200).await;
            continue;
        }
        let (ux, uz) = away_unit(own, peer);
        let goal = match own {
            Some(p) => (p.0 + ux * keep, p.2 + uz * keep),
            None => (peer.0 + ux * keep, peer.2 + uz * keep),
        };
        let slice = deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            .min(2_500) as u64;
        ctx.move_to(goal.0, goal.1, slice.max(400), 1.5).await;
        look_at_peer(ctx, mate, 250).await;
        legs += 1;
    }
    legs
}

/// Runners break for open ground on a curving route while their chasers close, circle them at arm's
/// length, land a few swings, and get shaken off again — the one script where players are
/// deliberately allowed inside each other's space.
///
/// Roles are fixed per bot (even indices run, odd indices chase) and paired on adjacent local
/// indices, so a larger swarm is several independent chases rather than one mob. The curve in the
/// runner's flight comes from drawing each goal at a bearing that is *away from the chaser plus a
/// random 40–80°* rather than straight away: the route bends without ever heading back into the
/// chaser. Every distance, arc, swing count and hold is a per-round draw.
pub(crate) async fn run(ctx: &mut Ctx) {
    let index = ctx.index();
    let bot_count = ctx.player_count().max(1);
    let anchor = ctx.anchor();
    let running = index.is_multiple_of(2);
    let role = if running { "runner" } else { "chaser" };
    let mate = partner(index, bot_count);
    let sprint_prob = ctx.cfg().sprint_bout_prob;
    let move_timeout = ctx.cfg().move_timeout_ms.min(14_000);
    // The curve has a handedness, drawn once, so a runner's flight reads as one continuous arc
    // rather than a random walk.
    let curl = if ctx.rng_next() < 0.5 { 1.0 } else { -1.0 };

    let armed = ctx.equip("diamond_sword").await;
    ctx.record(
        TYPE_LOOK,
        &format!("vis-chase-init-{index}"),
        &format!("No-contact pair chase: bot {index} is the {role}."),
        json!({"role": role, "mate": mate, "armed": armed, "curl": curl}),
    );

    let Some(mate) = mate else {
        // Nothing to chase in a swarm of one: patrol the arena instead of standing in it.
        while !ctx.target_met() {
            let heading = ctx.rng_next() * 360.0;
            let distance = rand_range(ctx, 8.0, 18.0);
            let sprint = ctx.rng_next() < sprint_prob;
            walk_heading(ctx, heading, distance, sprint).await;
            let span = rand_range(ctx, 60.0, 170.0);
            sweep(ctx, span, 0.0).await;
        }
        return;
    };

    let mut round = 0u32;
    while !ctx.target_met() {
        if running {
            // Break for open ground on a bearing that curves away from the chaser.
            let away = match (ctx.position(), ctx.peer_position(mate)) {
                (Some(own), Some(peer)) => (-(own.0 - peer.0)).atan2(own.2 - peer.2).to_degrees(),
                _ => ctx.rng_next() * 360.0,
            };
            // Redraw the bend until the goal lands on ground the runner can stand on. "Open ground"
            // used to mean "whatever the first draw picked", and inside this arena that includes cave
            // mouths and ravine floors — a flight into one blacks out the rest of the episode.
            let mut bend = 0.0;
            let mut goal = None;
            for _ in 0..4 {
                bend = curl * rand_range(ctx, 40.0, 80.0);
                let distance = rand_range(ctx, 10.0, 20.0);
                let yaw = (away + bend).to_radians();
                let drawn = leashed(
                    match ctx.position() {
                        Some(p) => (p.0 - yaw.sin() * distance, p.2 + yaw.cos() * distance),
                        None => anchor,
                    },
                    anchor,
                );
                goal = standable(ctx, drawn);
                if goal.is_some() {
                    break;
                }
            }
            // Every bend was over a hole: fall back on the anchor, which the coordinator prepared.
            let goal = goal.unwrap_or(anchor);
            let arrived = ctx.move_to(goal.0, goal.1, move_timeout, 3.0).await;
            ctx.record(
                TYPE_MOVE,
                &format!("vis-chase-break-{index}-{round}"),
                "Break for open ground on the curving route.",
                json!({"role": role, "round": round, "mate": mate, "arrived": arrived,
                    "bearing_deg": r2(away + bend), "bend_deg": r2(bend),
                    "goal": {"x": r2(goal.0), "z": r2(goal.1)}}),
            );
            if ctx.target_met() {
                break;
            }
            // Then one of: glance back, shake the chaser off, or sprint a free leg.
            match (ctx.rng_next() * 3.0) as u32 {
                0 => {
                    let span = rand_range(ctx, 90.0, 180.0);
                    let pitch = rand_range(ctx, -12.0, 12.0);
                    sweep(ctx, span, pitch).await;
                    ctx.record(
                        TYPE_LOOK,
                        &format!("vis-chase-glance-{index}-{round}"),
                        "Sweep the open ground behind the flight.",
                        json!({"role": role, "round": round, "sweep_deg": r2(span)}),
                    );
                }
                1 => {
                    let keep = rand_range(ctx, 9.0, 15.0);
                    let hold = rand_range(ctx, 8_000.0, 15_000.0) as u64;
                    let legs = shake_off(ctx, mate, keep, hold).await;
                    ctx.record(
                        TYPE_MOVE,
                        &format!("vis-chase-shake-{index}-{round}"),
                        &format!("Shake bot {mate} off to {} blocks.", r2(keep)),
                        json!({"role": role, "round": round, "mate": mate,
                            "keep_distance": r2(keep), "legs": legs, "hold_ms": hold}),
                    );
                }
                _ => {
                    let heading = ctx.rng_next() * 360.0;
                    let distance = rand_range(ctx, 12.0, 22.0);
                    let sprint = ctx.rng_next() < sprint_prob;
                    let hold = walk_heading(ctx, heading, distance, sprint).await;
                    ctx.record(
                        TYPE_MOVE,
                        &format!("vis-chase-run-{index}-{round}"),
                        "Run one free leg across the arena.",
                        json!({"role": role, "round": round, "heading_deg": r2(heading),
                            "requested_distance": r2(distance), "sprint": sprint,
                            "hold_ms": hold}),
                    );
                }
            }
        } else {
            // Close, circle at arm's length, land a few swings, then reposition.
            let standoff = rand_range(ctx, MIN_PEER_GAP, 6.5);
            let close_hold = rand_range(ctx, 8_000.0, 16_000.0) as u64;
            let deadline = Instant::now() + Duration::from_millis(close_hold);
            let mut legs = 0u32;
            while Instant::now() < deadline && !ctx.target_met() {
                let Some(peer) = ctx.peer_position(mate) else {
                    look_at_peer(ctx, mate, 250).await;
                    sleep_ms(250).await;
                    continue;
                };
                let (ux, uz) = away_unit(ctx.position(), peer);
                let slice = deadline
                    .saturating_duration_since(Instant::now())
                    .as_millis()
                    .min(2_500) as u64;
                ctx.move_to_watching_peer(
                    peer.0 + ux * standoff,
                    peer.2 + uz * standoff,
                    mate,
                    slice.max(400),
                    1.5,
                )
                .await;
                legs += 1;
            }
            ctx.record(
                TYPE_MOVE,
                &format!("vis-chase-close-{index}-{round}"),
                &format!("Close on bot {mate} and hold {} blocks off.", r2(standoff)),
                json!({"role": role, "round": round, "mate": mate, "legs": legs,
                    "standoff": r2(standoff), "hold_ms": close_hold}),
            );
            if ctx.target_met() {
                break;
            }

            let radius = rand_range(ctx, 5.0, 8.0);
            let arc = rand_range(ctx, 120.0, 260.0);
            let spin = if ctx.rng_next() < 0.5 { 1.0 } else { -1.0 };
            let orbit_legs = orbit(ctx, mate, radius, arc, spin).await;
            ctx.record(
                TYPE_MOVE,
                &format!("vis-chase-orbit-{index}-{round}"),
                &format!("Circle bot {mate} at arm's length."),
                json!({"role": role, "round": round, "mate": mate, "radius": r2(radius),
                    "arc_deg": r2(arc), "direction": spin, "legs": orbit_legs}),
            );
            if ctx.target_met() {
                break;
            }

            let swings = 2 + (ctx.rng_next() * 4.0) as u64;
            let hold = rand_range(ctx, 4_000.0, 9_000.0) as u64;
            let (issued, real) = press_attack(ctx, mate, swings, hold).await;
            ctx.record(
                TYPE_ATTACK,
                &format!("vis-chase-swing-{index}-{round}"),
                &format!("Land {issued} swings on bot {mate}."),
                json!({"role": role, "round": round, "mate": mate, "swings": issued,
                    "requested_swings": swings, "entity_attacks_issued": real,
                    "hold_ms": hold}),
            );
            if ctx.target_met() {
                break;
            }

            // Reposition, so the chaser is not glued to the runner for the whole episode.
            let heading = ctx.rng_next() * 360.0;
            let distance = rand_range(ctx, 8.0, 18.0);
            let sprint = ctx.rng_next() < sprint_prob;
            walk_heading(ctx, heading, distance, sprint).await;
            let re_leash = ctx
                .position()
                .map(|p| (p.0 - anchor.0).hypot(p.2 - anchor.1) > ARENA_LEASH)
                .unwrap_or(false);
            if re_leash {
                let home = (
                    anchor.0 + rand_range(ctx, -6.0, 6.0),
                    anchor.1 + rand_range(ctx, -6.0, 6.0),
                );
                ctx.move_to(home.0, home.1, move_timeout, 3.0).await;
            }
        }
        round += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairs_are_adjacent_and_never_leave_a_bot_chasing_itself() {
        assert_eq!(partner(0, 1), None);
        for bots in 2..=9 {
            for index in 0..bots {
                let mate = partner(index, bots).expect("a swarm of two or more pairs up");
                assert!(mate < bots, "bots={bots} index={index}");
                assert_ne!(mate, index, "bots={bots} index={index}");
            }
        }
        // The odd bot out doubles up on the last pair rather than naming a bot that is not there.
        assert_eq!(partner(4, 5), Some(3));
    }

    #[test]
    fn a_goal_past_the_leash_is_pulled_back_onto_it() {
        let anchor = (100.0, -40.0);
        let inside = (108.0, -44.0);
        assert_eq!(leashed(inside, anchor), inside);
        let far = leashed((100.0 + 500.0, -40.0), anchor);
        let out = (far.0 - anchor.0).hypot(far.1 - anchor.1);
        assert!((out - ARENA_LEASH).abs() < 1e-9, "clamped to {out}");
    }
}
