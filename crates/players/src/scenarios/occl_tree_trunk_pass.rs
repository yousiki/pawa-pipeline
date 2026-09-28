//! CATALOG: family=locomotion min_players=1 single_process=false world_edits=true scenery=none far_players=false

//! Walk past a tree trunk (or a narrow planted trunk fallback), lose the already observed marker,
//! then step around the trunk and recover the same marker.
//!
//! The revisit problem is addressed through a depth-order occlusion rather than a camera sweep: the
//! marker is seen from the post before the trunk is crossed, disappears when the bot stands beyond
//! the trunk, and re-enters the frame from a lateral station. A trunk is a genuinely sparse
//! occluder: most of the landscape remains visible, but the central subject is gone. The route
//! therefore tests whether the model remembers a small missing foreground object instead of filling
//! the whole frame with a guessed wall.
//!
//! The search accepts only a log or stem in the forward corridor. If terrain has no tree in that
//! corridor, three two-block columns are placed at the same depth and recorded as `fallback_screen`;
//! this keeps the episode moving and gives the evaluator an honest source label. The magic distances
//! are 3 blocks for a marker inside the opening view, 5 for the trunk plane, and 6 for the side
//! station: these leave a four-block hidden-to-side walk while keeping displacement below the v6
//! target. Every observation starts with `look_until_facing`; every locked dolly records its bool.

use super::*;

const MATERIAL: &str = "minecraft:oak_planks";
const SUBJECT_HEIGHT: i64 = 3;
const SUBJECT_DISTANCE: f64 = 3.0;
const TRUNK_MIN_DISTANCE: f64 = 5.0;
const TRUNK_MAX_DISTANCE: f64 = 7.0;
const HIDDEN_DISTANCE: f64 = 9.0;
const SIDE_OFFSET: f64 = 6.0;
const SCREEN_HEIGHT: i64 = 2;
const SCREEN_LANES: [i64; 3] = [-1, 0, 1];
const OBSERVE_MS: u64 = 2_800;
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

fn find_trunk(ctx: &Ctx, post: (f64, f64, f64), unit: (f64, f64)) -> Option<(f64, f64)> {
    let y = post.1.floor() as i32;
    for distance in [TRUNK_MIN_DISTANCE, 6.0, TRUNK_MAX_DISTANCE] {
        let x = post.0 + unit.0 * distance;
        let z = post.2 + unit.1 * distance;
        for lift in 0..=4 {
            let id =
                ctx.block_id_at(BlockPos::new(x.floor() as i32, y + lift, z.floor() as i32))?;
            if id.ends_with("log") || id.ends_with("stem") {
                return Some((x, z));
            }
        }
    }
    None
}

async fn observe(
    ctx: &mut Ctx,
    focus: (f64, f64, f64),
    tag: &str,
    cycle: usize,
    occluded: Option<bool>,
) {
    let arrived = ctx.look_until_facing(focus, 6.0, 5_000).await;
    ctx.record(
        TYPE_LOOK,
        &format!("vis-occl-tree-{tag}-{}-{cycle}", ctx.index()),
        "Re-acquire the marker before measuring this trunk station.",
        json!({"cycle": cycle, "camera_arrived": arrived, "occluded": occluded}),
    );
    ctx.still(OBSERVE_MS).await;
    let moved = ctx
        .hold_visual(WalkDirection::Forward, false, DOLLY_MS, 0)
        .await;
    ctx.record(
        TYPE_MOVE,
        &format!("vis-occl-tree-dolly-{}-{cycle}", ctx.index()),
        "Shuffle with the camera locked; false means terrain stopped the short dolly.",
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

    // Observe before planting the occluder: the subject is genuinely in the context view first.
    observe(ctx, subject, "open", 0, Some(false)).await;
    let natural = find_trunk(ctx, post, unit);
    let mut placed = 0u32;
    let (trunk, source) = match natural {
        Some(cell) => (cell, "tree_trunk"),
        None => {
            let centre = (
                post.0 + unit.0 * TRUNK_MIN_DISTANCE,
                post.2 + unit.1 * TRUNK_MIN_DISTANCE,
            );
            for lane in SCREEN_LANES {
                let cell = (
                    (centre.0 + perp.0 * lane as f64).round() as i64,
                    post.1.round() as i64,
                    (centre.1 + perp.1 * lane as f64).round() as i64,
                );
                if raise_landmark(ctx, cell, SCREEN_HEIGHT, MATERIAL)
                    .await
                    .is_some()
                {
                    placed += 1;
                }
            }
            (centre, "fallback_screen")
        }
    };
    ctx.record(
        TYPE_PLACE,
        &format!("vis-occl-tree-setup-{index}"),
        "Resolve a natural trunk or place a narrow fallback screen beyond the observed marker.",
        json!({
            "global_index": global,
            "source": source,
            "subject_placed": raised.is_some(),
            "fallback_columns": placed,
            "trunk": {"x": r2(trunk.0), "z": r2(trunk.1)},
        }),
    );

    let hidden = (
        trunk.0 + unit.0 * (HIDDEN_DISTANCE - TRUNK_MIN_DISTANCE),
        trunk.1 + unit.1 * (HIDDEN_DISTANCE - TRUNK_MIN_DISTANCE),
    );
    let side = (
        trunk.0 + perp.0 * SIDE_OFFSET,
        trunk.1 + perp.1 * SIDE_OFFSET,
    );
    let mut cycle = 1usize;
    while !ctx.target_met() {
        travel_to(ctx, hidden, TRAVEL_HOP, 1.8).await;
        let hidden_now = ctx
            .position()
            .map(head_of)
            .is_some_and(|eye| los_blocked(ctx, eye, subject));
        observe(ctx, subject, "hidden", cycle, Some(hidden_now)).await;
        if ctx.target_met() {
            break;
        }

        // A lateral route is the recovery, not an unexplained 180-degree turn.
        travel_to(ctx, side, TRAVEL_HOP, 1.8).await;
        let side_clear = ctx
            .position()
            .map(head_of)
            .is_some_and(|eye| !los_blocked(ctx, eye, subject));
        observe(ctx, subject, "around", cycle, Some(!side_clear)).await;
        if ctx.target_met() {
            break;
        }
        travel_to(ctx, (post.0, post.2), TRAVEL_HOP, 1.8).await;
        let home_clear = ctx
            .position()
            .map(head_of)
            .is_some_and(|eye| !los_blocked(ctx, eye, subject));
        observe(ctx, subject, "return", cycle, Some(!home_clear)).await;
        cycle += 1;
    }
}
