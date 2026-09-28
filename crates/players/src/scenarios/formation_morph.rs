use super::*;

const TAU: f64 = std::f64::consts::TAU;

/// Shapes the swarm morphs between. Every table keeps raw slots at least [`SLOT_GAP`] apart, so
/// the per-bot jitter can never close two bots below [`MIN_PEER_GAP`].
const SHAPES: [&str; 6] = ["line", "ring", "wedge", "grid", "arc", "scatter"];

/// Worst-case radius of the per-bot jitter added on top of a raw slot.
const SLOT_JITTER: f64 = 0.5;

/// Raw pairwise floor for slot tables: the peer gap plus both neighbours' worst-case jitter.
const SLOT_GAP: f64 = MIN_PEER_GAP + 2.0 * SLOT_JITTER;

/// How far a formation centre may be drawn from the spawn anchor. Bounds the arena so the whole
/// swarm stays inside the recorded area while still re-forming somewhere new every round.
const CENTRE_LEASH: f64 = 14.0;

fn range(rng: &mut Mulberry32, lo: f64, hi: f64) -> f64 {
    lo + rng.next() * (hi - lo)
}

fn pick(rng: &mut Mulberry32, len: usize) -> usize {
    ((rng.next() * len as f64) as usize).min(len.saturating_sub(1))
}

fn shuffled(rng: &mut Mulberry32, len: usize) -> Vec<usize> {
    let mut order: Vec<usize> = (0..len).collect();
    for i in (1..len).rev() {
        let j = pick(rng, i + 1);
        order.swap(i, j);
    }
    order
}

/// The per-round rng every bot reconstructs identically: same swarm seed, same round, therefore
/// the same formation plan on every process member — shared geometry without any barrier.
fn round_rng(seed: u32, round: i64) -> Mulberry32 {
    Mulberry32::new(seed ^ 0x0F0E_0D0C ^ ((round as u32).wrapping_add(1)).wrapping_mul(0x9E37_79B9))
}

/// Slot offsets in the formation's local frame (forward, right); rotation and translation are
/// applied by [`plan_round`]. Each arm's spacing draw keeps the tightest pair at `SLOT_GAP`.
fn slot_table(shape: &str, n: usize, rng: &mut Mulberry32) -> Vec<(f64, f64)> {
    let nf = n as f64;
    let mid = (nf - 1.0) / 2.0;
    match shape {
        "line" => {
            let s = range(rng, SLOT_GAP, SLOT_GAP + 1.5);
            (0..n).map(|i| (0.0, (i as f64 - mid) * s)).collect()
        }
        "ring" => {
            let chord = range(rng, SLOT_GAP, SLOT_GAP + 1.5);
            let r = (chord / (2.0 * (std::f64::consts::PI / nf.max(2.0)).sin())).max(8.0);
            (0..n)
                .map(|i| {
                    let a = i as f64 / nf * TAU;
                    (a.cos() * r, a.sin() * r)
                })
                .collect()
        }
        // Rows fan back from the apex; the tightest pair is apex-to-first-row at s*sqrt(2).
        "wedge" => {
            let s = range(rng, SLOT_GAP / std::f64::consts::SQRT_2, SLOT_GAP);
            (0..n)
                .map(|i| {
                    if i == 0 {
                        (0.0, 0.0)
                    } else {
                        let row = i.div_ceil(2) as f64;
                        let side = if i % 2 == 1 { -1.0 } else { 1.0 };
                        (-row * s, side * row * s)
                    }
                })
                .collect()
        }
        "grid" => {
            let s = range(rng, SLOT_GAP, SLOT_GAP + 1.5);
            let cols = (nf.sqrt().ceil() as usize).max(1);
            let rows = n.div_ceil(cols);
            let (rmid, cmid) = ((rows as f64 - 1.0) / 2.0, (cols as f64 - 1.0) / 2.0);
            (0..n)
                .map(|i| {
                    let (r, c) = ((i / cols) as f64, (i % cols) as f64);
                    ((r - rmid) * s, (c - cmid) * s)
                })
                .collect()
        }
        "arc" => {
            let span = range(rng, 140.0, 240.0).to_radians();
            let gap = span / (nf - 1.0).max(1.0);
            let chord = range(rng, SLOT_GAP, SLOT_GAP + 1.5);
            let r = chord / (2.0 * (gap / 2.0).sin());
            (0..n)
                .map(|i| {
                    let a = (i as f64 - mid) * gap;
                    (a.cos() * r, a.sin() * r)
                })
                .collect()
        }
        // scatter: a coarse grid where each slot is kicked around inside its own cell. The kick
        // budget is exactly the spacing surplus over SLOT_GAP, so raggedness never eats the gap.
        _ => {
            let s = range(rng, SLOT_GAP + 2.0, SLOT_GAP + 3.5);
            let kick = (s - SLOT_GAP) / 2.0;
            let cols = (nf.sqrt().ceil() as usize).max(1);
            let rows = n.div_ceil(cols);
            let (rmid, cmid) = ((rows as f64 - 1.0) / 2.0, (cols as f64 - 1.0) / 2.0);
            (0..n)
                .map(|i| {
                    let (r, c) = ((i / cols) as f64, (i % cols) as f64);
                    (
                        (r - rmid) * s + range(rng, -kick, kick),
                        (c - cmid) * s + range(rng, -kick, kick),
                    )
                })
                .collect()
        }
    }
}

struct MorphPlan {
    shape: &'static str,
    centre: (f64, f64),
    heading_deg: f64,
    /// Absolute world slot positions.
    slots: Vec<(f64, f64)>,
    /// Round-local slot assignment: bot `i` walks to `slots[perm[i]]`.
    perm: Vec<usize>,
}

/// Everything a round needs, reconstructed identically by every bot from (seed, round).
fn plan_round(seed: u32, round: i64, anchor: (f64, f64), n: usize) -> MorphPlan {
    let mut rng = round_rng(seed, round);
    let shape = SHAPES[pick(&mut rng, SHAPES.len())];
    let heading = rng.next() * TAU;
    let drift_a = rng.next() * TAU;
    let drift_r = rng.next().sqrt() * CENTRE_LEASH;
    let centre = (
        anchor.0 + drift_a.cos() * drift_r,
        anchor.1 + drift_a.sin() * drift_r,
    );
    let (hc, hs) = (heading.cos(), heading.sin());
    let slots = slot_table(shape, n, &mut rng)
        .into_iter()
        .map(|(f, r)| (centre.0 + f * hc - r * hs, centre.1 + f * hs + r * hc))
        .collect();
    let perm = shuffled(&mut rng, n);
    MorphPlan {
        shape,
        centre,
        heading_deg: heading.to_degrees(),
        slots,
        perm,
    }
}

/// Keep a moving peer framed for `hold_ms`, re-looking rather than freezing on a stale head.
async fn watch_peer(ctx: &Ctx, peer: usize, hold_ms: u64) {
    let deadline = Instant::now() + Duration::from_millis(hold_ms);
    while Instant::now() < deadline && !ctx.target_met() {
        if look_at_peer(ctx, peer, 350).await.is_none() {
            sleep_ms(300).await;
        }
    }
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

/// A triangle camera sweep of exactly `span_deg`, out to one edge, across, and back.
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

/// The swarm re-forms into a fresh geometric shape every round: line to ring to wedge to grid to
/// arc to loose scatter, in whatever order the round rng deals. Slots are re-shuffled each round,
/// so bots cross the formation instead of translating in lockstep, and the gaps between morphs
/// are spent glancing at peers, sweeping the new shape, or taking a short march away and back.
///
/// There is no shared barrier anywhere: every bot reconstructs the identical round plan from
/// (swarm seed, round index) and times itself off the process-shared phase clock, so a lagging
/// bot degrades only its own slot.
pub(crate) async fn run(ctx: &mut Ctx) {
    let index = ctx.index();
    let n = ctx.player_count().max(1);
    let anchor = ctx.anchor();
    let seed = ctx.engine().seed;
    // One boot draw shared by construction: same seed, same expression, every bot.
    let period_ms = {
        let mut boot = Mulberry32::new(seed ^ 0xA11C_E5ED);
        range(&mut boot, 11_000.0, 15_000.0)
    };
    let move_timeout = ctx.cfg().move_timeout_ms.min(10_000);

    ctx.record(
        TYPE_LOOK,
        &format!("vis-morph-{index}"),
        "Formation morph: re-form into a fresh shape every round, crossing to a shuffled slot.",
        json!({"period_ms": r2(period_ms), "bot_count": n}),
    );

    let mut last_round = -1i64;
    let mut my_slot = anchor;
    let mut centre = anchor;
    while !ctx.target_met() {
        let round = ctx.engine().shared.phase_at(period_ms);
        if round != last_round {
            last_round = round;
            let plan = plan_round(seed, round, anchor, n);
            let slot = plan.slots[plan.perm[index % n]];
            let jd = ctx.rng_next() * TAU;
            let jr = ctx.rng_next() * SLOT_JITTER;
            my_slot = (slot.0 + jd.cos() * jr, slot.1 + jd.sin() * jr);
            centre = plan.centre;
            // Crossing legs are where peers fill the frame: watch the nearest one on some of them.
            let watch = ctx.rng_next() < 0.4;
            let arrived = if watch && n > 1 {
                let peer = (index + 1 + (ctx.rng_next() * (n as f64 - 1.0)) as usize) % n;
                ctx.move_to_watching_peer(my_slot.0, my_slot.1, peer, move_timeout, 1.8)
                    .await
            } else {
                ctx.move_to(my_slot.0, my_slot.1, move_timeout, 1.8).await
            };
            if !arrived {
                shake_loose(ctx).await;
            }
            ctx.record(
                TYPE_MOVE,
                &format!("vis-morph-slot-{index}-{round}"),
                &format!(
                    "Cross to the shuffled {} slot of round {round}.",
                    plan.shape
                ),
                json!({
                    "round": round,
                    "shape": plan.shape,
                    "heading_deg": r2(plan.heading_deg),
                    "centre": {"x": r2(plan.centre.0), "z": r2(plan.centre.1)},
                    "slot_target": {"x": r2(my_slot.0), "z": r2(my_slot.1)},
                    "watched_peer_on_the_way": watch,
                    "arrived": arrived,
                }),
            );
            glance_at_nearest_peer(ctx, None).await;
        } else {
            // Garnish until the next morph — drawn per bot, so the gap never locks phase.
            match (ctx.rng_next() * 4.0) as u32 {
                0 if n > 1 => {
                    let far = ctx.rng_next() < 0.5;
                    let hold = range_mut(ctx, 2_500.0, 5_000.0) as u64;
                    let peer = if far { farthest_peer(ctx) } else { None };
                    if let Some(peer) = peer {
                        watch_peer(ctx, peer, hold).await;
                    } else {
                        glance_at_nearest_peer(ctx, None).await;
                        sleep_ms(600).await;
                    }
                }
                1 => {
                    let span = range_mut(ctx, 60.0, 160.0);
                    let pitch = range_mut(ctx, -12.0, 12.0);
                    sweep(ctx, span, pitch).await;
                }
                2 => {
                    // Short march outward and back: away from the formation centre, so two
                    // marchers can only ever diverge.
                    if let Some(p) = ctx.position() {
                        let out =
                            (p.2 - centre.1).atan2(p.0 - centre.0) + range_mut(ctx, -0.8, 0.8);
                        let dist = range_mut(ctx, 4.0, 7.0);
                        let settle = ctx.cfg().look_settle_ms.max(500);
                        ctx.look(
                            (p.0 + out.cos() * 32.0, p.1 + 1.62, p.2 + out.sin() * 32.0),
                            settle,
                        )
                        .await;
                        let hold = ((dist / 4.3 * 1000.0) as u64).clamp(900, 2_200);
                        ctx.hold_visual(WalkDirection::Forward, false, hold, 0)
                            .await;
                        ctx.move_to(my_slot.0, my_slot.1, 6_000, 1.8).await;
                    }
                }
                _ => {
                    // Face the formation centre and hold the pose briefly; correct real drift.
                    if let Some(p) = ctx.position() {
                        if (p.0 - my_slot.0).hypot(p.2 - my_slot.1) > 2.0 {
                            ctx.move_to(my_slot.0, my_slot.1, 5_000, 1.8).await;
                        } else {
                            ctx.look((centre.0, p.1 + 1.0, centre.1), 400).await;
                        }
                    }
                    let pause = range_mut(ctx, 500.0, 1_100.0) as u64;
                    sleep_ms(pause).await;
                }
            }
        }
        sleep_ms(150).await;
    }
}

fn range_mut(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

#[cfg(test)]
mod tests {
    use super::{SHAPES, SLOT_GAP, plan_round};

    /// Every shape, at every swarm size we record, must keep every pair of raw slots at least
    /// SLOT_GAP apart — which leaves MIN_PEER_GAP after both bots spend their jitter budget.
    #[test]
    fn every_shape_keeps_every_pair_clear_of_the_peer_gap() {
        for seed in [1u32, 7, 2026, 0xDEAD] {
            for n in 3..=16 {
                for round in 0..8i64 {
                    let plan = plan_round(seed, round, (0.0, 0.0), n);
                    assert!(SHAPES.contains(&plan.shape));
                    assert_eq!(plan.slots.len(), n);
                    assert_eq!(plan.perm.len(), n);
                    for i in 0..n {
                        for j in (i + 1)..n {
                            let (a, b) = (plan.slots[i], plan.slots[j]);
                            let d = (a.0 - b.0).hypot(a.1 - b.1);
                            assert!(
                                d >= SLOT_GAP - 1e-6,
                                "seed={seed} n={n} round={round} shape={} pair=({i},{j}) d={d}",
                                plan.shape
                            );
                        }
                    }
                }
            }
        }
    }

    /// The same (seed, round) must yield the same plan on every bot — that determinism is the
    /// whole synchronization mechanism.
    #[test]
    fn plans_are_reproducible_across_bots() {
        let a = plan_round(2026, 5, (10.0, -4.0), 8);
        let b = plan_round(2026, 5, (10.0, -4.0), 8);
        assert_eq!(a.shape, b.shape);
        assert_eq!(a.slots, b.slots);
        assert_eq!(a.perm, b.perm);
    }
}
