//! CATALOG: family=locomotion min_players=1 single_process=false world_edits=false scenery=none far_players=false

use super::*;

const NODES: usize = 4;
/// Four segments keep a foldback visibly longer than a single out-and-back. Each span is at least
/// 4.2 blocks, clearing the 0.5 s key-chatter floor even if sprint is selected.
const SPAN: f64 = 4.8;
/// A five-second station dwell covers one 93-frame (4.65 s) audit window while its 4-block shuttle
/// keeps the bot out of the frozen gate.
const DWELL_MS: i64 = 5_400;
const ARRIVE_RADIUS: f64 = 1.5;
const SHUTTLE_LEG: f64 = 4.0;
const TURN_RAD: f64 = 0.85;

fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

fn node(origin: (f64, f64), axis: f64, index: usize, span: f64) -> (f64, f64) {
    let bearing = axis + index as f64 * TURN_RAD;
    (
        origin.0 + bearing.cos() * span * (index as f64 + 1.0),
        origin.1 + bearing.sin() * span * (index as f64 + 1.0),
    )
}

/// Hold a view with the standard moving fallback. The return value is intentionally ignored: if a
/// natural-terrain obstacle shortens a shuttle, the next route leg still moves instead of retrying a
/// blocked visual hold forever.
async fn moving_dwell(ctx: &mut Ctx, bearing: f64, deadline_ms: i64) {
    hold_facing(ctx, bearing, deadline_ms, SHUTTLE_LEG).await;
}

/// Walk a bent route out, then fold back over the exact positions the bot actually reached.
///
/// The revisit problem's trajectory face is the foldback: an outbound chain is recorded as landed
/// coordinates, then traversed in reverse, so every return is to observed ground rather than a
/// planned cell. Its view face is the stationary back-sight at each return, confirmed by
/// `look_until_facing`; this is unlike `circuit_polygon_return`, whose stations are evenly spaced
/// around a hub. Alternating the bend sign per round prevents one-way camera accumulation. If
/// terrain makes a node unreachable,
/// `travel_to` sidesteps and the subsequent reverse uses the actual landing, while the dwell's
/// `hold_facing` fallback still supplies movement. The four nodes, 4.8-block base span, 0.85-radian
/// bend, 1.5-block arrival tolerance, and 5.4-second dwell are all chosen to clear QC thresholds.
pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let anchor = ctx.anchor();
    let start = ctx.position().unwrap_or((anchor.0, 64.0, anchor.1));
    let opened = ctx
        .look_until_facing((anchor.0, start.1 + 1.4, anchor.1), 10.0, 4_000)
        .await;
    let origin = walkable_xz_near(
        ctx,
        ((start.0 + anchor.0) * 0.5, (start.2 + anchor.1) * 0.5),
        start.1.round() as i32,
    )
    .unwrap_or((start.0, start.2));
    let axis = vary(ctx, -std::f64::consts::PI, std::f64::consts::PI);
    let span = vary(ctx, SPAN - 0.5, SPAN + 0.8);
    let direction = if global.is_multiple_of(2) { 1.0 } else { -1.0 };
    ctx.record(
        "observe",
        &format!("vis-circuit-foldback-open-{index}"),
        "Open an asymmetric foldback route that will retrace landed nodes.",
        json!({"global_index": global, "opened_on_anchor": opened, "origin": {"x": r2(origin.0), "z": r2(origin.1)}, "axis_deg": r2(axis.to_degrees()), "span": r2(span), "direction": direction}),
    );
    travel_to(ctx, origin, TRAVEL_HOP, ARRIVE_RADIUS).await;
    let mut round = 0u32;
    while !ctx.target_met() {
        let bend = TURN_RAD * direction * if round.is_multiple_of(2) { 1.0 } else { -1.0 };
        let mut landed = vec![origin];
        for step in 0..NODES {
            if ctx.target_met() {
                break;
            }
            let from = *landed.last().unwrap_or(&origin);
            let target = node(from, axis + bend, step, span / (step as f64 + 1.0));
            let gap = travel_to(ctx, target, TRAVEL_HOP, ARRIVE_RADIUS).await;
            let actual = ctx.position().map(|p| (p.0, p.2)).unwrap_or(target);
            landed.push(actual);
            let feet = ctx.position().map(|p| p.1).unwrap_or(start.1);
            let focus = (from.0, feet + 1.4, from.1);
            let confirmed = ctx.look_until_facing(focus, 8.0, 5_000).await;
            let bearing = ctx
                .position()
                .map(|p| look_angles(head_of(p), focus).0)
                .unwrap_or(0.0);
            moving_dwell(ctx, bearing, ctx.elapsed_ms() + DWELL_MS).await;
            ctx.record(TYPE_LOOK, &format!("vis-circuit-foldback-out-{index}-{round}-{step}"), "Dwell on the node just left before extending the foldback.", json!({"round": round, "step": step, "arrival_gap": r2(gap), "facing_confirmed": confirmed, "observed_node": {"x": r2(actual.0), "z": r2(actual.1)}, "elapsed_ms": ctx.elapsed_ms()}));
        }
        for back in (0..landed.len().saturating_sub(1)).rev() {
            if ctx.target_met() {
                break;
            }
            let target = landed[back];
            let leaving = landed[back + 1];
            let gap = travel_to(ctx, target, TRAVEL_HOP, ARRIVE_RADIUS).await;
            let feet = ctx.position().map(|p| p.1).unwrap_or(start.1);
            let focus = (leaving.0, feet + 1.4, leaving.1);
            let confirmed = ctx.look_until_facing(focus, 8.0, 5_000).await;
            let bearing = ctx
                .position()
                .map(|p| look_angles(head_of(p), focus).0)
                .unwrap_or(0.0);
            moving_dwell(ctx, bearing, ctx.elapsed_ms() + DWELL_MS).await;
            ctx.record(TYPE_LOOK, &format!("vis-circuit-foldback-back-{index}-{round}-{back}"), "Retrace an actually landed node and confirm the view just departed.", json!({"round": round, "waypoint": back, "arrival_gap": r2(gap), "facing_confirmed": confirmed, "revisited": {"x": r2(leaving.0), "z": r2(leaving.1)}, "elapsed_ms": ctx.elapsed_ms()}));
        }
        round += 1;
    }
}
