//! CATALOG: family=locomotion min_players=1 single_process=false world_edits=false scenery=none far_players=false

use super::*;

const RUNGS: usize = 3;
/// Five blocks between rungs and five blocks between rails make every planned segment at least four
/// blocks after the 1.5-block arrival tolerance, while keeping the whole switchback within 15 blocks.
const RUNG_STEP: f64 = 5.0;
const RAIL_GAP: f64 = 5.0;
const ARRIVE_RADIUS: f64 = 1.5;
const SHUTTLE_LEG: f64 = 4.0;
/// A 5.2-second dwell spans one 93-frame audit window and leaves enough episode budget to close a
/// full outbound-rail / return-rail loop.
const DWELL_MS: i64 = 5_200;

fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

fn station(hub: (f64, f64), axis: f64, rung: usize, rail: f64, step: f64) -> (f64, f64) {
    let along = rung as f64 * step;
    (
        hub.0 + axis.cos() * along - axis.sin() * rail,
        hub.1 + axis.sin() * along + axis.cos() * rail,
    )
}

async fn dwell(ctx: &mut Ctx, focus: (f64, f64, f64), deadline_ms: i64) {
    let bearing = ctx
        .position()
        .map(|p| look_angles(head_of(p), focus).0)
        .unwrap_or(0.0);
    hold_facing(ctx, bearing, deadline_ms, SHUTTLE_LEG).await;
}

/// Walk outward on one offset rail and return on the other, crossing rungs at both ends.
///
/// The revisit problem's trajectory face is a closed switchback with no exact retrace: the outbound
/// and return rails are parallel but five blocks apart, so the bot revisits the same corridor from
/// a displaced viewpoint before returning to the hub. Every station then confirms the previous
/// station with
/// `look_until_facing`, turning a route corner into a verified back-sight rather than an assumed one.
/// This differs from `circuit_foldback_route`, which reverses its actual waypoint chain, and from a
/// square patrol because the two interior rung stations create a ladder-like sequence. Natural-terrain
/// failures remain moving: `travel_to` sidesteps a blocked rail, the opposite rail is still attempted,
/// and each dwell uses 4-block shuttles. Three rungs, 5-block spacing/gap, 1.5-block tolerance, and
/// 5.2-second dwell are the magic numbers that keep movement presses long and permit loop closure.
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
    let axis = vary(ctx, 0.0, std::f64::consts::TAU) + global as f64 * 0.16;
    let step = vary(ctx, RUNG_STEP - 0.4, RUNG_STEP + 0.7);
    let gap = vary(ctx, RAIL_GAP - 0.4, RAIL_GAP + 0.7);
    let first_rail = if global.is_multiple_of(2) {
        gap * 0.5
    } else {
        -gap * 0.5
    };
    let second_rail = -first_rail;
    ctx.record("observe", &format!("vis-circuit-switchback-open-{index}"), "Open a two-rail switchback that returns beside its outbound path.", json!({"global_index": global, "opened_on_anchor": opened, "hub": {"x": r2(hub.0), "z": r2(hub.1)}, "axis_deg": r2(axis.to_degrees()), "rung_step": r2(step), "rail_gap": r2(gap), "first_rail": r2(first_rail)}));
    travel_to(ctx, hub, TRAVEL_HOP, ARRIVE_RADIUS).await;
    let mut lap = 0u32;
    while !ctx.target_met() {
        let mut previous = hub;
        for rung in 0..RUNGS {
            if ctx.target_met() {
                break;
            }
            let target = station(hub, axis, rung + 1, first_rail, step);
            let arrival_gap = travel_to(ctx, target, TRAVEL_HOP, ARRIVE_RADIUS).await;
            let feet = ctx.position().map(|p| p.1).unwrap_or(start.1);
            let focus = (previous.0, feet + 1.4, previous.1);
            let confirmed = ctx.look_until_facing(focus, 8.0, 5_000).await;
            dwell(ctx, focus, ctx.elapsed_ms() + DWELL_MS).await;
            ctx.record(TYPE_LOOK, &format!("vis-circuit-switchback-out-{index}-{lap}-{rung}"), "Advance on the outbound rail and confirm the station just left.", json!({"lap": lap, "rung": rung, "arrival_gap": r2(arrival_gap), "facing_confirmed": confirmed, "elapsed_ms": ctx.elapsed_ms()}));
            previous = target;
        }
        for rung in (0..RUNGS).rev() {
            if ctx.target_met() {
                break;
            }
            let target = station(hub, axis, rung + 1, second_rail, step);
            let arrival_gap = travel_to(ctx, target, TRAVEL_HOP, ARRIVE_RADIUS).await;
            let feet = ctx.position().map(|p| p.1).unwrap_or(start.1);
            let focus = (previous.0, feet + 1.4, previous.1);
            let confirmed = ctx.look_until_facing(focus, 8.0, 5_000).await;
            dwell(ctx, focus, ctx.elapsed_ms() + DWELL_MS).await;
            ctx.record(TYPE_LOOK, &format!("vis-circuit-switchback-back-{index}-{lap}-{rung}"), "Return on the offset rail and confirm the opposite-rail station.", json!({"lap": lap, "rung": rung, "arrival_gap": r2(arrival_gap), "facing_confirmed": confirmed, "elapsed_ms": ctx.elapsed_ms()}));
            previous = target;
        }
        if ctx.target_met() {
            break;
        }
        let home_gap = travel_to(ctx, hub, TRAVEL_HOP, ARRIVE_RADIUS).await;
        let feet = ctx.position().map(|p| p.1).unwrap_or(start.1);
        let focus = (previous.0, feet + 1.4, previous.1);
        let confirmed = ctx.look_until_facing(focus, 8.0, 5_000).await;
        dwell(ctx, focus, ctx.elapsed_ms() + DWELL_MS).await;
        ctx.record(TYPE_MOVE, &format!("vis-circuit-switchback-home-{index}-{lap}"), "Close the two-rail switchback at the hub.", json!({"lap": lap, "home_gap": r2(home_gap), "facing_confirmed": confirmed, "elapsed_ms": ctx.elapsed_ms()}));
        lap += 1;
    }
}
