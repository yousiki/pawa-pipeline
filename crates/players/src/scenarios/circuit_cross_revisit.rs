//! CATALOG: family=locomotion min_players=1 single_process=false world_edits=false scenery=none far_players=false

use super::*;

const ARMS: usize = 4;
/// Six-block arms make each centre-to-end press a real walk, while the four-way cross remains inside
/// the compact staging neighbourhood used by N=2 manifests.
const ARM_LENGTH: f64 = 6.0;
/// The centre dwell lasts longer than a 4.65-second audit window; 4-block shuttles prevent frozen and
/// key-chatter failures while the camera stays on the cross intersection.
const DWELL_MS: i64 = 5_500;
const ARRIVE_RADIUS: f64 = 1.6;
const SHUTTLE_LEG: f64 = 4.0;
const TAU: f64 = std::f64::consts::TAU;

fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

fn arm(hub: (f64, f64), axis: f64, slot: i64, length: f64) -> (f64, f64) {
    let theta = axis + slot.rem_euclid(ARMS as i64) as f64 * TAU / ARMS as f64;
    (hub.0 + theta.cos() * length, hub.1 + theta.sin() * length)
}

async fn dwell_at(ctx: &mut Ctx, focus: (f64, f64, f64), deadline_ms: i64) {
    let bearing = ctx
        .position()
        .map(|p| look_angles(head_of(p), focus).0)
        .unwrap_or(0.0);
    hold_facing(ctx, bearing, deadline_ms, SHUTTLE_LEG).await;
}

/// Traverse the four arms of a natural-terrain cross, returning through the intersection each time.
///
/// The revisit problem's trajectory face is the repeated crossing: every endpoint is reached from
/// the centre, then the centre is reached again before the next arm, so the same intersection is
/// revisited at a cadence much shorter than a full outer circuit. The view face is the confirmed
/// centre sightline at each endpoint and a confirmed endpoint sightline after each crossing. This
/// differs from a radial ladder because only four orthogonal arms are used and the centre crossing
/// is the designed subject, not a sequence of eight bearings. If an arm is blocked, `travel_to`
/// sidesteps and still returns to the centre; the 4-block `hold_facing` dwell is a moving fallback
/// rather than a stationary wait. The 6-block arm, 1.6-block tolerance, and 5.5-second dwell are
/// chosen to clear movement QC and fit a complete verified revisit inside ordinary episodes.
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
    let axis = vary(ctx, 0.0, TAU) + global as f64 * 0.17;
    let length = vary(ctx, ARM_LENGTH - 0.5, ARM_LENGTH + 1.0);
    let direction: i64 = if global.is_multiple_of(2) { 1 } else { -1 };
    let centre_focus = (hub.0, start.1 + 1.4, hub.1);
    ctx.record("observe", &format!("vis-circuit-cross-open-{index}"), "Open a four-arm cross whose intersection is revisited between every arm.", json!({"global_index": global, "opened_on_anchor": opened, "hub": {"x": r2(hub.0), "z": r2(hub.1)}, "axis_deg": r2(axis.to_degrees()), "arm_length": r2(length), "direction": direction}));
    travel_to(ctx, hub, TRAVEL_HOP, ARRIVE_RADIUS).await;
    let mut pass = 0u32;
    while !ctx.target_met() {
        for i in 0..ARMS {
            if ctx.target_met() {
                break;
            }
            let slot = (i as i64 + pass as i64 * direction).rem_euclid(ARMS as i64);
            let endpoint = arm(hub, axis, slot, length);
            let outbound_gap = travel_to(ctx, endpoint, TRAVEL_HOP, ARRIVE_RADIUS).await;
            if ctx.target_met() {
                break;
            }
            let feet = ctx.position().map(|p| p.1).unwrap_or(start.1);
            let confirmed_end = ctx.look_until_facing(centre_focus, 8.0, 5_000).await;
            dwell_at(ctx, centre_focus, ctx.elapsed_ms() + DWELL_MS).await;
            let home_gap = travel_to(ctx, hub, TRAVEL_HOP, ARRIVE_RADIUS).await;
            let endpoint_focus = (endpoint.0, feet + 1.4, endpoint.1);
            let confirmed_arm = ctx.look_until_facing(endpoint_focus, 8.0, 5_000).await;
            dwell_at(ctx, endpoint_focus, ctx.elapsed_ms() + DWELL_MS).await;
            ctx.record(TYPE_MOVE, &format!("vis-circuit-cross-{index}-{pass}-{i}"), "Return through the cross intersection and verify both sides of the arm.", json!({"pass": pass, "arm": slot, "outbound_gap": r2(outbound_gap), "home_gap": r2(home_gap), "centre_confirmed": confirmed_end, "endpoint_confirmed": confirmed_arm, "endpoint": {"x": r2(endpoint.0), "z": r2(endpoint.1)}, "elapsed_ms": ctx.elapsed_ms()}));
        }
        pass += 1;
    }
}
