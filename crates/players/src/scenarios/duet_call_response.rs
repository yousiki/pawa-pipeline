//! CATALOG: family=group_motion min_players=2 single_process=true world_edits=false scenery=none far_players=false
//!
//! A face-to-face call-and-response drill: one player traces a lateral “question” from its home mark,
//! the other answers with a shorter depth step, and the speaking role alternates every round. It targets
//! the revisit problem's peer-in-context failure without any blocks or mobs: the first action is a confirmed gaze
//! at the peer, and every movement phrase ends by stopping and re-acquiring them. Both phrases return to
//! their home mark, so path length grows while displacement stays bounded.
//!
//! Near-clones are prevented structurally: caller and responder use perpendicular routes, different
//! numbers of legs, opposite side keys, unequal pauses, and swap roles rather than moving in unison.
//! On hostile terrain an unreachable cue simply becomes a shorter phrase; the home return, stationary
//! gaze, and recorded `arrived` booleans keep the QC failure visible instead of pretending success.

use super::*;

const HOME_HALF: f64 = 5.0;
const QUESTION_SIDE: f64 = 4.0;
const ANSWER_DEPTH: f64 = 3.5;
const ROUND_MS: f64 = 7_500.0;
const PEER_TOL_DEG: f64 = 8.0;
const MIN_PRESS_MS: u64 = 700;

fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

fn axis(ctx: &Ctx) -> (f64, f64) {
    let (x, z) = ctx
        .engine()
        .shared
        .native_waypoints
        .first()
        .copied()
        .unwrap_or((1.0, 0.0));
    let d = x.hypot(z).max(0.001);
    // Keep roles aligned within each pair, but rotate pair workspaces for N=4+.
    let angle = ctx.index().saturating_div(2) as f64 * 0.83;
    let (sin, cos) = angle.sin_cos();
    (x / d * cos - z / d * sin, x / d * sin + z / d * cos)
}

async fn face_peer(ctx: &Ctx, peer: Option<usize>, fallback: (f64, f64, f64)) -> bool {
    let target = peer
        .and_then(|j| ctx.peer_position(j))
        .map(head_of)
        .unwrap_or(fallback);
    ctx.look_until_facing(target, PEER_TOL_DEG, 4_000).await
}

async fn away_and_back(
    ctx: &mut Ctx,
    peer: Option<usize>,
    fallback: (f64, f64, f64),
    delta: f64,
    side_key: WalkDirection,
) -> (bool, bool) {
    let Some(own) = ctx.position() else {
        return (false, false);
    };
    let target = peer
        .and_then(|j| ctx.peer_position(j))
        .map(head_of)
        .unwrap_or(fallback);
    let yaw = look_angles(head_of(own), target).0;
    let away = point_at_yaw(head_of(own), yaw + delta, 24.0);
    let left = ctx.look_until_facing(away, PEER_TOL_DEG, 5_000).await;
    ctx.hold_visual(side_key, false, MIN_PRESS_MS + 100, 0)
        .await;
    let returned = face_peer(ctx, peer, fallback).await;
    (left, returned)
}

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let peer = Some(index ^ 1).filter(|&j| j < ctx.player_count());
    let slot = global % 2;
    let side = if slot == 0 { 1.0 } else { -1.0 };
    let side_key = if slot == 0 {
        WalkDirection::Left
    } else {
        WalkDirection::Right
    };
    let anchor = ctx.anchor();
    let (ax, az) = axis(ctx);
    let perp = (-az, ax);
    let home = (
        anchor.0 + ax * side * HOME_HALF,
        anchor.1 + az * side * HOME_HALF,
    );

    let opening = face_peer(ctx, peer, (anchor.0, 64.0, anchor.1)).await;
    travel_to(ctx, home, TRAVEL_HOP, 1.2).await;
    let start = ctx.position().unwrap_or((home.0, 64.0, home.1));
    let centre = (anchor.0, start.1 + 1.4, anchor.1);
    ctx.record("observe", &format!("vis-duet-call-setup-{index}"), "Take a home mark and open face-to-face before the first call.", json!({
        "global_index": global, "role": if slot == 0 { "first_caller" } else { "first_responder" },
        "peer": peer, "opening_confirmed": opening, "home": {"x": r2(home.0), "z": r2(home.1)},
    }));

    let mut last_round = i64::MIN;
    while !ctx.target_met() {
        let round = ctx.engine().shared.phase_at(ROUND_MS);
        let calling = (round + global as i64).rem_euclid(2) == 0;
        if round != last_round {
            last_round = round;
            ctx.record(
                TYPE_MOVE,
                &format!("vis-duet-call-round-{index}-{round}"),
                if calling {
                    "Ask with a two-sided lateral phrase."
                } else {
                    "Answer with an in-and-out depth phrase."
                },
                json!({"round": round, "calling": calling}),
            );
        }

        face_peer(ctx, peer, centre).await;
        let (first, second) = if calling {
            (
                (
                    home.0 + perp.0 * QUESTION_SIDE * side,
                    home.1 + perp.1 * QUESTION_SIDE * side,
                ),
                (
                    home.0 - perp.0 * QUESTION_SIDE * 0.7 * side,
                    home.1 - perp.1 * QUESTION_SIDE * 0.7 * side,
                ),
            )
        } else {
            (
                (
                    home.0 - ax * side * ANSWER_DEPTH,
                    home.1 - az * side * ANSWER_DEPTH,
                ),
                home,
            )
        };
        let first_ok = ctx.move_to(first.0, first.1, 6_000, 0.8).await;
        let first_faced = face_peer(ctx, peer, centre).await;
        let second_ok = ctx.move_to(second.0, second.1, 6_000, 0.8).await;
        let second_faced = face_peer(ctx, peer, centre).await;
        let home_gap = travel_to(ctx, home, TRAVEL_HOP, 1.2).await;

        let reversal = if calling {
            None
        } else {
            let turn = 110.0 + slot as f64 * 18.0;
            Some(away_and_back(ctx, peer, centre, turn, side_key).await)
        };
        ctx.record(TYPE_MOVE, &format!("vis-duet-call-phrase-{index}-{round}"), "Complete a call or response and return to the same home mark.", json!({
            "round": round, "calling": calling, "first_arrived": first_ok, "second_arrived": second_ok,
            "first_peer_reacquired": first_faced, "second_peer_reacquired": second_faced,
            "home_gap": r2(home_gap), "away_and_back": reversal.map(|(l, r)| json!({"left_view": l, "return_confirmed": r})),
        }));
        sleep_ms(vary(
            ctx,
            if calling { 180.0 } else { 420.0 },
            if calling { 480.0 } else { 760.0 },
        ) as u64)
        .await;
    }
}
