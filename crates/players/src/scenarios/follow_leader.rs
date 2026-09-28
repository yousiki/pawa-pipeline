use super::*;

/// A rotating leader roams seeded waypoints; followers repeatedly retarget behind the leader's
/// measured direction of travel, hold a readable 3–5 block gap, and keep the leader in view.
pub(super) async fn run(ctx: &mut Ctx) {
    let index = ctx.index();
    let bot_count = ctx.player_count().max(1);
    let anchor = ctx.anchor();
    let mut waypoint_cursor: usize = 0;
    let mut last_phase: i64 = -1;
    let mut last_leader_position: Option<(f64, f64, f64)> = None;
    const COMPACT_ROUTE: [(f64, f64); 8] = [
        (-6.0, 0.0),
        (-4.0, -5.0),
        (0.0, -7.0),
        (5.0, -4.0),
        (7.0, 0.0),
        (4.0, 5.0),
        (0.0, 7.0),
        (-5.0, 4.0),
    ];
    let mut leader_direction: Option<(f64, f64)> = None;
    loop {
        let (phase, leader, waypoint, follow_gap) = {
            let shared = &ctx.engine().shared;
            let phase = shared.phase_at(shared.leader_term_ms);
            let leader = shared.leader_order[(phase as usize) % bot_count];
            let wp_idx = ((phase as usize) * 2 + waypoint_cursor) % COMPACT_ROUTE.len();
            let offset = COMPACT_ROUTE[wp_idx];
            let waypoint = (anchor.0 + offset.0, anchor.1 + offset.1);
            (phase, leader, waypoint, shared.follow_gap)
        };
        if phase != last_phase {
            last_phase = phase;
            last_leader_position = None;
            leader_direction = None;
            ctx.record(
                TYPE_LOOK,
                &format!("vis-term-{index}-p{phase}"),
                &format!("Leadership term {phase}: bot {leader} leads."),
                json!({"leader": leader, "role": if index == leader { "leader" } else { "follower" }}),
            );
        }
        if index == leader {
            waypoint_cursor += 1;
            let arrived = ctx.move_default(waypoint.0, waypoint.1).await;
            // Hold the compact leg until at least one follower catches the readable observation
            // band. This prevents the leader from chaining waypoints faster than followers can
            // retarget, while remaining bounded when a peer is unavailable.
            for _ in 0..40 {
                let peer_in_view_band = ctx.position().is_some_and(|own| {
                    (0..bot_count).filter(|&j| j != index).any(|j| {
                        ctx.peer_position(j).is_some_and(|peer| {
                            ((peer.0 - own.0).powi(2) + (peer.2 - own.2).powi(2)).sqrt() <= 5.0
                        })
                    })
                });
                if peer_in_view_band {
                    break;
                }
                sleep_ms(100).await;
            }
            ctx.record(
                TYPE_MOVE,
                &format!("vis-lead-{index}-p{phase}-{waypoint_cursor}"),
                &format!("Lead the group to compact waypoint ({}, {}).", waypoint.0, waypoint.1),
                json!({"role": "leader", "waypoint": {"x": waypoint.0, "z": waypoint.1}, "arrived": arrived}),
            );
            if let Some(peer) = nearest_peer_index(ctx) {
                let target_distance =
                    ctx.position()
                        .zip(ctx.peer_position(peer))
                        .map(|(own, other)| {
                            ((other.0 - own.0).powi(2) + (other.2 - own.2).powi(2)).sqrt()
                        });
                look_at_peer(ctx, peer, 3200).await;
                ctx.record(
                    TYPE_LOOK,
                    &format!("vis-leader-lookback-{index}-p{phase}-{}", ctx.elapsed_ms()),
                    &format!("Leader looks back at follower bot {peer}."),
                    json!({"target_peer": peer, "target_distance": target_distance.map(r2)}),
                );
            }
            ctx.pause().await;
        } else if let Some(lp) = ctx.peer_position(leader) {
            let own = ctx.position();
            if let Some(prev) = last_leader_position {
                let delta = (lp.0 - prev.0, lp.2 - prev.2);
                let moved = (delta.0 * delta.0 + delta.1 * delta.1).sqrt();
                if moved >= 0.2 {
                    leader_direction = Some((delta.0 / moved, delta.1 / moved));
                }
            }
            last_leader_position = Some(lp);

            // Before two live samples establish motion, face the leader's active waypoint. Once
            // movement is observed, retain that measured heading through brief pathfinder pauses.
            let (dir_x, dir_z) = leader_direction.unwrap_or_else(|| {
                let toward = (waypoint.0 - lp.0, waypoint.1 - lp.2);
                let mag = (toward.0 * toward.0 + toward.1 * toward.1).sqrt();
                if mag >= 0.2 {
                    (toward.0 / mag, toward.1 / mag)
                } else if let Some(p) = own {
                    let away_from_follower = (lp.0 - p.0, lp.2 - p.2);
                    let mag = (away_from_follower.0 * away_from_follower.0
                        + away_from_follower.1 * away_from_follower.1)
                        .sqrt()
                        .max(0.001);
                    (away_from_follower.0 / mag, away_from_follower.1 / mag)
                } else {
                    (1.0, 0.0)
                }
            });
            let follower_count = bot_count.saturating_sub(1).max(1);
            let follower_rank = (0..bot_count)
                .filter(|&j| j != leader)
                .position(|j| j == index)
                .unwrap_or(0);
            let centred_rank = follower_rank as f64 - (follower_count as f64 - 1.0) / 2.0;
            let max_rank = ((follower_count as f64 - 1.0) / 2.0).max(1.0);
            let lateral = centred_rank / max_rank * 1.8;
            let desired_gap = follow_gap.clamp(3.2, 4.4);
            // Spread N followers across a shallow arc behind the leader without changing their
            // radial distance or collapsing outer ranks onto the same target.
            let behind = (desired_gap * desired_gap - lateral * lateral).sqrt();
            let desired = (
                lp.0 - dir_x * behind - dir_z * lateral,
                lp.2 - dir_z * behind + dir_x * lateral,
            );
            let feet_y = own
                .map(|p| p.1.floor() as i32)
                .unwrap_or(lp.1.floor() as i32);
            let target = walkable_xz_near(ctx, desired, feet_y).unwrap_or(desired);

            // Short navigation slices keep the target live instead of committing to a stale point.
            let navigation_arrived = ctx
                .move_to_watching_peer(target.0, target.1, leader, 2200, 1.5)
                .await;
            let final_leader = ctx.peer_position(leader).unwrap_or(lp);
            let final_position = ctx.position();
            let final_distance = final_position
                .map(|p| ((final_leader.0 - p.0).powi(2) + (final_leader.2 - p.2).powi(2)).sqrt());
            let arrived = final_distance
                .map(|distance| (3.0..=5.0).contains(&distance))
                .unwrap_or(false);
            look_at_peer(ctx, leader, 350).await;
            ctx.record(
                TYPE_MOVE,
                &format!("vis-follow-{index}-p{phase}-{}", ctx.elapsed_ms()),
                &format!("Follow leader bot {leader} from behind at a 3–5 block distance."),
                json!({
                    "role": "follower", "leader": leader, "arrived": arrived,
                    "navigation_arrived": navigation_arrived,
                    "distance": final_distance.map(r2), "desired_gap": r2(desired_gap),
                    "target": {"x": r2(target.0), "z": r2(target.1)},
                    "leader_direction": {"x": r2(dir_x), "z": r2(dir_z)},
                }),
            );
        } else {
            sleep_ms(400).await;
        }
        if ctx.target_met() {
            break;
        }
    }
}

fn nearest_peer_index(ctx: &Ctx) -> Option<usize> {
    let own = ctx.position()?;
    (0..ctx.player_count())
        .filter(|&j| j != ctx.index())
        .filter_map(|j| {
            ctx.peer_position(j).map(|peer| {
                let distance2 = (peer.0 - own.0).powi(2) + (peer.2 - own.2).powi(2);
                (j, distance2)
            })
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(j, _)| j)
}
