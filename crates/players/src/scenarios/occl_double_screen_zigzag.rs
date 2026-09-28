//! CATALOG: family=construction min_players=1 single_process=false world_edits=true scenery=none far_players=false

//! Zigzag past two offset screens: the observed marker disappears behind the near screen, reappears
//! in the lane between them, disappears behind the far screen, then returns at the home station.
//!
//! The revisit problem is addressed with two occlusion depths in one trajectory. A model cannot
//! solve this by remembering only the last mask: the near and far screens cover different parts of
//! the marker's bearing, and the body alternates sides before returning to the exact opening
//! vantage. Four stations form a closed zigzag, producing repeated loss and recovery without a
//! one-way camera sweep.
//!
//! Screens are placed at 5 and 8 blocks, offset two blocks to opposite sides; the marker is 12 blocks
//! out. Each screen is three two-block columns wide, enough to survive arrival slop, while the zigzag
//! stations are five blocks sideways so their sight lines clear one screen at a time. Failed builds
//! are counted and the route still moves. Observations happen only after `move_to` settles and
//! `look_until_facing` confirms the bearing; every locked 1.4-second dolly records false.

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
const MATERIAL: &str = "minecraft:deepslate_bricks";
const SUBJECT_HEIGHT: i64 = 3;
const SUBJECT_DISTANCE: f64 = 12.0;
const SCREEN_DEPTHS: [f64; 2] = [5.0, 8.0];
const SCREEN_SIDES: [f64; 2] = [-2.0, 2.0];
const SCREEN_LANES: [f64; 3] = [-1.0, 0.0, 1.0];
const WALL_HEIGHT: i64 = 2;
const STATION_SIDE: f64 = 5.0;
const OBSERVE_MS: u64 = 2_500;
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

async fn observe(
    ctx: &mut Ctx,
    subject: (f64, f64, f64),
    phase: &str,
    cycle: usize,
    blocked: bool,
) {
    let arrived = ctx.look_until_facing(subject, 6.0, 5_000).await;
    ctx.record(
        TYPE_LOOK,
        &format!("vis-occl-zigzag-{phase}-{}-{cycle}", ctx.index()),
        "Face the marker after crossing the next screen depth.",
        json!({"cycle":cycle,"phase":phase,"camera_arrived":arrived,"occluded":blocked}),
    );
    ctx.still(OBSERVE_MS).await;
    let moved = ctx
        .hold_visual(WalkDirection::Forward, false, DOLLY_MS, 0)
        .await;
    ctx.record(
        TYPE_MOVE,
        &format!("vis-occl-zigzag-dolly-{}-{cycle}", ctx.index()),
        "Camera-locked shuffle records whether either screen blocks motion.",
        json!({"cycle":cycle,"phase":phase,"hold_visual_completed":moved}),
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
    for (depth, side) in SCREEN_DEPTHS.into_iter().zip(SCREEN_SIDES) {
        for lane in SCREEN_LANES {
            let lateral = side + lane;
            let cell = (
                (post.0 + unit.0 * depth + perp.0 * lateral).round() as i64,
                post_y as i64,
                (post.2 + unit.1 * depth + perp.1 * lateral).round() as i64,
            );
            if raise_landmark(ctx, cell, WALL_HEIGHT, MATERIAL)
                .await
                .is_some()
            {
                columns += 1;
            }
        }
    }
    ctx.record(TYPE_PLACE,&format!("vis-occl-zigzag-setup-{index}"),"Raise two offset screens after the marker is grounded.",
  json!({"global_index":global,"subject_placed":raised.is_some(),"screen_columns":columns,"depths":SCREEN_DEPTHS}));
    let stations = [
        (
            "near_shadow",
            (
                post.0 + unit.0 * 6.5 + perp.0 * SCREEN_SIDES[0],
                post.2 + unit.1 * 6.5 + perp.1 * SCREEN_SIDES[0],
            ),
        ),
        (
            "lane_reveal",
            (
                post.0 + unit.0 * 6.5 + perp.0 * STATION_SIDE,
                post.2 + unit.1 * 6.5 + perp.1 * STATION_SIDE,
            ),
        ),
        (
            "far_shadow",
            (
                post.0 + unit.0 * 9.5 + perp.0 * SCREEN_SIDES[1],
                post.2 + unit.1 * 9.5 + perp.1 * SCREEN_SIDES[1],
            ),
        ),
        ("home", (post.0, post.2)),
    ];
    let mut cycle = 0usize;
    while !ctx.target_met() {
        for (phase, station) in stations {
            if ctx.target_met() {
                return;
            }
            travel_to(
                ctx,
                route_target(ctx, station, post_y).unwrap_or((post.0, post.2)),
                TRAVEL_HOP,
                1.8,
            )
            .await;
            let blocked = ctx
                .position()
                .map(head_of)
                .is_some_and(|e| los_blocked(ctx, e, subject));
            observe(ctx, subject, phase, cycle, blocked).await;
        }
        cycle += 1;
    }
}
