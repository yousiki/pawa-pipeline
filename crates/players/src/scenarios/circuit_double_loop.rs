//! CATALOG: family=locomotion min_players=1 single_process=false world_edits=false scenery=none far_players=false

use super::*;

const STATIONS: usize = 4;
const TAU: f64 = std::f64::consts::TAU;
/// The inner loop still has 6-block radius, whose square chord is 8.5 blocks; the outer loop adds
/// a second scale without making either route a long one-way excursion.
const INNER_RADIUS: f64 = 6.0;
const OUTER_RADIUS: f64 = 9.0;
/// Each loop station gets a dwell longer than one 4.65-second audit window, with movement supplied by
/// 4-block out-and-back legs.
const DWELL_MS: i64 = 5_300;
const ARRIVE_RADIUS: f64 = 1.6;
const SHUTTLE_LEG: f64 = 4.0;

fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

fn station(hub: (f64, f64), axis: f64, radius: f64, slot: i64) -> (f64, f64) {
    let theta = axis + slot.rem_euclid(STATIONS as i64) as f64 * TAU / STATIONS as f64;
    (hub.0 + theta.cos() * radius, hub.1 + theta.sin() * radius)
}

async fn moving_dwell(ctx: &mut Ctx, focus: (f64, f64, f64), deadline_ms: i64) {
    let bearing = ctx
        .position()
        .map(|p| look_angles(head_of(p), focus).0)
        .unwrap_or(0.0);
    hold_facing(ctx, bearing, deadline_ms, SHUTTLE_LEG).await;
}

/// Alternate two concentric four-station loops, returning through the hub after each loop.
///
/// The revisit problem's trajectory face is the double closure: the inner and outer loops are each
/// closed, then the route explicitly returns to the common hub, giving repeated terrain at two
/// scales. The view face is a stationary `look_until_facing` on the hub after every station, so the
/// camera does not follow travel and every hub view is re-entered. This differs from
/// `circuit_polygon_return` by using two radii and a hub transition rather than one polygon, and
/// from ring patrols by stopping one loop before starting the other. If a station is blocked,
/// `travel_to`'s sidestep still advances; the hub return is always attempted, and `hold_facing`
/// keeps a failed visual dwell moving. Six and nine block radii, four stations, 1.6-block arrival
/// tolerance, and 5.3-second dwells are the magic numbers that keep chords long and windows
/// revisitable.
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
    let axis = vary(ctx, 0.0, TAU) + global as f64 * 0.23;
    let inner = vary(ctx, INNER_RADIUS - 0.5, INNER_RADIUS + 0.8);
    let outer = vary(ctx, OUTER_RADIUS - 0.6, OUTER_RADIUS + 1.0);
    let direction: i64 = if global.is_multiple_of(2) { 1 } else { -1 };
    ctx.record("observe", &format!("vis-circuit-double-open-{index}"), "Open two concentric closed loops around a shared hub.", json!({"global_index": global, "opened_on_anchor": opened, "hub": {"x": r2(hub.0), "z": r2(hub.1)}, "inner_radius": r2(inner), "outer_radius": r2(outer), "axis_deg": r2(axis.to_degrees()), "direction": direction}));
    travel_to(ctx, hub, TRAVEL_HOP, ARRIVE_RADIUS).await;
    let mut lap = 0u32;
    while !ctx.target_met() {
        let radius = if lap.is_multiple_of(2) { inner } else { outer };
        let mut slot = 0i64;
        for station_index in 0..STATIONS {
            if ctx.target_met() {
                break;
            }
            slot += direction;
            let target = station(hub, axis, radius, slot);
            let gap = travel_to(ctx, target, TRAVEL_HOP, ARRIVE_RADIUS).await;
            if ctx.target_met() {
                break;
            }
            let feet = ctx.position().map(|p| p.1).unwrap_or(start.1);
            let focus = (hub.0, feet + 1.4, hub.1);
            let confirmed = ctx.look_until_facing(focus, 8.0, 5_000).await;
            moving_dwell(ctx, focus, ctx.elapsed_ms() + DWELL_MS).await;
            ctx.record(TYPE_LOOK, &format!("vis-circuit-double-{index}-{lap}-{station_index}"), "Reconfirm the common hub while traversing this loop scale.", json!({"lap": lap, "station": station_index, "radius": r2(radius), "arrival_gap": r2(gap), "facing_confirmed": confirmed, "elapsed_ms": ctx.elapsed_ms()}));
        }
        if ctx.target_met() {
            break;
        }
        let home_gap = travel_to(ctx, hub, TRAVEL_HOP, ARRIVE_RADIUS).await;
        let feet = ctx.position().map(|p| p.1).unwrap_or(start.1);
        let focus = (hub.0, feet + 1.4, hub.1);
        let confirmed = ctx.look_until_facing(focus, 8.0, 5_000).await;
        moving_dwell(ctx, focus, ctx.elapsed_ms() + DWELL_MS).await;
        ctx.record(TYPE_MOVE, &format!("vis-circuit-double-home-{index}-{lap}"), "Close this loop at the shared hub before changing scale.", json!({"lap": lap, "radius": r2(radius), "home_gap": r2(home_gap), "facing_confirmed": confirmed, "elapsed_ms": ctx.elapsed_ms()}));
        lap += 1;
    }
}
