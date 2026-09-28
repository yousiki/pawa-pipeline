//! CATALOG: family=construction min_players=1 single_process=true world_edits=true scenery=none far_players=false
//!
//! Raise a wall panel one course at a time, and after every course walk back to the SAME review
//! stand — the same cell, the same aim point, every cycle — before returning to the work face.
//!
//! Which face of the revisit problem this answers: the OVERLAP, taken to its useful limit. v5's median
//! first/last-frame overlap is 0.58 and 26.7% of windows share nothing at all, because a build
//! scenario's viewpoint keeps moving. Here the review viewpoint is resolved once, before the first
//! block is placed, and never recomputed: `REVIEW_STAND` is a fixed pair of coordinates and the aim
//! point is pinned at the finished panel's mid-height rather than tracking the growing top. So the
//! review frames of cycle 0 and cycle 3 are nearly the same picture with different amounts of wall
//! in it. That is the cleanest possible training pair — the only thing that changed is the thing the
//! bot did, and the block probes in the record say exactly what it was.
//!
//! It is the fixed stand that separates this from `loopbuild_step_back_review`, where the stand
//! tracks the wall's growing end: there the reviewer's viewpoint translates, here it does not.
//!
//! Both loops are still present. Trajectory: `REVIEW_DISTANCE` out and back per course, so
//! `path_length` is many times the net `displacement`. Sight line: work face (2.5 blocks, panel
//! filling the frame) alternating with review (9 blocks, panel plus surroundings), each arrival
//! confirmed with `look_until_facing` so a timed-out turn is not recorded as a return.
//! Occlusion: real terrain between plot and stand, and from the work face the panel itself hides
//! everything behind it once two courses are up.
use super::*;

/// Panel material.
const MATERIAL: &str = "minecraft:stone_bricks";

/// Panel width, in blocks. Three cells per course is a short placement burst, which is what keeps
/// `build_blocks`' own short nav taps a minority of each cycle's movement presses (the key-chatter
/// gate fails an episode with over 35% presses under 0.5 s).
const PANEL_WIDTH: i64 = 3;

/// Courses the panel gets. Four leaves the top cell three blocks up, still inside `place_reach`
/// from the work stand (the far top cell is 3.3 blocks from the eye), so the whole panel is built
/// from the ground with no scaffolding and no walk-off descent.
const COURSES: i64 = 4;

/// Aim height above the panel's base, in blocks. Pinned at the FINISHED panel's mid-height rather
/// than the current top: an aim point that tracked the growth would rotate the camera a little every
/// cycle, and the point of this script is that the review frame does not move.
const REVIEW_AIM_HEIGHT: f64 = 2.0;

/// Retreat distance to the review stand, in blocks — `build_tower`'s measured 9. Far enough for
/// full-height cues of a four-course panel, near enough that a 4.65 s window containing the whole
/// walk still lands near the audit's 5-block displacement median.
const REVIEW_DISTANCE: f64 = 9.0;

/// Stand-off at the work face, in blocks. 2.5 puts every cell of every course inside `place_reach`,
/// so `build_blocks` finds the whole course already in reach.
const WORK_STANDOFF: f64 = 2.5;

/// Centre-to-centre distance between two bots' plots, in blocks — the pitch `parallel_plot_remodel`
/// uses, which keeps the bodies past the 4.5-block peer gap even when both work the sides facing
/// each other.
const PLOT_PITCH: f64 = 12.0;

/// Per-bot rotation of the plot's whole layout, in degrees. The golden angle means no two bots share
/// a work stand or a review stand, so their camera sequences never line up — which is what the
/// near-clones check reads.
const BOT_BEARING_SPREAD_DEG: f64 = 137.5;

/// Dwells at the review stand and the work face, in ms, per bot. Both clear `hold_facing`'s 2.2 s
/// leg budget; a shorter dwell fits no leg and becomes a stand, which the frozen gate fails.
const REVIEW_HOLD_MS: [i64; 3] = [6_000, 7_000, 5_000];
const WORK_HOLD_MS: [i64; 3] = [4_500, 4_000, 5_500];

/// One shuttle leg, in blocks. 4 clears the key-chatter floor at sprint speed; see `hold_facing`.
///
/// The bearing handed to `hold_facing` alongside it is only the OPENING aim. `move_to` overwrites
/// `camera_intent` with `CameraIntent::Hold`, which points the camera along the path whenever the
/// body is moving, so from the first leg onward a hold does NOT keep a subject framed. Every sight
/// line in this script is carried by a STATIONARY `look_until_facing` — walk, stop, look, walk. Do
/// not "fix" a hold by re-issuing `LookAt` inside it: that fights `Hold` for the camera and was
/// measured pushing three already-passing scripts to 96-100% on the camera/travel gate.
const SHUTTLE_LEG: f64 = 4.0;

/// This bot's plot centre on an anchor-centred grid — the layout `parallel_plot_remodel` uses.
fn plot_center(anchor: (f64, f64), index: usize, bot_count: usize) -> (f64, f64) {
    let cols = (bot_count.max(1) as f64).sqrt().ceil() as usize;
    let rows = bot_count.max(1).div_ceil(cols);
    let (col, row) = (index % cols, index / cols);
    (
        anchor.0 + (col as f64 - (cols as f64 - 1.0) / 2.0) * PLOT_PITCH,
        anchor.1 + (row as f64 - (rows as f64 - 1.0) / 2.0) * PLOT_PITCH,
    )
}

/// World point `radius` blocks from `origin` on compass `bearing_deg` (0 = +Z, the `point_at_yaw`
/// convention, so every recorded bearing means one thing).
fn ring_point(origin: (f64, f64), bearing_deg: f64, radius: f64) -> (f64, f64) {
    let a = bearing_deg.to_radians();
    (origin.0 - a.sin() * radius, origin.1 + a.cos() * radius)
}

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let slot = global % REVIEW_HOLD_MS.len();
    let bot_count = ctx.player_count().max(1);
    let anchor = ctx.anchor();
    let (px, pz) = plot_center(anchor, index, bot_count);
    let bearing = BOT_BEARING_SPREAD_DEG * global as f64;
    // Both stands are resolved ONCE, here, and reused verbatim for the whole episode. Recomputing
    // them per cycle is precisely the drift this scenario exists to remove.
    let planned_review_stand = ring_point((px, pz), bearing, REVIEW_DISTANCE);
    let planned_work_stand = ring_point((px, pz), bearing, WORK_STANDOFF);
    let hint_y = ctx.position().map(|p| p.1.round() as i32).unwrap_or(64);
    let review_stand =
        walkable_xz_near(ctx, planned_review_stand, hint_y).unwrap_or(planned_review_stand);
    let work_stand =
        walkable_xz_near(ctx, planned_work_stand, hint_y).unwrap_or(planned_work_stand);

    travel_to(ctx, work_stand, TRAVEL_HOP, 1.2).await;
    let feet_y = ctx.position().map(|p| p.1.round() as i64).unwrap_or(64);
    // The panel runs perpendicular to the review bearing, so it presents its broad face to the
    // fixed stand rather than its edge. On natural terrain `marker_base_near` may shift the base by
    // a couple of blocks; adopting whatever it returns keeps all four courses on one course line.
    let panel_base = marker_base_near(ctx, (px.round() as i64, feet_y, pz.round() as i64), 2);
    let (bx, by, bz) = match panel_base {
        Some(b) => (b.x as i64, b.y as i64, b.z as i64),
        None => (px.round() as i64, feet_y, pz.round() as i64),
    };
    let step = {
        // Unit step along the panel, perpendicular to the review bearing, snapped to an axis so the
        // cells form a straight run of blocks instead of a staircase.
        let a = bearing.to_radians();
        if a.cos().abs() > a.sin().abs() {
            (1i64, 0i64)
        } else {
            (0i64, 1i64)
        }
    };
    let review_focus = (
        bx as f64 + 0.5 + step.0 as f64,
        by as f64 + REVIEW_AIM_HEIGHT,
        bz as f64 + 0.5 + step.1 as f64,
    );
    ctx.record(
        "observe",
        &format!("vis-loopbuild-tier-claim-{index}"),
        "Fix the review stand and aim point before the first block: they never move again.",
        json!({
            "global_index": global,
            "plot": {"x": r2(px), "z": r2(pz)},
            "panel_base": {"x": bx, "y": by, "z": bz},
            "panel_step": {"x": step.0, "z": step.1},
            "panel_resolved": panel_base.is_some(),
            "review_stand": {"x": r2(review_stand.0), "z": r2(review_stand.1)},
            "review_focus": {"x": r2(review_focus.0), "y": r2(review_focus.1), "z": r2(review_focus.2)},
            "bearing_deg": r2(bearing % 360.0),
        }),
    );

    let mut cycle = 0usize;
    let mut courses_done = 0i64;
    while !ctx.target_met() {
        // ---- work face: this course, from arm's length ----
        travel_to(ctx, work_stand, TRAVEL_HOP, 1.2).await;
        let course = courses_done;
        let building = course < COURSES;
        let work_focus = (
            review_focus.0,
            by as f64 + 0.5 + course.min(COURSES - 1) as f64,
            review_focus.2,
        );
        let faced_work = ctx.look_until_facing(work_focus, 8.0, 4_000).await;
        let (placed, attempted) = if building {
            let cells: Vec<((i64, i64, i64), String)> = (0..PANEL_WIDTH)
                .map(|i| {
                    (
                        (bx + step.0 * i, by + course, bz + step.1 * i),
                        MATERIAL.to_string(),
                    )
                })
                .collect();
            let total = cells.len();
            ctx.build_blocks(&cells, |b, _target, _material| {
                (
                    format!("vis-loopbuild-tier-place-{index}-c{cycle}-{b}"),
                    format!("Place course cell {}/{total} of this tier.", b + 1),
                    json!({"cycle": cycle, "course": course, "cell_index": b}),
                )
            })
            .await
        } else {
            (0, 0)
        };
        if building {
            courses_done += 1;
        }
        ctx.record(
            TYPE_PLACE,
            &format!("vis-loopbuild-tier-course-{index}-c{cycle}"),
            "Course finished; the fixed review stand is next, then straight back here.",
            json!({
                "cycle": cycle,
                "course": course,
                "building": building,
                "is_return": cycle > 0,
                "faced_work": faced_work,
                "placed": placed,
                "attempted": attempted,
                "courses_done": courses_done,
            }),
        );
        let work_bearing = ctx
            .position()
            .map(|p| look_angles(head_of(p), work_focus).0)
            .unwrap_or(0.0);
        hold_facing(
            ctx,
            work_bearing,
            ctx.elapsed_ms() + WORK_HOLD_MS[slot],
            SHUTTLE_LEG,
        )
        .await;
        if ctx.target_met() {
            break;
        }

        // ---- review: the same cell, the same aim, one course further along ----
        travel_to(ctx, review_stand, TRAVEL_HOP, 1.2).await;
        let faced_review = ctx.look_until_facing(review_focus, 6.0, 5_000).await;
        // Read the whole panel from here, course by course: the count is what makes "the wall grew"
        // checkable against the frame instead of inferred from the placement log.
        let expected = MATERIAL.strip_prefix("minecraft:").unwrap_or(MATERIAL);
        let standing: Vec<i64> = (0..COURSES)
            .map(|c| {
                (0..PANEL_WIDTH)
                    .filter(|i| {
                        let cell = BlockPos::new(
                            (bx + step.0 * i) as i32,
                            (by + c) as i32,
                            (bz + step.1 * i) as i32,
                        );
                        ctx.block_id_at(cell).as_deref() == Some(expected)
                    })
                    .count() as i64
            })
            .collect();
        ctx.record(
            "observe",
            &format!("vis-loopbuild-tier-review-{index}-c{cycle}"),
            "Review from the unchanged stand: same viewpoint every cycle, taller panel each time.",
            json!({
                "cycle": cycle,
                "is_return": cycle > 0,
                "faced_review": faced_review,
                "courses_done": courses_done,
                "cells_standing_per_course": standing,
                "review_stand": {"x": r2(review_stand.0), "z": r2(review_stand.1)},
                "position": ctx.position().map(|p| json!({"x": r2(p.0), "y": r2(p.1), "z": r2(p.2)})),
                "elapsed_ms": ctx.elapsed_ms(),
            }),
        );
        let review_bearing = ctx
            .position()
            .map(|p| look_angles(head_of(p), review_focus).0)
            .unwrap_or(0.0);
        hold_facing(
            ctx,
            review_bearing,
            ctx.elapsed_ms() + REVIEW_HOLD_MS[slot],
            SHUTTLE_LEG,
        )
        .await;
        witness_peer_edits(ctx).await;
        cycle += 1;
    }
}
