//! CATALOG: family=construction min_players=1 single_process=false world_edits=true scenery=none far_players=false

//! Hide an observed marker behind a broad wall, walk to its open gate, and return through the gate to
//! the original view.
//!
//! The revisit problem is tested with route-conditioned recovery: the subject does not return
//! because the camera spins, but because the body follows the wall to a known opening. The hidden
//! station sees a continuous wall; the gate station sees the marker through a gap; the post then
//! restores the exact opening vantage. This produces a longer visual dependency than a one-step
//! peek while still closing the trajectory every cycle.
//!
//! Five two-block columns at offsets -4,-2,0,2,4 leave a gate at +3. The wall is 5 blocks from the
//! post and the marker 10 blocks away, so the hidden station is genuinely on the near side and the
//! gate line clears the adjacent columns. The gate is reached by a 6-block lateral leg, safely above
//! key chatter. Placement failures and sight-line truth are logged; movement never retries a failed
//! target forever, and the 1.4-second locked dolly records false on collision.

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
const SUBJECT_DISTANCE: f64 = 10.0;
const WALL_DISTANCE: f64 = 5.0;
const WALL_OFFSETS: [f64; 5] = [-4.0, -2.0, 0.0, 2.0, 4.0];
const GATE_OFFSET: f64 = 3.0;
const WALL_HEIGHT: i64 = 2;
const OBSERVE_MS: u64 = 2_700;
const DOLLY_MS: u64 = 1_400;

fn los_blocked(ctx: &Ctx, eye: (f64, f64, f64), target: (f64, f64, f64)) -> bool {
    let (dx, dy, dz) = (target.0 - eye.0, target.1 - eye.1, target.2 - eye.2);
    let steps = (dx.hypot(dy).hypot(dz) * 2.0).ceil() as i32;
    (1..steps).any(|i| {
        let t = i as f64 / steps as f64;
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

async fn observe(ctx: &mut Ctx, subject: (f64, f64, f64), tag: &str, cycle: usize, blocked: bool) {
    let arrived = ctx.look_until_facing(subject, 6.0, 5_000).await;
    ctx.record(
        TYPE_LOOK,
        &format!("vis-occl-gate-{tag}-{}-{cycle}", ctx.index()),
        "Face the marker after reaching the wall or gate station.",
        json!({"cycle":cycle,"camera_arrived":arrived,"occluded":blocked}),
    );
    ctx.still(OBSERVE_MS).await;
    let moved = ctx
        .hold_visual(WalkDirection::Forward, false, DOLLY_MS, 0)
        .await;
    ctx.record(
        TYPE_MOVE,
        &format!("vis-occl-gate-dolly-{}-{cycle}", ctx.index()),
        "Short locked-camera dolly; false is a recorded obstruction.",
        json!({"cycle":cycle,"hold_visual_completed":moved}),
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
        (-toward.0 / span, -toward.1 / span)
    } else {
        (0.0, 1.0)
    };
    let perp = (-unit.1, unit.0);
    glance_at_nearest_peer(ctx, None).await;
    let desired = (
        (post.0 + unit.0 * SUBJECT_DISTANCE).round() as i64,
        post_y as i64,
        (post.2 + unit.1 * SUBJECT_DISTANCE).round() as i64,
    );
    let guess = (
        desired.0 as f64 + 0.5,
        post_y as f64 + 2.0,
        desired.2 as f64 + 0.5,
    );
    ctx.look_until_facing(guess, 6.0, 5_000).await;
    let raised = raise_landmark(ctx, desired, SUBJECT_HEIGHT, MATERIAL).await;
    let subject = raised.map(|(_, p)| p).unwrap_or(guess);
    let mut columns = 0u32;
    for offset in WALL_OFFSETS {
        let cell = (
            (post.0 + unit.0 * WALL_DISTANCE + perp.0 * offset).round() as i64,
            post_y as i64,
            (post.2 + unit.1 * WALL_DISTANCE + perp.1 * offset).round() as i64,
        );
        if raise_landmark(ctx, cell, WALL_HEIGHT, MATERIAL)
            .await
            .is_some()
        {
            columns += 1;
        }
    }
    ctx.record(TYPE_PLACE,&format!("vis-occl-gate-setup-{index}"),"Build the wall while preserving one navigable gate.",
  json!({"global_index":global,"subject_placed":raised.is_some(),"wall_columns":columns,"gate_offset":GATE_OFFSET}));
    let gate = (
        post.0 + perp.0 * GATE_OFFSET * 2.0,
        post.2 + perp.1 * GATE_OFFSET * 2.0,
    );
    let mut cycle = 0usize;
    while !ctx.target_met() {
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
        observe(ctx, subject, "wall", cycle, blocked).await;
        if ctx.target_met() {
            break;
        }
        travel_to(
            ctx,
            route_target(ctx, gate, post_y).unwrap_or((post.0, post.2)),
            TRAVEL_HOP,
            1.8,
        )
        .await;
        let blocked = ctx
            .position()
            .map(head_of)
            .is_some_and(|e| los_blocked(ctx, e, subject));
        observe(ctx, subject, "gate", cycle, blocked).await;
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
