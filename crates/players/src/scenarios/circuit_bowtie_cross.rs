//! CATALOG: family=locomotion min_players=1 single_process=false world_edits=false scenery=none far_players=false

use super::*;

const CORNERS: usize = 4;
/// The diagonal corner radius gives an 8-block side on the self-crossing route, comfortably above
/// the four-block press minimum while remaining close enough to the shared anchor for the first two-player tier.
const RADIUS: f64 = 5.7;
const DWELL_MS: i64 = 5_300;
const ARRIVE_RADIUS: f64 = 1.6;
const SHUTTLE_LEG: f64 = 4.0;
const TAU: f64 = std::f64::consts::TAU;

fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

fn corner(hub: (f64, f64), axis: f64, radius: f64, slot: i64) -> (f64, f64) {
    let theta = axis + slot.rem_euclid(CORNERS as i64) as f64 * TAU / CORNERS as f64;
    (hub.0 + theta.cos() * radius, hub.1 + theta.sin() * radius)
}

async fn dwell(ctx: &mut Ctx, focus: (f64, f64, f64), deadline_ms: i64) {
    let bearing = ctx
        .position()
        .map(|p| look_angles(head_of(p), focus).0)
        .unwrap_or(0.0);
    hold_facing(ctx, bearing, deadline_ms, SHUTTLE_LEG).await;
}

/// Walk a self-crossing bowtie and revisit the crossing neighbourhood before returning home.
///
/// The revisit problem's trajectory face is a closed self-crossing route: the order 0, 2, 1, 3
/// causes two diagonals to pass through the same central neighbourhood, so the crossing is
/// revisited from opposite bearings before the final hub return. Its view face is a stationary,
/// confirmed back-sight to the corner just left at every arrival. This is unlike
/// `circuit_cross_revisit`, whose arms all terminate at one centre, and unlike a regular polygon,
/// whose edges never cross. Natural terrain can shorten a corner plan; `travel_to` sidesteps and
/// continues the fixed order, while its 4-block shuttles keep the dwell moving. The 5.7-block
/// radius, 1.6-block tolerance, and 5.3-second dwell are chosen so diagonal edges clear movement QC
/// and each crossing is visible in a full window.
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
    let axis = vary(ctx, 0.0, TAU) + global as f64 * 0.13;
    let radius = vary(ctx, RADIUS - 0.4, RADIUS + 0.8);
    let order = if global.is_multiple_of(2) {
        [0usize, 2, 1, 3]
    } else {
        [1usize, 3, 0, 2]
    };
    ctx.record("observe", &format!("vis-circuit-bowtie-open-{index}"), "Open a phased self-crossing bowtie route.", json!({"global_index": global, "opened_on_anchor": opened, "hub": {"x": r2(hub.0), "z": r2(hub.1)}, "axis_deg": r2(axis.to_degrees()), "radius": r2(radius), "order": order}));
    travel_to(ctx, hub, TRAVEL_HOP, ARRIVE_RADIUS).await;
    let mut lap = 0u32;
    while !ctx.target_met() {
        let mut previous = hub;
        for (step, &slot) in order.iter().enumerate() {
            if ctx.target_met() {
                break;
            }
            let target = corner(hub, axis, radius, slot as i64);
            let gap = travel_to(ctx, target, TRAVEL_HOP, ARRIVE_RADIUS).await;
            let feet = ctx.position().map(|p| p.1).unwrap_or(start.1);
            let focus = (previous.0, feet + 1.4, previous.1);
            let confirmed = ctx.look_until_facing(focus, 8.0, 5_000).await;
            dwell(ctx, focus, ctx.elapsed_ms() + DWELL_MS).await;
            ctx.record(TYPE_LOOK, &format!("vis-circuit-bowtie-{index}-{lap}-{step}"), "Confirm the preceding corner while traversing the crossing route.", json!({"lap": lap, "step": step, "corner": slot, "arrival_gap": r2(gap), "facing_confirmed": confirmed, "revisited_previous": {"x": r2(previous.0), "z": r2(previous.1)}, "elapsed_ms": ctx.elapsed_ms()}));
            previous = target;
        }
        if ctx.target_met() {
            break;
        }
        let gap = travel_to(ctx, hub, TRAVEL_HOP, ARRIVE_RADIUS).await;
        let feet = ctx.position().map(|p| p.1).unwrap_or(start.1);
        let focus = (hub.0, feet + 1.4, hub.1);
        let confirmed = ctx.look_until_facing(focus, 8.0, 5_000).await;
        dwell(ctx, focus, ctx.elapsed_ms() + DWELL_MS).await;
        ctx.record(TYPE_MOVE, &format!("vis-circuit-bowtie-home-{index}-{lap}"), "Return from the self-crossing route to the hub.", json!({"lap": lap, "home_gap": r2(gap), "facing_confirmed": confirmed, "elapsed_ms": ctx.elapsed_ms()}));
        lap += 1;
    }
}
