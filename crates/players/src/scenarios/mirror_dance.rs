use super::*;

const TAU: f64 = std::f64::consts::TAU;

/// Farthest a dancer can end up from its own pair centre: centre drift + widest stance + a step of
/// overshoot. The pair ring is sized so two adjacent pairs both at full reach still clear
/// [`MIN_PEER_GAP`].
const MAX_REACH: f64 = 3.5 + 4.4 + 1.5;

const MOVE_NAMES: [&str; 5] = ["advance_retreat", "shuffle", "twirl", "hop", "orbit"];

fn rand_range(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

/// Deterministic mixer both members of a pair evaluate identically — the shared phase that keeps
/// the choreography symmetric WITHOUT any barrier: each bot reads only its own clock and this hash.
fn hash2(a: u64, b: u64) -> u64 {
    let mut x = a
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(b.wrapping_mul(0xBF58_476D_1CE4_E5B9));
    x ^= x >> 31;
    x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 29)
}

/// Ring of pair centres wide enough that adjacent pairs at full reach still clear the peer gap.
fn pair_ring(pairs: usize) -> f64 {
    let n = pairs.max(2) as f64;
    let chord = MIN_PEER_GAP + 2.0 * MAX_REACH;
    (chord / (2.0 * (std::f64::consts::PI / n).sin())).max(10.0)
}

/// One dancer's ideal spot at time `t` (seconds): the pair centre drifts on a slow private circle,
/// the symmetry axis rotates on its own slower clock, and the stance width is re-drawn per beat
/// from the shared hash — so the whole figure wanders while the two sides stay symmetric.
fn dance_slot(home: (f64, f64), pair: u64, t: f64, side: f64, gap: f64) -> (f64, f64) {
    let phase = pair as f64 * 2.399_963;
    let drift_r = 2.5 + (pair % 3) as f64 * 0.5;
    let drift_w = 0.05 + 0.015 * (pair % 4) as f64;
    let cx = home.0 + (phase + drift_w * t).cos() * drift_r;
    let cz = home.1 + (phase + drift_w * t).sin() * drift_r;
    let axis = phase * 3.0 + (0.06 + 0.02 * (pair % 3) as f64) * t;
    (cx + axis.cos() * gap * side, cz + axis.sin() * gap * side)
}

/// How long the body may go without a real move before the watchdog forces one. Two checks per
/// beat put the worst frozen run at one beat (~10 s) plus this budget plus the escape leg —
/// comfortably inside the 20 s frozen gate that terrain luck broke twice (21.4 s, 23.1 s).
const STILL_BUDGET_MS: i64 = 7_000;

/// Displacement watchdog. The frozen QC gate reads POSITION only, and a dance figure whose every
/// step stalls (slot drifted into a bush, water, a pit) displaces nothing for a whole beat — so
/// every path through the loop funnels through this: note real movement, and once none has
/// happened for [`STILL_BUDGET_MS`], walk a real leg to walkable ground on some free bearing,
/// falling back to the blind-hop escape.
async fn ensure_moving(ctx: &mut Ctx, mark: &mut ((f64, f64, f64), i64)) {
    let Some(own) = ctx.position() else { return };
    let now = ctx.elapsed_ms();
    let moved =
        (own.0 - mark.0.0).hypot(own.2 - mark.0.2) >= 1.5 || (own.1 - mark.0.1).abs() >= 1.5;
    if moved {
        *mark = (own, now);
        return;
    }
    if now - mark.1 < STILL_BUDGET_MS {
        return;
    }
    let feet_y = own.1.floor() as i32;
    let mut escaped = false;
    for _ in 0..5 {
        let bearing = ctx.rng_next() * TAU;
        let reach = rand_range(ctx, 4.0, 6.0);
        let desired = (own.0 + bearing.cos() * reach, own.2 + bearing.sin() * reach);
        if let Some(target) = walkable_xz_near(ctx, desired, feet_y)
            && ctx.move_to(target.0, target.1, 5_000, 1.5).await
        {
            escaped = true;
            break;
        }
    }
    if !escaped {
        shake_loose(ctx).await;
    }
    if let Some(p) = ctx.position() {
        *mark = (p, ctx.elapsed_ms());
    }
}

/// Face the partner's head (or the drifting slot mirror for a solo dancer), returning the current
/// 2D distance to it.
async fn face_partner(ctx: &Ctx, partner: Option<usize>, fallback: (f64, f64)) -> f64 {
    if let Some(j) = partner
        && let Some(p) = look_at_peer(ctx, j, 400).await
    {
        let own = ctx.position().unwrap_or(p);
        return (p.0 - own.0).hypot(p.2 - own.2);
    }
    let own = ctx.position().unwrap_or((fallback.0, 64.0, fallback.1));
    ctx.look((fallback.0, own.1 + 1.4, fallback.1), 400).await;
    (fallback.0 - own.0).hypot(fallback.1 - own.2)
}

/// Pairs of bots dance short mirrored / rotationally symmetric step sequences facing each other:
/// advance-retreat, side shuffles, twirls, hop combos and quarter orbits, drawn per beat from a
/// shared hash of `(pair, beat)` so both sides pick the same figure from nothing but their own
/// clocks — no barrier, no waiting. The pair's centre and symmetry axis drift continuously, the
/// gaps between figures are spent glancing at the partner, and every hold length carries per-bot
/// jitter so the two action streams never become clones.
pub(crate) async fn run(ctx: &mut Ctx) {
    let index = ctx.index();
    let bot_count = ctx.player_count().max(1);
    let partner = Some(index ^ 1).filter(|&j| j < bot_count);
    let pair = (index / 2) as u64;
    let pairs = bot_count.div_ceil(2);
    let side = if index.is_multiple_of(2) { 1.0 } else { -1.0 };
    let anchor = ctx.anchor();
    let ring = pair_ring(pairs);
    let slot_angle = pair as f64 / pairs as f64 * TAU;
    let home = (
        anchor.0 + slot_angle.cos() * ring,
        anchor.1 + slot_angle.sin() * ring,
    );
    let beat_ms = 4_600 + (pair % 3) * 900;

    ctx.record(
        TYPE_MOVE,
        &format!("vis-dance-plan-{index}"),
        "Mirror dance: symmetric step figures with a partner, phased off a shared clock.",
        json!({
            "pair": pair,
            "side": side,
            "partner": partner,
            "beat_ms": beat_ms,
            "home": {"x": r2(home.0), "z": r2(home.1)},
        }),
    );

    travel_to(ctx, home, TRAVEL_HOP, 4.0).await;

    let mut move_mark = (
        ctx.position().unwrap_or((home.0, 64.0, home.1)),
        ctx.elapsed_ms(),
    );
    while !ctx.target_met() {
        ensure_moving(ctx, &mut move_mark).await;
        let beat = ctx.elapsed_ms().max(0) as u64 / beat_ms;
        let h = hash2(pair, beat);
        let gap = 3.0 + ((h >> 8) % 3) as f64 * 0.7;
        let mirror = (h >> 4) & 1 == 1;
        let figure = (h % 5) as usize;
        let t = ctx.elapsed_ms().max(0) as f64 / 1000.0;
        let slot = dance_slot(home, pair, t, side, gap);
        let mirror_slot = dance_slot(home, pair, t, -side, gap);

        // Per-bot micro delay so the two sides never lock frame-for-frame.
        sleep_ms(rand_range(ctx, 0.0, 400.0) as u64).await;

        // Recover the stance first when adrift; a step sequence from the wrong spot reads as
        // noise. The target is the nearest WALKABLE cell to the slot — a slot inside a bush or
        // slope otherwise pins the dancer against it all episode (p01, seed 2026).
        if let Some(own) = ctx.position()
            && (own.0 - slot.0).hypot(own.2 - slot.1) > 3.5
        {
            let target = walkable_xz_near(ctx, slot, own.1.floor() as i32).unwrap_or(slot);
            ctx.move_to(target.0, target.1, 6_000, 1.5).await;
        }
        let dist = face_partner(ctx, partner, mirror_slot).await;
        if ctx.target_met() {
            break;
        }

        let jitter = rand_range(ctx, 0.85, 1.2);
        // A `false` from hold_visual means the stall guard cut the press against an obstacle.
        // Grinding more holds there converts every step into a sub-0.5 s press — the round-2
        // key-chatter failure (p01, 37%) — so the figure bails and a real pathfinder leg to free
        // ground follows instead.
        let mut stalled = false;
        match figure {
            // Advance toward the partner and retreat, bounded so the pair never closes inside
            // the peer gap: each side may take at most half the slack.
            0 => {
                let slack_ms = ((dist - 5.4) / 2.0 / 4.317 * 1000.0).clamp(0.0, 1_400.0);
                let fwd = (slack_ms * jitter) as u64;
                if fwd > 550 {
                    stalled |= !ctx.hold_visual(WalkDirection::Forward, false, fwd, 0).await;
                    sleep_ms(rand_range(ctx, 150.0, 450.0) as u64).await;
                    stalled |= !ctx
                        .hold_visual(WalkDirection::Backward, false, fwd, 0)
                        .await;
                } else {
                    let back = rand_range(ctx, 700.0, 1_100.0) as u64;
                    stalled |= !ctx
                        .hold_visual(WalkDirection::Backward, false, back, 0)
                        .await;
                    sleep_ms(rand_range(ctx, 150.0, 450.0) as u64).await;
                    let return_ms = rand_range(ctx, 600.0, 950.0) as u64;
                    stalled |= !ctx
                        .hold_visual(WalkDirection::Forward, false, return_ms, 0)
                        .await;
                }
            }
            // Side shuffle, facing the partner throughout. Same key on both sides circles the
            // midpoint (rotational symmetry); opposite keys translate the figure (mirror).
            1 => {
                let lead = mirror == (side > 0.0);
                let reps = 2 + (ctx.rng_next() * 2.0) as u32;
                for rep in 0..reps {
                    if ctx.target_met() {
                        break;
                    }
                    let left = lead == (rep % 2 == 0);
                    let dir = if left {
                        WalkDirection::Left
                    } else {
                        WalkDirection::Right
                    };
                    let ms = (rand_range(ctx, 700.0, 1_300.0) * jitter) as u64;
                    if !ctx.hold_visual(dir, false, ms, 0).await {
                        stalled = true;
                        break;
                    }
                }
            }
            // Twirl: a full camera revolution in three sweeps, spin sign from the symmetry flag,
            // with a short side-step between sweeps. The steps are what keeps two twirl beats in
            // a row from parking the body past the 20 s frozen gate (measured 21.4 s on seed 2026
            // round 1 when this figure was camera-only).
            2 => {
                let spin = if mirror { side } else { 1.0 };
                for sweep in 0..3u32 {
                    if ctx.target_met() {
                        break;
                    }
                    let wobble = rand_range(ctx, -10.0, 10.0);
                    ctx.drag_view(120.0 * spin, wobble, 2_400).await;
                    let dir = if sweep.is_multiple_of(2) == (side > 0.0) {
                        WalkDirection::Left
                    } else {
                        WalkDirection::Right
                    };
                    let step_ms = rand_range(ctx, 550.0, 850.0) as u64;
                    if !ctx.hold_visual(dir, false, step_ms, 0).await {
                        stalled = true;
                        break;
                    }
                }
            }
            // Hop combo toward and away, one jump pulse per hold. Skipped forward when the pair
            // is already tight.
            3 => {
                let ms = (800.0 * jitter) as u64;
                if dist > 7.5 {
                    stalled |= !ctx
                        .hold_visual(WalkDirection::Forward, false, ms, 550)
                        .await;
                }
                stalled |= !ctx
                    .hold_visual(WalkDirection::Backward, false, ms, 550)
                    .await;
            }
            // Quarter orbit around the pair centre in short watched legs; the symmetry flag picks
            // whether the two sides wheel together or against each other. Opposed wheels close
            // the pair's angular separation by 2 steps per leg, so the mirrored step is small
            // enough that three legs still leave the chord above the peer gap at the tightest
            // stance (2 * 3.0 * sin((pi - 6*0.18)/2) = 5.1 blocks).
            _ => {
                let (wheel, step) = if mirror { (side, 0.18) } else { (1.0, 0.35) };
                let centre = (
                    (slot.0 + mirror_slot.0) / 2.0,
                    (slot.1 + mirror_slot.1) / 2.0,
                );
                let mut angle = ctx
                    .position()
                    .map(|p| (p.2 - centre.1).atan2(p.0 - centre.0))
                    .unwrap_or(0.0);
                let radius = gap.max(2.5);
                for _ in 0..3 {
                    if ctx.target_met() {
                        break;
                    }
                    angle += wheel * step;
                    let goal = (
                        centre.0 + angle.cos() * radius,
                        centre.1 + angle.sin() * radius,
                    );
                    match partner {
                        Some(j) => {
                            ctx.move_to_watching_peer(goal.0, goal.1, j, 3_000, 1.2)
                                .await
                        }
                        None => ctx.move_to(goal.0, goal.1, 3_000, 1.2).await,
                    };
                }
            }
        }

        if stalled && let Some(own) = ctx.position() {
            let target = walkable_xz_near(ctx, slot, own.1.floor() as i32).unwrap_or(slot);
            ctx.move_to(target.0, target.1, 6_000, 1.5).await;
        }
        ensure_moving(ctx, &mut move_mark).await;

        ctx.record(
            TYPE_MOVE,
            &format!("vis-dance-{index}-{beat}"),
            &format!(
                "Dance figure {} on beat {beat} ({}).",
                MOVE_NAMES[figure],
                if mirror { "mirrored" } else { "rotational" }
            ),
            json!({
                "beat": beat,
                "figure": MOVE_NAMES[figure],
                "mirror": mirror,
                "gap": r2(gap),
                "partner_distance": r2(dist),
            }),
        );

        // The gap between figures: hold the partner's gaze, sometimes checking the wider floor.
        face_partner(ctx, partner, mirror_slot).await;
        sleep_ms(rand_range(ctx, 500.0, 1_500.0) as u64).await;
        if ctx.rng_next() < 0.25 {
            glance_at_nearest_peer(ctx, partner).await;
        }
    }
}
