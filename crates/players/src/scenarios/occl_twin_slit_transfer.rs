//! CATALOG: family=construction min_players=1 single_process=false world_edits=true scenery=none far_players=false

//! Transfer an already observed marker between two gaps in a three-pier screen.
//!
//! The revisit problem is exercised as repeated partial occlusion inside one compact loop. From the
//! centre station the middle pier hides the marker; four blocks left or right put the marker
//! through a slit, so it reappears without a wide camera sweep. Moving left -> centre -> right ->
//! centre -> left gives two disappearances and two recoveries while nearly all background pixels
//! remain in the frustum.
//!
//! The geometry is measured rather than assumed: the marker is 12 blocks away, the screen is halfway
//! at 6, and piers sit at lateral -4, 0 and +4. A sight line from a station offset `s` crosses the
//! halfway plane at `s/2`, so ±4 stations look through the ±2 gaps and the centre hits the centre
//! pier. Two-block piers cover eye height. Failed columns are counted, routes still run, and every
//! locked 1.4-second dolly records its bool instead of claiming it reached anything.

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
const MATERIAL: &str = "minecraft:stone_bricks";
const SUBJECT_HEIGHT: i64 = 2;
const SUBJECT_DISTANCE: f64 = 12.0;
const SCREEN_DISTANCE: f64 = 6.0;
const PIER_OFFSETS: [f64; 3] = [-4.0, 0.0, 4.0];
const STATION_OFFSET: f64 = 4.0;
const PIER_HEIGHT: i64 = 2;
const OBSERVE_MS: u64 = 2_700;
const DOLLY_MS: u64 = 1_400;
const STATIONS: [i32; 5] = [-1, 0, 1, 0, -1];

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

async fn observe(ctx: &mut Ctx, subject: (f64, f64, f64), step: usize, slot: i32, blocked: bool) {
    let arrived = ctx.look_until_facing(subject, 6.0, 5_000).await;
    ctx.record(TYPE_LOOK,&format!("vis-occl-slit-{}-{step}",ctx.index()),
        "Measure whether this station frames the marker through a slit.",
        json!({"step":step,"station":slot,"camera_arrived":arrived,"occluded":blocked,"is_recovery":slot!=0}));
    ctx.still(OBSERVE_MS).await;
    let moved = ctx
        .hold_visual(WalkDirection::Forward, false, DOLLY_MS, 0)
        .await;
    ctx.record(
        TYPE_MOVE,
        &format!("vis-occl-slit-dolly-{}-{step}", ctx.index()),
        "Locked view dolly; false records obstruction by the screen or terrain.",
        json!({"step":step,"hold_visual_completed":moved}),
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
    // Build outward so two bots on opposite sides of the anchor do not merge their screens.
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
    let guess = (desired.0 as f64 + 0.5, post.1 + 1.5, desired.2 as f64 + 0.5);
    ctx.look_until_facing(guess, 6.0, 5_000).await;
    let raised = raise_landmark(ctx, desired, SUBJECT_HEIGHT, MATERIAL).await;
    let subject = raised.map(|(_, p)| p).unwrap_or(guess);
    let mut piers = 0u32;
    for offset in PIER_OFFSETS {
        let cell = (
            (post.0 + unit.0 * SCREEN_DISTANCE + perp.0 * offset).round() as i64,
            post_y as i64,
            (post.2 + unit.1 * SCREEN_DISTANCE + perp.1 * offset).round() as i64,
        );
        if raise_landmark(ctx, cell, PIER_HEIGHT, MATERIAL)
            .await
            .is_some()
        {
            piers += 1;
        }
    }
    ctx.record(TYPE_PLACE,&format!("vis-occl-slit-setup-{index}"),
        "Raise three piers whose two gaps carry the marker between side stations.",
        json!({"global_index":global,"subject_placed":raised.is_some(),"piers":piers,"screen_distance":SCREEN_DISTANCE}));

    let mut step = 0usize;
    while !ctx.target_met() {
        let slot = STATIONS[(step + global) % STATIONS.len()];
        let station = (
            post.0 + perp.0 * slot as f64 * STATION_OFFSET,
            post.2 + perp.1 * slot as f64 * STATION_OFFSET,
        );
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
        observe(ctx, subject, step, slot, blocked).await;
        step += 1;
    }
}
