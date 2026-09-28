use super::*;

/// Bots pair off into chaser/evader in compact anchor-local arenas. The chaser pursues in short,
/// opponent-facing bursts; a successful close approach plus real attack swing swaps the roles.
/// After a tag both bots briefly hold eye contact before the former chaser begins its short escape.
pub(super) async fn run(ctx: &mut Ctx) {
    let index = ctx.index();
    let (mate, pair_idx, pair_count, tag_distance) = {
        let shared = &ctx.engine().shared;
        let slot = shared.pairing.iter().position(|&v| v == index).unwrap_or(0);
        let pair_idx = (slot / 2).min(shared.pair_count.saturating_sub(1));
        let mate_slot = if slot % 2 == 0 {
            slot + 1
        } else {
            slot.saturating_sub(1)
        };
        // Odd leftover joins the last pair as a rover.
        let mate = if slot == shared.pair_count * 2 {
            shared.pairing[pair_idx * 2]
        } else {
            shared.pairing[mate_slot.min(shared.pairing.len().saturating_sub(1))]
        };
        (
            mate,
            pair_idx,
            shared.pair_count.max(1),
            shared.tag_distance,
        )
    };
    let slot = ctx
        .engine()
        .shared
        .pairing
        .iter()
        .position(|&v| v == index)
        .unwrap_or(0);
    let mut chasing = slot % 2 == 0;
    let anchor = ctx.anchor();

    // Keep every pair in a compact, readable arena around the spawn anchor. Each pair gets a
    // small ring whose adjacent goals are only a few blocks apart; unlike the shared parity
    // waypoints, these goals cannot send an evader across the full configured world bound.
    let sector_angle = ((pair_idx as f64 + 0.5) / pair_count as f64) * std::f64::consts::TAU;
    let center_radius = if pair_count == 1 { 0.0 } else { 4.5 };
    let pair_center = (
        anchor.0 + sector_angle.cos() * center_radius,
        anchor.1 + sector_angle.sin() * center_radius,
    );
    let waypoints: Vec<(f64, f64)> = (0..8)
        .map(|step| {
            let angle = sector_angle + step as f64 * std::f64::consts::TAU / 8.0;
            (
                r2(pair_center.0 + angle.cos() * 4.0),
                r2(pair_center.1 + angle.sin() * 4.0),
            )
        })
        .collect();

    let mut wp_cursor: usize = index % waypoints.len();
    let mut tags: u64 = 0;
    let mut stuck_streak: u64 = 0;
    let mut last_evade: Option<(f64, f64)> = None;
    let mut evade_until_ms = 0i64;
    ctx.record(
        TYPE_LOOK,
        &format!("vis-chase-init-{index}"),
        &format!("Pair chase: bot {index} vs bot {mate} in sector {pair_idx}."),
        json!({"mate": mate, "sector": pair_idx, "role": if chasing { "chaser" } else { "evader" }}),
    );

    loop {
        let Some(mate_pos) = ctx.peer_position(mate) else {
            sleep_ms(250).await;
            if ctx.target_met() {
                break;
            }
            continue;
        };
        let Some(self_pos) = ctx.position() else {
            sleep_ms(250).await;
            if ctx.target_met() {
                break;
            }
            continue;
        };
        let dist = ((mate_pos.0 - self_pos.0).powi(2) + (mate_pos.2 - self_pos.2).powi(2)).sqrt();

        if chasing {
            // Re-acquire before and after every short pursuit burst. move_to faces travel while it
            // is active, so keeping each burst short prevents the opponent from leaving the frame
            // for seconds at a time.
            look_at_peer(ctx, mate, 80).await;
            if dist <= tag_distance {
                look_at_peer(ctx, mate, 300).await;
                let mate_name = ctx.peer_username(mate).unwrap_or_default();
                let real_swing = ctx.attack_peer(&mate_name);
                ctx.set_pending("attack");
                ctx.record(
                    TYPE_ATTACK,
                    &format!("vis-tag-attempt-{index}-{tags}-{}", ctx.elapsed_ms()),
                    &format!(
                        "Attempt close-range tag on bot {mate}{}.",
                        if real_swing {
                            "; roles swap"
                        } else {
                            "; target not yet tracked"
                        }
                    ),
                    json!({"mate": mate, "tag_number": tags, "distance": r2(dist),
                        "tagged": real_swing, "entity_attack_issued": real_swing}),
                );
                if real_swing {
                    tags += 1;
                    chasing = false;
                    evade_until_ms = ctx.elapsed_ms() + 1800;
                    // Hold the readable tag tableau rather than immediately turning and sprinting.
                    look_at_peer(ctx, mate, 500).await;
                    sleep_ms(250).await;
                } else {
                    // Do not fabricate a successful tag when the peer entity was not attackable.
                    look_at_peer(ctx, mate, 180).await;
                    sleep_ms(120).await;
                }
            } else {
                let arrived = ctx
                    .move_to(mate_pos.0, mate_pos.2, 650, (tag_distance - 0.75).max(1.5))
                    .await;
                let refreshed_distance = match (ctx.position(), ctx.peer_position(mate)) {
                    (Some(p), Some(m)) => ((m.0 - p.0).powi(2) + (m.2 - p.2).powi(2)).sqrt(),
                    _ => f64::INFINITY,
                };
                look_at_peer(ctx, mate, 150).await;
                ctx.record(
                    TYPE_MOVE,
                    &format!("vis-pursue-{index}-{}", ctx.elapsed_ms()),
                    &format!("Pursue bot {mate} at {} blocks.", r2(refreshed_distance)),
                    json!({"role": "chaser", "mate": mate, "distance": r2(refreshed_distance),
                        "arrived": arrived}),
                );
            }
        } else {
            look_at_peer(ctx, mate, 80).await;
            wp_cursor = (wp_cursor + if stuck_streak >= 2 { 2 } else { 1 }) % waypoints.len();
            let wp = waypoints[wp_cursor];
            let arrived = ctx.move_to(wp.0, wp.1, 900, ctx.cfg().goal_radius).await;
            let self_after = ctx.position();
            let progress = match (self_after, last_evade) {
                (Some(p), Some((lx, lz))) => ((p.0 - lx).powi(2) + (p.2 - lz).powi(2)).sqrt(),
                _ => f64::INFINITY,
            };
            stuck_streak = if progress < 0.75 { stuck_streak + 1 } else { 0 };
            if let Some(p) = self_after {
                last_evade = Some((p.0, p.2));
            }
            let refreshed_distance = match (self_after, ctx.peer_position(mate)) {
                (Some(p), Some(m)) => ((m.0 - p.0).powi(2) + (m.2 - p.2).powi(2)).sqrt(),
                _ => f64::INFINITY,
            };
            look_at_peer(ctx, mate, 180).await;
            ctx.record(
                TYPE_MOVE,
                &format!("vis-evade-{index}-{}", ctx.elapsed_ms()),
                &format!(
                    "Evade along the compact pair arc toward ({}, {}); bot {mate} is {} blocks away{}.",
                    wp.0,
                    wp.1,
                    r2(refreshed_distance),
                    if stuck_streak > 0 { " [short reroute]" } else { "" }
                ),
                json!({"role": "evader", "mate": mate, "waypoint": {"x": wp.0, "z": wp.1},
                    "chaser_distance": r2(refreshed_distance), "stuck_streak": stuck_streak,
                    "arrived": arrived}),
            );
            if refreshed_distance <= tag_distance && ctx.elapsed_ms() >= evade_until_ms {
                // The paired chaser records the genuine swing. The evader observes the same real
                // close contact, pauses face-to-face, then assumes the chase role. A bot that just
                // issued a tag must first finish its compact escape so both sides cannot instantly
                // flip back into chasers while still standing inside the tag radius.
                look_at_peer(ctx, mate, 450).await;
                chasing = true;
                sleep_ms(250).await;
            }
        }
        if ctx.target_met() {
            break;
        }
    }
}
