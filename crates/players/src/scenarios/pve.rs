use super::*;

/// Draw in `[lo, hi)` from the per-bot stream, so a number this scenario could have hard-coded
/// varies per bot and per leg and still replays from the seed.
fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

/// How far sideways off its own spoke a guard post may sit. `station_ring` widens the patrol ring
/// to absorb two neighbours wandering toward each other by this much.
const PATROL_JITTER: f64 = 2.0;

fn mob_kind(ctx: &Ctx, mob: Entity) -> String {
    ctx.bot
        .get_entity_component::<EntityKindComponent>(mob)
        .map(|k| format!("{:?}", k.0).to_ascii_lowercase())
        .unwrap_or_else(|| "hostile".to_string())
}

/// Chase `mob` to melee range and swing on cooldown until it dies, is lost, or `kill_ms` runs out.
///
/// `last_focus` is left on wherever the fight ended, so the caller's next look starts on enemy
/// space rather than on unrelated terrain. `tag` distinguishes this fight's events from every other
/// fight the bot has in the episode. Returns the number of swings issued.
async fn engage(
    ctx: &Ctx,
    mob: Entity,
    kind: &str,
    kill_ms: u64,
    tag: &str,
    last_focus: &mut (f64, f64, f64),
) -> u64 {
    let index = ctx.index();
    let kill_deadline = Instant::now() + Duration::from_millis(kill_ms);
    let mut last_attack = Instant::now() - Duration::from_millis(ATTACK_COOLDOWN_MS);
    let mut swings: u64 = 0;
    loop {
        if Instant::now() >= kill_deadline || ctx.target_met() {
            break;
        }
        // Despawned/dead -> Position gone -> distance is +inf: target defeated/lost.
        let Some(mob_p) = ctx.entity_position(mob) else {
            break;
        };
        *last_focus = head_of(mob_p);
        ctx.look(*last_focus, 120).await;
        let dist = ctx.distance_to_entity(mob);
        if dist <= MELEE_RANGE {
            // The target head was focused above immediately before every melee decision.
            if last_attack.elapsed() >= Duration::from_millis(ATTACK_COOLDOWN_MS) {
                ctx.attack_entity(mob);
                ctx.set_pending("attack");
                swings += 1;
                last_attack = Instant::now();
                ctx.record(
                    TYPE_ATTACK,
                    &format!("vis-pve-hit-{index}-{tag}-{swings}"),
                    &format!("Swing #{swings} at {kind}."),
                    json!({"tag": tag, "mob_kind": kind, "swing": swings,
                        "distance": r2(dist), "entity_attack_issued": true}),
                );
            } else {
                sleep_ms(COMBAT_TICK_MS).await;
            }
        } else {
            // Chase the mob's live position to melee range, then immediately reacquire its live
            // head position so movement never leaves the camera held on the travel heading.
            ctx.move_to(mob_p.0, mob_p.2, 2500, (MELEE_RANGE - 1.0).max(1.5))
                .await;
            if let Some(live_p) = ctx.entity_position(mob) {
                *last_focus = head_of(live_p);
            }
            ctx.look(*last_focus, 120).await;
        }
    }
    // If the mob survived the bounded fight, keep its head framed; otherwise retain its last
    // position so the next acquisition starts by watching enemy space.
    if let Some(mob_p) = ctx.entity_position(mob) {
        *last_focus = head_of(mob_p);
    }
    swings
}

/// PVE: each bot equips a sword and runs bounded find-kill cycles against the nearest hostile mob.
/// Once a target is acquired its head remains the camera focus throughout approach and melee. Bots
/// stay near the combat site after each cycle instead of returning to widely separated spawn posts.
///
/// The coordinator summons its hostiles once, at episode start, and the scenario has no RCON of its
/// own — so there is no restocking the arena. When the summoned mobs are gone the bots go on guard
/// patrol instead: a leg to a fresh post on their own ring, a scan of the ground they just crossed,
/// and a guard swing toward a peer at a distance that keeps the two of them out of each other's
/// frame. Anything that wanders into view during the patrol is fought with the same `engage` loop.
pub(super) async fn run(ctx: &mut Ctx) {
    let index = ctx.index();
    let bot_count = ctx.player_count().max(1);
    let cycles = ctx.engine().shared.native_mob_cycles.clamp(1, 8);
    // Per-cycle budgets (solaris guardAndFight: 15 s to acquire, 15 s to kill).
    let acquire_ms: u64 = 15000;
    let kill_ms: u64 = 15000;
    let view_distance: f64 = 16.0; // solaris VIEW_DISTANCE

    // STEP 1: equip a sword. Do not preserve the potentially far-away spawn as a return post.
    let armed = ctx.equip("diamond_sword").await;
    let initial_position = loop {
        if let Some(p) = ctx.position() {
            break p;
        }
        sleep_ms(100).await;
    };
    let anchor = ctx.anchor();
    let mut last_focus = (anchor.0, initial_position.1 + 1.4, anchor.1);
    ctx.record(
        TYPE_ATTACK,
        &format!("vis-pve-arm-{index}"),
        "Equip sword and watch the shared combat area.",
        json!({"armed": armed, "cycles": cycles,
            "initial_position": {"x": r2(initial_position.0), "y": r2(initial_position.1),
                "z": r2(initial_position.2)},
            "combat_anchor": {"x": r2(anchor.0), "z": r2(anchor.1)}}),
    );
    ctx.pause().await;

    for cycle in 0..cycles {
        if ctx.target_met() {
            break;
        }
        // --- acquire: wait (bounded) for a hostile to come within view ---
        let acquire_deadline = Instant::now() + Duration::from_millis(acquire_ms);
        let mut target: Option<Entity> = None;
        while Instant::now() < acquire_deadline && !ctx.target_met() {
            if let Some(mob) = ctx.nearest_hostile(view_distance) {
                target = Some(mob);
                break;
            }
            // No target yet: keep a level, stable view toward the last enemy area (or the shared
            // anchor initially) rather than sweeping into unrelated terrain.
            ctx.look(last_focus, (COMBAT_TICK_MS * 4).max(400)).await;
            sleep_ms(200).await;
        }
        let Some(mob) = target else {
            // Nothing in view after a full acquire wait: this spot is spent. `continue` stacked
            // 15 s standing waits back to back (21.6 s frozen on the shipped run) — the guard
            // patrol below covers the same job while actually walking, and it still engages
            // anything it meets.
            ctx.record(
                TYPE_LOOK,
                &format!("vis-pve-idle-{index}-c{cycle}"),
                &format!(
                    "No hostile mob within {view_distance} blocks (cycle {cycle}); going on patrol."
                ),
                json!({"cycle": cycle, "found": false}),
            );
            break;
        };
        let kind = mob_kind(ctx, mob);
        if let Some(mob_p) = ctx.entity_position(mob) {
            last_focus = head_of(mob_p);
            ctx.look(last_focus, ctx.cfg().look_settle_ms).await;
        }
        ctx.record(
            TYPE_LOOK,
            &format!("vis-pve-target-{index}-c{cycle}"),
            &format!("Acquired nearest hostile ({kind}) for cycle {cycle}."),
            json!({"cycle": cycle, "found": true, "mob_kind": kind,
                "distance": r2(ctx.distance_to_entity(mob))}),
        );

        let swings = engage(
            ctx,
            mob,
            &kind,
            kill_ms,
            &format!("c{cycle}"),
            &mut last_focus,
        )
        .await;
        ctx.record(
            TYPE_ATTACK,
            &format!("vis-pve-killed-{index}-c{cycle}"),
            &format!("Cycle {cycle} combat ended ({swings} swings)."),
            json!({"cycle": cycle, "mob_kind": kind, "swings": swings,
                "mob_alive": ctx.entity_position(mob).is_some()}),
        );

        // Stay at the combat site so the next acquisition starts by watching enemy space.
        ctx.look(last_focus, ctx.cfg().look_settle_ms).await;
        ctx.record(
            TYPE_LOOK,
            &format!("vis-pve-hold-{index}-c{cycle}"),
            &format!("Hold view on the cycle {cycle} combat area."),
            json!({"cycle": cycle, "combat_focus": {"x": r2(last_focus.0),
                "y": r2(last_focus.1), "z": r2(last_focus.2)}}),
        );
    }

    // --- guard patrol: the summoned wave is spent, so walk the arena instead of standing in it ---
    //
    // Posts sit on this bot's own spoke, stepped out and slid along it per leg. Both draws only push
    // the post outward from the ring `station_ring` sized for the jitter, so a swarm on patrol never
    // converges into one body of players however long it walks.
    let ang = (index as f64 / bot_count as f64) * std::f64::consts::TAU;
    let patrol_ring = station_ring(bot_count, PATROL_JITTER);
    let mut leg = 0usize;
    while !ctx.target_met() {
        if let Some(mob) = ctx.nearest_hostile(view_distance) {
            // Mobs move, so the arena is not necessarily empty just because this bot's wave is.
            let kind = mob_kind(ctx, mob);
            let swings = engage(
                ctx,
                mob,
                &kind,
                kill_ms,
                &format!("g{leg}"),
                &mut last_focus,
            )
            .await;
            ctx.record(
                TYPE_ATTACK,
                &format!("vis-pve-guard-kill-{index}-g{leg}"),
                &format!("Guard patrol engaged a wandering {kind} ({swings} swings)."),
                json!({"leg": leg, "mob_kind": kind, "swings": swings,
                    "mob_alive": ctx.entity_position(mob).is_some()}),
            );
            leg += 1;
            continue;
        }

        // Alternate an inner and an outer post: one jittered ring let consecutive posts land
        // inside move_to's 2-block tolerance whenever the wave died near the spoke, and the bot
        // then "patrolled" 17 legs without a step (p00 on the 08-19 audit — 94 s of standing
        // spar). The 6-block ring gap guarantees every leg is a real walk.
        let radius =
            patrol_ring + if leg.is_multiple_of(2) { 0.0 } else { 6.0 } + vary(ctx, 0.0, 2.0);
        let tangent = vary(ctx, -PATROL_JITTER, PATROL_JITTER);
        let post = (
            anchor.0 + ang.cos() * radius - ang.sin() * tangent,
            anchor.1 + ang.sin() * radius + ang.cos() * tangent,
        );
        let timeout = ctx.cfg().move_timeout_ms.min(12_000);
        let arrived = ctx.move_to(post.0, post.1, timeout, 2.0).await;
        // Scan the ground just crossed. The sweep runs after the walk and leaves the camera
        // wherever it ends, so the next leg's heading is not a function of where the bot is looking.
        let scan_ms = vary(ctx, 1_800.0, 4_000.0) as u64;
        ctx.sweep(scan_ms).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-pve-patrol-{index}-g{leg}"),
            "No hostiles left from the summoned wave: walk the next guard post and scan for more.",
            json!({"leg": leg, "arrived": arrived, "scan_ms": scan_ms,
                "post": {"x": r2(post.0), "z": r2(post.1)}, "radius": r2(radius)}),
        );

        // Spar with whoever is nearest, but only from outside the range where two players occlude
        // each other: these are practice swings at a peer across the arena, not a melee, and the
        // recorded distance says so.
        if let Some((peer, gap)) = nearest_peer(ctx).filter(|(_, gap)| *gap >= MIN_PEER_GAP) {
            let settle = ctx.cfg().look_settle_ms.max(500);
            look_at_peer(ctx, peer, settle).await;
            let username = ctx.peer_username(peer).unwrap_or_default();
            let bouts = vary(ctx, 2.0, 5.0) as u32;
            for _ in 0..bouts {
                if ctx.target_met() {
                    break;
                }
                ctx.attack_peer(&username);
                ctx.set_pending("attack");
                sleep_ms(ATTACK_COOLDOWN_MS).await;
            }
            ctx.record(
                TYPE_ATTACK,
                &format!("vis-pve-spar-{index}-g{leg}"),
                &format!("Guard swings toward peer bot {peer} from outside melee range."),
                json!({"leg": leg, "peer": peer, "distance": r2(gap), "swings": bouts,
                    "in_melee_range": false}),
            );
        } else {
            glance_at_nearest_peer(ctx, None).await;
        }
        leg += 1;
    }
}

/// Nearest peer bot and its 2D distance, or `None` if no peer has replicated a position yet.
fn nearest_peer(ctx: &Ctx) -> Option<(usize, f64)> {
    let own = ctx.position()?;
    (0..ctx.player_count())
        .filter(|&j| j != ctx.index())
        .filter_map(|j| {
            ctx.peer_position(j)
                .map(|p| (j, (p.0 - own.0).hypot(p.2 - own.2)))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
}
