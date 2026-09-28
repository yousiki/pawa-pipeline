//! CATALOG: family=locomotion min_players=1 single_process=false world_edits=false scenery=none far_players=false

use super::*;

const VERTICES: usize = 5;
/// Circumradius of the polygon. A five-sided polygon with this radius has edges over four blocks,
/// so every `travel_to` leg clears the key-chatter floor while the whole route remains local enough
/// for a 93-frame window to see repeated terrain.
const RADIUS: f64 = 7.0;
/// Station dwell keeps the bot moving without turning during a view. Five seconds spans at least one
/// audit window (4.65 s), and the 4-block shuttle leg is longer than the 0.5 s short-press limit.
const DWELL_MS: i64 = 5_600;
const ARRIVE_RADIUS: f64 = 1.6;
const SHUTTLE_LEG: f64 = 4.0;
const TAU: f64 = std::f64::consts::TAU;

fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

fn vertex(hub: (f64, f64), axis: f64, radius: f64, slot: i64) -> (f64, f64) {
    let theta = axis + slot.rem_euclid(VERTICES as i64) as f64 * TAU / VERTICES as f64;
    (hub.0 + theta.cos() * radius, hub.1 + theta.sin() * radius)
}

/// Move while holding a confirmed view. `hold_facing` reuses 4-block out-and-back legs; it is the
/// deliberate fallback when terrain blocks a planned polygon edge, because it still produces motion
/// instead of repeatedly retrying an unreachable point.
async fn dwell(ctx: &mut Ctx, bearing: f64, deadline_ms: i64) {
    hold_facing(ctx, bearing, deadline_ms, SHUTTLE_LEG).await;
}

/// Walk a closed five-sided route and revisit the hub-facing view at every vertex.
///
/// The revisit problem's trajectory face is explicit: the polygon returns to its entry vertex and
/// then to its hub, so `path_length` grows while displacement stays local. Its view face is the
/// confirmed turn at each station: after walking an edge, the bot uses `look_until_facing` on the
/// hub direction before the dwell, re-entering the same neighbourhood from many polygon positions.
/// Unlike
/// `circuit_square_backsight`, the odd polygon has no axis-aligned corner pattern and its non-divisible
/// phase makes N=2 routes differ without changing the shared terrain. Failed edge plans degrade to the
/// moving shuttle rather than a stationary retry; the route still ends with an explicit hub return.
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
    .unwrap_or((anchor.0, anchor.1));
    let axis = vary(ctx, 0.0, TAU) + global as f64 * 0.19;
    let radius = vary(ctx, RADIUS - 0.8, RADIUS + 1.0);
    let direction: i64 = if global.is_multiple_of(2) { 1 } else { -1 };

    ctx.record(
        "observe",
        &format!("vis-circuit-polygon-open-{index}"),
        "Open a phased polygon whose stations all return to the hub.",
        json!({
            "global_index": global,
            "opened_on_anchor": opened,
            "hub": {"x": r2(hub.0), "z": r2(hub.1)},
            "vertices": VERTICES,
            "axis_deg": r2(axis.to_degrees()),
            "radius": r2(radius),
            "direction": direction,
        }),
    );
    travel_to(ctx, hub, TRAVEL_HOP, ARRIVE_RADIUS).await;
    let mut slot = 0i64;
    let mut round = 0u32;
    while !ctx.target_met() {
        slot += direction;
        let target = vertex(hub, axis, radius, slot);
        let gap = travel_to(ctx, target, TRAVEL_HOP, ARRIVE_RADIUS).await;
        if ctx.target_met() {
            break;
        }
        let feet = ctx.position().map(|p| p.1).unwrap_or(start.1);
        let focus = (hub.0, feet + 1.4, hub.1);
        let confirmed = ctx.look_until_facing(focus, 8.0, 5_000).await;
        let bearing = ctx
            .position()
            .map(|p| look_angles(head_of(p), focus).0)
            .unwrap_or(0.0);
        dwell(ctx, bearing, ctx.elapsed_ms() + DWELL_MS).await;
        ctx.record(
            TYPE_LOOK,
            &format!("vis-circuit-polygon-{index}-{round}"),
            "Reconfirm the hub-facing view at a polygon vertex before the next edge.",
            json!({
                "round": round,
                "slot": slot.rem_euclid(VERTICES as i64),
                "arrival_gap": r2(gap),
                "facing_confirmed": confirmed,
                "hub_view": {"x": r2(focus.0), "y": r2(focus.1), "z": r2(focus.2)},
                "elapsed_ms": ctx.elapsed_ms(),
            }),
        );
        round += 1;
        if slot.rem_euclid(VERTICES as i64) == 0 {
            travel_to(ctx, hub, TRAVEL_HOP, ARRIVE_RADIUS).await;
            ctx.record(
                TYPE_MOVE,
                &format!("vis-circuit-polygon-home-{index}-{round}"),
                "Close the polygon lap by returning to its hub.",
                json!({"lap": round / VERTICES as u32, "hub": {"x": r2(hub.0), "z": r2(hub.1)}}),
            );
        }
    }
}
