//! CATALOG: family=construction min_players=1 single_process=false world_edits=true scenery=none far_players=false
//!
//! Extend an L-shaped corner by one arm each cycle, inspect its inside, then retrace to the outside.
//!
//! Which face of the revisit problem this answers: a return around a STRUCTURAL CORNER rather than a straight
//! wall. The close station faces the inside of the L, where the corner hides the second arm; the
//! outside station sees both arms and natural terrain. Each cycle revisits both stations in reverse
//! order and records the exact placed cell. `marker_base_near` makes the corner sit on actual terrain,
//! while `look_until_facing` distinguishes a completed corner turn from a timed-out look.
use super::*;

const MATERIAL: &str = "minecraft:cut_sandstone";
/// Six cells make two visible arms, but each cycle places only one new cell.
const ARM_LENGTH: i64 = 3;
const HEIGHT: i64 = 2;
const INSIDE_STANDOFF: f64 = 2.5;
const OUTSIDE_DISTANCE: f64 = 7.0;
const INSIDE_HOLD_MS: [i64; 3] = [4_500, 5_000, 5_500];
const OUTSIDE_HOLD_MS: [i64; 3] = [5_500, 6_000, 4_500];
/// The helper's four-block shuttle is long enough to avoid frozen and chatter gates.
const SHUTTLE_LEG: f64 = 4.0;

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let slot = global % INSIDE_HOLD_MS.len();
    let anchor = ctx.anchor();
    let inside = (anchor.0 - INSIDE_STANDOFF, anchor.1 - INSIDE_STANDOFF);
    let outside = (anchor.0 + OUTSIDE_DISTANCE, anchor.1 + OUTSIDE_DISTANCE);
    travel_to(ctx, inside, TRAVEL_HOP, 1.2).await;
    let feet_y = ctx.position().map(|p| p.1.round() as i64).unwrap_or(64);
    let base = marker_base_near(
        ctx,
        (anchor.0.round() as i64, feet_y, anchor.1.round() as i64),
        HEIGHT,
    );
    let focus = base
        .map(|b| (b.x as f64 + 1.0, b.y as f64 + 0.8, b.z as f64 + 1.0))
        .unwrap_or((anchor.0, feet_y as f64 + 1.0, anchor.1));
    ctx.record(
        "observe",
        &format!("vis-loopbuild-corner-setup-{index}"),
        "Resolve the corner base before the first build and keep both stations bounded.",
        json!({"global_index": global, "base": base.map(|b| json!({"x": b.x, "y": b.y, "z": b.z})), "inside": {"x": r2(inside.0), "z": r2(inside.1)}, "outside": {"x": r2(outside.0), "z": r2(outside.1)}}),
    );

    let mut cycle = 0usize;
    while !ctx.target_met() {
        travel_to(ctx, inside, TRAVEL_HOP, 1.2).await;
        let arm = cycle % 2;
        let step = (cycle / 2).min((ARM_LENGTH - 1) as usize) as i64;
        let target = base.map(|b| {
            if arm == 0 {
                BlockPos::new(b.x + step as i32, b.y + (cycle / 6) as i32, b.z)
            } else {
                BlockPos::new(b.x, b.y + (cycle / 6) as i32, b.z + step as i32)
            }
        });
        let faced_inside = ctx.look_until_facing(focus, 6.0, 4_000).await;
        let (placed, attempted) = if let Some(target) = target {
            let cells = [(
                (target.x as i64, target.y as i64, target.z as i64),
                MATERIAL.to_string(),
            )];
            ctx.build_blocks(&cells, |b, _target, _material| {
                (
                    format!("vis-loopbuild-corner-place-{index}-c{cycle}-{b}"),
                    format!("Place corner arm {arm} cell {step}.",),
                    json!({"cycle": cycle, "arm": arm, "step": step}),
                )
            })
            .await
        } else {
            (0, 0)
        };
        ctx.record(
            TYPE_PLACE,
            &format!("vis-loopbuild-corner-inside-{index}-c{cycle}"),
            "Place one cell from inside the L, where the corner masks the other arm.",
            json!({"cycle": cycle, "arm": arm, "step": step, "faced_inside": faced_inside, "placed": placed, "attempted": attempted}),
        );
        let inside_bearing = ctx
            .position()
            .map(|p| look_angles(head_of(p), focus).0)
            .unwrap_or(0.0);
        hold_facing(
            ctx,
            inside_bearing,
            ctx.elapsed_ms() + INSIDE_HOLD_MS[slot],
            SHUTTLE_LEG,
        )
        .await;
        if ctx.target_met() {
            break;
        }

        travel_to(ctx, outside, TRAVEL_HOP, 1.2).await;
        let faced_outside = ctx.look_until_facing(focus, 6.0, 5_000).await;
        let visible_cells = base.map(|b| {
            (0..HEIGHT)
                .flat_map(|dy| {
                    (0..ARM_LENGTH).flat_map(move |d| {
                        [
                            BlockPos::new(b.x + d as i32, b.y + dy as i32, b.z),
                            BlockPos::new(b.x, b.y + dy as i32, b.z + d as i32),
                        ]
                    })
                })
                .filter(|p| {
                    ctx.block_id_at(*p).as_deref()
                        == Some(MATERIAL.strip_prefix("minecraft:").unwrap_or(MATERIAL))
                })
                .count()
        });
        ctx.record(
            "observe",
            &format!("vis-loopbuild-corner-outside-{index}-c{cycle}"),
            "Retrace to the outside and recover the complete L silhouette.",
            json!({"cycle": cycle, "is_return": cycle > 0, "faced_outside": faced_outside, "visible_cells": visible_cells, "outside_distance": OUTSIDE_DISTANCE}),
        );
        let outside_bearing = ctx
            .position()
            .map(|p| look_angles(head_of(p), focus).0)
            .unwrap_or(0.0);
        hold_facing(
            ctx,
            outside_bearing,
            ctx.elapsed_ms() + OUTSIDE_HOLD_MS[slot],
            SHUTTLE_LEG,
        )
        .await;
        cycle += 1;
    }
}
