//! CATALOG: family=group_motion min_players=2 single_process=true world_edits=false scenery=none far_players=false
//!
//! Two bots take turns: one walks an out-and-back spur, the other watches it, and the roles swap on
//! a clock. Every swap is a sightline handover — the watcher becomes the walker and vice versa.
//!
//! The revisit problem. **The trajectory loop** is explicit: a term's walker leaves its home post,
//! covers
//! `SPUR_BLOCKS` on one bearing, and comes straight back to the post it started from, so every term
//! ends where it began and net displacement over the episode is bounded by the arrival tolerance
//! rather than by the episode length. This is the shape `straight_line_walk` gets wrong (four legs
//! out, no return, 16 blocks one way). **The sightline loop** is the handover: at each term boundary
//! both bots turn to face each other, so a direction each of them last looked at one term ago comes
//! back into frame on a fixed cadence, and the two POVs of the same handover are mutually
//! verifiable — each one contains the other bot doing the turn.
//!
//! The spur bearing is re-drawn per term, so successive terms sweep the terrain around the anchor
//! from different sides: different occluders, different skyline, but always the same home post to
//! come back to.
//!
//! **How this clears the camera/travel independence gate.** Walk-stop-look-walk on both sides.
//! Every camera aim in this script happens from a standstill — the handover turn and
//! each watch gaze — so `qc.rs:420` discards those frames before the sign test. Travel is plain
//! `move_to`/`travel_to`, camera untouched, the idiom all 65 generation-5 production scenarios pass on. Nothing
//! re-issues `LookAt` inside a movement loop, which is the shape measured at 96-100%; and since the
//! gate is TWO-SIDED (`agreement()` is `max(p, 1-p)`), a correction that consistently pointed AWAY
//! from travel would fail just as hard.
//!
//! Because the roles swap, both POVs accumulate both kinds of motion, so neither can starve the
//! participation gate and the two action streams stay far apart for the near-clones check.

use super::*;

/// One leadership term. Six terms fit a 120 s episode at N=2, so each bot walks three spurs and
/// watches three — enough that neither POV is dominated by one behaviour.
const TERM_MS: i64 = 18_000;
/// How far out the spur goes before turning around, in blocks. `travel_to` breaks it into
/// [`TRAVEL_HOP`]-block legs, each one a single press well over the 0.5 s key-chatter floor, and
/// 8 blocks out is inside `LEASH`-style range of the anchor so the walker never wanders off the
/// prepared ground.
const SPUR_BLOCKS: f64 = 8.0;
/// One observation round inside a watch term: the walking gap between two gazes at the walker. Kept
/// under one 4.65 s training window so most windows contain a gaze — `hold_facing` does not hold the
/// bearing past its first leg, so gaze density is the only thing keeping the walker in
/// frame — and over its ~2.2 s leg budget so each round still walks a full leg.
const WATCH_MS: i64 = 3_000;
/// Shuttle leg for `hold_facing`, in blocks. 4.0 is the validated value — see its doc comment.
const SHUTTLE_LEG: f64 = 4.0;

/// Who holds the walker role at `elapsed_ms`. A pure function of each bot's own episode clock, the
/// `carousel_chase` / `follow_cam_chain` recipe: no shared barrier, no election, and the two bots
/// agree because they agree on the clock.
fn walker_at(elapsed_ms: i64, bot_count: usize) -> usize {
    ((elapsed_ms.max(0) / TERM_MS) as usize) % bot_count.max(1)
}

/// The bot this one watches, and is watched by. At N=2 — the swarm size collected first — the
/// two bots pair with each other; a larger swarm pairs by index parity, with an unmatched odd bot
/// falling back to bot 0 so nobody watches itself.
fn partner_of(index: usize, bot_count: usize) -> usize {
    let paired = index ^ 1;
    if paired < bot_count { paired } else { 0 }
}

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let bot_count = ctx.player_count().max(1);
    let partner = partner_of(index, bot_count);
    let anchor = ctx.anchor();
    // The post every spur returns to: where this bot settled after the spawn fall. Using the spawn
    // cell rather than a computed slot means the first term starts immediately, with both bodies
    // still inside `spread_range` of each other and therefore inside each other's opening frame.
    let home = ctx
        .position()
        .map(|(x, _, z)| (x, z))
        .unwrap_or((anchor.0, anchor.1));

    if bot_count >= 2 {
        look_at_peer(ctx, partner, 800).await;
    }
    ctx.record(
        TYPE_LOOK,
        &format!("vis-swap-open-{index}"),
        &format!("Open facing partner bot {partner} before the first term."),
        json!({
            "partner": partner,
            "home": {"x": r2(home.0), "z": r2(home.1)},
            "term_ms": TERM_MS,
            "spur_blocks": SPUR_BLOCKS,
        }),
    );

    let mut last_term: i64 = -1;
    let mut round = 0usize;
    while !ctx.target_met() {
        let term = ctx.elapsed_ms().max(0) / TERM_MS;
        let walker = walker_at(ctx.elapsed_ms(), bot_count);
        if term != last_term {
            last_term = term;
            // The handover beat, and the reason this scenario exists: whatever either bot was
            // looking at, it now turns to the other one. Confirmed with `look_until_facing` because
            // a handover that timed out mid-turn would otherwise be recorded as a handover that
            // happened.
            let confirmed = match ctx.peer_position(partner) {
                Some(peer) if bot_count >= 2 => {
                    ctx.look_until_facing(head_of(peer), 6.0, 4_000).await
                }
                _ => false,
            };
            ctx.record(
                TYPE_LOOK,
                &format!("vis-swap-handover-{index}-t{term}"),
                &format!("Term {term}: bot {walker} walks, the other watches; sightlines cross."),
                json!({
                    "term": term,
                    "walker": walker,
                    "role": if walker == index { "walker" } else { "watcher" },
                    "partner": partner,
                    "facing_confirmed": confirmed,
                }),
            );
        }

        if walker == index || bot_count < 2 {
            walk_spur(ctx, home, anchor, term).await;
        } else {
            watch_round(ctx, partner, round).await;
        }
        round += 1;
    }
}

/// One out-and-back spur from the home post. The return leg is the point: it is what makes the
/// trajectory a loop instead of a ray, and it re-enters ground the bot walked seconds earlier.
async fn walk_spur(ctx: &mut Ctx, home: (f64, f64), anchor: (f64, f64), term: i64) {
    let index = ctx.index();
    // Re-draw the bearing per term so successive spurs face different terrain, and bias it away
    // from the anchor so the walker does not repeatedly cross the watcher's post.
    let bias = (home.1 - anchor.1).atan2(home.0 - anchor.0);
    let bearing = bias + (ctx.rng_next() - 0.5) * std::f64::consts::PI;
    let out = (
        home.0 + bearing.cos() * SPUR_BLOCKS,
        home.1 + bearing.sin() * SPUR_BLOCKS,
    );
    let out_residual = travel_to(ctx, out, TRAVEL_HOP, 1.5).await;
    ctx.record(
        TYPE_MOVE,
        &format!("vis-swap-out-{index}-t{term}-{}", ctx.elapsed_ms()),
        &format!("Walk {} blocks out from the home post.", r2(SPUR_BLOCKS)),
        json!({
            "role": "walker", "term": term, "leg": "out",
            "bearing_rad": r2(bearing),
            "target": {"x": r2(out.0), "z": r2(out.1)},
            "residual": r2(out_residual),
        }),
    );
    if ctx.target_met() {
        return;
    }
    let home_residual = travel_to(ctx, home, TRAVEL_HOP, 1.5).await;
    ctx.record(
        TYPE_MOVE,
        &format!("vis-swap-home-{index}-t{term}-{}", ctx.elapsed_ms()),
        "Return to the home post, closing the spur into a loop.",
        json!({
            "role": "walker", "term": term, "leg": "home",
            "target": {"x": r2(home.0), "z": r2(home.1)},
            "residual": r2(home_residual),
        }),
    );
}

/// One watch round: a standing turn onto the walker, then a fixed-bearing hold.
async fn watch_round(ctx: &mut Ctx, partner: usize, round: usize) {
    let (Some(peer), Some(own)) = (ctx.peer_position(partner), ctx.position()) else {
        // Missing peer state is a transient chunk-load artifact; a third of a second of waiting is
        // nothing against the 20 s frozen run that fails an episode.
        sleep_ms(300).await;
        return;
    };
    let facing = ctx.look_until_facing(head_of(peer), 6.0, 4_000).await;
    let bearing = look_angles(head_of(own), head_of(peer)).0;
    ctx.record(
        TYPE_LOOK,
        &format!("vis-swap-watch-{}-r{round}", ctx.index()),
        &format!(
            "Watch the walker, bot {partner}, from bearing {}.",
            r2(bearing)
        ),
        json!({
            "role": "watcher", "round": round, "partner": partner,
            "bearing_deg": r2(bearing),
            "distance": r2((peer.0 - own.0).hypot(peer.2 - own.2)),
            "facing_confirmed": facing,
        }),
    );
    hold_facing(ctx, bearing, ctx.elapsed_ms() + WATCH_MS, SHUTTLE_LEG).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The swap is the whole scenario: it has to land on exact term boundaries, visit both bots,
    /// and repeat several times inside one episode. A clock function with no test is a clock
    /// function that silently pins one bot in one role.
    #[test]
    fn the_walker_role_alternates_on_term_boundaries() {
        assert_eq!(walker_at(0, 2), 0);
        assert_eq!(walker_at(TERM_MS - 1, 2), 0);
        assert_eq!(walker_at(TERM_MS, 2), 1);
        assert_eq!(walker_at(2 * TERM_MS, 2), 0);
        for bots in 2..=8 {
            let mut seen = vec![false; bots];
            for term in 0..bots {
                seen[walker_at(term as i64 * TERM_MS, bots)] = true;
            }
            assert!(seen.iter().all(|&s| s), "bots={bots}: a bot never walked");
        }
        // A 120 s episode must contain several terms, or a bot only ever plays one role.
        const {
            assert!(120_000 / TERM_MS >= 6);
        }
    }

    /// Nobody watches itself, at any swarm size.
    #[test]
    fn partners_are_distinct() {
        assert_eq!(partner_of(0, 2), 1);
        assert_eq!(partner_of(1, 2), 0);
        for bots in 2..=16 {
            for index in 0..bots {
                assert_ne!(partner_of(index, bots), index, "bots={bots} index={index}");
                assert!(partner_of(index, bots) < bots);
            }
        }
    }
}
