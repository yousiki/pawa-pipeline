//! CATALOG: family=locomotion min_players=1 single_process=false world_edits=false scenery=none far_players=false

use super::*;

/// Edge length of the patrol square, in blocks. Two `TRAVEL_HOP` legs, so a whole edge is walked
/// as two long presses; and short enough that the pathfinder's detours around natural terrain
/// still leave the four corners recognisably square (the fixed-track-on-terrain failure the
/// figure-eight scenario measured in its round 2).
const SIDE: f64 = 8.0;
/// Legs of a dolly dwell, in blocks — one press of ~1 s at walking pace, well over the 0.5 s
/// key-chatter floor, and under half an edge so the dolly never leaves the corner it belongs to.
const DOLLY_LEG: f64 = 4.0;

/// Draw in `[lo, hi)` from the per-bot stream, so a number this scenario could have hard-coded
/// varies per bot and per corner and still replays from the seed.
fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

/// Corner `step` of a square of side `side` centred on `centre` and rotated by `axis_rad`, in the
/// order a counter-clockwise lap walks them. The square is the one inscribed in a circle of radius
/// `side * sqrt(2) / 2`, so a corner is one `cos`/`sin` and consecutive corners are exactly `side`
/// apart — worth deriving rather than writing four hard-coded offsets, because the rotation is
/// per-bot and hand-rotated offsets are where a sign error hides silently.
fn corner(centre: (f64, f64), axis_rad: f64, side: f64, step: i64) -> (f64, f64) {
    let radius = side * std::f64::consts::SQRT_2 / 2.0;
    let theta = axis_rad + step.rem_euclid(4) as f64 * std::f64::consts::FRAC_PI_2;
    (
        centre.0 + theta.cos() * radius,
        centre.1 + theta.sin() * radius,
    )
}

/// Dwell on the CURRENT view by dollying: walk `leg` blocks along the facing, then the same
/// distance backward, leaving the camera exactly where the caller pointed it, until `deadline_ms`.
/// Returns how many halves actually displaced the body.
///
/// `hold_visual` pins the camera to the angle it finds, so across a dwell the view only
/// TRANSLATES along the sight line. That is what the frustum audit rewards: `overlap_mean` is
/// measured against the window's FIRST frame and a window with no rotation scores 1.0, while
/// `net_over_sweep` is defined as 0.0 when the yaw range is empty. A dwell longer than a window
/// (4.65 s) therefore contributes whole windows of perfect revisit, which is the entire point of
/// the revisit-oriented scenarios — and it does it while still covering ground, so nothing is bought from the frozen
/// gate.
///
/// Each half is ONE press of 0.9-1.4 s, over the `SHORT_RUN_FRAMES` (0.5 s) chatter floor, and a
/// half that walks into a tree is cut by `hold_visual`'s stall guard only after 1.2 s — so even
/// blocked halves are long presses.
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

/// Walk a closed square and, at every corner, turn back and dwell on the edge just walked.
///
/// The revisit problem from both ends. The trajectory closes because the route is a square: the bot
/// passes through the same four cells every lap, so `path_length` grows while displacement stays
/// inside one edge. The gaze closes because of the back-sight — the corner turn points the camera
/// at the corner the bot left ~10 seconds ago, which is the most recently observed region there is,
/// and the dwell that follows is a dolly straight back down that sight line. That dwell is the
/// scenario's real payload: it is longer than an audit window, so whole windows are pure revisit.
///
/// Lap chirality FLIPS every lap. A one-way patrol is the v5 defect in miniature — every corner
/// turn shares a sign, so windows accumulate net rotation as fast as they accumulate sweep, which
/// is the 0.80 net/sweep ratio this family exists to fix. Reversing the lap makes each corner's
/// onward turn the opposite sign to the previous lap's at the same corner.
///
/// Distinct from `gully_ring_traffic`, the only other rectangular patrol: that one is a
/// `gap_jump`-scenery script that lays causeway blocks and glances at oncoming traffic; this one
/// edits nothing, runs on natural terrain, and its camera does exactly one thing — look back at
/// where it came from.
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

    // Centre the square between the bot and the anchor. At N=2 both bots run this script, and
    // squares pulled toward the shared anchor overlap, so a teammate is in frame from the FIRST
    // frame rather than walking into it mid-window (the 22.4% barge-in defect in the v5 audit).
    let centre = ((start.0 + anchor.0) * 0.5, (start.2 + anchor.1) * 0.5);
    // Per-bot rotation and size: two bots tracing congruent squares produce near-identical action
    // streams, which is what the `near-clones` QC check reads.
    let axis_rad = vary(ctx, 0.0, std::f64::consts::TAU);
    let side = vary(ctx, SIDE - 1.0, SIDE + 2.0);
    let dwell_ms = vary(ctx, 8_000.0, 10_500.0) as i64;

    ctx.record(
        "observe",
        &format!("vis-square-backsight-open-{index}"),
        "Open the square patrol: every corner turns back onto the edge just walked.",
        json!({
            "global_index": global,
            "role": "backsight_patrol",
            "opened_on_anchor": opened,
            "centre": {"x": r2(centre.0), "z": r2(centre.1)},
            "side": r2(side),
            "axis_deg": r2(axis_rad.to_degrees()),
            "dwell_ms": dwell_ms,
        }),
    );

    // Enter the square at its nearest corner so the first edge is a real edge, not an approach.
    let mut here: i64 = (0..4)
        .min_by(|&a, &b| {
            let pa = corner(centre, axis_rad, side, a);
            let pb = corner(centre, axis_rad, side, b);
            (pa.0 - start.0)
                .hypot(pa.1 - start.2)
                .total_cmp(&(pb.0 - start.0).hypot(pb.1 - start.2))
        })
        .unwrap_or(0);
    let entry = corner(centre, axis_rad, side, here);
    travel_to(ctx, entry, TRAVEL_HOP, 2.0).await;

    // +1 walks the square counter-clockwise, -1 clockwise; flipped at the end of every lap.
    let mut turn: i64 = 1;
    let mut edge = 0u32;
    while !ctx.target_met() {
        let from = here;
        here += turn;
        let target = corner(centre, axis_rad, side, here);
        let gap = travel_to(ctx, target, TRAVEL_HOP, 1.6).await;
        if ctx.target_met() {
            break;
        }

        // The back-sight. Aim at the corner just left, at head height, and CONFIRM it: a plain
        // `look` returns on its deadline, so a wide turn that timed out would be recorded as a
        // return that never happened. The tolerance is 8 deg — the corner is a place, not a block
        // face, and a tighter bar just burns turn budget chasing pathfinder drift.
        let behind = corner(centre, axis_rad, side, from);
        let feet = ctx.position().map(|p| p.1).unwrap_or(start.1);
        let aim = (behind.0, feet + 1.4, behind.1);
        let confirmed = ctx.look_until_facing(aim, 8.0, 5_000).await;
        let leg = vary(ctx, DOLLY_LEG - 0.5, DOLLY_LEG + 0.8);
        let halves = dolly_dwell(ctx, leg, ctx.elapsed_ms() + dwell_ms).await;
        ctx.record(
            TYPE_LOOK,
            &format!("vis-square-backsight-{index}-{edge}"),
            &format!("Corner {here}: look back down the edge from corner {from} and dolly on it."),
            json!({
                "edge": edge,
                "lap": edge / 4,
                "chirality": turn,
                "from_corner": from,
                "at_corner": here.rem_euclid(4),
                "arrival_gap": r2(gap),
                "facing_confirmed": confirmed,
                "backsight": {"x": r2(behind.0), "z": r2(behind.1)},
                "dolly_halves_moved": halves,
                "elapsed_ms": ctx.elapsed_ms(),
            }),
        );

        // Face the next corner before walking to it, so the traverse itself carries no camera
        // turn: `travel_to`'s legs steer the camera onto the travel bearing, and it is already
        // there. This is also the second half of the back-sight pair — the turn that undoes it,
        // which is what keeps a window's NET rotation small while its sweep keeps accumulating.
        let onward = corner(centre, axis_rad, side, here + turn);
        if let Some(p) = ctx.position() {
            ctx.look_default((onward.0, p.1 + 1.4, onward.1)).await;
        }

        edge += 1;
        if edge.is_multiple_of(4) {
            turn = -turn;
            ctx.record(
                TYPE_MOVE,
                &format!("vis-square-reverse-{index}-{edge}"),
                "Lap closed: reverse the patrol direction so corner turns change sign.",
                json!({"lap": edge / 4, "chirality": turn}),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::corner;

    /// The bug this pins: a wrong radius or a missing rotation turns the "square" into a rectangle
    /// or a diamond of the wrong size, and every edge length — which is what the leg budget and the
    /// dolly reach are sized against — silently changes. Also pins that the walk closes, in both
    /// chiralities.
    #[test]
    fn corners_close_a_square_of_the_requested_side() {
        let centre = (-12.0, 41.0);
        for &axis in &[0.0_f64, 0.7, 2.9, -1.3] {
            for &side in &[6.0_f64, 8.0, 10.5] {
                for step in 0..4 {
                    let a = corner(centre, axis, side, step);
                    let b = corner(centre, axis, side, step + 1);
                    let edge = (b.0 - a.0).hypot(b.1 - a.1);
                    assert!(
                        (edge - side).abs() < 1e-9,
                        "axis={axis} side={side} edge={edge}"
                    );
                    // Walking the other way visits the same four places.
                    let mirrored = corner(centre, axis, side, -step);
                    let same = corner(centre, axis, side, 4 - step);
                    assert!(
                        (mirrored.0 - same.0).abs() < 1e-9 && (mirrored.1 - same.1).abs() < 1e-9
                    );
                }
                // A lap closes exactly.
                let start = corner(centre, axis, side, 0);
                let closed = corner(centre, axis, side, 4);
                assert!((start.0 - closed.0).abs() < 1e-9 && (start.1 - closed.1).abs() < 1e-9);
            }
        }
    }
}
