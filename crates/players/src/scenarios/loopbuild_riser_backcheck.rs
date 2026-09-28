//! CATALOG: family=construction min_players=1 single_process=false world_edits=true scenery=none far_players=false
//!
//! Grow a narrow riser one cell at a time, then leave the work face for a fixed back-check and return.
//!
//! Which face of the revisit problem this answers: a construction loop with a stable reference view. The riser
//! is built bottom-up on a natural-terrain base, while every cycle alternates a close work view with
//! the same six-block review stand. The return is confirmed with `look_until_facing`, so a delayed
//! wide turn is not mislabeled as a successful revisit. The riser occludes the ground immediately
//! behind the work face once its upper course exists; stepping back restores that context.
//!
//! The trajectory is also a loop: work -> review -> work. `hold_facing` supplies four-block
//! out-and-back shuttles at each stationary view, so the bot does not freeze during placement and
//! review. No level type is assumed; `marker_base_near` resolves support on the actual terrain.
use super::*;

const MATERIAL: &str = "minecraft:mossy_stone_bricks";
/// Two columns of three cells make a visible riser without requiring scaffolding.
const WIDTH: i64 = 2;
const HEIGHT: i64 = 3;
/// Six blocks gives the review a different silhouette over several cycles while keeping each pass short.
const REVIEW_DISTANCE: f64 = 6.0;
/// Arm's-length stand keeps every riser cell inside placement reach.
const WORK_STANDOFF: f64 = 2.5;
const WORK_HOLD_MS: [i64; 3] = [4_500, 5_000, 5_500];
const REVIEW_HOLD_MS: [i64; 3] = [5_500, 6_000, 4_500];
/// Four blocks clears the short-key floor; `hold_facing` returns to its entry point each leg.
const SHUTTLE_LEG: f64 = 4.0;

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let slot = global % WORK_HOLD_MS.len();
    let anchor = ctx.anchor();
    let hint_y = ctx.position().map(|p| p.1.round() as i64).unwrap_or(64);
    let planned_stand = (anchor.0, anchor.1 - WORK_STANDOFF);
    let stand = marker_base_near(
        ctx,
        (
            planned_stand.0.round() as i64,
            hint_y,
            planned_stand.1.round() as i64,
        ),
        1,
    )
    .map(|b| (b.x as f64 + 0.5, b.z as f64 + 0.5))
    .unwrap_or(planned_stand);
    let planned_review = (anchor.0, anchor.1 + REVIEW_DISTANCE);
    let review = marker_base_near(
        ctx,
        (
            planned_review.0.round() as i64,
            hint_y,
            planned_review.1.round() as i64,
        ),
        1,
    )
    .map(|b| (b.x as f64 + 0.5, b.z as f64 + 0.5))
    .unwrap_or(planned_review);
    travel_to(ctx, stand, TRAVEL_HOP, 1.2).await;
    let feet_y = ctx.position().map(|p| p.1.round() as i64).unwrap_or(64);
    let base = marker_base_near(
        ctx,
        (anchor.0.round() as i64, feet_y, anchor.1.round() as i64),
        HEIGHT,
    );
    let focus = base
        .map(|b| (b.x as f64 + 0.5, b.y as f64 + 1.0, b.z as f64 + 0.5))
        .unwrap_or((anchor.0, feet_y as f64 + 1.0, anchor.1));
    ctx.record(
        "observe",
        &format!("vis-loopbuild-riser-setup-{index}"),
        "Resolve the natural-terrain riser base and its fixed review line.",
        json!({
            "global_index": global,
            "base": base.map(|b| json!({"x": b.x, "y": b.y, "z": b.z})),
            "review": {"x": r2(review.0), "z": r2(review.1)},
            "height": HEIGHT,
            "width": WIDTH,
        }),
    );

    let mut cycle = 0usize;
    while !ctx.target_met() {
        travel_to(ctx, stand, TRAVEL_HOP, 1.2).await;
        let layer = (cycle / WIDTH as usize).min(HEIGHT as usize - 1) as i64;
        let column = cycle % WIDTH as usize;
        let target = base.map(|b| BlockPos::new(b.x + column as i32, b.y + layer as i32, b.z));
        let work_focus = target
            .map(|p| (p.x as f64 + 0.5, p.y as f64 + 0.5, p.z as f64 + 0.5))
            .unwrap_or(focus);
        let faced_work = ctx.look_until_facing(work_focus, 6.0, 4_000).await;
        let (placed, attempted) = if let Some(target) = target {
            let cells = [(
                (target.x as i64, target.y as i64, target.z as i64),
                MATERIAL.to_string(),
            )];
            ctx.build_blocks(&cells, |b, _target, _material| {
                (
                    format!("vis-loopbuild-riser-place-{index}-c{cycle}-{b}"),
                    format!("Place riser cell {}/{WIDTH} in layer {layer}.", b + 1),
                    json!({"cycle": cycle, "layer": layer, "column": column}),
                )
            })
            .await
        } else {
            (0, 0)
        };
        ctx.record(
            TYPE_PLACE,
            &format!("vis-loopbuild-riser-work-{index}-c{cycle}"),
            "Add one riser cell from the close work face before stepping back.",
            json!({
                "cycle": cycle,
                "layer": layer,
                "column": column,
                "faced_work": faced_work,
                "placed": placed,
                "attempted": attempted,
                "base_resolved": base.is_some(),
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

        travel_to(ctx, review, TRAVEL_HOP, 1.2).await;
        let faced_review = ctx.look_until_facing(focus, 6.0, 5_000).await;
        let standing = base.map(|b| {
            (0..HEIGHT)
                .flat_map(|dy| {
                    (0..WIDTH).map(move |dx| BlockPos::new(b.x + dx as i32, b.y + dy as i32, b.z))
                })
                .filter(|p| {
                    ctx.block_id_at(*p).as_deref()
                        == Some(MATERIAL.strip_prefix("minecraft:").unwrap_or(MATERIAL))
                })
                .count()
        });
        ctx.record(
            "observe",
            &format!("vis-loopbuild-riser-review-{index}-c{cycle}"),
            "Review the growing riser from the same retreat stand, then return to work.",
            json!({
                "cycle": cycle,
                "is_return": cycle > 0,
                "faced_review": faced_review,
                "cells_standing": standing,
                "review_distance": REVIEW_DISTANCE,
            }),
        );
        let review_bearing = ctx
            .position()
            .map(|p| look_angles(head_of(p), focus).0)
            .unwrap_or(0.0);
        hold_facing(
            ctx,
            review_bearing,
            ctx.elapsed_ms() + REVIEW_HOLD_MS[slot],
            SHUTTLE_LEG,
        )
        .await;
        cycle += 1;
    }
}
