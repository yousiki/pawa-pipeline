use super::*;

/// Closer than this and the fighters are in a clinch: the opponent's head box fills the frame and
/// neither first-person view shows a fight. Swings still land out at [`MELEE_RANGE`], so opening
/// back up costs nothing but the walk.
const CLINCH_RANGE: f64 = 2.25;

/// Draw in `[lo, hi)` from the per-bot stream, so a number this scenario could have hard-coded
/// varies per bot and per round and still replays from the seed.
fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

/// Resolve this bot's pvp opponent index from its swarm index: pairs are (0,1),(2,3),… via
/// `index ^ 1`. With an odd bot count the last bot has no partner (`index ^ 1 == bot_count`) and
/// returns `None` — that bot attacks its nearest peer instead (handled in `run`).
fn pvp_opponent(index: usize, bot_count: usize) -> Option<usize> {
    let partner = index ^ 1;
    if partner < bot_count {
        Some(partner)
    } else {
        None
    }
}

/// Compact arena slot for this bot. Each pair gets a nearby centre and its two members stand on
/// opposite sides of that centre; the layout remains bounded around the anchor for larger swarms.
fn pvp_arena_slot(index: usize, bot_count: usize, anchor: (f64, f64)) -> (f64, f64) {
    let pair_count = bot_count.max(1).div_ceil(2).max(1);
    let pair_index = index / 2;
    let angle = (pair_index as f64 / pair_count as f64) * std::f64::consts::TAU;
    let center_radius = if pair_count > 1 { 3.5 } else { 0.0 };
    let center = (
        anchor.0 + center_radius * angle.cos(),
        anchor.1 + center_radius * angle.sin(),
    );
    let side = if index.is_multiple_of(2) { -1.0 } else { 1.0 };
    let half_span = 1.75;
    (
        center.0 - angle.sin() * half_span * side,
        center.1 + angle.cos() * half_span * side,
    )
}

/// One bounded melee engagement: approach the opponent and swing on cooldown until `combat_ms` runs
/// out or the episode target elapses. Returns the number of swings issued.
async fn melee_round(
    ctx: &Ctx,
    partner: Option<usize>,
    anchor: (f64, f64),
    combat_ms: u64,
    round: usize,
) -> u64 {
    let index = ctx.index();
    let combat_deadline = Instant::now() + Duration::from_millis(combat_ms);
    let mut last_attack = Instant::now() - Duration::from_millis(ATTACK_COOLDOWN_MS);
    let mut swings: u64 = 0;
    let mut last_move = Instant::now();
    while Instant::now() < combat_deadline && !ctx.target_met() {
        // Re-acquire the opponent each tick (paired -> fixed partner; rover -> nearest live peer).
        let Some(opp) = partner.or_else(|| nearest_peer_index(ctx)) else {
            // Degenerate single-bot run: hold a level view into the arena rather than sweeping away.
            let own_y = ctx.position().map(|p| p.1).unwrap_or(64.0);
            ctx.look((anchor.0, own_y + 1.4, anchor.1), 300).await;
            sleep_ms((COMBAT_TICK_MS * 4).max(400)).await;
            continue;
        };
        let opp_name = ctx.peer_username(opp).unwrap_or_default();
        let (Some(self_p), Some(opp_p)) = (ctx.position(), ctx.peer_position(opp)) else {
            sleep_ms(COMBAT_TICK_MS).await;
            continue;
        };
        let dist = ((opp_p.0 - self_p.0).powi(2) + (opp_p.2 - self_p.2).powi(2)).sqrt();
        if dist <= MELEE_RANGE {
            // Both fighters close at once, so each one's approach halves the gap and the pair ends
            // up inside one another — 31% of frames under two blocks, which is an opponent's skin
            // filling the view rather than a fight. Back onto the outer edge of reach first; the
            // camera stays on the opponent while the body opens the distance.
            if dist < CLINCH_RANGE
                && let Some(p) = ctx.position()
            {
                let (dx, dz) = (p.0 - opp_p.0, p.2 - opp_p.2);
                let away = dx.hypot(dz).max(0.01);
                ctx.move_to_watching_peer(
                    opp_p.0 + dx / away * MELEE_RANGE,
                    opp_p.2 + dz / away * MELEE_RANGE,
                    opp,
                    1_200,
                    1.0,
                )
                .await;
            }
            // In range: face the opponent and swing on cooldown.
            look_at_peer(ctx, opp, 120).await;
            if last_attack.elapsed() >= Duration::from_millis(ATTACK_COOLDOWN_MS) {
                let real_swing = ctx.attack_peer(&opp_name);
                ctx.set_pending("attack");
                swings += 1;
                last_attack = Instant::now();
                ctx.record(
                    TYPE_ATTACK,
                    &format!("vis-pvp-hit-{index}-r{round}-{swings}"),
                    &format!("Melee swing #{swings} at opponent bot {opp}."),
                    json!({"round": round, "opponent": opp, "swing": swings,
                        "distance": r2(dist), "entity_attack_issued": real_swing}),
                );
            } else {
                sleep_ms(COMBAT_TICK_MS).await;
            }
        } else {
            // Out of range: approach to the far edge of melee reach, not a block inside it. Closing
            // to MELEE_RANGE-1 put the pair at a 2.1-block median for the whole engagement, which is
            // the head-box occlusion the corpus audit measured — and a swing lands either way.
            let approached = ctx
                .move_to(opp_p.0, opp_p.2, 2500, MELEE_RANGE - 0.25)
                .await;
            look_at_peer(ctx, opp, 120).await;
            // Stall detection (solaris re-engage): if we didn't get closer for >1 s, log a re-acquire.
            let new_dist = ctx
                .position()
                .map(|p| ((opp_p.0 - p.0).powi(2) + (opp_p.2 - p.2).powi(2)).sqrt())
                .unwrap_or(dist);
            if approached || new_dist < dist - 0.5 {
                last_move = Instant::now();
            } else if last_move.elapsed() > Duration::from_millis(1000) {
                ctx.record(
                    TYPE_MOVE,
                    &format!("vis-pvp-reacq-{index}-{}", ctx.elapsed_ms()),
                    &format!(
                        "Stalled at {} blocks from bot {opp}; re-acquiring.",
                        r2(dist)
                    ),
                    json!({"round": round, "opponent": opp, "distance": r2(dist),
                        "reacquire": true}),
                );
                last_move = Instant::now();
            }
        }
    }
    swings
}

/// PVP: pair bots (0↔1, 2↔3, …), stage each pair in a compact arena around the shared anchor,
/// equip swords, make eye contact, then trade bounded melee engagements for the whole episode. The
/// odd bot out attacks its nearest peer.
///
/// One engagement used to be the whole scenario, which left the pair inside melee range staring at
/// each other for the rest of the episode. Each round now ends with a real break: both fighters
/// open the distance and walk an arc around the opponent before closing again, so the next round is
/// a fresh approach rather than a continuation of the same shoving match. The arc is walked with
/// `move_to_watching_peer`, which holds the camera on the opponent while the body moves tangentially
/// — the walk keys and the look direction stay independent for the whole break.
pub(super) async fn run(ctx: &mut Ctx) {
    let index = ctx.index();
    let bot_count = ctx.player_count().max(1);
    let combat_ms = ctx.engine().shared.native_combat_duration_ms.max(1000.0) as u64;
    let partner = pvp_opponent(index, bot_count);

    // STEP 1: equip a sword from the kit (solaris equipSword). diamond_sword is the kit's blade.
    let armed = ctx.equip("diamond_sword").await;
    ctx.record(
        TYPE_ATTACK,
        &format!("vis-pvp-arm-{index}"),
        &format!(
            "Equip sword for PVP vs {}.",
            partner
                .map(|p| format!("bot {p}"))
                .unwrap_or_else(|| "nearest peer".to_string())
        ),
        json!({"armed": armed, "opponent": partner, "combat_ms": combat_ms,
            "role": if partner.is_some() { "paired" } else { "rover" }}),
    );
    ctx.pause().await;

    // Gather into a compact anchor-centred arena before combat. This replaces fighting across the
    // coordinator's potentially wide spawn spread and gives both first-person views a nearby target.
    let anchor = ctx.anchor();
    let arena_slot = pvp_arena_slot(index, bot_count, anchor);
    let arena_timeout = ctx.cfg().move_timeout_ms.min(30000);
    let arrived = ctx
        .move_to(arena_slot.0, arena_slot.1, arena_timeout, 1.5)
        .await;
    ctx.record(
        TYPE_MOVE,
        &format!("vis-pvp-arena-{index}"),
        "Stage at compact PVP arena slot near the shared anchor.",
        json!({"arrived": arrived,
            "arena_slot": {"x": r2(arena_slot.0), "z": r2(arena_slot.1)},
            "anchor": {"x": r2(anchor.0), "z": r2(anchor.1)}}),
    );

    let mut round = 0usize;
    loop {
        // STEP 2: make eye contact before closing, on every round and not just the first.
        if let Some(opp) = partner.or_else(|| nearest_peer_index(ctx)) {
            let settle = ctx.cfg().look_settle_ms;
            look_at_peer(ctx, opp, settle).await;
            ctx.record(
                TYPE_LOOK,
                &format!("vis-pvp-eye-{index}-r{round}"),
                &format!("Eye contact with opponent bot {opp} before round {round}."),
                json!({"round": round, "opponent": opp}),
            );
        }
        sleep_ms(vary(ctx, 350.0, 900.0) as u64).await;

        // STEP 3: bounded engagement. Stops at the seeded budget OR the episode target.
        let swings = melee_round(ctx, partner, anchor, combat_ms, round).await;
        ctx.record(
            TYPE_ATTACK,
            &format!("vis-pvp-done-{index}-r{round}"),
            &format!("PVP engagement round {round} complete: {swings} swings."),
            json!({"round": round, "swings": swings, "opponent": partner}),
        );
        if ctx.target_met() {
            break;
        }

        // STEP 4: break. Open to a standoff outside melee range and walk an arc around the
        // opponent, so the pair is genuinely apart between engagements instead of locked together.
        let standoff = vary(ctx, MIN_PEER_GAP + 1.5, MIN_PEER_GAP + 4.5);
        let sign = if ctx.rng_next() < 0.5 { 1.0 } else { -1.0 };
        let arc = vary(ctx, 0.8, 2.2) * sign;
        let opponent = partner.or_else(|| nearest_peer_index(ctx));
        let opponent_at = opponent.and_then(|opp| ctx.peer_position(opp));
        match (opponent, ctx.position(), opponent_at) {
            (Some(opp), Some(self_p), Some(opp_p)) => {
                let angle = (self_p.2 - opp_p.2).atan2(self_p.0 - opp_p.0) + arc;
                let target = (
                    opp_p.0 + angle.cos() * standoff,
                    opp_p.2 + angle.sin() * standoff,
                );
                let timeout = ctx.cfg().move_timeout_ms.min(10_000);
                let broke = ctx
                    .move_to_watching_peer(target.0, target.1, opp, timeout, 2.0)
                    .await;
                ctx.record(
                    TYPE_MOVE,
                    &format!("vis-pvp-break-{index}-r{round}"),
                    &format!("Disengage from bot {opp} and circle before the next round."),
                    json!({"round": round, "opponent": opp, "arrived": broke,
                        "standoff": r2(standoff), "arc_rad": r2(arc),
                        "target": {"x": r2(target.0), "z": r2(target.1)}}),
                );
            }
            _ => {
                // No opponent replicated: cross the arena and read it rather than standing.
                let angle = vary(ctx, 0.0, std::f64::consts::TAU);
                let radius = vary(ctx, 5.0, 10.0);
                let timeout = ctx.cfg().move_timeout_ms.min(10_000);
                ctx.move_to(
                    anchor.0 + angle.cos() * radius,
                    anchor.1 + angle.sin() * radius,
                    timeout,
                    2.0,
                )
                .await;
                let scan_ms = vary(ctx, 1_500.0, 3_500.0) as u64;
                ctx.sweep(scan_ms).await;
                ctx.record(
                    TYPE_MOVE,
                    &format!("vis-pvp-break-{index}-r{round}"),
                    "No opponent in view: cross the arena and scan for one.",
                    json!({"round": round, "opponent": Value::Null, "scan_ms": scan_ms,
                        "radius": r2(radius)}),
                );
            }
        }
        if ctx.target_met() {
            break;
        }
        round += 1;
    }
}

/// Index of the nearest peer bot by live 2D position (excludes self), or `None` if no peer has a
/// known position yet. Shared by the pvp rover (odd bot out) path.
fn nearest_peer_index(ctx: &Ctx) -> Option<usize> {
    let own = ctx.position()?;
    let mut best: Option<(usize, f64)> = None;
    for j in 0..ctx.player_count() {
        if j == ctx.index() {
            continue;
        }
        let Some(p) = ctx.peer_position(j) else {
            continue;
        };
        let d = (p.0 - own.0).powi(2) + (p.2 - own.2).powi(2);
        if best.map(|b| d < b.1).unwrap_or(true) {
            best = Some((j, d));
        }
    }
    best.map(|(j, _)| j)
}
