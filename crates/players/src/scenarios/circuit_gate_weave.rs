//! CATALOG: family=construction min_players=1 single_process=false world_edits=true scenery=none far_players=false

use super::*;

const GATES: usize = 3;
/// Gate offsets form a shallow S: consecutive spans remain over four blocks, but the route is not a
/// renamed straight corridor. The short radius also keeps both bots near the shared anchor.
const GATE_STEP: f64 = 5.0;
const POST_HEIGHT: i64 = 2;
const MATERIAL: &str = "minecraft:stone_bricks";
const DWELL_MS: i64 = 5_200;
const ARRIVE_RADIUS: f64 = 1.6;
const SHUTTLE_LEG: f64 = 4.0;

fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

fn gate(hub: (f64, f64), axis: f64, index: usize, step: f64) -> (f64, f64) {
    let along = (index as f64 + 1.0) * step;
    let side = if index.is_multiple_of(2) { 1.0 } else { -1.0 };
    (
        hub.0 + axis.cos() * along + (-axis.sin()) * side * 2.0,
        hub.1 + axis.sin() * along + axis.cos() * side * 2.0,
    )
}

async fn dwell(ctx: &mut Ctx, focus: (f64, f64, f64), deadline_ms: i64) {
    let bearing = ctx
        .position()
        .map(|p| look_angles(head_of(p), focus).0)
        .unwrap_or(0.0);
    hold_facing(ctx, bearing, deadline_ms, SHUTTLE_LEG).await;
}

/// Pass three marked gates in an S-shaped order, then weave back through the middle gate home.
///
/// The revisit problem's route face is a gated loop: the order is gate 0, 1, 2, 1, 0, then the hub,
/// so the middle gate is a deliberate cross-revisit rather than a terminal waypoint. Each gate
/// carries a small landmark, and each arrival stops to `look_until_facing` it before moving again.
/// Unlike
/// `circuit_double_loop`, this route is a single asymmetric corridor with an explicit gate sequence;
/// unlike a straight shuttle, its alternating lateral offsets create an S. A placement failure leaves
/// the coordinate gate in the route and its focus falls back to head height, while `hold_facing` still
/// moves through the dwell. Three gates, 5-block spacing, 2-block posts, 1.6-block tolerance, and
/// 5.2-second dwells are sized for terrain routing, chatter, and one full audit window.
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
    let axis = vary(ctx, 0.0, std::f64::consts::TAU);
    let step = vary(ctx, GATE_STEP - 0.5, GATE_STEP + 0.8);
    let mut gates = [(0.0, 0.0); GATES];
    let mut focuses: [Option<(f64, f64, f64)>; GATES] = [None; GATES];
    for i in 0..GATES {
        gates[i] = gate(hub, axis, i, step);
        let gap = travel_to(ctx, gates[i], TRAVEL_HOP, ARRIVE_RADIUS).await;
        let feet = ctx.position().map(|p| p.1).unwrap_or(start.1);
        let desired = (
            gates[i].0.round() as i64,
            feet.round() as i64,
            gates[i].1.round() as i64,
        );
        focuses[i] = raise_landmark(ctx, desired, POST_HEIGHT, MATERIAL)
            .await
            .map(|(_, p)| p);
        ctx.record(TYPE_PLACE, &format!("vis-circuit-gate-setup-{index}-{i}"), "Mark a gate on the natural-terrain route.", json!({"gate": i, "arrival_gap": r2(gap), "desired": {"x": desired.0, "y": desired.1, "z": desired.2}, "placed": focuses[i].is_some()}));
    }
    travel_to(ctx, hub, TRAVEL_HOP, ARRIVE_RADIUS).await;
    ctx.record("observe", &format!("vis-circuit-gate-open-{index}"), "Open an S-shaped route through three gates and back.", json!({"global_index": global, "opened_on_anchor": opened, "hub": {"x": r2(hub.0), "z": r2(hub.1)}, "axis_deg": r2(axis.to_degrees()), "step": r2(step), "gates": gates.iter().map(|(x,z)| json!({"x": r2(*x), "z": r2(*z)})).collect::<Vec<_>>() }));
    let order = [0usize, 1, 2, 1, 0];
    let mut pass = 0u32;
    while !ctx.target_met() {
        for &gate_index in &order {
            if ctx.target_met() {
                break;
            }
            let gap = travel_to(ctx, gates[gate_index], TRAVEL_HOP, ARRIVE_RADIUS).await;
            let feet = ctx.position().map(|p| p.1).unwrap_or(start.1);
            let focus = focuses[gate_index].unwrap_or((
                gates[gate_index].0,
                feet + 1.4,
                gates[gate_index].1,
            ));
            let confirmed = ctx.look_until_facing(focus, 8.0, 5_000).await;
            dwell(ctx, focus, ctx.elapsed_ms() + DWELL_MS).await;
            ctx.record(TYPE_LOOK, &format!("vis-circuit-gate-pass-{index}-{pass}"), "Stop at a gate, confirm its marker, and continue the weave.", json!({"pass": pass, "gate": gate_index, "arrival_gap": r2(gap), "facing_confirmed": confirmed, "marked": focuses[gate_index].is_some(), "elapsed_ms": ctx.elapsed_ms()}));
            pass += 1;
        }
        if ctx.target_met() {
            break;
        }
        let gap = travel_to(ctx, hub, TRAVEL_HOP, ARRIVE_RADIUS).await;
        let feet = ctx.position().map(|p| p.1).unwrap_or(start.1);
        let focus = (hub.0, feet + 1.4, hub.1);
        let confirmed = ctx.look_until_facing(focus, 8.0, 5_000).await;
        dwell(ctx, focus, ctx.elapsed_ms() + DWELL_MS).await;
        ctx.record(TYPE_MOVE, &format!("vis-circuit-gate-home-{index}-{pass}"), "Return from the gate weave to the hub.", json!({"arrival_gap": r2(gap), "facing_confirmed": confirmed, "elapsed_ms": ctx.elapsed_ms()}));
    }
}
