use super::*;

const TAU: f64 = std::f64::consts::TAU;
const SLOTS: usize = 3;
const SLOT_NAMES: [&str; SLOTS] = ["slot_a", "slot_b", "slot_c"];

/// How far the ring centre may wander from the spawn anchor. The formation is supposed to re-form
/// "somewhere new" every round, not to leave the recorded arena.
const CENTRE_LEASH: f64 = 20.0;

fn rand_range(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

fn nearest_peer(ctx: &Ctx) -> Option<usize> {
    let own = ctx.position()?;
    (0..ctx.player_count())
        .filter(|&j| j != ctx.index())
        .filter_map(|j| {
            ctx.peer_position(j)
                .map(|p| (j, (p.0 - own.0).powi(2) + (p.2 - own.2).powi(2)))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(j, _)| j)
}

fn farthest_peer(ctx: &Ctx) -> Option<usize> {
    let own = ctx.position()?;
    (0..ctx.player_count())
        .filter(|&j| j != ctx.index())
        .filter_map(|j| {
            ctx.peer_position(j)
                .map(|p| (j, (p.0 - own.0).powi(2) + (p.2 - own.2).powi(2)))
        })
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(j, _)| j)
}

async fn watch_peer(ctx: &Ctx, peer: usize, hold_ms: u64) {
    let deadline = Instant::now() + Duration::from_millis(hold_ms);
    while Instant::now() < deadline && !ctx.target_met() {
        if look_at_peer(ctx, peer, 350).await.is_none() {
            sleep_ms(300).await;
        }
    }
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

async fn orbit(ctx: &Ctx, peer: usize, radius: f64, arc_deg: f64, direction: f64) -> u32 {
    // 35° slices: at the 5-9 block radii drawn below a 20° chord was under two blocks — a 0.4 s
    // key tap, and an episode of those reads as key chatter.
    let slices = (arc_deg / 35.0).ceil().max(1.0) as u32;
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

/// The group settles onto a loose ring, then every third of it crosses the open middle to the slot
/// opposite, so the formation turns itself inside out and re-forms somewhere new.
///
/// Which third crosses is `round % 3`, so the three slot groups take turns cutting through the
/// centre while the other two slide along the rim and watch them come. Everything a viewer can
/// measure — the ring centre, its radius, the crossing angle, the rim shuffle, the camera beat — is
/// drawn per bot and per round instead of being the fixed offsets the JSON listed.
pub(crate) async fn run(ctx: &mut Ctx) {
    let index = ctx.index();
    let bot_count = ctx.player_count().max(1);
    let anchor = ctx.anchor();
    let slot = index % SLOTS;
    let base_radius = ring_radius(bot_count);
    let slot_arc = TAU / bot_count as f64;
    let move_timeout = ctx.cfg().move_timeout_ms.min(15_000);

    let mut angle = index as f64 * slot_arc + rand_range(ctx, -0.25, 0.25) * slot_arc;
    let mut radius = base_radius + rand_range(ctx, -1.0, 2.5);
    let mut centre = anchor;

    ctx.record(
        TYPE_LOOK,
        &format!("vis-slot-exchange-{index}"),
        "Ring slot exchange: hold a slot on the ring, then cross the middle when this third's turn comes.",
        json!({
            "role": SLOT_NAMES[slot],
            "slot": slot,
            "base_radius": r2(base_radius),
            "slot_deg": r2(angle.to_degrees()),
        }),
    );

    let mut round = 0u32;
    while !ctx.target_met() {
        let crossing = round as usize % SLOTS == slot;
        if crossing {
            // Cut through the open middle to the slot opposite. The overshoot is what stops the
            // formation from simply rotating: it re-forms with the ring inverted.
            angle += std::f64::consts::PI + rand_range(ctx, -0.6, 0.6);
            radius = base_radius + rand_range(ctx, -1.0, 3.5);
        } else {
            // Everyone still moves: the rim slides while the crossers cut across it. The slide has
            // a floor because a draw near zero is a sub-two-block shuffle — a short key tap.
            let sign = if ctx.rng_next() < 0.5 { 1.0 } else { -1.0 };
            angle += sign * rand_range(ctx, 0.25, 0.55);
            radius += rand_range(ctx, -1.2, 1.2);
            radius = radius.clamp(base_radius - 2.0, base_radius + 4.0);
        }
        // Drift the whole ring so it re-forms somewhere new, leashed to the recorded arena.
        let drift_dir = ctx.rng_next() * TAU;
        let drift = rand_range(ctx, 0.0, 4.0);
        let moved = (
            centre.0 + drift_dir.cos() * drift,
            centre.1 + drift_dir.sin() * drift,
        );
        let leash = (moved.0 - anchor.0).hypot(moved.1 - anchor.1);
        centre = if leash <= CENTRE_LEASH {
            moved
        } else {
            (
                anchor.0 + (moved.0 - anchor.0) * CENTRE_LEASH / leash,
                anchor.1 + (moved.1 - anchor.1) * CENTRE_LEASH / leash,
            )
        };

        let scatter_dir = ctx.rng_next() * TAU;
        let scatter = rand_range(ctx, 0.0, 1.8);
        let goal = (
            centre.0 + angle.cos() * radius + scatter_dir.cos() * scatter,
            centre.1 + angle.sin() * radius + scatter_dir.sin() * scatter,
        );
        let arrived = ctx.move_to(goal.0, goal.1, move_timeout, 2.5).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-slot-move-{index}-{round}"),
            if crossing {
                "Cross the open middle to the slot opposite."
            } else {
                "Slide along the rim while another third crosses."
            },
            json!({
                "role": SLOT_NAMES[slot],
                "round": round,
                "crossing": crossing,
                "centre": {"x": r2(centre.0), "z": r2(centre.1)},
                "slot_target": {"x": r2(goal.0), "z": r2(goal.1)},
                "radius": r2(radius),
                "angle_deg": r2(angle.to_degrees()),
                "arrived": arrived,
            }),
        );
        if ctx.target_met() {
            break;
        }

        // Camera beat, drawn independently of whether this round crossed or slid.
        match (ctx.rng_next() * 3.0) as u32 {
            0 => {
                let span = rand_range(ctx, 70.0, 175.0);
                let pitch = rand_range(ctx, -12.0, 12.0);
                sweep(ctx, span, pitch).await;
                ctx.record(
                    TYPE_LOOK,
                    &format!("vis-slot-sweep-{index}-{round}"),
                    "Read the re-formed ring with one camera sweep.",
                    json!({"sweep_deg": r2(span), "pitch_deg": r2(pitch)}),
                );
            }
            1 => {
                let hold = rand_range(ctx, 3_000.0, 6_000.0) as u64;
                let far = ctx.rng_next() < 0.5;
                let peer = if far {
                    farthest_peer(ctx)
                } else {
                    nearest_peer(ctx)
                };
                if let Some(peer) = peer {
                    watch_peer(ctx, peer, hold).await;
                    ctx.record(
                        TYPE_LOOK,
                        &format!("vis-slot-watch-{index}-{round}"),
                        &format!("Track bot {peer} across the open middle."),
                        json!({"peer": peer, "farthest": far, "hold_ms": hold}),
                    );
                }
            }
            _ => {
                let orbit_radius = rand_range(ctx, 5.0, 9.0);
                let arc = rand_range(ctx, 80.0, 220.0);
                let spin = if ctx.rng_next() < 0.5 { 1.0 } else { -1.0 };
                if let Some(peer) = nearest_peer(ctx) {
                    let legs = orbit(ctx, peer, orbit_radius, arc, spin).await;
                    ctx.record(
                        TYPE_MOVE,
                        &format!("vis-slot-orbit-{index}-{round}"),
                        &format!("Circle bot {peer} while the ring settles."),
                        json!({"peer": peer, "radius": r2(orbit_radius), "arc_deg": r2(arc),
                            "direction": spin, "legs": legs}),
                    );
                }
            }
        }
        round += 1;
    }
}
