use super::*;

/// Shared implementation for the toward/away walk-and-look variants.
pub(super) async fn run(ctx: &mut Ctx, away: bool) {
    const PRIMARY_MOVE_MS: u64 = 6500;
    const FALLBACK_MOVE_MS: u64 = 3000;

    let cfg = ctx.cfg().clone();
    let index = ctx.index();
    let center = ctx.anchor();
    let bot_count = ctx.player_count().max(1);
    let loop_points = bot_count.max(8);
    // Enough circumference for N stable slots, while remaining a short, readable loop near spawn.
    let radius = ((bot_count as f64 * 2.0) / std::f64::consts::TAU).clamp(5.0, 8.0);
    let mut step = 0usize;

    while !ctx.target_met() {
        // Capture the same peer before movement for both variants. Toward visibly looks now and
        // again after movement; away records this pre-move reference and performs its two-stage
        // turn after movement from the new viewpoint.
        let reference_before_move = nearest_peer(ctx);
        if let Some(peer) = reference_before_move {
            look_at_peer(ctx, peer, cfg.look_settle_ms.clamp(250, 500)).await;
            record_peer_look(ctx, peer, step, "pre_move_peer", away);
        }

        let point = (index + step) % loop_points;
        let angle = point as f64 / loop_points as f64 * std::f64::consts::TAU;
        let target = (
            center.0 + angle.cos() * radius,
            center.1 + angle.sin() * radius,
        );
        let (arrived, primary_arrived, fallback) =
            move_with_near_fallback(ctx, target, center, PRIMARY_MOVE_MS, FALLBACK_MOVE_MS).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-walklook-{index}-{step}"),
            "Walk one short leg around the spawn anchor.",
            json!({
                "variant": if away { "away" } else { "toward" },
                "loop_point": point,
                "loop_points": loop_points,
                "waypoint": {"x": r2(target.0), "z": r2(target.1)},
                "fallback_waypoint": fallback.map(|p| json!({"x": r2(p.0), "z": r2(p.1)})),
                "primary_arrived": primary_arrived,
                "arrived": arrived
            }),
        );

        if away {
            // The away direction is visually grounded in one real peer: first look directly at
            // that peer and record the reference, then turn 180 degrees from that exact bearing.
            let reference_peer = reference_before_move.or_else(|| nearest_peer(ctx));
            if let Some(peer) = reference_peer
                && let Some(peer_pos) = look_at_peer(ctx, peer, cfg.look_settle_ms.max(350)).await
            {
                ctx.record(
                    TYPE_LOOK,
                    &format!("vis-look-away-{index}-{step}-reference"),
                    "Away turn phase 1: look directly at the reference peer.",
                    json!({
                        "variant": "away",
                        "reference_peer": peer,
                        "turn_phase": 1,
                        "turn_phase_name": "reference_peer",
                        "reference_position": {"x": r2(peer_pos.0), "z": r2(peer_pos.2)},
                        "two_stage_turn": true
                    }),
                );
                if let Some(self_pos) = ctx.position() {
                    let (dx, dz) = (self_pos.0 - peer_pos.0, self_pos.2 - peer_pos.2);
                    let length = (dx * dx + dz * dz).sqrt();
                    let (ux, uz) = if length > 0.25 {
                        (dx / length, dz / length)
                    } else {
                        (angle.cos(), angle.sin())
                    };
                    let away_target = (
                        self_pos.0 + ux * 8.0,
                        self_pos.1 + 1.4,
                        self_pos.2 + uz * 8.0,
                    );
                    ctx.look(away_target, cfg.look_settle_ms.max(500)).await;
                    let hold_ms = cfg.action_hold_ms.clamp(700, 1600);
                    ctx.record(
                        TYPE_LOOK,
                        &format!("vis-look-away-{index}-{step}-hold"),
                        "Away turn phase 2: face opposite the reference peer and hold.",
                        json!({
                            "variant": "away",
                            "reference_peer": peer,
                            "turn_phase": 2,
                            "turn_phase_name": "away_hold",
                            "away_target": {"x": r2(away_target.0), "z": r2(away_target.2)},
                            "hold_ms": hold_ms,
                            "two_stage_turn": true
                        }),
                    );
                    ctx.still(hold_ms).await;
                }
            }
        } else if let Some(peer) = reference_before_move.or_else(|| nearest_peer(ctx)) {
            look_at_peer(ctx, peer, cfg.look_settle_ms.clamp(250, 500)).await;
            record_peer_look(ctx, peer, step, "post_move_peer", false);
            ctx.still(cfg.action_hold_ms.clamp(350, 900)).await;
        }

        step += 1;
        sleep_ms(cfg.operation_pause_ms.min(500)).await;
    }
}

async fn move_with_near_fallback(
    ctx: &Ctx,
    target: (f64, f64),
    center: (f64, f64),
    primary_timeout_ms: u64,
    fallback_timeout_ms: u64,
) -> (bool, bool, Option<(f64, f64)>) {
    let cfg = ctx.cfg();
    let primary_arrived = ctx
        .move_to(
            target.0,
            target.1,
            cfg.move_timeout_ms.min(primary_timeout_ms),
            cfg.goal_radius,
        )
        .await;
    if primary_arrived {
        return (true, true, None);
    }

    // Retain the same loop bearing but retry on a much closer inner ring instead of expanding to
    // another distant random point after a path failure.
    let (dx, dz) = (target.0 - center.0, target.1 - center.1);
    let length = (dx * dx + dz * dz).sqrt().max(0.001);
    let fallback = (center.0 + dx / length * 2.75, center.1 + dz / length * 2.75);
    let fallback_arrived = ctx
        .move_to(
            fallback.0,
            fallback.1,
            cfg.move_timeout_ms.min(fallback_timeout_ms),
            cfg.goal_radius,
        )
        .await;
    (fallback_arrived, false, Some(fallback))
}

fn nearest_peer(ctx: &Ctx) -> Option<usize> {
    let own = ctx.position()?;
    (0..ctx.player_count())
        .filter(|&j| j != ctx.index())
        .filter_map(|j| {
            ctx.peer_position(j).map(|p| {
                let distance2 = (p.0 - own.0).powi(2) + (p.2 - own.2).powi(2);
                (j, distance2)
            })
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(j, _)| j)
}

fn record_peer_look(ctx: &Ctx, peer: usize, step: usize, phase: &str, away: bool) {
    ctx.record(
        TYPE_LOOK,
        &format!("vis-look-{}-{step}-{phase}", ctx.index()),
        if phase == "pre_move_peer" {
            "Look toward the reference peer before moving."
        } else {
            "Look toward the reference peer after moving."
        },
        json!({
            "variant": if away { "away" } else { "toward" },
            "reference_peer": peer,
            "turn_phase_name": phase
        }),
    );
}
