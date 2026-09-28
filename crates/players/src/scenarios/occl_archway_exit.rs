//! CATALOG: family=construction min_players=1 single_process=false world_edits=true scenery=none far_players=false
//! Walk behind a low stone arch, then exit sideways and recover a marker that was observed before the
//! arch was built.
//!
//! The revisit problem is addressed with a combined vertical and horizontal cut: the arch hides the
//! marker but leaves a bright opening above its lintel. The recovery station is outside the arch
//! footprint, so the body—not an arbitrary camera spin—explains why the marker returns. This
//! supplies a real underpass-like transition without requiring a cave or a particular biome.
//!
//! The two four-block pillars and their lintel are a self-built fallback for missing terrain. Pillars
//! are 5 blocks out, the marker is 3 blocks out, and the exit is 6 blocks to the side; those numbers
//! keep the marker in the opening shot and give `move_to` a route around the arch. The lintel is
//! placed only after its supports, and failed placements are counted. `hold_visual` is intentionally
//! short and its false result is persisted, so a blocked arch never becomes a silent frozen loop.

use super::*;

const MATERIAL: &str = "minecraft:stone_bricks";
const SUBJECT_HEIGHT: i64 = 3;
const SUBJECT_DISTANCE: f64 = 3.0;
const ARCH_DISTANCE: f64 = 5.0;
const HIDDEN_DISTANCE: f64 = 8.0;
const EXIT_OFFSET: f64 = 6.0;
const PILLAR_LANES: [i64; 2] = [-2, 2];
const PILLAR_HEIGHT: i64 = 3;
const LINTEL_Y: i64 = 3;
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
        &format!("vis-occl-arch-{tag}-{}-{cycle}", ctx.index()),
        "Face the marker at this arch station.",
        json!({"cycle":cycle,"camera_arrived":arrived,"occluded":occluded}),
    );
    ctx.still(OBSERVE_MS).await;
    let moved = ctx
        .hold_visual(WalkDirection::Forward, false, DOLLY_MS, 0)
        .await;
    ctx.record(
        TYPE_MOVE,
        &format!("vis-occl-arch-dolly-{}-{cycle}", ctx.index()),
        "Locked-camera shuffle records a possible arch stall.",
        json!({"cycle":cycle,"hold_visual_completed":moved}),
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
    let subject = raised.map(|(_, p)| p).unwrap_or((
        desired.0 as f64 + 0.5,
        post.1 + 2.0,
        desired.2 as f64 + 0.5,
    ));
    observe(ctx, subject, "open", 0, false).await;
    let arch = (
        post.0 + unit.0 * ARCH_DISTANCE,
        post.2 + unit.1 * ARCH_DISTANCE,
    );
    let mut pillars = 0u32;
    for lane in PILLAR_LANES {
        let cell = (
            (arch.0 + perp.0 * lane as f64).round() as i64,
            post.1.round() as i64,
            (arch.1 + perp.1 * lane as f64).round() as i64,
        );
        if raise_landmark(ctx, cell, PILLAR_HEIGHT, MATERIAL)
            .await
            .is_some()
        {
            pillars += 1;
        }
    }
    let top = (
        (arch.0).round() as i64,
        (post.1 + LINTEL_Y as f64).round() as i64,
        (arch.1).round() as i64,
    );
    let lintel = ctx.equip(MATERIAL).await
        && ctx
            .place_block(
                BlockPos::new(top.0 as i32, top.1 as i32, top.2 as i32),
                MATERIAL,
            )
            .await;
    ctx.record(TYPE_PLACE,&format!("vis-occl-arch-setup-{index}"),
        "Build the supported lintel only after recording the open marker view.",
        json!({"global_index":global,"subject_placed":raised.is_some(),"pillars":pillars,"lintel_placed":lintel,
            "arch":{"x":r2(arch.0),"z":r2(arch.1)}}));
    let hidden = (
        post.0 + unit.0 * HIDDEN_DISTANCE,
        post.2 + unit.1 * HIDDEN_DISTANCE,
    );
    let exit = (arch.0 + perp.0 * EXIT_OFFSET, arch.1 + perp.1 * EXIT_OFFSET);
    let mut cycle = 1usize;
    while !ctx.target_met() {
        travel_to(ctx, hidden, TRAVEL_HOP, 1.8).await;
        let blocked = ctx
            .position()
            .map(head_of)
            .is_some_and(|e| los_blocked(ctx, e, subject));
        observe(ctx, subject, "under", cycle, blocked).await;
        if ctx.target_met() {
            break;
        }
        travel_to(ctx, exit, TRAVEL_HOP, 1.8).await;
        let blocked = ctx
            .position()
            .map(head_of)
            .is_some_and(|e| los_blocked(ctx, e, subject));
        observe(ctx, subject, "exit", cycle, blocked).await;
        if ctx.target_met() {
            break;
        }
        travel_to(ctx, (post.0, post.2), TRAVEL_HOP, 1.8).await;
        let blocked = ctx
            .position()
            .map(head_of)
            .is_some_and(|e| los_blocked(ctx, e, subject));
        observe(ctx, subject, "return", cycle, blocked).await;
        cycle += 1;
    }
}
