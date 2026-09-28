use super::*;

/// One pivot stays near the live group while the other bots step around it on a single shared ring
/// centred on the pivot's live position, so an orbiter's distance to the pivot *is* the ring radius
/// and its distance to its neighbours is the chord between adjacent slots. Both are sized off
/// `bot_count` and the inward gaze keeps everyone mutually visible without anyone standing inside
/// anyone else.
///
/// Slot phase is a pure function of episode time rather than a per-iteration accumulator: bots that
/// spend a term as the pivot, or that lose a round to a slow path, rejoin on the slot they would
/// have held anyway instead of drifting onto a neighbour's.
pub(super) async fn run(ctx: &mut Ctx) {
    /// Wall-clock time one `angular_step` of the orbit takes. Sized so the tangential speed stays
    /// under a walk even on the widest ring a 32-bot swarm needs (~23 blocks), which is also about
    /// one full lap per pivot term.
    const STEP_MS: f64 = 2_500.0;

    let cfg = ctx.cfg().clone();
    let index = ctx.index();
    let bot_count = ctx.player_count().max(1);
    let base_angle = (index as f64 / bot_count as f64) * std::f64::consts::TAU;
    let mut last_phase: i64 = -1;
    // Consecutive refused goals. Two in a row is terrain the planner cannot solve from here, not a
    // slot that happened to be busy.
    let mut misses = 0u32;
    loop {
        let (phase, pivot, direction, angular_step, radius) = {
            let shared = &ctx.engine().shared;
            // The seeded term is 50-80 s, which is most of a 120-second episode: only two of four
            // bots ever took a turn. Where the episode length is known, fit four terms into it so
            // the role keeps rotating; an open-ended episode keeps the seeded term.
            let term = match cfg.episode_duration_ms {
                0 => shared.pivot_term_ms,
                total => shared.pivot_term_ms.min((total / 4) as f64).max(8_000.0),
            };
            let phase = shared.phase_at(term);
            let pivot = shared.pivot_order[(phase as usize) % bot_count];
            // Fine steps read as an orbit rather than repeated teleports across the circle — but
            // not TOO fine: at π/18 the chord on an 8-block ring is ~1.4 blocks, a 0.4 s key tap,
            // and an episode of those is 40% key chatter. π/12..π/8 keeps each step a real stride
            // (2-3 blocks) while the tangential speed stays under a walk.
            let angular_step = shared
                .angular_step_rad
                .clamp(std::f64::consts::PI / 12.0, std::f64::consts::PI / 8.0);
            // One ring for the whole swarm, because a per-bot radius only guarantees spacing when
            // the rings are a peer gap apart — the seeded plan draws them three blocks apart, which
            // two bots on neighbouring slots close to under two. The seeded radii still widen the
            // ring; they can no longer shrink it below what this swarm size needs.
            let radius = ring_radius(bot_count).max(shared.radii.first().copied().unwrap_or(0.0));
            (phase, pivot, shared.direction, angular_step, radius)
        };
        let phase_changed = phase != last_phase;
        if phase_changed {
            last_phase = phase;
            ctx.record(
                TYPE_LOOK,
                &format!("vis-orbit-term-{index}-p{phase}"),
                &format!("Orbit term {phase}: bot {pivot} is the pivot."),
                json!({"pivot": pivot, "role": if index == pivot { "pivot" } else { "orbiter" }}),
            );
        }

        if index == pivot {
            // Anchor at the live centroid, not the seeded random centre. This target is necessarily
            // beside the current players and therefore avoids a long, potentially unreachable trip.
            let centre = ctx
                .group_center()
                .map(|p| (p.0, p.2))
                .or_else(|| ctx.position().map(|p| (p.0, p.2)))
                .unwrap_or(ctx.anchor());
            // Pace inside the ring rather than standing on the centroid. Holding the centre exactly
            // meant the pivot only ever moved when the centroid drifted two blocks, which on a term
            // this long is a bot standing still for half a minute at a time. The paces stay well
            // inside `radius`, so the ring keeps its clearance to the orbiters.
            let step = ctx.rng_next() * std::f64::consts::TAU;
            let reach = 2.0 + ctx.rng_next() * (radius * 0.25);
            let target = (centre.0 + step.cos() * reach, centre.1 + step.sin() * reach);
            let needs_move = phase_changed
                || ctx
                    .position()
                    .map(|p| ((p.0 - target.0).powi(2) + (p.2 - target.1).powi(2)).sqrt() > 1.5)
                    .unwrap_or(true);
            if needs_move {
                let arrived = ctx
                    .move_to(target.0, target.1, cfg.move_timeout_ms.min(6_000), 1.5)
                    .await;
                ctx.record(
                    TYPE_MOVE,
                    &format!("vis-orbit-pivot-{index}-p{phase}-{}", ctx.elapsed_ms()),
                    &format!("Pivot settles near the live group centre for term {phase}."),
                    json!({
                        "pivot": pivot,
                        "center": {"x": r2(target.0), "z": r2(target.1)},
                        "arrived": arrived,
                    }),
                );
                misses = if arrived { 0 } else { misses + 1 };
                if misses >= 2 {
                    misses = 0;
                    shake_loose(ctx).await;
                }
            }

            // For two bots this continuously faces the sole orbiter. For larger groups it rotates
            // through peers without sweeping away from all of them.
            let mut watched = false;
            for offset in 1..bot_count {
                let peer = (pivot + offset) % bot_count;
                if look_at_peer(ctx, peer, 250).await.is_some() {
                    watched = true;
                    ctx.record(
                        TYPE_LOOK,
                        &format!(
                            "vis-pivot-watch-{index}-p{phase}-{peer}-{}",
                            ctx.elapsed_ms()
                        ),
                        &format!("Pivot watches orbiter bot {peer}."),
                        json!({"target_peer": peer}),
                    );
                    break;
                }
            }
            if !watched {
                sleep_ms(250).await;
            }
        } else if let Some(pivot_pos) = ctx.peer_position(pivot) {
            let angle = base_angle + direction * angular_step * (ctx.elapsed_ms() as f64 / STEP_MS);
            let tx = pivot_pos.0 + angle.cos() * radius;
            let tz = pivot_pos.2 + angle.sin() * radius;
            let arrived = ctx
                .move_to_watching_peer(tx, tz, pivot, cfg.move_timeout_ms.min(6_000), 1.5)
                .await;
            ctx.record(
                TYPE_MOVE,
                &format!("vis-orbit-{index}-{}", ctx.elapsed_ms()),
                &format!("Orbit pivot bot {pivot} at radius {}.", r2(radius)),
                json!({
                    "pivot": pivot,
                    "radius": r2(radius),
                    "angle": r2(angle),
                    "slot_target": {"x": r2(tx), "z": r2(tz)},
                    "arrived": arrived,
                }),
            );
            misses = if arrived { 0 } else { misses + 1 };
            if misses >= 2 {
                misses = 0;
                shake_loose(ctx).await;
            }
            // Reacquire the live pivot after every real movement, including a failed path attempt.
            look_at_peer(ctx, pivot, 300).await;
        } else {
            sleep_ms(300).await;
        }

        sleep_ms(350).await;
        if ctx.target_met() {
            break;
        }
    }
}
