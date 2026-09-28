//! CATALOG: family=construction min_players=1 single_process=false world_edits=true scenery=none far_players=false
//! Place a squat boulder cluster past an observed marker, hide behind its bulk, and slalom around it
//! until the marker is visible again.
//!
//! The revisit problem is targeted at foreground occlusion with a broad, low silhouette. The marker
//! is first observed from the post. A two-by-two cluster then removes the marker from the tucked
//! view while leaving sky and distant terrain around it; the side station restores the same marker
//! with only a lateral body excursion. This is different from a tall picket: the model must retain
//! the block's footprint and the horizon cut by its top surface.
//!
//! A boulder is self-built because natural rock is not guaranteed at any spawn. The cluster is two
//! adjacent columns, each two blocks high, at 5 blocks from the post. If a placement misses, the count
//! is recorded and the route still proceeds; `travel_to` is never retried forever. Three blocks to the
//! marker, eight to the tucked station, and five sideways are the smallest distances that keep the
//! cluster between eye and subject while clearing the arrival tolerance. Locked shuffles log false.

use super::*;

const SURFACE_SCAN: i32 = 64;

/// Resolve an open-sky standing target instead of trusting the current feet Y. A bot can be
/// temporarily below a cave lip; probing from above makes the target recover the natural surface.
fn surface_y(ctx: &Ctx, x: i32, z: i32, hint: i32) -> Option<i32> {
    for y in ((hint - SURFACE_SCAN)..=(hint + SURFACE_SCAN)).rev() {
        let cell = BlockPos::new(x, y, z);
        let id = ctx.block_id_at(cell)?;
        if id.contains("water") || id.contains("lava") {
            return None;
        }
        if block_is_safe_support(ctx, cell) {
            if id.contains("leaves") || id.ends_with("log") {
                return None;
            }
            return Some(y + 1);
        }
    }
    None
}

/// Snap a route to the topmost measured surface, with a small lateral retry when a column is
/// occupied or unloaded.
fn route_target(ctx: &Ctx, desired: (f64, f64), hint: i32) -> Option<(f64, f64)> {
    const OFFSETS: [(i32, i32); 9] = [
        (0, 0),
        (1, 0),
        (-1, 0),
        (0, 1),
        (0, -1),
        (1, 1),
        (1, -1),
        (-1, 1),
        (-1, -1),
    ];
    let bx = desired.0.floor() as i32;
    let bz = desired.1.floor() as i32;
    OFFSETS.iter().find_map(|(dx, dz)| {
        let x = bx + dx;
        let z = bz + dz;
        let y = surface_y(ctx, x, z, hint)?;
        walkable_xz_near(ctx, (x as f64 + 0.5, z as f64 + 0.5), y)
    })
}

const MATERIAL: &str = "minecraft:cobblestone";
const SUBJECT_HEIGHT: i64 = 3;
const SUBJECT_DISTANCE: f64 = 3.0;
const BOULDER_DISTANCE: f64 = 5.0;
const HIDDEN_DISTANCE: f64 = 8.0;
const SIDE_OFFSET: f64 = 5.0;
const HEIGHT: i64 = 2;
const OBSERVE_MS: u64 = 2_600;
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
        &format!("vis-occl-boulder-{tag}-{}-{cycle}", ctx.index()),
        "Re-frame the marker at this boulder station.",
        json!({"cycle": cycle, "camera_arrived": arrived, "occluded": occluded}),
    );
    ctx.still(OBSERVE_MS).await;
    let moved = ctx
        .hold_visual(WalkDirection::Forward, false, DOLLY_MS, 0)
        .await;
    ctx.record(
        TYPE_MOVE,
        &format!("vis-occl-boulder-dolly-{}-{cycle}", ctx.index()),
        "Camera-locked shuffle records whether terrain allowed progress.",
        json!({"cycle": cycle, "hold_visual_completed": moved}),
    );
}

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let anchor = ctx.anchor();
    let post = ctx.position().unwrap_or((anchor.0, 64.0, anchor.1));
    let post_y = surface_y(
        ctx,
        post.0.floor() as i32,
        post.2.floor() as i32,
        post.1.floor() as i32,
    )
    .unwrap_or(post.1.floor() as i32);
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
        post_y as i64,
        (post.2 + unit.1 * SUBJECT_DISTANCE).round() as i64,
    );
    let raised = raise_landmark(ctx, desired, SUBJECT_HEIGHT, MATERIAL).await;
    let subject = raised.map(|(_, p)| p).unwrap_or((
        desired.0 as f64 + 0.5,
        post_y as f64 + 2.0,
        desired.2 as f64 + 0.5,
    ));
    observe(ctx, subject, "open", 0, false).await;

    let boulder = (
        post.0 + unit.0 * BOULDER_DISTANCE,
        post.2 + unit.1 * BOULDER_DISTANCE,
    );
    let mut columns = 0u32;
    for side in [-0.6, 0.6] {
        let cell = (
            (boulder.0 + perp.0 * side).round() as i64,
            post_y as i64,
            (boulder.1 + perp.1 * side).round() as i64,
        );
        if raise_landmark(ctx, cell, HEIGHT, MATERIAL).await.is_some() {
            columns += 1;
        }
    }
    ctx.record(
        TYPE_PLACE,
        &format!("vis-occl-boulder-setup-{index}"),
        "Build a squat two-column boulder after the marker's opening shot.",
        json!({"global_index": global, "subject_placed": raised.is_some(), "columns": columns,
            "boulder": {"x": r2(boulder.0), "z": r2(boulder.1)}}),
    );

    let hidden = (
        post.0 + unit.0 * HIDDEN_DISTANCE,
        post.2 + unit.1 * HIDDEN_DISTANCE,
    );
    let side = (
        boulder.0 + perp.0 * SIDE_OFFSET,
        boulder.1 + perp.1 * SIDE_OFFSET,
    );
    let mut cycle = 1usize;
    while !ctx.target_met() {
        travel_to(
            ctx,
            route_target(ctx, hidden, post_y).unwrap_or((post.0, post.2)),
            TRAVEL_HOP,
            1.8,
        )
        .await;
        let blocked = ctx
            .position()
            .map(head_of)
            .is_some_and(|e| los_blocked(ctx, e, subject));
        observe(ctx, subject, "behind", cycle, blocked).await;
        if ctx.target_met() {
            break;
        }
        travel_to(
            ctx,
            route_target(ctx, side, post_y).unwrap_or((post.0, post.2)),
            TRAVEL_HOP,
            1.8,
        )
        .await;
        let blocked = ctx
            .position()
            .map(head_of)
            .is_some_and(|e| los_blocked(ctx, e, subject));
        observe(ctx, subject, "slalom", cycle, blocked).await;
        if ctx.target_met() {
            break;
        }
        travel_to(
            ctx,
            route_target(ctx, (post.0, post.2), post_y).unwrap_or((post.0, post.2)),
            TRAVEL_HOP,
            1.8,
        )
        .await;
        let blocked = ctx
            .position()
            .map(head_of)
            .is_some_and(|e| los_blocked(ctx, e, subject));
        observe(ctx, subject, "return", cycle, blocked).await;
        cycle += 1;
    }
}
