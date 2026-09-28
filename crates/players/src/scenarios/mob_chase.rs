use super::*;

/// Bot 0 is prey, taking short arc steps inside a compact anchor-local arena; bots 1..N-1
/// frequently retarget its live position. No PvP damage: capture is claimed only from measured
/// proximity, after which prey and chasers remain stopped and mutually visible.
pub(super) async fn run(ctx: &mut Ctx) {
    let index = ctx.index();
    let prey = 0usize;
    let is_prey = index == prey;
    let anchor = ctx.anchor();
    const ARC_RADIUS: f64 = 4.5;
    const ARC_STEPS: usize = 12;

    // The shared plan's tag distance (2.5–3.5 blocks) is inside a player's own head box, so driving
    // the chase to it ended every episode on a face filling the frame. Capture is a proximity claim,
    // not a collision: chasers hold `standoff` blocks off and claim the capture from `capture_at`,
    // which they cross while still approaching. Both roles derive both numbers from the same shared
    // plan, so prey and chaser agree on when the chase is over without exchanging anything.
    let standoff = (ctx.engine().shared.tag_distance + 2.0).max(MIN_PEER_GAP);
    let capture_at = standoff + 1.5;

    // The prey advances around this compact ring one short chord at a time. Keeping all goals near
    // the anchor prevents the old centroid bias from feeding an unbounded outward escape.
    let arc_ring = |start_angle: f64| -> Vec<(f64, f64)> {
        (0..ARC_STEPS)
            .map(|step| {
                let angle = start_angle + step as f64 * std::f64::consts::TAU / ARC_STEPS as f64;
                (
                    r2(anchor.0 + angle.cos() * ARC_RADIUS),
                    r2(anchor.1 + angle.sin() * ARC_RADIUS),
                )
            })
            .collect()
    };
    let start_angle = ctx.rng_next() * std::f64::consts::TAU;
    let mut arc = arc_ring(start_angle);
    let mut arc_cursor = index % ARC_STEPS;
    let mut captured = false;
    // A capture only counts once the pair has genuinely opened up again since the last tableau:
    // without the re-arm, round two "captures" at frame one because everyone is still standing in
    // the tableau huddle. Bounded so a cornered prey cannot deadlock the release.
    let mut rearmed = true;
    let mut release_holds = 0u32;
    let mut round = 0u32;
    let rearm_at = capture_at + 2.0;

    // Preserve a role cue in the rendered pixels: prey carries a torch, chasers carry swords.
    // Both items already exist in the shared hotbar kit; this is visual identification only and
    // does not turn proximity capture into damaging PvP.
    let role_item = if is_prey { "torch" } else { "diamond_sword" };
    let role_item_equipped = ctx.equip(role_item).await;

    ctx.record(
        TYPE_LOOK,
        &format!("vis-mobchase-init-{index}"),
        &format!(
            "Mob chase: bot {prey} is prey inside a compact anchor arena; {} chasers.",
            ctx.player_count().saturating_sub(1)
        ),
        json!({"prey": prey, "role": if is_prey { "prey" } else { "chaser" },
            "arena_radius": ARC_RADIUS, "role_item": role_item,
            "standoff": r2(standoff), "capture_at": r2(capture_at),
            "role_item_equipped": role_item_equipped}),
    );

    // How long the post-capture tableau may hold before the scenario ends. It used to run until
    // `target_met()`, i.e. for the whole remaining episode, and with 8 bots in a compact arena the
    // capture happens early — which is where mob_chase's dead air comes from. Measured on the
    // shipped corpus: 91.6% of frames idle, one frozen stretch of 114.5 s; on a later
    // validation capture, still 85.3% and 154.2 s. Ending the scenario instead of standing in it
    // hands the remaining time to the post-finish ambient wander in `control::apply_intents`, which
    // keeps the action stream and the video alive without inventing behaviour inside the script.
    // Wall-clock bound, not an iteration count: one iteration costs hold_off (up to 900 ms move +
    // operation pause) + a camera settle with a 1 s floor, so 20 "250 ms ticks" really ran 6–22 s
    // on the 08-19 audit — long enough to trip the frozen-run QC on its own.
    const TABLEAU_MS: i64 = 5_000;
    let mut tableau_until: Option<i64> = None;

    while !ctx.target_met() {
        if captured {
            // Preserve a clear face-to-face tableau, then RELEASE the prey for another round. At
            // four bots in this compact arena the first capture lands within seconds; ending the
            // scenario there handed 110 s of a 120 s episode to the ambient wander (50 moving
            // frames, 84% key chatter on the shipped run). The chase itself is the content, so it
            // repeats on a fresh ring until the episode clock says stop.
            let now = ctx.elapsed_ms();
            let until = *tableau_until.get_or_insert(now + TABLEAU_MS);
            if now >= until {
                round += 1;
                captured = false;
                tableau_until = None;
                rearmed = false;
                release_holds = 0;
                let start_angle = ctx.rng_next() * std::f64::consts::TAU;
                arc = arc_ring(start_angle);
                ctx.record(
                    TYPE_MOVE,
                    &format!("vis-tableau-done-{index}-{}", ctx.elapsed_ms()),
                    &format!("Tableau held; releasing the prey for chase round {round}."),
                    json!({"role": if is_prey { "prey" } else { "chaser" },
                        "round": round, "tableau_ms": TABLEAU_MS}),
                );
                continue;
            }
            let counterpart = if is_prey {
                nearest_peer(ctx).map(|(peer, _, _)| peer)
            } else {
                Some(prey)
            };
            match counterpart {
                Some(peer) => {
                    // Two stopped bodies still drift together — a late chaser's last leg, or the
                    // pathfinder settling. Back out of the peer's space before framing it.
                    hold_off(ctx, peer, standoff).await;
                    look_at_peer(ctx, peer, 350).await;
                }
                None => sleep_ms(250).await,
            }
            sleep_ms(250).await;
            continue;
        }

        if is_prey {
            // Check contact before fleeing. A chaser that reached the true capture radius ends the
            // escape immediately; this is based on live positions, not move_to's return value.
            if let Some((chaser, distance, _)) = nearest_peer(ctx) {
                if distance > rearm_at {
                    rearmed = true;
                }
                look_at_peer(ctx, chaser, 120).await;
                if rearmed && distance <= capture_at {
                    captured = true;
                    ctx.record(
                        TYPE_MOVE,
                        &format!("vis-capture-observed-{index}-{}", ctx.elapsed_ms()),
                        &format!("Prey bot {index} is captured by chaser bot {chaser}."),
                        json!({"role": "prey", "captured_by": chaser,
                            "capture": true, "distance": r2(distance)}),
                    );
                    look_at_peer(ctx, chaser, 500).await;
                    continue;
                }
            }

            arc_cursor = (arc_cursor + 1) % arc.len();
            let target = arc[arc_cursor];
            let arrived = ctx
                .move_to(target.0, target.1, 550, ctx.cfg().goal_radius)
                .await;
            let nearest_after = nearest_peer(ctx);
            if let Some((chaser, _, _)) = nearest_after {
                look_at_peer(ctx, chaser, 250).await;
            }
            ctx.record(
                TYPE_MOVE,
                &format!("vis-flee-{index}-{}", ctx.elapsed_ms()),
                &format!(
                    "Prey takes a short arc step toward ({}, {}).",
                    target.0, target.1
                ),
                json!({"role": "prey", "waypoint": {"x": target.0, "z": target.1},
                    "arrived": arrived,
                    "nearest_chaser_distance": nearest_after.map(|v| r2(v.1))}),
            );
            if let Some((chaser, distance, _)) = nearest_after {
                look_at_peer(ctx, chaser, 300).await;
                if rearmed && distance <= capture_at {
                    captured = true;
                    ctx.record(
                        TYPE_MOVE,
                        &format!("vis-capture-observed-{index}-{}", ctx.elapsed_ms()),
                        &format!("Prey bot {index} is captured by chaser bot {chaser}."),
                        json!({"role": "prey", "captured_by": chaser,
                            "capture": true, "distance": r2(distance)}),
                    );
                    look_at_peer(ctx, chaser, 500).await;
                }
            }
        } else if let Some(prey_pos) = ctx.peer_position(prey) {
            // Frequent short re-targeting follows the prey's live position without the former
            // three-second blocking goal. Look before and after each burst to keep the prey visible.
            look_at_peer(ctx, prey, 80).await;
            let own = ctx.position();
            let distance_before = own
                .map(|p| ((prey_pos.0 - p.0).powi(2) + (prey_pos.2 - p.2).powi(2)).sqrt())
                .unwrap_or(f64::INFINITY);
            // Freshly released prey gets a head start. The prey alone cannot open the re-arm gap:
            // it flees on a 4.5-block ring while rearm_at is ~8.2, so on the 08-19 audit the wait
            // always expired, the fake re-arm recaptured at tableau distance on the very next
            // line, and rounds degenerated into statue ping-pong (26.6 s frozen). The chaser
            // therefore backs out past the re-arm line itself — a real walk, and the gap opens in
            // a few legs. The bounded expiry stays as a wall-jam backstop; with the retreat behind
            // it, the chaser is by then far outside capture range, so it re-arms into a real
            // pursuit leg instead of an instant recapture.
            if !rearmed {
                if distance_before > rearm_at || release_holds >= 24 {
                    rearmed = true;
                } else {
                    release_holds += 1;
                    hold_off(ctx, prey, rearm_at + 1.6).await;
                    look_at_peer(ctx, prey, 250).await;
                    continue;
                }
            }
            if distance_before <= capture_at {
                captured = true;
                ctx.record(
                    TYPE_MOVE,
                    &format!("vis-capture-{index}-{}", ctx.elapsed_ms()),
                    &format!("Chaser bot {index} captures prey bot {prey}."),
                    json!({"role": "chaser", "prey": prey, "capture": true,
                        "distance": r2(distance_before)}),
                );
                look_at_peer(ctx, prey, 500).await;
                continue;
            }

            // Aim for a point `standoff` blocks out on the chaser's own side of the prey rather than
            // the prey itself, so the leg ends beside it instead of on top of it.
            let (ux, uz) = away_unit(own, prey_pos);
            // 900 ms, not 500: the leg's key press lasts exactly this timeout, and 500 ms is 8–9
            // frames — one under the QC short-press bar, so every pursuit leg counted as chatter.
            let arrived = ctx
                .move_to(
                    prey_pos.0 + ux * standoff,
                    prey_pos.2 + uz * standoff,
                    900,
                    1.5,
                )
                .await;
            let distance_after = match (ctx.position(), ctx.peer_position(prey)) {
                (Some(p), Some(pp)) => ((pp.0 - p.0).powi(2) + (pp.2 - p.2).powi(2)).sqrt(),
                _ => f64::INFINITY,
            };
            let in_capture_range = distance_after <= capture_at;
            look_at_peer(ctx, prey, if in_capture_range { 500 } else { 150 }).await;
            ctx.record(
                TYPE_MOVE,
                &format!("vis-pursue-{index}-{}", ctx.elapsed_ms()),
                &format!(
                    "Chaser pursues prey bot {prey} at {} blocks.",
                    r2(distance_after)
                ),
                json!({"role": "chaser", "prey": prey, "distance": r2(distance_after),
                    "in_capture_range": in_capture_range, "arrived": arrived,
                    "capture": in_capture_range}),
            );
            if in_capture_range {
                captured = true;
            }
        } else {
            sleep_ms(200).await;
        }
    }
}

/// Step straight out of `peer`'s space until `gap` blocks separate them. A no-op when the gap
/// already holds.
async fn hold_off(ctx: &Ctx, peer: usize, gap: f64) {
    let Some(peer_pos) = ctx.peer_position(peer) else {
        return;
    };
    let own = ctx.position();
    if own
        .map(|p| (p.0 - peer_pos.0).hypot(p.2 - peer_pos.2) >= gap)
        .unwrap_or(true)
    {
        return;
    }
    let (ux, uz) = away_unit(own, peer_pos);
    // Overshoot by the arrival radius: aiming exactly at `gap` lets move_to stop 1.5 blocks short
    // of it, still inside the gap, so the very next iteration nudges again — a stream of 1–7-frame
    // presses that reads as key chatter. One overshot step settles outside the gap for good.
    let out = gap + 1.6;
    ctx.move_to(peer_pos.0 + ux * out, peer_pos.2 + uz * out, 900, 1.0)
        .await;
}

/// Nearest live chaser to the prey (or nearest peer for a generic caller), based only on current
/// replicated positions. The returned distance is therefore suitable for a real capture claim.
fn nearest_peer(ctx: &Ctx) -> Option<(usize, f64, (f64, f64, f64))> {
    let own = ctx.position()?;
    let mut nearest: Option<(usize, f64, (f64, f64, f64))> = None;
    for peer in 0..ctx.player_count() {
        if peer == ctx.index() {
            continue;
        }
        let Some(pos) = ctx.peer_position(peer) else {
            continue;
        };
        let distance = ((pos.0 - own.0).powi(2) + (pos.2 - own.2).powi(2)).sqrt();
        if nearest
            .as_ref()
            .map(|(_, best, _)| distance < *best)
            .unwrap_or(true)
        {
            nearest = Some((peer, distance, pos));
        }
    }
    nearest
}
