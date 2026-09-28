use super::*;

const TAU: f64 = std::f64::consts::TAU;
const LANE_NAMES: [&str; 2] = ["right_lane", "left_lane"];

/// Blocks kept between two bots' faces, so the one mining action in frame is unambiguously one
/// bot's.
const MIN_SITE_GAP: f64 = 5.0;

fn rand_range(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

/// An approach point on a ring around the anchor, kept clear of every peer this process can see.
fn approach_point(ctx: &mut Ctx, anchor: (f64, f64), lane_bearing: f64) -> (f64, f64) {
    let mut best = anchor;
    let mut best_gap = -1.0;
    for _ in 0..6 {
        let bearing = (lane_bearing + rand_range(ctx, -40.0, 40.0)).to_radians();
        let distance = rand_range(ctx, 7.0, 15.0);
        let candidate = (
            anchor.0 - bearing.sin() * distance,
            anchor.1 + bearing.cos() * distance,
        );
        let gap = (0..ctx.player_count())
            .filter(|&j| j != ctx.index())
            .filter_map(|j| ctx.peer_position(j))
            .map(|p| (p.0 - candidate.0).hypot(p.2 - candidate.1))
            .fold(f64::INFINITY, f64::min);
        if gap >= MIN_SITE_GAP {
            return candidate;
        }
        if gap > best_gap {
            best_gap = gap;
            best = candidate;
        }
    }
    best
}

/// The first solid cell within `reach` blocks of the bot along `heading_deg`, at feet or head
/// height. `None` means the bot is standing in the open and there is no face to teach with.
fn face_ahead(ctx: &Ctx, heading_deg: f64, reach: i32) -> Option<BlockPos> {
    let position = ctx.position()?;
    let yaw = heading_deg.to_radians();
    let (fx, fz) = (-yaw.sin(), yaw.cos());
    let feet_y = position.1.floor() as i32;
    for step in 1..=reach {
        let x = (position.0 + fx * step as f64).floor() as i32;
        let z = (position.2 + fz * step as f64).floor() as i32;
        for dy in [0, 1] {
            let cell = BlockPos::new(x, feet_y + dy, z);
            if !block_is_air(ctx, cell) {
                return Some(cell);
            }
        }
    }
    None
}

async fn sweep(ctx: &Ctx, span_deg: f64, pitch_deg: f64) {
    let rate = ctx.cfg().yaw_rate_deg_s.max(1.0);
    for (yaw, pitch) in [
        (span_deg * 0.5, pitch_deg),
        (-span_deg, -pitch_deg),
        (span_deg * 0.5, 0.0),
    ] {
        if ctx.target_met() {
            return;
        }
        let settle = (yaw.abs() / rate * 1000.0) as u64 + 700;
        ctx.drag_view(yaw, pitch, settle).await;
    }
}

/// Stand still, but only while the accumulated dead air is under a tenth of the episode so far.
async fn budgeted_pause(ctx: &Ctx, spent_ms: &mut u64, hold_ms: u64) -> bool {
    let hold = hold_ms.clamp(200, 3_000);
    let elapsed = ctx.elapsed_ms().max(1) as u64;
    if *spent_ms + hold > elapsed / 10 {
        return false;
    }
    *spent_ms += hold;
    ctx.still(hold).await;
    true
}

/// Each bot walks to one prepared face, takes out a pickaxe, breaks exactly one block, stands a
/// moment, and walks back — a single mining action with a long clean approach on either side.
///
/// One block per lesson is the contract, so the loop never digs a second cell even when the first
/// one fails; what varies is the approach bearing, the distance, which face is chosen, how long the
/// beat afterwards runs and where the bot returns to. Keeping the vocabulary this thin is the point
/// of the episode, which is why the randomness lives entirely in the parameters.
pub(crate) async fn run(ctx: &mut Ctx) {
    let index = ctx.index();
    let anchor = ctx.anchor();
    let lane = index % 2;
    let lane_bearing = if lane == 0 { 75.0 } else { 255.0 };
    let move_timeout = ctx.cfg().move_timeout_ms.min(18_000);

    ctx.record(
        TYPE_LOOK,
        &format!("vis-lesson-init-{index}"),
        "Single block lesson: long clean approach, one broken block, a moment, and the walk back.",
        json!({"role": LANE_NAMES[lane], "lane_bearing_deg": lane_bearing}),
    );

    let mut pause_ms = 0u64;
    let mut lesson = 0u32;
    while !ctx.target_met() {
        let equipped = ctx.equip("diamond_pickaxe").await;
        let site = approach_point(ctx, anchor, lane_bearing);
        let arrived = ctx.move_to(site.0, site.1, move_timeout, 1.8).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-lesson-approach-{index}-{lesson}"),
            &format!("Walk the clean approach to face {lesson}."),
            json!({"role": LANE_NAMES[lane], "lesson": lesson, "arrived": arrived,
                "pickaxe": equipped, "site": {"x": r2(site.0), "z": r2(site.1)}}),
        );
        if ctx.target_met() {
            break;
        }

        // Face a drawn bearing, then take whatever face is actually there — the arena's, not one
        // the script assumed.
        let heading = ctx.rng_next() * 360.0;
        if let Some(p) = ctx.position() {
            let yaw = heading.to_radians();
            let settle = ctx.cfg().look_settle_ms.max(600);
            ctx.look(
                (p.0 - yaw.sin() * 16.0, p.1 + 1.0, p.2 + yaw.cos() * 16.0),
                settle,
            )
            .await;
        }
        let target = face_ahead(ctx, heading, 3);
        let mut broke = false;
        if let Some(cell) = target {
            let centre = cell.center();
            let settle = ctx.cfg().look_settle_ms.max(400);
            ctx.look((centre.x, centre.y, centre.z), settle).await;
            broke = ctx.dig(cell).await;
        }
        ctx.record(
            TYPE_MINE,
            &format!("vis-lesson-break-{index}-{lesson}"),
            if target.is_some() {
                "Break exactly one block from the prepared face."
            } else {
                "No face within reach of this approach; nothing is broken."
            },
            json!({
                "role": LANE_NAMES[lane], "lesson": lesson, "blocks": 1,
                "heading_deg": r2(heading), "dug": broke,
                "cell": target.map(|c| json!({"x": c.x, "y": c.y, "z": c.z})),
            }),
        );

        let want_pause = rand_range(ctx, 1_400.0, 2_800.0) as u64;
        let paused = budgeted_pause(ctx, &mut pause_ms, want_pause).await;
        let span = rand_range(ctx, 50.0, 140.0);
        let pitch = rand_range(ctx, -14.0, 14.0);
        sweep(ctx, span, pitch).await;
        ctx.record(
            TYPE_LOOK,
            &format!("vis-lesson-beat-{index}-{lesson}"),
            "Stand a moment at the broken face, then read the room once.",
            json!({"lesson": lesson, "paused_ms": if paused { want_pause } else { 0 },
                "pause_budget_ms": pause_ms, "sweep_deg": r2(span), "pitch_deg": r2(pitch)}),
        );
        if ctx.target_met() {
            break;
        }

        let home = ctx.group_center().map(|c| (c.0, c.2)).unwrap_or(anchor);
        let back_dir = ctx.rng_next() * TAU;
        let back_spread = rand_range(ctx, 2.0, 5.0);
        let goal = (
            home.0 + back_dir.cos() * back_spread,
            home.1 + back_dir.sin() * back_spread,
        );
        let returned = ctx.move_to(goal.0, goal.1, move_timeout, 2.5).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-lesson-return-{index}-{lesson}"),
            "Walk the whole way back from the single broken block.",
            json!({"role": LANE_NAMES[lane], "lesson": lesson, "arrived": returned,
                "goal": {"x": r2(goal.0), "z": r2(goal.1)}}),
        );
        witness_peer_edits(ctx).await;
        lesson += 1;
    }
}
