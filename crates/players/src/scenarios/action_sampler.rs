use super::*;

/// The eight movement identities the controller can express. Sampling over all of them is the
/// point of this scenario: the 2026-08 action audit found shipped episodes using three to six
/// distinct key combinations out of 128.
const DIRECTIONS: [(&str, WalkDirection); 8] = [
    ("W", WalkDirection::Forward),
    ("WD", WalkDirection::ForwardRight),
    ("D", WalkDirection::Right),
    ("SD", WalkDirection::BackwardRight),
    ("S", WalkDirection::Backward),
    ("SA", WalkDirection::BackwardLeft),
    ("A", WalkDirection::Left),
    ("WA", WalkDirection::ForwardLeft),
];

/// The forward-ish subset, used only while a round is correcting a leash or a peer gap: a
/// correction that draws `S` walks the wrong way and the bound never clears.
const FORWARD_KEYS: [(&str, WalkDirection); 3] = [
    ("W", WalkDirection::Forward),
    ("WD", WalkDirection::ForwardRight),
    ("WA", WalkDirection::ForwardLeft),
];

/// Blocks this scenario will try to place. The same closed kit `Ctx::equip` can actually find in
/// the hotbar, minus the tools — an item the bots do not carry equips nothing and leaves a step
/// that only looks like an action.
const PLACEABLE: [&str; 6] = [
    "oak_planks",
    "stone_bricks",
    "bricks",
    "cobblestone",
    "stone",
    "oak_log",
];

/// How far from the spawn anchor a sampler may wander before the next round is steered home. A
/// walk drawn from a uniform bearing is a random walk, and a random walk has no bound.
const ANCHOR_LEASH: f64 = 40.0;

/// Below this gap the next round is spent opening distance instead of sampling freely. Two bodies
/// in one cell is the head-box occlusion the corpus audit measured in 11.95% of posed frames.
const MIN_PEER_GAP: f64 = 2.0;

/// Share of the episode the accumulated `pause` may occupy. The contract's ceiling is 10%; staying
/// under it needs a margin, because the check runs before the pause rather than after it.
const PAUSE_SHARE: u64 = 12;

fn rand_range(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

fn pick<T: Copy>(ctx: &mut Ctx, options: &[T]) -> T {
    let i = (ctx.rng_next() * options.len() as f64) as usize;
    options[i.min(options.len() - 1)]
}

/// Camera yaw the controller last applied, in the same 0° = +Z convention headings use.
fn facing_yaw(ctx: &Ctx) -> f64 {
    ctx.bot.direction().unwrap_or_default().y_rot() as f64
}

/// Minecraft yaw pointing from `from` at `(x, z)`.
fn bearing_to(from: (f64, f64, f64), to: (f64, f64)) -> f64 {
    (-(to.0 - from.0)).atan2(to.1 - from.2).to_degrees()
}

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

fn visible_peers(ctx: &Ctx) -> Vec<usize> {
    (0..ctx.player_count())
        .filter(|&j| j != ctx.index())
        .filter(|&j| ctx.peer_position(j).is_some())
        .collect()
}

/// Point the camera at a world bearing without moving the body.
async fn face_bearing(ctx: &Ctx, heading_deg: f64) {
    let Some(p) = ctx.position() else { return };
    let yaw = heading_deg.to_radians();
    let settle = ctx.cfg().look_settle_ms.max(500);
    ctx.look(
        (p.0 - yaw.sin() * 32.0, p.1 + 1.62, p.2 + yaw.cos() * 32.0),
        settle,
    )
    .await;
}

/// The cell one block ahead of where the camera is pointing, at feet or head height.
fn cell_ahead(ctx: &Ctx, distance: f64) -> Option<BlockPos> {
    let position = ctx.position()?;
    let yaw = facing_yaw(ctx).to_radians();
    Some(BlockPos::new(
        (position.0 - yaw.sin() * distance).floor() as i32,
        position.1.floor() as i32,
        (position.2 + yaw.cos() * distance).floor() as i32,
    ))
}

/// What the body did this round, as the action stream names it.
struct Outcome {
    kind: &'static str,
    action_type: &'static str,
    detail: Value,
}

/// Every bot samples the action vocabulary independently until the episode ends, drawing the body
/// action and the camera action from separate rolls each round.
///
/// This is not a translated script — it is the deliberately unstructured episode the corpus is
/// missing. It exists to cover the joint action space rather than any behaviour a viewer would
/// name: every heading, distance, duration, sweep angle, sprint bit and jump cadence is its own
/// draw, and the eight-way movement identity is sampled uniformly instead of being whatever the
/// pathfinder happened to need.
///
/// The one thing that is NOT free is the pair of bounds a purely random walk cannot respect on its
/// own. A bot past `ANCHOR_LEASH` from the anchor, or inside `MIN_PEER_GAP` of a peer, spends its
/// round opening that distance; both overrides are recorded on the event so the audit can tell a
/// corrected round from a sampled one. Everything else, including whether the camera moves at all,
/// stays a draw.
///
/// Camera and body are drawn from separate rolls and applied in that order, because `hold_visual`
/// freezes the camera where it finds it: the walk key and the direction the bot is looking are
/// therefore genuinely independent for the whole hold, which is exactly the decoupling the audit
/// asked for and what a strafing or backpedalling frame looks like.
pub(crate) async fn run(ctx: &mut Ctx) {
    let index = ctx.index();
    let anchor = ctx.anchor();
    let sprint_prob = ctx.cfg().sprint_bout_prob;
    let kit = pawa_schema::catalog::EQUIP_ITEMS;

    ctx.record(
        TYPE_LOOK,
        &format!("vis-sampler-init-{index}"),
        "Action sampler: draw a body action and a camera action independently, every round, until the episode ends.",
        json!({"directions": DIRECTIONS.len(), "kit": kit, "anchor_leash": ANCHOR_LEASH}),
    );

    let mut pause_ms = 0u64;
    let mut round = 0u32;
    // Consecutive stalled holds. One is an obstacle the next draw walks around; two in a row means
    // the body is somewhere open-loop walking cannot leave — the floor of a ravine, a one-block
    // hole, the inside of a thicket — and a third blind draw would be a third wasted round.
    let mut stalls = 0u32;
    while !ctx.target_met() {
        let own = ctx.position();
        let anchor_gap = own
            .map(|p| (p.0 - anchor.0).hypot(p.2 - anchor.1))
            .unwrap_or(0.0);
        let close = nearest_peer(ctx).filter(|(_, gap)| *gap < MIN_PEER_GAP);
        let leashed = anchor_gap > ANCHOR_LEASH;

        // --- camera roll, drawn before and independently of the body roll below -----------------
        let camera_roll = (ctx.rng_next() * 4.0) as u32;
        let camera = if leashed || close.is_some() {
            // The correction only aims the camera; which forward-ish key the body then holds, and
            // whether it sprints, are still their own draws.
            let heading = match (own, close) {
                (Some(p), Some((peer, _))) => ctx
                    .peer_position(peer)
                    .map(|q| bearing_to(p, (q.0, q.2)) + 180.0)
                    .unwrap_or_else(|| bearing_to(p, anchor)),
                (Some(p), None) => bearing_to(p, anchor),
                _ => 0.0,
            };
            face_bearing(ctx, heading).await;
            "corrective"
        } else {
            match camera_roll {
                0 => {
                    let dyaw = rand_range(ctx, -180.0, 180.0);
                    let dpitch = rand_range(ctx, -35.0, 35.0);
                    let rate = ctx.cfg().yaw_rate_deg_s.max(1.0);
                    let settle = (dyaw.abs() / rate * 1000.0) as u64 + 700;
                    ctx.drag_view(dyaw, dpitch, settle).await;
                    "drag"
                }
                1 => {
                    let peers = visible_peers(ctx);
                    if peers.is_empty() {
                        let heading = ctx.rng_next() * 360.0;
                        face_bearing(ctx, heading).await;
                        "bearing"
                    } else {
                        let peer = pick(ctx, &peers);
                        let settle = rand_range(ctx, 400.0, 2_500.0) as u64;
                        look_at_peer(ctx, peer, settle).await;
                        "peer"
                    }
                }
                2 => {
                    let heading = ctx.rng_next() * 360.0;
                    face_bearing(ctx, heading).await;
                    "bearing"
                }
                // Leaving the camera exactly where it is has to be in the vocabulary too, or
                // "camera moved" stops being an independent bit and becomes a constant.
                _ => "held",
            }
        };

        // --- body roll ---------------------------------------------------------------------------
        let body_roll = (ctx.rng_next() * 10.0) as u32;
        let forced_walk = leashed || close.is_some();
        let outcome = if stalls >= 2 {
            // Spend the round on the pathfinder instead of another blind hold: it plans over the
            // terrain the open loop cannot see, so it climbs out of what the body walked into.
            stalls = 0;
            let arrived = ctx
                .move_to(
                    anchor.0,
                    anchor.1,
                    ctx.cfg().move_timeout_ms.min(8_000),
                    3.0,
                )
                .await;
            Outcome {
                kind: "unstick",
                action_type: TYPE_MOVE,
                detail: json!({"arrived": arrived, "anchor_gap": r2(anchor_gap)}),
            }
        } else if forced_walk || body_roll <= 3 {
            // Forward-ish keys only while correcting, so the correction actually opens distance;
            // the full eight otherwise.
            let (name, dir) = if forced_walk {
                pick(ctx, &FORWARD_KEYS)
            } else {
                pick(ctx, &DIRECTIONS)
            };
            let sprint = ctx.rng_next() < sprint_prob;
            let jump_period = if ctx.rng_next() < 0.25 {
                rand_range(ctx, 800.0, 2_400.0) as u64
            } else {
                0
            };
            let hold = rand_range(ctx, 600.0, 4_500.0) as u64;
            let before = ctx.position();
            let completed = ctx.hold_visual(dir, sprint, hold, jump_period).await;
            let travelled = match (before, ctx.position()) {
                (Some(a), Some(b)) => Some(r2((b.0 - a.0).hypot(b.2 - a.2))),
                _ => None,
            };
            // The hold gave up against something solid. Turn well off the blocked bearing before the
            // next round draws, or the draw is made from inside the same tree and jams again.
            if completed {
                stalls = 0;
            } else {
                stalls += 1;
                let away = facing_yaw(ctx) + 90.0 + rand_range(ctx, 0.0, 180.0);
                face_bearing(ctx, away).await;
            }
            Outcome {
                kind: "walk",
                action_type: TYPE_MOVE,
                detail: json!({"keys": name, "sprint": sprint, "jump_period_ms": jump_period,
                    "hold_ms": hold, "travelled": travelled, "stalled": !completed}),
            }
        } else {
            match body_roll {
                4 => {
                    let equipped = ctx.equip("diamond_pickaxe").await;
                    let cell = cell_ahead(ctx, 1.0)
                        .map(|c| if block_is_air(ctx, c) { c.up(1) } else { c });
                    let dug = match cell {
                        Some(c) if !block_is_air(ctx, c) => ctx.dig(c).await,
                        _ => false,
                    };
                    Outcome {
                        kind: "mine",
                        action_type: TYPE_MINE,
                        detail: json!({"pickaxe": equipped, "dug": dug,
                            "cell": cell.map(|c| json!({"x": c.x, "y": c.y, "z": c.z}))}),
                    }
                }
                5 => {
                    let material = pick(ctx, &PLACEABLE);
                    let equipped = ctx.equip(material).await;
                    let desired = cell_ahead(ctx, 2.0);
                    let base = desired.and_then(|c| {
                        marker_base_near(ctx, (c.x as i64, c.y as i64, c.z as i64), 1)
                    });
                    let placed = match base {
                        Some(target) if equipped => ctx.place_block(target, material).await,
                        _ => false,
                    };
                    Outcome {
                        kind: "place",
                        action_type: TYPE_PLACE,
                        detail: json!({"block": material, "equipped": equipped, "placed": placed,
                            "cell": base.map(|c| json!({"x": c.x, "y": c.y, "z": c.z}))}),
                    }
                }
                6 => {
                    let item = pick(ctx, kit);
                    let equipped = ctx.equip(item).await;
                    let held = ctx
                        .inventory()
                        .and_then(|inventory| item_name(inventory.held_item()));
                    Outcome {
                        kind: "equip",
                        action_type: TYPE_LOOK,
                        detail: json!({"item": item, "equipped": equipped, "held_item": held}),
                    }
                }
                7 => {
                    let peers = visible_peers(ctx);
                    if peers.is_empty() {
                        let heading = ctx.rng_next() * 360.0;
                        face_bearing(ctx, heading).await;
                        Outcome {
                            kind: "bearing_only",
                            action_type: TYPE_LOOK,
                            detail: json!({"heading_deg": r2(heading), "reason": "no peer"}),
                        }
                    } else {
                        let peer = pick(ctx, &peers);
                        let orbiting = ctx.rng_next() < 0.5;
                        let standoff = rand_range(ctx, 4.0, 9.0);
                        let hold = rand_range(ctx, 2_500.0, 7_000.0) as u64;
                        let deadline = Instant::now() + Duration::from_millis(hold);
                        let mut angle = match (ctx.position(), ctx.peer_position(peer)) {
                            (Some(o), Some(q)) => (o.2 - q.2).atan2(o.0 - q.0),
                            _ => 0.0,
                        };
                        let step = rand_range(ctx, 0.25, 0.6)
                            * if ctx.rng_next() < 0.5 { 1.0 } else { -1.0 };
                        let mut legs = 0u32;
                        while Instant::now() < deadline && !ctx.target_met() {
                            let Some(q) = ctx.peer_position(peer) else {
                                sleep_ms(250).await;
                                continue;
                            };
                            if orbiting {
                                angle += step;
                            } else if let Some(o) = ctx.position() {
                                angle = (o.2 - q.2).atan2(o.0 - q.0);
                            }
                            let slice = deadline
                                .saturating_duration_since(Instant::now())
                                .as_millis()
                                .min(2_500) as u64;
                            ctx.move_to_watching_peer(
                                q.0 + angle.cos() * standoff,
                                q.2 + angle.sin() * standoff,
                                peer,
                                slice.max(400),
                                1.5,
                            )
                            .await;
                            legs += 1;
                        }
                        Outcome {
                            kind: if orbiting { "orbit" } else { "follow" },
                            action_type: TYPE_MOVE,
                            detail: json!({"peer": peer, "standoff": r2(standoff), "legs": legs,
                                "hold_ms": hold}),
                        }
                    }
                }
                8 => {
                    let want = rand_range(ctx, 400.0, 3_000.0) as u64;
                    let elapsed = ctx.elapsed_ms().max(1) as u64;
                    let allowed = pause_ms + want <= elapsed / PAUSE_SHARE;
                    if allowed {
                        pause_ms += want;
                        ctx.still(want).await;
                    }
                    Outcome {
                        kind: "pause",
                        action_type: TYPE_MOVE,
                        detail: json!({"requested_ms": want, "held": allowed,
                            "pause_budget_ms": pause_ms, "share_denominator": PAUSE_SHARE}),
                    }
                }
                _ => {
                    // A swing is only a real swing when the peer entity is tracked; the pending
                    // flag and the event always reflect the intent either way.
                    let target = nearest_peer(ctx);
                    let username = target
                        .and_then(|(peer, _)| ctx.peer_username(peer))
                        .unwrap_or_default();
                    if let Some((peer, _)) = target {
                        look_at_peer(ctx, peer, 200).await;
                    }
                    let real = ctx.attack_peer(&username);
                    ctx.set_pending("attack");
                    sleep_ms(ATTACK_COOLDOWN_MS).await;
                    Outcome {
                        kind: "attack",
                        action_type: TYPE_ATTACK,
                        detail: json!({"peer": target.map(|(peer, _)| peer),
                            "distance": target.map(|(_, gap)| r2(gap)),
                            "entity_attack_issued": real, "swings": 1}),
                    }
                }
            }
        };

        ctx.record(
            outcome.action_type,
            &format!("vis-sampler-{index}-{round}"),
            &format!("Sampled {} with the camera {camera}.", outcome.kind),
            merged(
                &outcome.detail,
                json!({
                    "round": round,
                    "body": outcome.kind,
                    "camera": camera,
                    "body_roll": body_roll,
                    "camera_roll": camera_roll,
                    "anchor_gap": r2(anchor_gap),
                    "leashed": leashed,
                    "peer_gap": close.map(|(_, gap)| r2(gap)),
                    "corrected": leashed || close.is_some(),
                }),
            ),
        );
        // Outside the sampled vocabulary on purpose: peers place and dig at random around this
        // bot, and the corpus needs A's edits showing up in B's view. Self-gated (min interval +
        // ripe-edit availability), so most rounds it is a no-op.
        witness_peer_edits(ctx).await;
        round += 1;
    }
}
