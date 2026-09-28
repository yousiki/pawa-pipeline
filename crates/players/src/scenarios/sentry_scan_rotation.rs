use super::*;

const TAU: f64 = std::f64::consts::TAU;

/// Shift length: five changeovers in a 120 s episode, so a four-bot rota walks every post.
const ROTA_MS: i64 = 22_000;

/// Patrol wander allowed off the post while on watch; [`post_ring`] absorbs it.
const PATROL: f64 = 4.0;

/// Draw in `[lo, hi)` from the per-bot stream, so a number this scenario could have hard-coded
/// varies per bot and per beat and still replays from the seed.
fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

/// Post ring radius, solved directly for the chord this scene needs: neighbouring posts must
/// clear [`MIN_PEER_GAP`] even with both sentries at full patrol wander toward each other.
/// (`station_ring`'s floored base radius would land the same guarantee farther off the arena.)
fn post_ring(n: usize) -> f64 {
    let n = n.max(2) as f64;
    ((MIN_PEER_GAP + 2.0 * PATROL) / (2.0 * (std::f64::consts::PI / n).sin())).max(8.0)
}

/// Index-only post formula shared by every bot, so the rota needs no cross-bot channel.
fn post_of(index: usize, shift: usize, n: usize) -> usize {
    (index + shift) % n.max(1)
}

fn post_xz(anchor: (f64, f64), post: usize, n: usize) -> (f64, f64) {
    let ang = post as f64 / n.max(1) as f64 * TAU;
    let r = post_ring(n);
    (anchor.0 + ang.cos() * r, anchor.1 + ang.sin() * r)
}

/// A watch rota on a ring of ground posts: on station a sentry alternates slow sector scans with
/// short patrol shuffles (the walking metronome that keeps displacement alive), and on a shared
/// clock cadence every sentry rotates one post over — all in the same direction, glancing at the
/// reliever taking the old post on the way. The camera work (sector sweeps, handover glances,
/// cross-ring checks) is the signal; no world edits.
pub(crate) async fn run(ctx: &mut Ctx) {
    let index = ctx.index();
    let n = ctx.player_count().max(1);
    let anchor = ctx.anchor();
    // Per-bot lag on the rota boundary, so changeovers ripple instead of snapping in sync.
    let clock_jitter = (ctx.rng_next() * 2_500.0) as i64;
    let mut last_post: Option<usize> = None;
    // Alternating patrol bearing, so consecutive stands are always a full stride apart instead of
    // two draws landing on the same spot.
    let mut patrol_ang = ctx.rng_next() * TAU;
    // Watch beats without displacement in a row. A post backed against terrain the strides cannot
    // solve otherwise alternates failed strides with 4-5 s scans — the first test round measured
    // one POV accruing a 16.3 s frozen run that way, a hair under the 20 s gate.
    let mut stalled = 0u32;
    let mut round = 0u32;
    ctx.record(
        TYPE_LOOK,
        &format!("vis-sentry-init-{index}"),
        &format!("Sentry rota: bot {index} takes post {index} of {n}."),
        json!({"post": index, "posts": n, "ring_radius": r2(post_ring(n)),
            "rota_ms": ROTA_MS, "clock_jitter_ms": clock_jitter}),
    );
    while !ctx.target_met() {
        let shift = ((ctx.elapsed_ms() + clock_jitter).max(0) / ROTA_MS) as usize;
        let post = post_of(index, shift, n);
        if last_post != Some(post) {
            change_over(ctx, anchor, n, last_post, post, round).await;
            last_post = Some(post);
            stalled = 0;
        } else {
            if stalled >= 2 {
                // Two beats without moving: stop scanning and walk the open interior first — the
                // ring centre is cleared ground in a way the post's uphill flank may not be.
                shake_loose(ctx).await;
                if let Some(p) = ctx.position() {
                    let toward = ((anchor.0 - p.0), (anchor.1 - p.2));
                    let mag = toward.0.hypot(toward.1).max(0.01);
                    let d = vary(ctx, 5.0, 7.0);
                    ctx.move_to(
                        p.0 + toward.0 / mag * d,
                        p.2 + toward.1 / mag * d,
                        6_000,
                        2.0,
                    )
                    .await;
                }
                stalled = 0;
            }
            let moved = watch_beat(ctx, anchor, n, post, &mut patrol_ang, round).await;
            stalled = if moved { 0 } else { stalled + 1 };
        }
        round += 1;
    }
}

/// Walk to the next post along the ring arc — every sentry rotates the same direction, so no two
/// ever cross head-on — glancing at the reliever inheriting the old post along the way.
async fn change_over(
    ctx: &mut Ctx,
    anchor: (f64, f64),
    n: usize,
    from: Option<usize>,
    post: usize,
    round: u32,
) {
    let index = ctx.index();
    let (px, pz) = post_xz(anchor, post, n);
    ctx.record(
        TYPE_MOVE,
        &format!("vis-sentry-shift-{index}-r{round}"),
        &format!("Shift change: bot {index} rotates onto post {post}."),
        json!({"round": round, "from": from, "post": post}),
    );
    if let Some(from) = from
        && n >= 2
    {
        let a0 = from as f64 / n as f64 * TAU;
        let mid = a0 + TAU / n as f64 * 0.5;
        let r = post_ring(n) + vary(ctx, -1.0, 1.0);
        ctx.move_to(
            anchor.0 + mid.cos() * r,
            anchor.1 + mid.sin() * r,
            8_000,
            2.5,
        )
        .await;
        // Handover glance: the bot arriving on the post this one just left.
        let reliever = (index + n - 1) % n;
        if ctx.rng_next() < 0.7
            && let Some(p) = look_at_peer(ctx, reliever, 600).await
        {
            let own = ctx.position();
            ctx.record(
                TYPE_LOOK,
                &format!("vis-sentry-handover-{index}-r{round}"),
                &format!("Glance at bot {reliever} taking over the old post."),
                json!({"round": round, "reliever": reliever,
                    "reliever_distance": r2(own
                        .map(|o| (o.0 - p.0).hypot(o.2 - p.2))
                        .unwrap_or(0.0))}),
            );
        }
        // Final approach with the camera already reading the new post.
        let own_y = ctx.position().map(|p| p.1).unwrap_or(64.0);
        ctx.move_to_watching_point(px, pz, (px, own_y + 1.0, pz), 8_000, 2.0)
            .await;
    } else {
        ctx.move_to(px, pz, ctx.cfg().move_timeout_ms.min(12_000), 2.0)
            .await;
    }
}

/// One on-station beat: a slow sector scan from the current stand, then a patrol stride to a
/// fresh stand inside the post's disc. The stride alternates sides of the post, so it is always a
/// full press and never a sub-second shuffle. Returns whether the beat displaced the body — the
/// caller's anti-freeze bookkeeping.
async fn watch_beat(
    ctx: &mut Ctx,
    anchor: (f64, f64),
    n: usize,
    post: usize,
    patrol_ang: &mut f64,
    round: u32,
) -> bool {
    let index = ctx.index();
    let start = ctx.position();
    let (px, pz) = post_xz(anchor, post, n);
    // Re-aim the scan centre most beats: half outward over the perimeter, half across the ring
    // toward another post (where the other sentries are — a hillside post scanning only outward
    // spends whole beats reading the dirt bank behind it, as the first test round's frames showed).
    let roll = ctx.rng_next();
    if roll < 0.8
        && let Some(p) = ctx.position()
    {
        let toward = if roll < 0.4 {
            let out = (p.0 - anchor.0, p.2 - anchor.1);
            let mag = out.0.hypot(out.1).max(0.01);
            (p.0 + out.0 / mag * 24.0, p.2 + out.1 / mag * 24.0)
        } else {
            let other = (post + 1 + (ctx.rng_next() * (n.max(2) - 1) as f64) as usize) % n.max(1);
            post_xz(anchor, other, n)
        };
        ctx.look(
            (toward.0, p.1 + 1.62, toward.1),
            ctx.cfg().look_settle_ms.max(500),
        )
        .await;
    }
    // Triangle sector scan: out to one edge, across, back to centre, with a drawn pitch drift.
    let span = vary(ctx, 60.0, 130.0);
    let settle = (((span / ctx.cfg().yaw_rate_deg_s.max(1.0)) * 1000.0) as u64 + 500).min(4_000);
    for leg in [span * 0.5, -span, span * 0.5] {
        let pitch_drift = vary(ctx, -4.0, 4.0);
        ctx.drag_view(leg, pitch_drift, settle).await;
    }
    ctx.record(
        TYPE_LOOK,
        &format!("vis-sentry-scan-{index}-r{round}"),
        &format!("Sector scan {} deg from post {post}.", r2(span)),
        json!({"round": round, "post": post, "span_deg": r2(span)}),
    );
    sleep_ms(vary(ctx, 200.0, 700.0) as u64).await;
    // Patrol stride: cross the post to the opposite flank (a bent reversal, not a retrace), so
    // every stand is a fresh camera position and the displacement clock never runs long.
    *patrol_ang += std::f64::consts::PI + vary(ctx, -0.8, 0.8);
    let d = vary(ctx, 2.6, PATROL);
    let to = (px + patrol_ang.cos() * d, pz + patrol_ang.sin() * d);
    if ctx.rng_next() < 0.3 && n >= 2 {
        // Some strides keep the camera across the ring on a neighbouring post while the body
        // walks its own way — the decoupled leg this rota is being recorded for.
        let across = post_xz(anchor, (post + 1) % n, n);
        let own_y = ctx.position().map(|p| p.1).unwrap_or(64.0);
        ctx.move_to_watching_point(to.0, to.1, (across.0, own_y + 1.2, across.1), 4_000, 1.5)
            .await;
    } else {
        ctx.move_to(to.0, to.1, 4_000, 1.5).await;
    }
    if ctx.rng_next() < 0.3 {
        glance_at_nearest_peer(ctx, None).await;
    }
    match (start, ctx.position()) {
        (Some(a), Some(b)) => (a.0 - b.0).hypot(a.2 - b.2) >= 1.2,
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rota must be a permutation at every shift (no post doubled, none abandoned), and one
    /// changeover must move every sentry exactly one post over.
    #[test]
    fn every_shift_assigns_every_post_exactly_once() {
        for n in 2..=9 {
            for shift in 0..(2 * n) {
                let mut taken = vec![false; n];
                for index in 0..n {
                    let post = post_of(index, shift, n);
                    assert!(!taken[post], "n={n} shift={shift} post={post} doubled");
                    taken[post] = true;
                }
                for index in 0..n {
                    assert_eq!(
                        (post_of(index, shift, n) + 1) % n,
                        post_of(index, shift + 1, n),
                        "n={n} shift={shift}: changeover is not one post over"
                    );
                }
            }
        }
    }

    /// Adjacent sentries at full patrol wander toward each other are the tightest pair the rota
    /// can produce; the ring must keep them over the peer gap at every swarm size.
    #[test]
    fn adjacent_posts_clear_the_peer_gap_at_every_swarm_size() {
        for n in 2..=32 {
            let r = post_ring(n);
            let chord = 2.0 * r * (std::f64::consts::PI / n.max(2) as f64).sin();
            let worst = chord - 2.0 * PATROL;
            assert!(worst >= MIN_PEER_GAP - 1e-9, "n={n} worst={worst}");
        }
        // A 120 s episode must see several shifts.
        const {
            assert!(120_000 / ROTA_MS >= 5);
        }
    }
}
