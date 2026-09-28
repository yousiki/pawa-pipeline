//! CATALOG: family=construction min_players=2 single_process=true world_edits=true scenery=none far_players=false
//!
//! One bot grows a template at the anchor; the other studies it from twelve blocks away, turns a
//! full half-circle onto its own workstation, copies the block it just saw, then turns back to the
//! template to check the copy against the original.
//!
//! Which face of the revisit problem this answers: a revisit with a REASON. The audit's `net_over_sweep` of
//! 0.80 says v5's camera turns away and stays turned; the fix is not just to turn back, it is to
//! give the script something that only a turn back can supply. The copier physically cannot place
//! the next cell without having looked at the template, and it cannot verify the placement without
//! looking at the template again — so every cycle contains two returns to the same subject with a
//! world edit between them, and each return is confirmed with `look_until_facing` rather than a
//! `look` that would report a timed-out half-circle as an arrival.
//!
//! Why the workstation sits BEYOND the copier, further from the anchor: it puts the copy site and
//! the template on exactly opposite bearings, so the study/copy swing is past the 102.5 deg
//! horizontal FOV and the two views share no pixels — which is what makes the return a genuine
//! re-observation rather than a nudge. Both sites are inside `place_reach` of their stands, so the
//! swing costs no translation at all.
//!
//! The keeper's own loop is the trajectory half: it walks out `REVIEW_DISTANCE` after every addition
//! and comes back, so its `path_length` is many times its net `displacement`. Occlusion is real
//! terrain across the twelve-block sight line plus, for the copier, its own growing copy blocking
//! the low view of the ground beyond it.
use super::*;

/// Both sides use the SAME material: the copy is meant to be a copy, and the two structures are
/// twelve blocks apart, so a probe is never ambiguous about which one it read.
const MATERIAL: &str = "minecraft:stone_bricks";

/// The shape both sides grow, one cell per cycle, in build order. Bottom-up and near-first, so every
/// cell has support by the time it is placed. Six cells fills a 120 s episode at roughly one cell
/// per cycle without ever asking for an occupied cell (a dead target, which `build_blocks` gives up
/// on after three).
const SHAPE: [(i64, i64, i64); 6] = [
    (0, 0, 0),
    (1, 0, 0),
    (0, 1, 0),
    (1, 1, 0),
    (0, 2, 0),
    (0, 0, 1),
];

/// How far the copier stands from the anchor, in blocks. Twelve is far enough that the template is a
/// distant object needing a real re-acquisition, and it keeps the copier clear of the keeper working
/// at the anchor by well over the 4.5-block peer gap.
const STUDY_DISTANCE: f64 = 12.0;

/// How far beyond the copier its own copy sits, in blocks. Three keeps every cell of the shape
/// inside `place_reach` from the workstation while putting the copy on the opposite bearing from the
/// template — that opposition is what makes the swing exceed the field of view.
const COPY_OFFSET: f64 = 3.0;

/// Stand-off the keeper builds the template from, in blocks. Inside `place_reach` for the whole
/// shape, so `build_blocks` never issues its own short nav taps.
const TEMPLATE_STANDOFF: f64 = 2.5;

/// The keeper's retreat distance, in blocks — `build_tower`'s measured 9. Far enough for full cues
/// of the template, near enough that a 4.65 s window containing the walk stays near the audit's
/// 5-block displacement median.
const REVIEW_DISTANCE: f64 = 9.0;

/// Bearings the two roles occupy, in degrees, so the keeper's review stand and the copier's
/// workstation end up on nearly opposite sides of the anchor: ~18 blocks apart, and each has the
/// other roughly in frame while both look at the template.
const KEEPER_BEARING_DEG: f64 = 20.0;
const COPIER_BEARING_DEG: f64 = 200.0;

/// Per-bot rotation applied on top of the role bearing, so a swarm larger than two still spreads
/// instead of stacking two keepers on one stand.
const BOT_BEARING_SPREAD_DEG: f64 = 47.0;

/// Dwells, in ms, per bot. All clear `hold_facing`'s 2.2 s leg budget — a shorter dwell fits no leg
/// and becomes a stand, which the frozen gate fails outright. Unequal across bots so the two POVs
/// never share a phase, which is what the near-clones check reads.
const STUDY_HOLD_MS: [i64; 2] = [5_000, 6_000];
const WORK_HOLD_MS: [i64; 2] = [4_500, 5_500];

/// One shuttle leg, in blocks. 4 clears the key-chatter floor at sprint speed; see `hold_facing`.
///
/// The bearing handed to `hold_facing` alongside it is only the OPENING aim. `move_to` overwrites
/// `camera_intent` with `CameraIntent::Hold`, which points the camera along the path whenever the
/// body is moving, so from the first leg onward a hold does NOT keep a subject framed. Every sight
/// line in this script is carried by a STATIONARY `look_until_facing` — walk, stop, look, walk. Do
/// not "fix" a hold by re-issuing `LookAt` inside it: that fights `Hold` for the camera and was
/// measured pushing three already-passing scripts to 96-100% on the camera/travel gate.
const SHUTTLE_LEG: f64 = 4.0;

/// World point `radius` blocks from `origin` on compass `bearing_deg` (0 = +Z, the `point_at_yaw`
/// convention, so every recorded bearing means one thing).
fn ring_point(origin: (f64, f64), bearing_deg: f64, radius: f64) -> (f64, f64) {
    let a = bearing_deg.to_radians();
    (origin.0 - a.sin() * radius, origin.1 + a.cos() * radius)
}

/// The absolute cell of shape entry `k` relative to a resolved base.
fn shape_cell(base: BlockPos, k: usize) -> Option<BlockPos> {
    let (dx, dy, dz) = *SHAPE.get(k)?;
    Some(BlockPos::new(
        base.x + dx as i32,
        base.y + dy as i32,
        base.z + dz as i32,
    ))
}

/// How many cells of the shape are standing at `base`.
fn shape_standing(ctx: &Ctx, base: BlockPos) -> usize {
    let expected = MATERIAL.strip_prefix("minecraft:").unwrap_or(MATERIAL);
    (0..SHAPE.len())
        .filter(|&k| {
            shape_cell(base, k)
                .and_then(|cell| ctx.block_id_at(cell))
                .as_deref()
                == Some(expected)
        })
        .count()
}

/// Place shape entry `k` at `base`, reporting whether it went in.
async fn place_shape_cell(ctx: &mut Ctx, base: BlockPos, k: usize, vis_prefix: &str) -> u64 {
    let Some(cell) = shape_cell(base, k) else {
        return 0;
    };
    let blocks = [(
        (cell.x as i64, cell.y as i64, cell.z as i64),
        MATERIAL.to_string(),
    )];
    let (placed, _) = ctx
        .build_blocks(&blocks, |_b, _target, _material| {
            (
                format!("{vis_prefix}-{k}"),
                format!("Place shape cell {k} of the pattern."),
                json!({"shape_index": k}),
            )
        })
        .await;
    placed
}

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let slot = global % 2;
    // Parity assigns the roles, so a two-bot episode always gets exactly one keeper and one copier
    // without either side having to be told.
    let keeper = slot == 0;
    let spread = BOT_BEARING_SPREAD_DEG * global as f64;
    let anchor = ctx.anchor();
    let start = ctx.position().unwrap_or((anchor.0, 64.0, anchor.1));
    let ground_y = start.1.round() as i64;
    let anchor_z = anchor.1.round() as i64;

    if keeper {
        let planned_stand = ring_point(anchor, KEEPER_BEARING_DEG + spread, TEMPLATE_STANDOFF);
        let planned_review_stand = ring_point(anchor, KEEPER_BEARING_DEG + spread, REVIEW_DISTANCE);
        let stand = marker_base_near(
            ctx,
            (
                planned_stand.0.round() as i64,
                ground_y,
                planned_stand.1.round() as i64,
            ),
            1,
        )
        .map(|b| (b.x as f64 + 0.5, b.z as f64 + 0.5))
        .unwrap_or(planned_stand);
        let review_stand = marker_base_near(
            ctx,
            (
                planned_review_stand.0.round() as i64,
                ground_y,
                planned_review_stand.1.round() as i64,
            ),
            1,
        )
        .map(|b| (b.x as f64 + 0.5, b.z as f64 + 0.5))
        .unwrap_or(planned_review_stand);
        travel_to(ctx, stand, TRAVEL_HOP, 1.2).await;
        let feet_y = ctx
            .position()
            .map(|p| p.1.round() as i64)
            .unwrap_or(ground_y);
        let base = marker_base_near(ctx, (anchor.0.round() as i64, feet_y, anchor_z), 3);
        let focus = base
            .map(|b| (b.x as f64 + 0.5, b.y as f64 + 1.0, b.z as f64 + 0.5))
            .unwrap_or((anchor.0, feet_y as f64 + 1.0, anchor.1));
        ctx.record(
            "observe",
            &format!("vis-loopbuild-copy-keeper-{index}"),
            "Keeper claims the template site the copier will be reading all episode.",
            json!({
                "global_index": global,
                "role": "keeper",
                "template_base": base.map(|b| json!({"x": b.x, "y": b.y, "z": b.z})),
                "template_focus": {"x": r2(focus.0), "y": r2(focus.1), "z": r2(focus.2)},
                "review_stand": {"x": r2(review_stand.0), "z": r2(review_stand.1)},
            }),
        );

        let mut cycle = 0usize;
        while !ctx.target_met() {
            // Grow the template by one cell, then walk out and back: the addition is what the copier
            // has to notice, and the retreat is this POV's own trajectory loop.
            travel_to(ctx, stand, TRAVEL_HOP, 1.2).await;
            let faced_work = ctx.look_until_facing(focus, 8.0, 4_000).await;
            let placed = match base {
                Some(base) if cycle < SHAPE.len() => {
                    let prefix = format!("vis-loopbuild-copy-tpl-{index}-c{cycle}");
                    place_shape_cell(ctx, base, cycle, &prefix).await
                }
                _ => 0,
            };
            let standing = base.map(|b| shape_standing(ctx, b)).unwrap_or(0);
            ctx.record(
                TYPE_PLACE,
                &format!("vis-loopbuild-copy-grow-{index}-c{cycle}"),
                "Template grown by one cell; next the keeper steps back to see the whole of it.",
                json!({"cycle": cycle, "faced_work": faced_work, "placed": placed,
                    "shape_index": cycle, "cells_standing": standing}),
            );
            let work_bearing = ctx
                .position()
                .map(|p| look_angles(head_of(p), focus).0)
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

            travel_to(ctx, review_stand, TRAVEL_HOP, 1.2).await;
            let faced_review = ctx.look_until_facing(focus, 6.0, 5_000).await;
            ctx.record(
                "observe",
                &format!("vis-loopbuild-copy-keeper-review-{index}-c{cycle}"),
                "Keeper reviews the whole template from nine blocks back, then returns to it.",
                json!({"cycle": cycle, "is_return": cycle > 0, "faced_review": faced_review,
                    "cells_standing": base.map(|b| shape_standing(ctx, b)),
                    "elapsed_ms": ctx.elapsed_ms()}),
            );
            let review_bearing = ctx
                .position()
                .map(|p| look_angles(head_of(p), focus).0)
                .unwrap_or(0.0);
            hold_facing(
                ctx,
                review_bearing,
                ctx.elapsed_ms() + STUDY_HOLD_MS[slot],
                SHUTTLE_LEG,
            )
            .await;
            witness_peer_edits(ctx).await;
            cycle += 1;
        }
        return;
    }

    // ---- copier ----
    let bearing = COPIER_BEARING_DEG + spread;
    let planned_workstation = ring_point(anchor, bearing, STUDY_DISTANCE);
    let copy_site = ring_point(anchor, bearing, STUDY_DISTANCE + COPY_OFFSET);
    let workstation = marker_base_near(
        ctx,
        (
            planned_workstation.0.round() as i64,
            ground_y,
            planned_workstation.1.round() as i64,
        ),
        1,
    )
    .map(|b| (b.x as f64 + 0.5, b.z as f64 + 0.5))
    .unwrap_or(planned_workstation);
    travel_to(ctx, workstation, TRAVEL_HOP, 1.2).await;
    let feet_y = ctx
        .position()
        .map(|p| p.1.round() as i64)
        .unwrap_or(ground_y);
    let copy_base = marker_base_near(
        ctx,
        (
            copy_site.0.round() as i64,
            feet_y,
            copy_site.1.round() as i64,
        ),
        3,
    );
    let copy_focus = copy_base
        .map(|b| (b.x as f64 + 0.5, b.y as f64 + 1.0, b.z as f64 + 0.5))
        .unwrap_or((copy_site.0, feet_y as f64 + 1.0, copy_site.1));
    ctx.record(
        "observe",
        &format!("vis-loopbuild-copy-copier-{index}"),
        "Copier claims a workstation with the template ahead of it and its own site behind.",
        json!({
            "global_index": global,
            "role": "copier",
            "workstation": {"x": r2(workstation.0), "z": r2(workstation.1)},
            "copy_base": copy_base.map(|b| json!({"x": b.x, "y": b.y, "z": b.z})),
            "study_distance": STUDY_DISTANCE,
        }),
    );

    let mut cycle = 0usize;
    while !ctx.target_met() {
        travel_to(ctx, workstation, TRAVEL_HOP, 1.2).await;

        // ---- study: acquire the template across twelve blocks ----
        // Probe rather than assume: the keeper's base may have shifted off the anchor column on
        // natural terrain, and `area_find_block` searches exactly the neighbourhood
        // `marker_base_near` could have moved it into, so a hit is the real structure.
        let template = area_find_block(
            ctx,
            (anchor.0.round() as i32, anchor_z as i32),
            (ground_y as i32 - 4, ground_y as i32 + 6),
            MATERIAL,
        );
        let template_focus = template
            .map(|c| (c.x as f64 + 0.5, c.y as f64 + 1.0, c.z as f64 + 0.5))
            .unwrap_or((anchor.0, ground_y as f64 + 1.0, anchor.1));
        let studied = ctx.look_until_facing(template_focus, 6.0, 6_000).await;
        let template_standing = template.map(|c| shape_standing(ctx, c));
        ctx.record(
            "observe",
            &format!("vis-loopbuild-copy-study-{index}-c{cycle}"),
            "Study the template: this look is what the next placement is copied from.",
            json!({
                "cycle": cycle,
                "is_return": cycle > 0,
                "studied": studied,
                "template_found": template.is_some(),
                "template_cells_standing": template_standing,
                "template_focus": {"x": r2(template_focus.0), "y": r2(template_focus.1),
                    "z": r2(template_focus.2)},
            }),
        );
        let study_bearing = ctx
            .position()
            .map(|p| look_angles(head_of(p), template_focus).0)
            .unwrap_or(0.0);
        hold_facing(
            ctx,
            study_bearing,
            ctx.elapsed_ms() + STUDY_HOLD_MS[slot],
            SHUTTLE_LEG,
        )
        .await;
        if ctx.target_met() {
            break;
        }

        // ---- copy: half-circle turn onto the bot's own site, place the cell just studied ----
        travel_to(ctx, workstation, TRAVEL_HOP, 1.5).await;
        let framed_copy = ctx.look_until_facing(copy_focus, 8.0, 6_000).await;
        let placed = match copy_base {
            Some(base) if cycle < SHAPE.len() => {
                let prefix = format!("vis-loopbuild-copy-cell-{index}-c{cycle}");
                place_shape_cell(ctx, base, cycle, &prefix).await
            }
            _ => 0,
        };
        let copy_standing = copy_base.map(|b| shape_standing(ctx, b));
        ctx.record(
            TYPE_PLACE,
            &format!("vis-loopbuild-copy-place-{index}-c{cycle}"),
            "Copy the studied cell onto this bot's own site, a half-circle from the template.",
            json!({
                "cycle": cycle,
                "framed_copy": framed_copy,
                "exceeds_horizontal_fov": true,
                "placed": placed,
                "shape_index": cycle,
                "copy_cells_standing": copy_standing,
            }),
        );
        let copy_bearing = ctx
            .position()
            .map(|p| look_angles(head_of(p), copy_focus).0)
            .unwrap_or(0.0);
        hold_facing(
            ctx,
            copy_bearing,
            ctx.elapsed_ms() + WORK_HOLD_MS[slot],
            SHUTTLE_LEG,
        )
        .await;
        if ctx.target_met() {
            break;
        }

        // ---- compare: turn back onto the template with the copy fresh behind ----
        // The second return of the cycle, and the one that carries the check: both structures are
        // counted here, so a scored window can be told whether the copy matched what was on screen.
        let compared = ctx.look_until_facing(template_focus, 6.0, 6_000).await;
        let template_after = template.map(|c| shape_standing(ctx, c));
        // Hoisted out of the record payload: a `match` inside `json!` is a tt-muncher hazard, and
        // the comparison is the point of this beat, so it deserves a name.
        let matched = match (template_after, copy_standing) {
            (Some(a), Some(b)) => Some(a == b),
            _ => None,
        };
        ctx.record(
            "observe",
            &format!("vis-loopbuild-copy-compare-{index}-c{cycle}"),
            "Turn back onto the template to check the copy: same subject, one cell later.",
            json!({
                "cycle": cycle,
                "compared": compared,
                "exceeds_horizontal_fov": true,
                "template_cells_standing": template_after,
                "copy_cells_standing": copy_standing,
                "matched": matched,
                "elapsed_ms": ctx.elapsed_ms(),
            }),
        );
        hold_facing(
            ctx,
            study_bearing,
            ctx.elapsed_ms() + STUDY_HOLD_MS[slot],
            SHUTTLE_LEG,
        )
        .await;
        witness_peer_edits(ctx).await;
        cycle += 1;
    }
}
