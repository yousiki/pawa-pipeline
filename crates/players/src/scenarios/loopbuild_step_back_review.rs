//! CATALOG: family=construction min_players=2 single_process=true world_edits=true scenery=none far_players=false
//!
//! Build a wall segment, walk back to a review stand, confirm the finished wall is in frame, return
//! to the work face — the build loop, as opposed to the build orbit.
//!
//! Which face of the revisit problem this answers: the v5 scenarios with the WORST first/last-frame overlap
//! are all construction (`radial_trail_build` 0.40, `rosette_tower_ring` 0.42, `build_house` 0.54),
//! and they share one shape — the camera circles the structure in a single direction and never
//! looks back at the part it just passed. This script replaces that orbit with a shuttle, so both
//! kinds of loop the audit measures are present:
//!
//! - **Trajectory** loops: every cycle walks out `REVIEW_DISTANCE` and back to the work face, so
//!   `path_length` grows while net `displacement` stays inside a single leg.
//! - **Sight line** loops: the work-face view (2.5 blocks off the wall) and the review view (9
//!   blocks back) are two ends of a yaw that swings back and forth, and each arrival is confirmed
//!   with `look_until_facing` — a plain `look` returns on its deadline, so a wide turn that timed
//!   out would be logged as a return that never happened.
//! - **Occlusion** comes from real terrain plus, from the second segment on, the wall itself: from
//!   the work face the two-block wall hides the columns beyond it, and stepping back is what
//!   restores them. That recovery is the thing worth learning.
//!
//! Roles split by global parity so a two-bot episode is never two bots doing the same thing: the
//! mason places, the foreman only walks the same loop and watches. The foreman is also the
//! participation insurance — building is standing time (turn to the cell, place, verify), and the
//! measured failure mode is a builder whose moving-frame count drops under the gate.
use super::*;

/// The wall material. `stone_bricks` stays legible against natural terrain at the review distance,
/// where a wood tone tends to sink into the ground cover.
const MATERIAL: &str = "minecraft:stone_bricks";

/// Wall height. Two blocks is tall enough to occlude the far columns from the work face — the
/// occlusion this script wants — while the top layer is still well inside the 4.0-block
/// `place_reach` clamp from the ground.
const WALL_HEIGHT: i64 = 2;

/// Columns added per cycle. Two keeps the burst at four cells: `build_blocks` navigates itself when
/// a cell is out of reach, and those short pathfinder taps are exactly what the key-chatter gate
/// counts (over 35% of movement presses under 0.5 s fails), so the burst has to stay a minority of
/// each cycle's movement.
const COLUMNS_PER_CYCLE: i64 = 2;

/// Cycles that still extend the wall. Five cycles x two columns is a 10-block wall, which spans
/// ~58 deg at the review distance — inside the 102.5 deg horizontal FOV, so the review frame really
/// does contain everything built so far. Later cycles keep looping without placing rather than
/// wandering off to find fresh ground.
const GROWING_CYCLES: usize = 5;

/// Retreat distance to the review stand, in blocks. 9 is `build_tower`'s measured retreat: far
/// enough for full-height cues, near enough that a 4.65 s window spent walking it still lands under
/// the audit's 5-block displacement median.
const REVIEW_DISTANCE: f64 = 9.0;

/// Sideways splay of the review stands, in degrees off the wall normal. +/-35 deg at 9 blocks puts
/// the two stands 10.3 blocks apart, clear of `MIN_PEER_GAP`, while both still frame the wall — so
/// each POV opens with the teammate roughly in view, which is what the compact-staging rule wants.
const REVIEW_SPLAY_DEG: [f64; 2] = [35.0, -35.0];

/// Stand-off while placing, in blocks. Inside `place_reach` for both wall layers, so `build_blocks`
/// finds every cell already in reach and issues no nav taps at all.
const WORK_STANDOFF: f64 = 2.5;

/// How far along the axis, past the growing end, the foreman sights down the wall. Five blocks
/// along plus one off it keeps the two bodies 5.2 blocks apart at the tightest cycle, past the
/// 4.5-block peer gap, without either role having to path around the wall.
const FOREMAN_OFFSET: (f64, f64) = (5.0, 1.0);

/// Dwell lengths per role, in ms. Deliberately unequal: the near-clones check fails a pair whose
/// action sequences agree over 80% of the episode, and unequal dwells put the two POVs permanently
/// out of phase. Both are well over `hold_facing`'s 2.2 s leg budget, so every dwell really
/// shuttles instead of degenerating into a stand (which the frozen gate fails outright).
const WORK_HOLD_MS: [i64; 2] = [4_500, 5_500];
const REVIEW_HOLD_MS: [i64; 2] = [7_000, 5_500];

/// One shuttle leg, in blocks. 4 clears the key-chatter floor at sprint speed and matches Solaris's
/// 3-4 block random-walk primitive; see `hold_facing`.
///
/// The bearing handed to `hold_facing` alongside it is only the OPENING aim. `move_to` overwrites
/// `camera_intent` with `CameraIntent::Hold`, which points the camera along the path whenever the
/// body is moving, so from the first leg onward a hold does NOT keep a subject framed. Every sight
/// line in this script is carried by a STATIONARY `look_until_facing` — walk, stop, look, walk. Do
/// not "fix" a hold by re-issuing `LookAt` inside it: that fights `Hold` for the camera and was
/// measured pushing three already-passing scripts to 96-100% on the camera/travel gate.
const SHUTTLE_LEG: f64 = 4.0;

/// The cells of one wall column, in `build_blocks` form.
fn column_cells(base: BlockPos) -> Vec<((i64, i64, i64), String)> {
    (0..WALL_HEIGHT)
        .map(|dy| {
            (
                (base.x as i64, base.y as i64 + dy, base.z as i64),
                MATERIAL.to_string(),
            )
        })
        .collect()
}

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    // Parity, not `index`, so the roles survive a sharded swarm: a two-bot episode always gets
    // exactly one mason and one foreman.
    let mason = global.is_multiple_of(2);
    let slot = global % 2;
    let anchor = ctx.anchor();
    let start = ctx.position().unwrap_or((anchor.0, 64.0, anchor.1));
    // The wall axis is fixed in WORLD coordinates (+X, normal +Z) rather than derived from either
    // bot's own spawn: the two roles have to agree on where the wall is without exchanging
    // anything, and only the anchor is common knowledge.
    let anchor_z = anchor.1.round() as i64;
    let mut wall_y = start.1.round() as i64;

    ctx.record(
        "observe",
        &format!("vis-loopbuild-review-open-{index}"),
        "Open the step-back review loop: claim a role and frame the wall site.",
        json!({
            "global_index": global,
            "role": if mason { "mason" } else { "foreman" },
            "wall_origin": {"x": r2(anchor.0), "z": r2(anchor.1)},
            "review_distance": REVIEW_DISTANCE,
            "review_splay_deg": REVIEW_SPLAY_DEG[slot],
        }),
    );

    let mut cycle = 0usize;
    while !ctx.target_met() {
        let growing = cycle < GROWING_CYCLES;
        // The lead column stops advancing once the wall is done, so the loop keeps its geometry
        // instead of marching off the levelled ground the swarm was staged on.
        let segments = (cycle + 1).min(GROWING_CYCLES) as i64;
        let lead_x = anchor.0 + ((segments - 1) * COLUMNS_PER_CYCLE) as f64;

        // ---- work face: close in, confirm the view, place ----
        let planned_work_stand = if mason {
            (lead_x, anchor.1 + WORK_STANDOFF)
        } else {
            (lead_x + FOREMAN_OFFSET.0, anchor.1 + FOREMAN_OFFSET.1)
        };
        let work_stand =
            walkable_xz_near(ctx, planned_work_stand, wall_y as i32).unwrap_or(planned_work_stand);
        travel_to(ctx, work_stand, TRAVEL_HOP, 1.2).await;
        // The foreman never places, so it has no base cell of its own to learn the wall's Y from;
        // probe the mason's work instead. `area_find_block` searches the same neighbourhood
        // `marker_base_near` may have shifted the column into, so a hit is real and a miss is a
        // genuine "nothing built here yet" rather than a one-cell near-miss.
        if !mason
            && let Some(found) = area_find_block(
                ctx,
                (anchor.0.round() as i32, anchor_z as i32),
                (wall_y as i32 - 3, wall_y as i32 + 4),
                MATERIAL,
            )
        {
            wall_y = found.y as i64;
        }
        let work_focus = (lead_x, wall_y as f64 + 1.0, anchor.1);
        let faced_work = ctx.look_until_facing(work_focus, 6.0, 4_000).await;

        let mut placed = 0u64;
        let mut attempted = 0u64;
        if mason && growing {
            let mut cells: Vec<((i64, i64, i64), String)> = Vec::new();
            for j in 0..COLUMNS_PER_CYCLE {
                let cx = (lead_x + j as f64).round() as i64;
                // Natural terrain is why this is a search and not arithmetic: the desired column can
                // be inside a slope or filled with grass, and `marker_base_near` walks out to a cell
                // that is genuinely supported air. Adopting its Y keeps the whole wall on one course.
                if let Some(base) = marker_base_near(ctx, (cx, wall_y, anchor_z), WALL_HEIGHT) {
                    wall_y = base.y as i64;
                    cells.extend(column_cells(base));
                }
            }
            let total = cells.len();
            let (p, a) = ctx
                .build_blocks(&cells, |b, _target, _material| {
                    (
                        format!("vis-loopbuild-review-place-{index}-c{cycle}-{b}"),
                        format!("Place wall cell {}/{total} of segment {cycle}.", b + 1),
                        json!({"cycle": cycle, "cell_index": b, "segment": segments}),
                    )
                })
                .await;
            placed = p;
            attempted = a;
        }
        ctx.record(
            TYPE_LOOK,
            &format!("vis-loopbuild-review-face-{index}-c{cycle}"),
            "Work face: the wall from arm's length, before stepping back to review it.",
            json!({
                "cycle": cycle,
                "is_return": cycle > 0,
                "faced_work": faced_work,
                "growing": growing,
                "placed": placed,
                "attempted": attempted,
                "wall_y": wall_y,
                "lead_x": r2(lead_x),
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

        // ---- review stand: the same subject, from the far end of the loop ----
        let mid_x = anchor.0 + ((segments * COLUMNS_PER_CYCLE - 1) as f64) * 0.5;
        let review_focus = (mid_x, wall_y as f64 + 1.0, anchor.1);
        let splay = REVIEW_SPLAY_DEG[slot].to_radians();
        let planned_stand = (
            mid_x - splay.sin() * REVIEW_DISTANCE,
            anchor.1 + splay.cos() * REVIEW_DISTANCE,
        );
        let stand = walkable_xz_near(ctx, planned_stand, wall_y as i32).unwrap_or(planned_stand);
        travel_to(ctx, stand, TRAVEL_HOP, 1.5).await;
        let faced_review = ctx.look_until_facing(review_focus, 6.0, 5_000).await;
        // Probe the FIRST column, not the newest: it is the part of the wall that has been out of
        // frame longest, so reading it here is what makes "the wall is still there" evidence rather
        // than an assumption when the footage is scored.
        let origin_block = column_find_block(
            ctx,
            anchor.0.round() as i32,
            anchor_z as i32,
            (wall_y as i32 - 3, wall_y as i32 + 4),
            MATERIAL,
        );
        ctx.record(
            "observe",
            &format!("vis-loopbuild-review-stand-{index}-c{cycle}"),
            "Review stand: the whole wall re-framed from 9 blocks back.",
            json!({
                "cycle": cycle,
                "is_return": cycle > 0,
                "faced_review": faced_review,
                "review_stand": {"x": r2(stand.0), "z": r2(stand.1)},
                "review_focus": {"x": r2(mid_x), "y": wall_y + 1, "z": r2(anchor.1)},
                "origin_column_y": origin_block,
                "origin_column_persisted": origin_block.is_some(),
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
        // The foreman's whole job is that the mason's edits show up in another POV a few seconds
        // later; this is the channel that records it.
        if !mason {
            witness_peer_edits(ctx).await;
        }
        cycle += 1;
    }
}
