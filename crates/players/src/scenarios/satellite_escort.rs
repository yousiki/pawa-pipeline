use super::*;

const TAU: f64 = std::f64::consts::TAU;

/// Widest per-satellite addition to the base orbit radius, so two satellites of one host fly at
/// visibly different altitudes instead of a single shared ring.
const SAT_JITTER: f64 = 1.5;

/// How far a host may wander off its home slot. Bounded because the host separation budget in
/// [`host_home`] pays for two orbits plus the peer gap plus BOTH hosts' wander — but wide enough
/// that a stroll is a real held walk: round 1 leashed the host to 3 blocks and its pathfinder
/// legs recorded as sub-0.5 s taps, 56% key chatter on the host POV.
const LEASH: f64 = 5.0;

/// Below this 2D distance to the nearest peer a satellite sidesteps away before its next leg.
const CROWD_GAP: f64 = 4.0;

/// Draw in `[lo, hi)` from the per-bot stream, so a number this scenario could have hard-coded
/// varies per bot and per leg and still replays from the seed.
fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

/// How many hosts ("planets") a swarm of `n` fields; everyone else is a satellite.
fn host_count(n: usize) -> usize {
    (n / 4).max(1)
}

/// Most satellites any single host can be assigned under round-robin.
fn max_sats_per_host(n: usize) -> usize {
    let hosts = host_count(n);
    n.saturating_sub(hosts).div_ceil(hosts).max(1)
}

/// `(host, slot, members)` for satellite bot `index` (callers guarantee it is not a host):
/// round-robin over hosts so every host gets escorts at every swarm size.
fn sat_assignment(index: usize, n: usize) -> (usize, usize, usize) {
    let hosts = host_count(n);
    let s = index - hosts;
    let host = s % hosts;
    let slot = s / hosts;
    let members = (n - hosts - host).div_ceil(hosts).max(1);
    (host, slot, members)
}

/// Base orbit radius for a host escorted by `members` satellites: the ring that keeps evenly
/// slotted satellites a peer gap apart (floored at 8 by [`ring_radius`]).
fn base_sat_radius(members: usize) -> f64 {
    ring_radius(members.max(1))
}

/// Home slot of host `h`: hosts stand on their own wide ring around the anchor, separated by two
/// full satellite orbits plus the peer gap plus both hosts' wander, so two orbit systems never
/// interleave.
fn host_home(anchor: (f64, f64), h: usize, n: usize) -> (f64, f64) {
    let hosts = host_count(n);
    if hosts == 1 {
        return anchor;
    }
    let orbit = base_sat_radius(max_sats_per_host(n)) + SAT_JITTER;
    let sep = 2.0 * (orbit + LEASH) + MIN_PEER_GAP;
    let r = sep / (2.0 * (std::f64::consts::PI / hosts as f64).sin());
    let a = h as f64 / hosts as f64 * TAU;
    (anchor.0 + a.cos() * r, anchor.1 + a.sin() * r)
}

/// Nearest live peer on this shard — the crowd-guard input.
fn nearest_peer(ctx: &Ctx) -> Option<(usize, f64)> {
    let own = ctx.position()?;
    (0..ctx.player_count())
        .filter(|&j| j != ctx.index())
        .filter_map(|j| {
            ctx.peer_position(j)
                .map(|p| (j, (p.0 - own.0).hypot(p.2 - own.2)))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
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

/// Triangle camera sweep of `span_deg` total: out to one edge, across to the other, back.
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

/// A host's episode: wander slowly on a short leash around its own home slot — never far, its
/// escorts orbit its LIVE position — pausing to follow its satellites with the camera.
async fn run_host(ctx: &mut Ctx) {
    let index = ctx.index();
    let n = ctx.player_count().max(1);
    let home = host_home(ctx.anchor(), index, n);
    let timeout = ctx.cfg().move_timeout_ms.min(8_000);
    ctx.record(
        TYPE_LOOK,
        &format!("vis-sat-host-open-{index}"),
        &format!("Satellite escort: bot {index} is a wandering host planet."),
        json!({"role": "host", "home": {"x": r2(home.0), "z": r2(home.1)}}),
    );
    ctx.move_to(home.0, home.1, timeout, 2.0).await;
    let settle = ctx.cfg().look_settle_ms.max(400);
    let mut leg = 0u32;
    let mut misses = 0u32;
    while !ctx.target_met() {
        // A stroll leg: ONE HELD walk of a few blocks — never a hail of short pathfinder taps
        // (round 1 issued 1.5-3-block `move_to` goals and the host POV logged 56% key chatter
        // on them alone). The bearing is free inside the leash and homeward beyond it; every
        // fourth leg is a pathfinder correction, half of those with the camera riding on the
        // nearest escort so the host's view orbits with its moons.
        let (dx, dz) = ctx
            .position()
            .map(|p| (home.0 - p.0, home.1 - p.2))
            .unwrap_or((0.0, 0.0));
        let bearing = if dx.hypot(dz) > LEASH {
            dz.atan2(dx)
        } else {
            ctx.rng_next() * TAU
        };
        let step = vary(ctx, 3.0, 6.0);
        let correction = leg % 4 == 3;
        if correction {
            let a = ctx.rng_next() * TAU;
            let d = vary(ctx, 2.5, LEASH);
            let target = (home.0 + a.cos() * d, home.1 + a.sin() * d);
            let social = ctx.rng_next() < 0.5;
            let arrived = if social && let Some((peer, _)) = nearest_peer(ctx) {
                ctx.move_to_watching_peer(target.0, target.1, peer, timeout.min(6_000), 1.5)
                    .await
            } else {
                ctx.move_to(target.0, target.1, timeout.min(6_000), 1.5)
                    .await
            };
            misses = if arrived { 0 } else { misses + 1 };
        } else if let Some(p) = ctx.position() {
            ctx.look(
                (
                    p.0 + bearing.cos() * 16.0,
                    p.1 + 1.3,
                    p.2 + bearing.sin() * 16.0,
                ),
                settle,
            )
            .await;
            let hold = ((step / 4.3 * 1000.0) as u64).clamp(900, 2_200);
            if ctx
                .hold_visual(WalkDirection::Forward, false, hold, 400)
                .await
            {
                misses = 0;
            } else {
                misses += 1;
            }
        }
        if misses >= 2 {
            misses = 0;
            shake_loose(ctx).await;
        }
        ctx.record(
            TYPE_MOVE,
            &format!("vis-sat-host-stroll-{index}-{leg}"),
            "Host strolls a held-walk leg inside its leash.",
            json!({"leg": leg, "bearing": r2(bearing), "step": r2(step),
                "correction": correction}),
        );
        if ctx.target_met() {
            break;
        }
        // Between strolls: follow an escort across the sky, or sweep the whole orbit system.
        let roll = ctx.rng_next();
        if roll < 0.55 {
            if let Some((peer, dist)) = nearest_peer(ctx) {
                let hold = vary(ctx, 1_200.0, 2_600.0) as u64;
                watch_peer(ctx, peer, hold).await;
                ctx.record(
                    TYPE_LOOK,
                    &format!("vis-sat-host-watch-{index}-{leg}"),
                    &format!("Host follows escort bot {peer} across its orbit."),
                    json!({"leg": leg, "peer": peer, "distance": r2(dist), "hold_ms": hold}),
                );
            }
        } else if roll < 0.8 {
            let span = vary(ctx, 120.0, 220.0);
            let pitch = vary(ctx, -6.0, 10.0);
            sweep(ctx, span, pitch).await;
            ctx.record(
                TYPE_LOOK,
                &format!("vis-sat-host-sweep-{index}-{leg}"),
                "Host sweeps its own orbit system.",
                json!({"leg": leg, "sweep_deg": r2(span)}),
            );
        } else {
            glance_at_nearest_peer(ctx, None).await;
            sleep_ms(vary(ctx, 500.0, 1_200.0) as u64).await;
        }
        leg += 1;
    }
}

/// A satellite's episode: orbit its host's LIVE position — a moving-centre revolution no merged
/// scenario produces — and on a private timer defect to a different host, crossing the open
/// ground between the two orbit systems.
async fn run_satellite(ctx: &mut Ctx) {
    let index = ctx.index();
    let n = ctx.player_count().max(1);
    let hosts = host_count(n);
    let (mut host, slot, members) = sat_assignment(index, n);
    let direction = if ctx.rng_next() < 0.5 { 1.0 } else { -1.0 };
    let radius = base_sat_radius(members) + vary(ctx, 0.0, SAT_JITTER);
    let mut angle = slot as f64 / members as f64 * TAU + vary(ctx, 0.0, 0.3);
    let mut next_switch_ms = ctx.elapsed_ms() + vary(ctx, 20_000.0, 45_000.0) as i64;
    let settle = ctx.cfg().look_settle_ms.max(400);
    let timeout = ctx.cfg().move_timeout_ms.min(6_000);
    ctx.record(
        TYPE_LOOK,
        &format!("vis-sat-open-{index}"),
        &format!("Satellite escort: bot {index} orbits wandering host bot {host}."),
        json!({"role": "satellite", "host": host, "radius": r2(radius),
            "direction": direction, "slot": slot}),
    );
    let mut leg = 0u32;
    let mut misses = 0u32;
    while !ctx.target_met() {
        // Defection beat: pick a different host and cross to its orbit with hop-wise travel,
        // camera on the new host — the long leg between systems is the scenario's set piece.
        if hosts >= 2 && ctx.elapsed_ms() >= next_switch_ms {
            let mut pick = (ctx.rng_next() * (hosts - 1) as f64) as usize;
            if pick >= host {
                pick += 1;
            }
            host = pick.min(hosts - 1);
            next_switch_ms = ctx.elapsed_ms() + vary(ctx, 20_000.0, 45_000.0) as i64;
            ctx.record(
                TYPE_MOVE,
                &format!("vis-sat-defect-{index}-{leg}"),
                &format!("Satellite defects to host bot {host}, crossing between orbit systems."),
                json!({"leg": leg, "host": host}),
            );
            if let Some(hp) = ctx.peer_position(host) {
                let goal = (hp.0 + angle.cos() * radius, hp.2 + angle.sin() * radius);
                travel_to(ctx, goal, TRAVEL_HOP, 2.5).await;
                look_at_peer(ctx, host, settle).await;
            }
        }
        let Some(hp) = ctx.peer_position(host) else {
            sleep_ms(300).await;
            continue;
        };
        let chord = vary(ctx, 4.0, 6.0);
        angle += direction * chord / radius;
        let target = (
            hp.0 + angle.cos() * (radius + vary(ctx, -0.5, 0.5)),
            hp.2 + angle.sin() * (radius + vary(ctx, -0.5, 0.5)),
        );

        // Crowd guard: a sibling satellite converging on the same arc — sidestep away from it
        // with one held press rather than shoving through.
        if let (Some(own), Some((peer, dist))) = (ctx.position(), nearest_peer(ctx))
            && dist < CROWD_GAP
            && let Some(pp) = ctx.peer_position(peer)
        {
            let (ux, uz) = away_unit(Some(own), pp);
            ctx.look((own.0 + ux * 12.0, own.1 + 1.3, own.2 + uz * 12.0), settle)
                .await;
            let hold = vary(ctx, 900.0, 1_300.0) as u64;
            ctx.hold_visual(WalkDirection::Forward, false, hold, 0)
                .await;
        }

        // The orbit leg: half the legs revolve with the camera pinned on the moving host (body
        // travels tangentially, gaze points radially — fully decoupled), the rest walk the
        // tangent open-loop so the camera is not host-locked all episode.
        let watch_host = ctx.rng_next() < 0.5;
        if watch_host {
            let arrived = ctx
                .move_to_watching_peer(target.0, target.1, host, timeout, 1.5)
                .await;
            misses = if arrived { 0 } else { misses + 1 };
        } else if let Some(p) = ctx.position() {
            let dist = (target.0 - p.0).hypot(target.1 - p.2);
            if dist > 1.5 {
                let (ux, uz) = ((target.0 - p.0) / dist, (target.1 - p.2) / dist);
                ctx.look((p.0 + ux * 16.0, p.1 + 1.3, p.2 + uz * 16.0), settle)
                    .await;
                let hold = ((dist / 4.3 * 1000.0) as u64).clamp(700, 2_200);
                if !ctx
                    .hold_visual(WalkDirection::Forward, false, hold, 400)
                    .await
                {
                    misses += 1;
                } else {
                    misses = 0;
                }
            }
        }
        // Two failed legs running means the arc ahead is blocked from where the body stands
        // (round 1 measured a 15.2 s frozen run on a satellite grinding one obstacle). Shake
        // loose, skip a few chords past the blockage, and re-pin hop-wise — `travel_to` carries
        // its own sidesteps and progress budget.
        if misses >= 2 {
            misses = 0;
            shake_loose(ctx).await;
            angle += direction * 3.0 * chord / radius;
            if let Some(hp) = ctx.peer_position(host) {
                let goal = (hp.0 + angle.cos() * radius, hp.2 + angle.sin() * radius);
                travel_to(ctx, goal, TRAVEL_HOP, 2.5).await;
            }
        }
        ctx.record(
            TYPE_MOVE,
            &format!("vis-sat-orbit-{index}-{leg}"),
            &format!("Revolve one chord around host bot {host}."),
            json!({"leg": leg, "host": host, "angle": r2(angle),
                "radius": r2(radius), "watching_host": watch_host}),
        );
        if ctx.target_met() {
            break;
        }

        // Garnish: read a sibling moon, check the host, or look out at the OTHER orbit system.
        let roll = ctx.rng_next();
        if roll < 0.14 {
            if let Some((peer, dist)) = nearest_peer(ctx) {
                let hold = vary(ctx, 1_000.0, 2_000.0) as u64;
                watch_peer(ctx, peer, hold).await;
                ctx.record(
                    TYPE_LOOK,
                    &format!("vis-sat-sibling-{index}-{leg}"),
                    &format!("Watch sibling satellite bot {peer} on its own orbit."),
                    json!({"leg": leg, "peer": peer, "distance": r2(dist)}),
                );
            }
        } else if roll < 0.24 {
            look_at_peer(ctx, host, settle).await;
            sleep_ms(vary(ctx, 500.0, 1_200.0) as u64).await;
        } else if roll < 0.32 {
            let other = host_home(ctx.anchor(), (host + 1) % hosts.max(1), n);
            let feet = ctx.position().map(|p| p.1).unwrap_or(64.0);
            ctx.look_default((other.0, feet + 1.5, other.1)).await;
            sleep_ms(vary(ctx, 500.0, 1_000.0) as u64).await;
            ctx.record(
                TYPE_LOOK,
                &format!("vis-sat-far-system-{index}-{leg}"),
                "Look out at the neighbouring orbit system.",
                json!({"leg": leg}),
            );
        }
        leg += 1;
    }
}

/// The swarm splits into wandering "planet" hosts and the satellites that escort them: each
/// satellite revolves around its host's LIVE position while the host strolls on a short leash,
/// and on private timers satellites defect between hosts, crossing the open ground between orbit
/// systems. Moving-centre revolution plus host-hopping matches no merged scenario: group_orbit's
/// single pivot holds the group centre, patrol rings and lanes are anchored to the world.
pub(super) async fn run(ctx: &mut Ctx) {
    if ctx.index() < host_count(ctx.player_count().max(1)) {
        run_host(ctx).await;
    } else {
        run_satellite(ctx).await;
    }
}

#[cfg(test)]
mod tests {
    use super::{
        LEASH, SAT_JITTER, base_sat_radius, host_count, host_home, max_sats_per_host,
        sat_assignment,
    };
    use crate::scenarios::common::MIN_PEER_GAP;

    /// The spacing this file promises at every recorded swarm size: every satellite maps to a
    /// real host, evenly slotted siblings of one host clear the peer gap on the base orbit, and
    /// two adjacent orbit SYSTEMS — worst case: both hosts at full leash toward each other, both
    /// satellites at full radius between them — still clear it.
    #[test]
    fn orbit_systems_clear_the_peer_gap_at_every_swarm_size() {
        for n in 2..=32 {
            let hosts = host_count(n);
            assert!(hosts >= 1 && hosts < n, "n={n} hosts={hosts}");
            let mut per_host = vec![0usize; hosts];
            for i in hosts..n {
                let (host, slot, members) = sat_assignment(i, n);
                assert!(host < hosts && slot < members, "n={n} i={i}");
                per_host[host] += 1;
                let chord = 2.0
                    * base_sat_radius(members)
                    * (std::f64::consts::PI / members.max(2) as f64).sin();
                assert!(
                    chord >= MIN_PEER_GAP - 1e-9,
                    "n={n} host={host} chord={chord}"
                );
            }
            for (h, count) in per_host.iter().enumerate() {
                assert!(*count <= max_sats_per_host(n), "n={n} host={h}");
            }
            if hosts >= 2 {
                let anchor = (100.0, -40.0);
                let a = host_home(anchor, 0, n);
                let b = host_home(anchor, 1, n);
                let sep = (a.0 - b.0).hypot(a.1 - b.1);
                let orbit = base_sat_radius(max_sats_per_host(n)) + SAT_JITTER;
                assert!(
                    sep - 2.0 * (orbit + LEASH) >= MIN_PEER_GAP - 1e-9,
                    "n={n} sep={sep} orbit={orbit}"
                );
            }
        }
    }
}
