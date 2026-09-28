use super::*;

/// Return a compact formation slot measured in forward/right coordinates. Adjacent slots stay
/// within 3–5 blocks even for the two-bot case, including the circle (whose old diameter was
/// four times the configured spacing).
fn compact_slot_offset(index: usize, bot_count: usize, shape: &str, spacing: f64) -> (f64, f64) {
    let i = index as f64;
    let n = bot_count.max(1) as f64;
    let midpoint = (n - 1.0) / 2.0;
    match shape {
        "line" => (0.0, (i - midpoint) * spacing),
        "column" => ((midpoint - i) * spacing, 0.0),
        "wedge" if index > 0 => {
            let row = index.div_ceil(2) as f64;
            let side = if index % 2 == 1 { -1.0 } else { 1.0 };
            (-row * spacing, side * row * spacing)
        }
        "wedge" => (0.0, 0.0),
        _ if bot_count > 1 => {
            let angle = (i / n) * std::f64::consts::TAU;
            let radius = spacing / (2.0 * (std::f64::consts::PI / n).sin());
            (angle.cos() * radius, angle.sin() * radius)
        }
        _ => (0.0, 0.0),
    }
}

/// Turn the seeded long-range waypoints into a shared course of short, synchronized steps. The
/// source legs still provide each step's direction and formation, but no bot is sent across the
/// whole landmark area (which made its partner disappear from low-resolution recordings).
fn short_leg(legs: &[FormationLeg], phase: i64, anchor: (f64, f64)) -> (f64, f64, f64, String) {
    if legs.is_empty() {
        return (anchor.0, anchor.1, 0.0, "line".to_string());
    }
    let phase = phase.max(0) as usize;
    let len = legs.len();
    let mut x = anchor.0;
    let mut z = anchor.1;
    let mut heading = 0.0;
    for step in 0..=phase {
        let current = &legs[step % len];
        let previous = &legs[(step + len - 1) % len];
        let dx = current.x - previous.x;
        let dz = current.z - previous.z;
        let magnitude = (dx * dx + dz * dz).sqrt();
        if magnitude > 0.001 {
            heading = dz.atan2(dx);
            x += heading.cos() * 4.0;
            z += heading.sin() * 4.0;
        }
    }
    (x, z, heading, legs[phase % len].shape.clone())
}

/// The whole swarm walks a seeded route holding a compact formation. Legs are short and
/// time-sliced so every bot moves together, then settles looking at its nearest partner.
pub(super) async fn run(ctx: &mut Ctx) {
    let cfg = ctx.cfg().clone();
    let index = ctx.index();
    let bot_count = ctx.player_count().max(1);
    let anchor = ctx.anchor();
    let mut last_leg: i64 = -1;
    loop {
        let (leg_idx, leg_x, leg_z, heading, shape, spacing) = {
            let shared = &ctx.engine().shared;
            let period_ms = shared.leg_period_ms.clamp(8_000.0, 12_000.0);
            let leg_idx = shared.phase_at(period_ms);
            let (x, z, heading, shape) = short_leg(&shared.legs, leg_idx, anchor);
            (
                leg_idx,
                x,
                z,
                heading,
                shape,
                shared.slot_spacing.clamp(3.0, 3.5),
            )
        };
        let off = compact_slot_offset(index, bot_count, &shape, spacing);
        let mut requested_tx = leg_x + heading.cos() * off.0 - heading.sin() * off.1;
        let mut requested_tz = leg_z + heading.sin() * off.0 + heading.cos() * off.1;
        let peer = (bot_count > 1).then(|| if index == 0 { 1 } else { index - 1 });
        if leg_idx != last_leg {
            last_leg = leg_idx;
            // The random-walk course can hand out a slot target the bot already stands on:
            // consecutive waypoint deltas anti-correlate, and the slot offset rotates with the
            // flipped heading, so the net per-leg target motion collapses under the 1.5 arrival
            // radius (measured 0.35–1.45 blocks on the 08-19 audit — the move returns instantly
            // and the leg degenerates into a 12 s glance loop). A too-near target is pushed out
            // along the leg heading into a real walk.
            if let Some(p) = ctx.position() {
                let d = ((requested_tx - p.0).powi(2) + (requested_tz - p.2).powi(2)).sqrt();
                if d < 4.0 {
                    requested_tx = p.0 + heading.cos() * 6.0;
                    requested_tz = p.2 + heading.sin() * 6.0;
                }
            }
            let mut final_tx = requested_tx;
            let mut final_tz = requested_tz;
            let mut fallback = false;
            let mut arrived = if let Some(peer) = peer {
                ctx.move_to_watching_peer(
                    requested_tx,
                    requested_tz,
                    peer,
                    cfg.move_timeout_ms.min(8_000),
                    1.5,
                )
                .await
            } else {
                ctx.move_to(
                    requested_tx,
                    requested_tz,
                    cfg.move_timeout_ms.min(8_000),
                    1.5,
                )
                .await
            };

            // If terrain blocks the shared course, regroup around the live swarm centre instead of
            // leaving one bot stranded off-camera. The event reports the result of this real retry.
            if !arrived && let Some(center) = ctx.group_center() {
                fallback = true;
                let fallback_off = compact_slot_offset(index, bot_count, &shape, spacing.min(3.25));
                final_tx =
                    center.0 + heading.cos() * fallback_off.0 - heading.sin() * fallback_off.1;
                final_tz =
                    center.2 + heading.sin() * fallback_off.0 + heading.cos() * fallback_off.1;
                arrived = if let Some(peer) = peer {
                    ctx.move_to_watching_peer(
                        final_tx,
                        final_tz,
                        peer,
                        cfg.move_timeout_ms.min(6_000),
                        1.5,
                    )
                    .await
                } else {
                    ctx.move_to(final_tx, final_tz, cfg.move_timeout_ms.min(6_000), 1.5)
                        .await
                };
            }

            ctx.record(
                TYPE_MOVE,
                &format!("vis-leg-{index}-{leg_idx}"),
                &format!("Hold {shape} formation slot {index} on short leg {leg_idx}."),
                json!({
                    "formation": shape,
                    "leg": leg_idx,
                    "waypoint": {"x": r2(leg_x), "z": r2(leg_z)},
                    "requested_slot_target": {"x": r2(requested_tx), "z": r2(requested_tz)},
                    "slot_target": {"x": r2(final_tx), "z": r2(final_tz)},
                    "fallback_to_group": fallback,
                    "arrived": arrived,
                }),
            );
            if bot_count > 1 {
                glance_at_nearest_peer(ctx, None).await;
            }
            sleep_ms(500).await;
        } else {
            // Correct only meaningful drift, then keep the partner in frame while the synchronized
            // leg timer runs out. Avoid sweeps: on a two-bot recording they repeatedly hid the mate.
            if let Some(self_p) = ctx.position()
                && ((requested_tx - self_p.0).powi(2) + (requested_tz - self_p.2).powi(2)).sqrt()
                    > 2.0
            {
                if let Some(peer) = peer {
                    ctx.move_to_watching_peer(
                        requested_tx,
                        requested_tz,
                        peer,
                        cfg.move_timeout_ms.min(5_000),
                        1.5,
                    )
                    .await;
                } else {
                    ctx.move_to(
                        requested_tx,
                        requested_tz,
                        cfg.move_timeout_ms.min(5_000),
                        1.5,
                    )
                    .await;
                }
            }
            if bot_count > 1 {
                glance_at_nearest_peer(ctx, None).await;
            }
            sleep_ms(500).await;
        }
        if ctx.target_met() {
            break;
        }
    }
}
