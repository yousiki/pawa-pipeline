use super::*;

const TAU: f64 = std::f64::consts::TAU;

/// Topologies the relay chain re-forms into between waves.
const TOPOLOGIES: [&str; 3] = ["line", "arc", "ring"];

/// The gesture a wave carries down the chain. All displacing gestures fire along the slot's
/// dispersal bearing, so simultaneous neighbours can only translate apart, never together.
const ACTIONS: [&str; 4] = ["surge", "hop", "spin", "bow"];

/// Raw floor between adjacent slots; gestures displace along parallel/outward bearings only, so
/// the topology gap is preserved through every wave.
const SLOT_GAP: f64 = MIN_PEER_GAP + 1.0;

/// How far a topology centre may be drawn from the spawn anchor.
const CENTRE_LEASH: f64 = 10.0;

/// Slice of each cycle reserved for walking to the (freshly drifted) slot before the wave starts.
const REFORM_MS: f64 = 4_000.0;

/// Tail after the last firer so a wave always finishes inside its own cycle.
const TAIL_MS: f64 = 3_500.0;

fn range(rng: &mut Mulberry32, lo: f64, hi: f64) -> f64 {
    lo + rng.next() * (hi - lo)
}

fn pick(rng: &mut Mulberry32, len: usize) -> usize {
    ((rng.next() * len as f64) as usize).min(len.saturating_sub(1))
}

fn mixed_rng(seed: u32, salt: u32, k: i64) -> Mulberry32 {
    Mulberry32::new(seed ^ salt ^ ((k as u32).wrapping_add(1)).wrapping_mul(0x9E37_79B9))
}

/// Shared cycle clock: index of the current cycle plus milliseconds into it. Reads the same
/// process-shared epoch `SharedPlan::phase_at` uses, so every bot times the wave off one clock
/// without any barrier — a lagging bot only misses its own beat.
fn cycle_clock(ctx: &Ctx, period_ms: f64) -> (i64, f64) {
    let t0 = {
        let mut epoch = ctx.engine().shared.epoch_t0.lock();
        *epoch.get_or_insert_with(Instant::now)
    };
    let elapsed = t0.elapsed().as_secs_f64() * 1000.0;
    let period = period_ms.max(1.0);
    ((elapsed / period).floor() as i64, elapsed % period)
}

struct Topology {
    kind: &'static str,
    centre: (f64, f64),
    /// Absolute slot positions, indexed by bot index (chain order IS index order).
    slots: Vec<(f64, f64)>,
    /// Dispersal bearing per slot: perpendicular for a line, radially outward for arc/ring.
    out: Vec<f64>,
}

/// The chain layout for one topology epoch, reconstructed identically by every bot.
fn topology(seed: u32, epoch: i64, anchor: (f64, f64), n: usize) -> Topology {
    let mut rng = mixed_rng(seed, 0x7A9E_11B3, epoch);
    let kind = TOPOLOGIES[pick(&mut rng, TOPOLOGIES.len())];
    let heading = rng.next() * TAU;
    let drift_a = rng.next() * TAU;
    let drift_r = rng.next().sqrt() * CENTRE_LEASH;
    let centre = (
        anchor.0 + drift_a.cos() * drift_r,
        anchor.1 + drift_a.sin() * drift_r,
    );
    let s = range(&mut rng, SLOT_GAP, SLOT_GAP + 2.0);
    let nf = n as f64;
    let mid = (nf - 1.0) / 2.0;
    let (slots, out): (Vec<_>, Vec<_>) = match kind {
        "line" => (0..n)
            .map(|i| {
                let along = (i as f64 - mid) * s;
                (
                    (
                        centre.0 + heading.cos() * along,
                        centre.1 + heading.sin() * along,
                    ),
                    heading + std::f64::consts::FRAC_PI_2,
                )
            })
            .unzip(),
        "arc" => {
            let span = range(&mut rng, 140.0, 230.0).to_radians();
            let gap = span / (nf - 1.0).max(1.0);
            let r = s / (2.0 * (gap / 2.0).sin());
            (0..n)
                .map(|i| {
                    let a = heading + (i as f64 - mid) * gap;
                    (
                        (centre.0 + a.cos() * r, centre.1 + a.sin() * r),
                        a, // radially outward
                    )
                })
                .unzip()
        }
        _ => {
            let r = (s / (2.0 * (std::f64::consts::PI / nf.max(2.0)).sin())).max(8.0);
            (0..n)
                .map(|i| {
                    let a = heading + i as f64 / nf * TAU;
                    ((centre.0 + a.cos() * r, centre.1 + a.sin() * r), a)
                })
                .unzip()
        }
    };
    Topology {
        kind,
        centre,
        slots,
        out,
    }
}

struct WavePlan {
    action: &'static str,
    lag_ms: f64,
    forward: bool,
    /// Ring waves start at a drawn slot; line/arc always start at an end.
    start: usize,
    /// ±1 applied to line dispersal (which side of the line the gesture fires toward).
    flip: f64,
    /// Per-cycle micro-drift of the whole chain, so every cycle begins with a real walk.
    drift: (f64, f64),
    /// Displacement amplitude of surge/hop gestures, in blocks.
    dash: f64,
}

/// One wave's parameters, reconstructed identically by every bot from (seed, cycle).
fn wave_plan(seed: u32, cycle: i64, period_ms: f64, n: usize) -> WavePlan {
    let mut rng = mixed_rng(seed, 0x51AC_0FFE, cycle);
    let window = (period_ms - REFORM_MS - TAIL_MS).max(1_000.0);
    let lag_ms = range(&mut rng, 700.0, 1_800.0).min(window / (n.max(2) - 1) as f64);
    let action = ACTIONS[pick(&mut rng, ACTIONS.len())];
    let forward = rng.next() < 0.5;
    let start = pick(&mut rng, n.max(1));
    let flip = if rng.next() < 0.5 { 1.0 } else { -1.0 };
    let drift_a = rng.next() * TAU;
    let drift_r = range(&mut rng, 1.5, 3.5);
    let dash = range(&mut rng, 3.5, 6.0);
    WavePlan {
        action,
        lag_ms,
        forward,
        start,
        flip,
        drift: (drift_a.cos() * drift_r, drift_a.sin() * drift_r),
        dash,
    }
}

/// Position of bot `index` in this wave's firing order.
fn rank_of(plan: &WavePlan, ring: bool, n: usize, index: usize) -> usize {
    let i = if plan.forward { index } else { n - 1 - index };
    if ring { (i + plan.start) % n } else { i }
}

/// Which bot fires at rank `r` — the inverse of [`rank_of`], used to watch the wave travel.
fn index_at_rank(plan: &WavePlan, ring: bool, n: usize, r: usize) -> usize {
    let i = if ring {
        (r + n - plan.start % n) % n
    } else {
        r
    };
    if plan.forward { i } else { n - 1 - i }
}

/// Face a world-plane bearing (x = cos, z = sin) before an open-loop key hold.
async fn face(ctx: &Ctx, bearing: f64) {
    if let Some(p) = ctx.position() {
        let settle = ctx.cfg().look_settle_ms.max(500);
        ctx.look(
            (
                p.0 + bearing.cos() * 32.0,
                p.1 + 1.62,
                p.2 + bearing.sin() * 32.0,
            ),
            settle,
        )
        .await;
    }
}

/// Perform this bot's beat of the wave, then return to `home` if the gesture displaced it.
async fn fire(ctx: &mut Ctx, action: &str, bearing: f64, dash: f64, home: (f64, f64)) {
    match action {
        // Sprint (or walk) a few blocks out along the dispersal bearing.
        "surge" => {
            face(ctx, bearing).await;
            let sprint = ctx.rng_next() < 0.6;
            let speed = if sprint { 5.6 } else { 4.3 };
            let hold = ((dash / speed * 1000.0) as u64).clamp(700, 1_800);
            ctx.hold_visual(WalkDirection::Forward, sprint, hold, 0)
                .await;
        }
        // Bound outward with jump pulses — the classic stadium-wave pop.
        "hop" => {
            face(ctx, bearing).await;
            let hold = 1_000 + (ctx.rng_next() * 500.0) as u64;
            ctx.hold_visual(WalkDirection::Forward, false, hold, 350)
                .await;
        }
        // A full pirouette in three camera drags; no displacement.
        "spin" => {
            let sign = if ctx.rng_next() < 0.5 { 1.0 } else { -1.0 };
            for _ in 0..3 {
                let pitch = (ctx.rng_next() - 0.5) * 16.0;
                ctx.drag_view(sign * 120.0, pitch, 1_500).await;
            }
        }
        // Dip the camera, step back, rise — a bow that reads on both the pose and the position.
        _ => {
            face(ctx, bearing).await;
            ctx.drag_view(0.0, 32.0, 700).await;
            let hold = 800 + (ctx.rng_next() * 300.0) as u64;
            ctx.hold_visual(WalkDirection::Backward, false, hold, 0)
                .await;
            ctx.drag_view(0.0, -32.0, 700).await;
        }
    }
    if let Some(p) = ctx.position()
        && (p.0 - home.0).hypot(p.2 - home.1) > 1.8
    {
        ctx.move_to(home.0, home.1, 5_000, 1.5).await;
    }
}

/// Bots stand in a chain — line, arc, or ring — and a gesture travels down it like a stadium
/// wave: each bot fires `lag` after its upstream neighbour, watching the wave come, then watching
/// it go. Waveform (surge / hop / spin / bow), speed, travel direction and the chain topology all
/// re-draw as the episode runs, and the whole chain drifts a few blocks every cycle so each wave
/// begins with a real walk to a fresh slot.
///
/// Relay timing is pure time-phase off the process-shared epoch clock — no barriers: every bot
/// reconstructs the identical wave plan from (seed, cycle) and fires at its own offset, so a
/// stalled bot leaves a hole in the wave instead of freezing the chain.
pub(crate) async fn run(ctx: &mut Ctx) {
    let index = ctx.index();
    let n = ctx.player_count().max(1);
    let anchor = ctx.anchor();
    let seed = ctx.engine().seed;
    let (period_ms, cycles_per_epoch) = {
        let mut boot = Mulberry32::new(seed ^ 0xBEE5_1000);
        (
            range(&mut boot, 13_000.0, 17_000.0),
            2 + pick(&mut boot, 3) as i64,
        )
    };
    let move_timeout = ctx.cfg().move_timeout_ms.min(8_000);

    ctx.record(
        TYPE_LOOK,
        &format!("vis-wave-{index}"),
        "Wave relay: hold a chain slot and pass a gesture down the chain like a stadium wave.",
        json!({
            "period_ms": r2(period_ms),
            "cycles_per_epoch": cycles_per_epoch,
            "bot_count": n,
        }),
    );

    let mut last_cycle = -1i64;
    while !ctx.target_met() {
        let (cycle, _) = cycle_clock(ctx, period_ms);
        if cycle == last_cycle {
            sleep_ms(150).await;
            continue;
        }
        last_cycle = cycle;
        let topo = topology(seed, cycle / cycles_per_epoch, anchor, n);
        let plan = wave_plan(seed, cycle, period_ms, n);
        let ring = topo.kind == "ring";
        let raw = topo.slots[index % n];
        let home = (raw.0 + plan.drift.0, raw.1 + plan.drift.1);
        let rank = rank_of(&plan, ring, n, index % n);
        let fire_at = REFORM_MS + rank as f64 * plan.lag_ms;
        let bearing = if ring {
            topo.out[index % n]
        } else {
            topo.out[index % n]
                + if plan.flip < 0.0 {
                    std::f64::consts::PI
                } else {
                    0.0
                }
        };

        // 1. Walk to this cycle's slot, sometimes framing the upstream neighbour on the way.
        let upstream = (rank + n - 1) % n;
        let arrived = if n > 1 && rank > 0 && ctx.rng_next() < 0.5 {
            let peer = index_at_rank(&plan, ring, n, upstream);
            ctx.move_to_watching_peer(home.0, home.1, peer, move_timeout, 1.5)
                .await
        } else {
            ctx.move_to(home.0, home.1, move_timeout, 1.5).await
        };
        if !arrived {
            shake_loose(ctx).await;
        }
        ctx.record(
            TYPE_MOVE,
            &format!("vis-wave-slot-{index}-{cycle}"),
            &format!(
                "Take rank {rank} on the {} chain for a {} wave.",
                topo.kind, plan.action
            ),
            json!({
                "cycle": cycle,
                "topology": topo.kind,
                "action": plan.action,
                "rank": rank,
                "lag_ms": r2(plan.lag_ms),
                "forward": plan.forward,
                "slot": {"x": r2(home.0), "z": r2(home.1)},
                "arrived": arrived,
            }),
        );

        // 2. Wait for this bot's beat, tracking whichever peer is firing right now.
        let mut missed = false;
        loop {
            let (c, t) = cycle_clock(ctx, period_ms);
            if c != cycle {
                missed = true;
                break;
            }
            if t >= fire_at || ctx.target_met() {
                break;
            }
            let firing = ((t - REFORM_MS) / plan.lag_ms).floor();
            if n > 1 {
                // Track the beat that is live right now, or the upstream neighbour before the
                // wave reaches this side (look_at_peer records nothing, so the tracking is free).
                let r = if firing >= 0.0 && (firing as usize) < rank {
                    firing as usize
                } else {
                    upstream
                };
                look_at_peer(ctx, index_at_rank(&plan, ring, n, r), 300).await;
            }
            sleep_ms(200).await;
        }
        if missed || ctx.target_met() {
            continue;
        }

        // 3. Fire this bot's beat of the wave.
        fire(ctx, plan.action, bearing, plan.dash, home).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-wave-fire-{index}-{cycle}"),
            &format!("Fire the {} beat at rank {rank}.", plan.action),
            json!({
                "cycle": cycle,
                "action": plan.action,
                "rank": rank,
                "bearing_deg": r2(bearing.to_degrees()),
                "dash": r2(plan.dash),
            }),
        );

        // 4. Ride out the cycle watching the wave move on downstream.
        loop {
            let (c, t) = cycle_clock(ctx, period_ms);
            if c != cycle || ctx.target_met() {
                break;
            }
            let firing = ((t - REFORM_MS) / plan.lag_ms).floor();
            if n > 1 && firing >= 0.0 && (firing as usize) < n {
                // The wave is still travelling: keep whoever is firing in frame, downstream or
                // not — after this bot's own beat every live beat is worth watching.
                let peer = index_at_rank(&plan, ring, n, firing.max(rank as f64) as usize);
                look_at_peer(ctx, peer, 300).await;
            } else if ctx.rng_next() < 0.06 && n > 1 {
                glance_at_nearest_peer(ctx, None).await;
            } else {
                ctx.look((topo.centre.0, anchor_eye_y(ctx), topo.centre.1), 350)
                    .await;
            }
            sleep_ms(250).await;
        }
    }
}

/// A look height for the chain centre: the bot's own eye level, so the gaze stays level.
fn anchor_eye_y(ctx: &Ctx) -> f64 {
    ctx.position().map(|p| p.1 + 1.0).unwrap_or(70.0)
}

#[cfg(test)]
mod tests {
    use super::{REFORM_MS, SLOT_GAP, TAIL_MS, index_at_rank, rank_of, topology, wave_plan};

    /// Every topology keeps every pair of slots clear of the raw gap, at every recorded size.
    #[test]
    fn every_topology_keeps_every_pair_clear_of_the_gap() {
        for seed in [1u32, 7, 2026] {
            for n in 4..=16 {
                for epoch in 0..6i64 {
                    let topo = topology(seed, epoch, (0.0, 0.0), n);
                    for i in 0..n {
                        for j in (i + 1)..n {
                            let (a, b) = (topo.slots[i], topo.slots[j]);
                            let d = (a.0 - b.0).hypot(a.1 - b.1);
                            assert!(
                                d >= SLOT_GAP - 1e-6,
                                "seed={seed} n={n} epoch={epoch} kind={} pair=({i},{j}) d={d}",
                                topo.kind
                            );
                        }
                    }
                }
            }
        }
    }

    /// The last firer plus the tail must always fit inside the cycle, and the rank maps must be
    /// mutual inverses — that is what lets a waiting bot find the peer that is firing right now.
    #[test]
    fn waves_fit_their_cycle_and_rank_maps_invert() {
        for seed in [1u32, 2026] {
            for n in [4usize, 8, 16, 32] {
                for cycle in 0..8i64 {
                    let period = 13_000.0;
                    let plan = wave_plan(seed, cycle, period, n);
                    let last = REFORM_MS + (n - 1) as f64 * plan.lag_ms;
                    assert!(last + TAIL_MS <= period + 1e-6, "n={n} lag={}", plan.lag_ms);
                    for ring in [false, true] {
                        for i in 0..n {
                            let r = rank_of(&plan, ring, n, i);
                            assert!(r < n);
                            assert_eq!(index_at_rank(&plan, ring, n, r), i, "ring={ring} i={i}");
                        }
                    }
                }
            }
        }
    }
}
