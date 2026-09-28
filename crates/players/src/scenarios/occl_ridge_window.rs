//! CATALOG: family=locomotion min_players=1 single_process=false world_edits=false scenery=none far_players=false
//! Cross a measured natural ridge: first frame the marker from open ground, then descend behind the
//! ridge and climb back to the same view.
//!
//! The revisit problem is tested through terrain depth ordering. The marker is in the initial view;
//! a raised shoulder between the hidden station and that marker removes it without asking the
//! camera to turn; returning to the post re-establishes the observed view. The route is a short
//! out-and-back, so path length grows while displacement closes. A ridge is selected from sampled
//! terrain rather than a presumed biome, and its measured rise and sight-line result are recorded.
//!
//! The probe checks the topmost surface in six bearings and three ranges. If no usable ridge resolves,
//! the script still walks a fixed 10-block leg and returns, recording `probe_failed`; it never stands
//! waiting for terrain that is not there. The anchor-side terrain is the subject, a 6-block ridge is
//! the candidate occluder, and the 10-block station stays within the v6 displacement budget once the
//! return closes. Every visual beat uses `look_until_facing`; the short locked dolly logs false.

use super::*;

const RIDGE_RANGE: [f64; 3] = [4.0, 6.0, 8.0];
const STATION_DISTANCE: f64 = 10.0;
const MIN_RISE: i32 = 2;
const SCAN_SPAN: i32 = 20;
const BEARINGS: usize = 6;
const OBSERVE_MS: u64 = 2_700;
const DOLLY_MS: u64 = 1_400;

fn surface_y(ctx: &Ctx, x: i32, z: i32, hint: i32) -> Option<i32> {
    for y in ((hint - SCAN_SPAN)..=(hint + SCAN_SPAN)).rev() {
        let cell = BlockPos::new(x, y, z);
        let id = ctx.block_id_at(cell)?;
        if id.contains("water")
            || id.contains("lava")
            || id.contains("leaves")
            || id.ends_with("log")
        {
            return None;
        }
        if block_is_safe_support(ctx, cell) {
            return Some(y + 1);
        }
    }
    None
}

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

fn ridge(ctx: &Ctx, post: (f64, f64, f64), bearing: f64) -> Option<(f64, f64, i32)> {
    let hint = post.1.floor() as i32;
    let base = surface_y(ctx, post.0.floor() as i32, post.2.floor() as i32, hint)?;
    let mut best = None;
    for d in RIDGE_RANGE {
        let x = post.0 + bearing.cos() * d;
        let z = post.2 + bearing.sin() * d;
        if let Some(y) = surface_y(ctx, x.floor() as i32, z.floor() as i32, hint)
            && y - base >= MIN_RISE
            && best.is_none_or(|(_, _, rise): (f64, f64, i32)| y - base > rise)
        {
            best = Some((x, z, y - base));
        }
    }
    best
}

async fn observe(ctx: &mut Ctx, focus: (f64, f64, f64), tag: &str, cycle: usize, blocked: bool) {
    let arrived = ctx.look_until_facing(focus, 6.0, 5_000).await;
    ctx.record(
        TYPE_LOOK,
        &format!("vis-occl-ridge-{tag}-{}-{cycle}", ctx.index()),
        "Re-acquire the marker at the measured ridge station.",
        json!({"cycle":cycle,"camera_arrived":arrived,"occluded":blocked}),
    );
    ctx.still(OBSERVE_MS).await;
    let moved = ctx
        .hold_visual(WalkDirection::Forward, false, DOLLY_MS, 0)
        .await;
    ctx.record(
        TYPE_MOVE,
        &format!("vis-occl-ridge-dolly-{}-{cycle}", ctx.index()),
        "Camera-locked terrain shuffle; false records a stall.",
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
    // The terrain itself is the subject: keeping this file world-edit free means every return
    // tests memory of a generated landmark rather than a script-created block.
    let subject = (anchor.0, post.1 + 1.4, anchor.1);
    observe(ctx, subject, "open", 0, false).await;

    let bearing = unit.1.atan2(unit.0) + std::f64::consts::PI + global as f64 * 0.21;
    let found = (0..BEARINGS).find_map(|i| {
        ridge(
            ctx,
            post,
            bearing + i as f64 * std::f64::consts::TAU / BEARINGS as f64,
        )
    });
    let (hidden, rise, failed) = match found {
        Some((x, z, rise)) => (
            (
                x + (x - post.0).signum() * 4.0,
                z + (z - post.2).signum() * 4.0,
            ),
            rise,
            false,
        ),
        None => (
            (
                post.0 + bearing.cos() * STATION_DISTANCE,
                post.2 + bearing.sin() * STATION_DISTANCE,
            ),
            0,
            true,
        ),
    };
    ctx.record(
        TYPE_MOVE,
        &format!("vis-occl-ridge-setup-{index}"),
        "Measure a natural shoulder before the out-and-back.",
        json!({"global_index":global,"subject_placed":false,"probe_failed":failed,"rise":rise,
            "hidden":{"x":r2(hidden.0),"z":r2(hidden.1)}}),
    );

    let mut cycle = 1usize;
    while !ctx.target_met() {
        travel_to(ctx, hidden, TRAVEL_HOP, 2.0).await;
        let blocked = ctx
            .position()
            .map(head_of)
            .is_some_and(|e| los_blocked(ctx, e, subject));
        observe(ctx, subject, "shadow", cycle, blocked).await;
        if ctx.target_met() {
            break;
        }
        travel_to(ctx, (post.0, post.2), TRAVEL_HOP, 2.0).await;
        let blocked = ctx
            .position()
            .map(head_of)
            .is_some_and(|e| los_blocked(ctx, e, subject));
        observe(ctx, subject, "recovery", cycle, blocked).await;
        cycle += 1;
    }
}
