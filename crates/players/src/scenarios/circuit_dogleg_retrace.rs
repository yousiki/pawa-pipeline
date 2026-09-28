//! CATALOG: family=locomotion min_players=1 single_process=false world_edits=false scenery=none far_players=false

use super::*;

/// Waypoints per dog-leg, not counting the hub. Three is the smallest chain with two interior
/// corners, so the retrace has more than one view to re-enter; more than three and a round outlasts
/// the shortest episodes we record (30 s smoke runs) before it ever comes home.
const LEGS: usize = 3;
/// Length of one dog-leg segment. Solaris's random-walk primitive is 3-4 blocks out then back, and
/// 4 is also the length at which one press clears the 0.5 s key-chatter floor even at sprint speed.
const SEGMENT: f64 = 4.5;
/// Turn between consecutive segments. Above the 102.5 deg horizontal FOV would make each segment's
/// view disjoint from the last, which is the v5 defect; well under it and the "dog-leg" is a
/// straight line. 55-75 deg keeps consecutive views partly overlapping and the corner readable.
const CORNER_DEG: (f64, f64) = (55.0, 75.0);
/// Legs of a dolly dwell, in blocks.
const DOLLY_LEG: f64 = 4.0;

/// Draw in `[lo, hi)` from the per-bot stream, so a number this scenario could have hard-coded
/// varies per bot and per round and still replays from the seed.
fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

/// Dwell on the CURRENT view by dollying: walk `leg` blocks along the facing, then the same
/// distance backward, leaving the camera exactly where the caller pointed it, until `deadline_ms`.
/// Returns how many halves actually displaced the body.
///
/// `hold_visual` pins the camera to the angle it finds, so across a dwell the view only
/// TRANSLATES along the sight line. That is what the frustum audit rewards: `overlap_mean` is
/// measured against the window's FIRST frame and a window with no rotation scores 1.0, while
/// `net_over_sweep` is 0.0 whenever the yaw range is empty. A dwell longer than one window
/// (93 frames = 4.65 s) therefore contributes whole windows of pure revisit.
///
/// It also clears three displacement gates at once. Each half is ONE press of 0.9-1.4 s, over the
/// `SHORT_RUN_FRAMES` (0.5 s) chatter floor — and a blocked half is cut by `hold_visual`'s stall
/// guard only after 1.2 s, so even walking into a tree cannot produce a short press. The body
/// covers `2 * leg` blocks per round trip, over the frozen gate's 0.5 blocks/s. And because the two
/// halves cancel, the dwell cannot inflate the window displacement median.
///
/// Turns are never taken during a dolly. `qc.rs`'s camera/travel gate is TWO-SIDED (`agreement()`
/// is `max(toward, away) / total`), so a turn while translating is evidence against the episode
/// whichever way it points; a turn below `CAMERA_SPEED_FLOOR` (0.05 blocks/frame) is not evidence
/// at all, which is why every scripted turn in this family happens standing still.
async fn dolly_dwell(ctx: &mut Ctx, leg: f64, deadline_ms: i64) -> u32 {
    // Walking is ~4.3 blocks/s and sprint is forward-effective only, so time the press off the
    // walking figure; the 900 ms floor keeps a short drawn leg from becoming a short press.
    let hold_ms = ((leg / 4.3 * 1_000.0) as u64).clamp(900, 2_600);
    let sprint_prob = ctx.cfg().sprint_bout_prob;
    let mut moved = 0u32;
    let mut outbound = true;
    while !ctx.target_met() && ctx.elapsed_ms() + hold_ms as i64 + 200 < deadline_ms {
        let before = ctx.position();
        // Sprint only on forward halves (it does nothing backward) and only sometimes: `qc.rs`
        // fails an episode whose sustained forward runs all agree on the sprint bit.
        let sprint = outbound && ctx.rng_next() < sprint_prob;
        let dir = if outbound {
            WalkDirection::Forward
        } else {
            WalkDirection::Backward
        };
        ctx.hold_visual(dir, sprint, hold_ms, 0).await;
        if let (Some(a), Some(b)) = (before, ctx.position())
            && (b.0 - a.0).hypot(b.2 - a.2) > 1.0
        {
            moved += 1;
        }
        outbound = !outbound;
    }
    moved
}

/// Walk a multi-segment dog-leg away from a fixed hub, then retrace the SAME waypoints in exact
/// reverse order, stopping at each one to look back at the waypoint just left.
///
/// The revisit problem, the path-memory face of it. A back-sight taken on the retrace is not merely
/// "a direction we turned away from": at waypoint w2, looking back at w3 reproduces the frustum the
/// bot actually had at w2 while it was walking out to w3. So the ground truth of every retrace
/// dwell is a region this POV genuinely rendered, ten to twenty seconds earlier, from the same
/// cell — the strongest form of already-observed GT a training episode can carry. The trajectory
/// closes for free: the retrace ends on the hub it started from, so `path_length` is twice the
/// dog-leg while displacement over any window stays inside one segment.
///
/// The corner signs alternate per round (round `k` bends one way, `k + 1` the other), so the net
/// rotation the audit divides by sweep cancels across rounds instead of accumulating into the
/// one-way spin that made v5's net/sweep ratio 0.80.
///
/// Distinct from `depart_and_revisit`, which departs ONCE to a far waypoint and comes back to a
/// single marked cell, and from `sparse_checkpoint_return`, whose outbound is a straight line to
/// one checkpoint and whose return is unremarked: here the excursion is a chain with interior
/// corners, the return path is the outbound path reversed rather than a fresh route, and every
/// waypoint on the way home is a confirmed re-entry of a specific earlier view.
pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let anchor = ctx.anchor();
    let start = ctx.position().unwrap_or((anchor.0, 64.0, anchor.1));

    // Brief rule 5: open facing the SHARED subject. Every bot in the episode turns onto the anchor
    // before it does anything else, so the opening frame — the only context frame a 93-frame
    // training window gets — already contains the anchor's neighbourhood and whoever else is
    // standing in it. Solaris gets the same effect from a forced `lookAt` with `randomized:false`.
    // The turn is stationary, so it is invisible to the camera/travel gate.
    let opened = ctx
        .look_until_facing((anchor.0, start.1 + 1.4, anchor.1), 10.0, 4_000)
        .await;
    // The hub is where the bot spawned. Keeping it at the spread position — rather than pulling
    // every bot onto the shared anchor — is what leaves N=2 with two bodies a peer gap apart while
    // both dog-legs still fan out through the anchor's neighbourhood.
    let hub = (start.0, start.2);
    let dwell_ms = vary(ctx, 7_000.0, 9_500.0) as i64;

    ctx.record(
        "observe",
        &format!("vis-dogleg-open-{index}"),
        "Open the dog-leg retrace: every excursion comes home along its own outbound waypoints.",
        json!({
            "global_index": global,
            "role": "retrace",
            "opened_on_anchor": opened,
            "hub": {"x": r2(hub.0), "z": r2(hub.1)},
            "legs": LEGS,
            "dwell_ms": dwell_ms,
        }),
    );

    let mut round = 0u32;
    while !ctx.target_met() {
        // Aim the round's first segment roughly at the anchor on even rounds and away on odd ones.
        // The inward rounds are what keep a teammate in frame at N=2 (the 22.4% barge-in defect in
        // the v5 audit); the outward ones are what stop the bot from grinding one patch of ground.
        let toward_anchor = look_angles(head_of(start), (anchor.0, start.1, anchor.1)).0;
        let outward = if round.is_multiple_of(2) { 0.0 } else { 180.0 };
        let mut bearing = toward_anchor + outward + vary(ctx, -35.0, 35.0);
        // Corner sign flips every round, so the round's total turn changes sign with it.
        let sign = if round.is_multiple_of(2) { 1.0 } else { -1.0 };

        // OUTBOUND. Record where the bot ACTUALLY ended up, not where it was sent: on natural
        // terrain the pathfinder rounds a boulder and stops short, and a retrace of the planned
        // cells would then walk a line the bot never walked — which is exactly the claim this
        // scenario makes about its ground truth.
        let mut visited: Vec<(f64, f64)> = Vec::with_capacity(LEGS + 1);
        visited.push(hub);
        for leg in 0..LEGS {
            if ctx.target_met() {
                break;
            }
            if leg > 0 {
                bearing += sign * vary(ctx, CORNER_DEG.0, CORNER_DEG.1);
            }
            let from = *visited.last().unwrap_or(&hub);
            let span = vary(ctx, SEGMENT - 0.5, SEGMENT + 1.0);
            let step = point_at_yaw((from.0, start.1, from.1), bearing, span);
            travel_to(ctx, (step.0, step.2), TRAVEL_HOP, 1.6).await;
            let landed = ctx
                .position()
                .map(|p| (p.0, p.2))
                .unwrap_or((step.0, step.2));
            visited.push(landed);
            ctx.record(
                TYPE_MOVE,
                &format!("vis-dogleg-out-{index}-{round}-{leg}"),
                &format!("Outbound segment {leg} of the dog-leg."),
                json!({
                    "round": round,
                    "leg": leg,
                    "bearing_deg": r2(bearing),
                    "corner_sign": sign,
                    "landed": {"x": r2(landed.0), "z": r2(landed.1)},
                }),
            );
        }

        // RETRACE. Walk the visited chain backwards; at each arrival turn onto the waypoint just
        // left and dwell there. `look_until_facing` is what makes the dwell evidence: a plain
        // `look` returns on its deadline, so a wide turn that timed out would be recorded as a
        // re-entry that never happened.
        for back in (0..visited.len().saturating_sub(1)).rev() {
            if ctx.target_met() {
                break;
            }
            let leaving = visited[back + 1];
            let target = visited[back];
            let gap = travel_to(ctx, target, TRAVEL_HOP, 1.6).await;
            let feet = ctx.position().map(|p| p.1).unwrap_or(start.1);
            let confirmed = ctx
                .look_until_facing((leaving.0, feet + 1.4, leaving.1), 8.0, 5_000)
                .await;
            let leg_len = vary(ctx, DOLLY_LEG - 0.5, DOLLY_LEG + 0.8);
            let halves = dolly_dwell(ctx, leg_len, ctx.elapsed_ms() + dwell_ms).await;
            ctx.record(
                TYPE_LOOK,
                &format!("vis-dogleg-back-{index}-{round}-{back}"),
                "Re-enter the outbound view: from this waypoint, look at the one just left.",
                json!({
                    "round": round,
                    "waypoint": back,
                    "is_hub": back == 0,
                    "arrival_gap": r2(gap),
                    "facing_confirmed": confirmed,
                    "revisited_view": {"x": r2(leaving.0), "z": r2(leaving.1)},
                    "dolly_halves_moved": halves,
                    "elapsed_ms": ctx.elapsed_ms(),
                }),
            );
        }
        round += 1;
    }
}
