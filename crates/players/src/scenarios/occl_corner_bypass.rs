//! CATALOG: family=construction min_players=1 single_process=false world_edits=true scenery=none far_players=false
//! Build an L-shaped rock corner beyond an observed marker, walk behind its bend, and recover the
//! marker from the open side.
//!
//! The revisit problem is exercised with a foreground corner rather than a flat screen. The opening
//! shot sees the marker; the corner's first leg then hides it from the tucked station; a lateral
//! route around the second leg restores it without requiring a large camera turn. This matters
//! because a model can explain a flat disappearance as a yaw sweep, but cannot explain the same
//! bearing changing from marker to stone corner and back unless it remembers the scene geometry.
//!
//! The fallback is deliberate: real terrain is not assumed, so the two perpendicular two-block-high
//! legs are built from stone bricks and the event is labelled `built_corner`. The 5-block corner
//! depth puts the marker 3 blocks in front of the occluder, the 8-block tucked station leaves room for
//! the bend, and a 6-block side offset clears both legs. A failed `move_to` still advances to the next
//! station, and every `hold_visual` result is recorded rather than treated as arrival.

use super::*;

const MATERIAL: &str = "minecraft:stone_bricks";
const SUBJECT_HEIGHT: i64 = 3;
const SUBJECT_DISTANCE: f64 = 3.0;
const CORNER_DISTANCE: f64 = 5.0;
const TUCK_DISTANCE: f64 = 8.0;
const SIDE_OFFSET: f64 = 6.0;
const LEG_LANES: [i64; 5] = [-2, -1, 0, 1, 2];
const WALL_HEIGHT: i64 = 2;
const OBSERVE_MS: u64 = 2_700;
const DOLLY_MS: u64 = 1_400;

fn los_blocked(ctx: &Ctx, eye: (f64, f64, f64), target: (f64, f64, f64)) -> bool {
    let (dx, dy, dz) = (target.0 - eye.0, target.1 - eye.1, target.2 - eye.2);
    let steps = (dx.hypot(dy).hypot(dz) * 2.0).ceil() as i32;
    (1..steps).any(|step| {
        let t = step as f64 / steps as f64;
        block_is_safe_support(
            ctx,
            BlockPos::new(
                (eye.0 + dx * t).floor() as i32,
                (eye.1 + dy * t).floor() as i32,
                (eye.2 + dz * t).floor() as i32,
            ),
        )
    })
}

async fn observe(ctx: &mut Ctx, focus: (f64, f64, f64), tag: &str, cycle: usize, occluded: bool) {
    let arrived = ctx.look_until_facing(focus, 6.0, 5_000).await;
    ctx.record(
        TYPE_LOOK,
        &format!("vis-occl-corner-{tag}-{}-{cycle}", ctx.index()),
        "Look at the marker after reaching the corner station.",
        json!({"cycle": cycle, "camera_arrived": arrived, "occluded": occluded}),
    );
    ctx.still(OBSERVE_MS).await;
    let moved = ctx
        .hold_visual(WalkDirection::Forward, false, DOLLY_MS, 0)
        .await;
    ctx.record(
        TYPE_MOVE,
        &format!("vis-occl-corner-dolly-{}-{cycle}", ctx.index()),
        "Short camera-locked shuffle; false records a terrain stall.",
        json!({"cycle": cycle, "hold_visual_completed": moved}),
    );
}

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let anchor = ctx.anchor();
    let post = ctx.position().unwrap_or((anchor.0, 64.0, anchor.1));
    let toward = (anchor.0 - post.0, anchor.1 - post.2);
    let span = toward.0.hypot(toward.1);
    let unit = if span > 1.0 {
        (toward.0 / span, toward.1 / span)
    } else {
        (0.0, 1.0)
    };
    let perp = (-unit.1, unit.0);
    let desired = (
        (post.0 + unit.0 * SUBJECT_DISTANCE).round() as i64,
        post.1.round() as i64,
        (post.2 + unit.1 * SUBJECT_DISTANCE).round() as i64,
    );
    let raised = raise_landmark(ctx, desired, SUBJECT_HEIGHT, MATERIAL).await;
    let subject = raised.map(|(_, point)| point).unwrap_or((
        desired.0 as f64 + 0.5,
        post.1 + 2.0,
        desired.2 as f64 + 0.5,
    ));
    observe(ctx, subject, "open", 0, false).await;

    let corner = (
        post.0 + unit.0 * CORNER_DISTANCE,
        post.2 + unit.1 * CORNER_DISTANCE,
    );
    let mut blocks = 0u32;
    // First leg crosses the subject bearing; the second leg turns toward the side route.
    for lane in LEG_LANES {
        let cell = (
            (corner.0 + perp.0 * lane as f64).round() as i64,
            post.1.round() as i64,
            (corner.1 + perp.1 * lane as f64).round() as i64,
        );
        if raise_landmark(ctx, cell, WALL_HEIGHT, MATERIAL)
            .await
            .is_some()
        {
            blocks += 1;
        }
    }
    for step in [1.0, 2.0, 3.0] {
        let cell = (
            (corner.0 + unit.0 * step + perp.0 * 2.0).round() as i64,
            post.1.round() as i64,
            (corner.1 + unit.1 * step + perp.1 * 2.0).round() as i64,
        );
        if raise_landmark(ctx, cell, WALL_HEIGHT, MATERIAL)
            .await
            .is_some()
        {
            blocks += 1;
        }
    }
    ctx.record(
        TYPE_PLACE,
        &format!("vis-occl-corner-setup-{index}"),
        "Build the measured L-corner after the marker has been observed.",
        json!({"global_index": global, "subject_placed": raised.is_some(), "corner_blocks": blocks,
            "corner": {"x": r2(corner.0), "z": r2(corner.1)}}),
    );

    let tucked = (
        post.0 + unit.0 * TUCK_DISTANCE + perp.0 * 1.0,
        post.2 + unit.1 * TUCK_DISTANCE + perp.1 * 1.0,
    );
    let around = (
        corner.0 + perp.0 * SIDE_OFFSET,
        corner.1 + perp.1 * SIDE_OFFSET,
    );
    let mut cycle = 1usize;
    while !ctx.target_met() {
        travel_to(ctx, tucked, TRAVEL_HOP, 1.8).await;
        let hidden = ctx
            .position()
            .map(head_of)
            .is_some_and(|e| los_blocked(ctx, e, subject));
        observe(ctx, subject, "tucked", cycle, hidden).await;
        if ctx.target_met() {
            break;
        }
        travel_to(ctx, around, TRAVEL_HOP, 1.8).await;
        let hidden = ctx
            .position()
            .map(head_of)
            .is_some_and(|e| los_blocked(ctx, e, subject));
        observe(ctx, subject, "bypass", cycle, hidden).await;
        if ctx.target_met() {
            break;
        }
        travel_to(ctx, (post.0, post.2), TRAVEL_HOP, 1.8).await;
        let hidden = ctx
            .position()
            .map(head_of)
            .is_some_and(|e| los_blocked(ctx, e, subject));
        observe(ctx, subject, "return", cycle, hidden).await;
        cycle += 1;
    }
}
