//! CATALOG: family=construction min_players=1 single_process=false world_edits=true scenery=none far_players=false

use super::*;

const CHECKPOINTS: usize = 3;
type MarkedPost = (BlockPos, (f64, f64, f64));
const MATERIALS: [&str; CHECKPOINTS] = [
    "minecraft:stone_bricks",
    "minecraft:bricks",
    "minecraft:oak_planks",
];
/// Seven blocks separates checkpoint subjects enough that each approach is a real traversal, while
/// all three remain in the compact natural-terrain neighbourhood shared by N=2 bots.
const RADIUS: f64 = 7.0;
const POST_HEIGHT: i64 = 2;
/// Five and a half seconds spans one 4.65-second audit window; the standard 4-block shuttle clears
/// frozen and key chatter without turning during the critical sightline.
const DWELL_MS: i64 = 5_500;
const ARRIVE_RADIUS: f64 = 1.6;
const SHUTTLE_LEG: f64 = 4.0;
const TAU: f64 = std::f64::consts::TAU;

fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

fn checkpoint(hub: (f64, f64), axis: f64, radius: f64, slot: usize) -> (f64, f64) {
    let theta = axis + slot as f64 * TAU / CHECKPOINTS as f64;
    (hub.0 + theta.cos() * radius, hub.1 + theta.sin() * radius)
}

async fn dwell(ctx: &mut Ctx, focus: (f64, f64, f64), deadline_ms: i64) {
    let bearing = ctx
        .position()
        .map(|p| look_angles(head_of(p), focus).0)
        .unwrap_or(0.0);
    hold_facing(ctx, bearing, deadline_ms, SHUTTLE_LEG).await;
}

/// Build three distinct checkpoint posts, then patrol their closed chain and verify each on return.
///
/// The revisit problem's checkability face is the landmark identity: stone bricks, bricks, and oak
/// planks make the three revisited positions distinguishable in pixels and in the event stream. The
/// trajectory is a closed chain that returns to the hub after every three checkpoints; every
/// arrival stands still and calls `look_until_facing` on the post before its moving dwell. Unlike
/// `revisit_anchor_triangle`, marker placement is a setup pass and the recurring route includes an
/// explicit shared-hub return, so the same centre view separates every landmark lap. If natural
/// terrain refuses a post, the checkpoint remains in the route with a coordinate focus and the bot
/// keeps moving; no failed probe becomes a stationary episode. The 7-block radius, 2-block posts,
/// 1.6-block arrival tolerance, 4-block shuttles, and 5.5-second dwells are all QC-sized constants.
pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let anchor = ctx.anchor();
    let start = ctx.position().unwrap_or((anchor.0, 64.0, anchor.1));
    let opened = ctx
        .look_until_facing((anchor.0, start.1 + 1.4, anchor.1), 10.0, 4_000)
        .await;
    let hub = walkable_xz_near(
        ctx,
        ((start.0 + anchor.0) * 0.5, (start.2 + anchor.1) * 0.5),
        start.1.round() as i32,
    )
    .unwrap_or((start.0, start.2));
    let axis = vary(ctx, 0.0, TAU) + global as f64 * 0.21;
    let radius = vary(ctx, RADIUS - 0.6, RADIUS + 0.9);
    let mut points = [(0.0, 0.0); CHECKPOINTS];
    let mut posts: [Option<MarkedPost>; CHECKPOINTS] = [None; CHECKPOINTS];
    for slot in 0..CHECKPOINTS {
        points[slot] = checkpoint(hub, axis, radius, slot);
        let gap = travel_to(ctx, points[slot], TRAVEL_HOP, ARRIVE_RADIUS).await;
        let feet = ctx.position().map(|p| p.1).unwrap_or(start.1);
        let desired = (
            points[slot].0.round() as i64,
            feet.round() as i64,
            points[slot].1.round() as i64,
        );
        posts[slot] = raise_landmark(ctx, desired, POST_HEIGHT, MATERIALS[slot]).await;
        ctx.record(TYPE_PLACE, &format!("vis-circuit-landmark-setup-{index}-{slot}"), "Place one material-coded checkpoint post on the closed path.", json!({"checkpoint": slot, "material": MATERIALS[slot], "arrival_gap": r2(gap), "placed": posts[slot].is_some(), "desired": {"x": desired.0, "y": desired.1, "z": desired.2}}));
    }
    travel_to(ctx, hub, TRAVEL_HOP, ARRIVE_RADIUS).await;
    ctx.record("observe", &format!("vis-circuit-landmark-open-{index}"), "Open a closed path through three material-coded landmarks.", json!({"global_index": global, "opened_on_anchor": opened, "hub": {"x": r2(hub.0), "z": r2(hub.1)}, "axis_deg": r2(axis.to_degrees()), "radius": r2(radius), "placed": posts.iter().filter(|p| p.is_some()).count()}));
    let direction: i64 = if global.is_multiple_of(2) { 1 } else { -1 };
    let mut visit = 0u32;
    while !ctx.target_met() {
        for step in 0..CHECKPOINTS {
            if ctx.target_met() {
                break;
            }
            let slot = (step as i64 * direction).rem_euclid(CHECKPOINTS as i64) as usize;
            let gap = travel_to(ctx, points[slot], TRAVEL_HOP, ARRIVE_RADIUS).await;
            let feet = ctx.position().map(|p| p.1).unwrap_or(start.1);
            let focus =
                posts[slot]
                    .map(|(_, p)| p)
                    .unwrap_or((points[slot].0, feet + 1.4, points[slot].1));
            let confirmed = ctx.look_until_facing(focus, 7.0, 5_000).await;
            let observed = posts[slot].and_then(|(base, _)| {
                area_find_block(
                    ctx,
                    (base.x, base.z),
                    (base.y - 2, base.y + POST_HEIGHT as i32 + 1),
                    MATERIALS[slot],
                )
            });
            dwell(ctx, focus, ctx.elapsed_ms() + DWELL_MS).await;
            ctx.record(TYPE_LOOK, &format!("vis-circuit-landmark-{index}-{visit}"), "Verify the material-coded checkpoint before continuing the closed path.", json!({"visit": visit, "lap": visit / CHECKPOINTS as u32, "checkpoint": slot, "material": MATERIALS[slot], "arrival_gap": r2(gap), "facing_confirmed": confirmed, "persisted": observed.is_some(), "elapsed_ms": ctx.elapsed_ms()}));
            visit += 1;
        }
        if ctx.target_met() {
            break;
        }
        let gap = travel_to(ctx, hub, TRAVEL_HOP, ARRIVE_RADIUS).await;
        let feet = ctx.position().map(|p| p.1).unwrap_or(start.1);
        let focus = (hub.0, feet + 1.4, hub.1);
        let confirmed = ctx.look_until_facing(focus, 8.0, 5_000).await;
        dwell(ctx, focus, ctx.elapsed_ms() + DWELL_MS).await;
        ctx.record(TYPE_MOVE, &format!("vis-circuit-landmark-home-{index}-{visit}"), "Close the landmark lap at the shared hub.", json!({"home_gap": r2(gap), "facing_confirmed": confirmed, "elapsed_ms": ctx.elapsed_ms()}));
    }
}
