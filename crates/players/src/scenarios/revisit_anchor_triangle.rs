//! CATALOG: family=many_player_spatial min_players=1 single_process=true world_edits=true scenery=none far_players=false
//!
//! Round-robin revisit: three marked vertices walked A -> B -> C -> A, so every vertex is
//! re-observed once per lap and the trajectory is a closed circuit by construction.
//!
//! The trajectory side of the revisit problem: `straight_line_walk`'s 16-block one-way ray is replaced by a
//! triangle whose net displacement per lap is zero while its path length is three full edges, which
//! is the direct evidence the frustum audit asks for (`path_length` >> `displacement`).
//!
//! The view side: an edge walk leaves the camera on the travel heading, so the vertex marker is out
//! of frame for most of the edge and comes back into it only when the bot turns onto it at arrival.
//! That turn is the return being scored, and it is confirmed with `look_until_facing` rather than
//! `look` — a `look` returns on a deadline, so a wide turn that timed out would be logged as a
//! return that never happened.
//!
//! What makes the revisit CHECKABLE is material identity: the three vertices are marked in three
//! different blocks, so "am I back at A" is answerable from the pixels and from the event stream,
//! not only from coordinates. Distinct from `depart_and_revisit` (one mark, one departure, one
//! return) and from `sparse_checkpoint_return` (no marker, no verification at all): here the same
//! three subjects are each verified several times inside one episode, which turns a single return
//! into a series whose consistency can be compared across laps.
use super::*;

/// A marked triangle vertex: the cell its marker occupies, and the point to aim the camera at when
/// the circuit comes back around to it.
type Vertex = (BlockPos, (f64, f64, f64));

/// Triangle circumradius. The side is `r * sqrt(3)` = 13.9 blocks: long enough that a vertex leaves
/// the 102.5 deg horizontal frame during the edge walk (so the arrival really is a re-acquisition),
/// short enough that all three markers stay inside one loaded neighbourhood and the per-window
/// displacement stays under the 5-block target.
const VERTEX_RADIUS: f64 = 8.0;

/// One marker per vertex, each a different block, because the vertex's IDENTITY is what the return
/// verifies. Two tall so it clears grass and a shallow slope while staying inside placing reach
/// from the ground.
const MATERIALS: [&str; 3] = [
    "minecraft:stone_bricks",
    "minecraft:bricks",
    "minecraft:oak_planks",
];
const MARKER_HEIGHT: i64 = 2;

/// Per-bot rotation of the whole triangle. 40 deg is deliberately NOT a divisor of 120, so no two
/// bots ever share a vertex, while every vertex stays inside the same ~8-block ring around the
/// anchor and peers therefore stay in frame from the opening (compact-staging rule).
const FAN_DEG: f64 = 40.0;

/// Arrival radius for an edge. Well inside the 4.5-block placing reach of the vertex cell, and far
/// enough from the marker that the marker is not clipping the near plane on arrival.
const ARRIVE_RADIUS: f64 = 2.0;

/// One shuttle leg for the vertex dwell, in blocks. 4 clears the key-chatter floor at sprint speed
/// and matches Solaris's 3-4 block random-walk primitive; see `hold_facing`.
const SHUTTLE_LEG: f64 = 4.0;

/// Dwell facing the vertex after verifying it. Long enough that a 93-frame (4.65 s) training window
/// can sit entirely inside a verified re-observation, short enough that three of them plus three
/// edges still fit inside a normal episode.
const VERTEX_HOLD_MS: i64 = 5_000;

/// Ceiling on the arrival turn. A 180 deg reversal at the default 72 deg/s needs 2.5 s; double that
/// leaves room for the poll granularity and for drift while turning.
const FACE_MAX_WAIT_MS: u64 = 5_000;

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let anchor = ctx.anchor();
    let start = ctx.position().unwrap_or((anchor.0, 64.0, anchor.1));
    let feet_y = start.1.round() as i64;

    // Rotate this bot's triangle off every peer's, and let its parity choose the walking direction.
    // Same structure, never the same action sequence — the near-clones QC check is pairwise over
    // action streams, so two bots tracing identical laps would fail it however good the lap is.
    let fan = global as f64 * FAN_DEG;
    let clockwise = global.is_multiple_of(2);
    let aims: [(f64, f64); 3] = std::array::from_fn(|v| {
        let bearing = (fan + v as f64 * 120.0).to_radians();
        (
            anchor.0 - bearing.sin() * VERTEX_RADIUS,
            anchor.1 + bearing.cos() * VERTEX_RADIUS,
        )
    });

    ctx.record(
        "observe",
        &format!("vis-revisit-triangle-setup-{index}"),
        "Open a three-vertex revisit circuit: mark each vertex once, then verify it every lap.",
        json!({
            "global_index": global,
            "role": "circuit_revisitor",
            "triangle_fan_deg": fan,
            "clockwise": clockwise,
            "vertex_radius": VERTEX_RADIUS,
            "vertices": aims.iter().map(|(x, z)| json!({"x": r2(*x), "z": r2(*z)}))
                .collect::<Vec<_>>(),
        }),
    );

    // The marker actually placed at each vertex, once it exists. `None` means the vertex has not
    // been marked yet — on real terrain a placement can legitimately fail (a slope, water, leaves),
    // and a failed vertex just gets another chance next lap instead of poisoning the circuit.
    let mut vertices: [Option<Vertex>; 3] = [None; 3];
    let mut step = 0usize;
    while !ctx.target_met() {
        let turn = if clockwise {
            step as i64
        } else {
            -(step as i64)
        };
        let v = (global as i64 + turn).rem_euclid(3) as usize;
        let material = MATERIALS[v];
        let travelled = travel_to(ctx, aims[v], TRAVEL_HOP, ARRIVE_RADIUS).await;
        if ctx.target_met() {
            break;
        }

        let first_visit = vertices[v].is_none();
        if first_visit {
            let desired = (aims[v].0.round() as i64, feet_y, aims[v].1.round() as i64);
            vertices[v] = raise_landmark(ctx, desired, MARKER_HEIGHT, material).await;
        }
        let base = vertices[v].map(|(base, _)| base);
        let focus = vertices[v].map(|(_, point)| point).unwrap_or((
            aims[v].0,
            start.1 + MARKER_HEIGHT as f64,
            aims[v].1,
        ));
        let faced = ctx.look_until_facing(focus, 5.0, FACE_MAX_WAIT_MS).await;
        // Probe the NEIGHBOURHOOD, not the one cell. `marker_base_near` may have shifted the column
        // by up to two blocks when it was placed, so an exact-cell probe would report a false
        // negative and make a shoved marker indistinguishable from a vertex the world reverted.
        let observed = base.and_then(|b| {
            area_find_block(
                ctx,
                (b.x, b.z),
                (b.y - 2, b.y + MARKER_HEIGHT as i32 + 1),
                material,
            )
        });
        ctx.record(
            "observe",
            &format!("vis-revisit-triangle-{index}-{step}"),
            "Arrive at a circuit vertex and verify which vertex it is by its marker material.",
            json!({
                "vertex": v,
                "lap": step / 3,
                "first_visit": first_visit,
                "is_revisit": !first_visit,
                "expected_material": material,
                "observed_cell": observed.map(|c| json!({"x": c.x, "y": c.y, "z": c.z})),
                "persisted": observed.is_some(),
                "faced_marker": faced,
                "arrival_distance": r2(travelled),
                "elapsed_ms": ctx.elapsed_ms(),
            }),
        );

        // Dwell on the FIXED bearing to the marker, never tracking it. A tracked hold turns the
        // camera continuously to keep the subject centred, and the direction of that turn follows
        // the body — exactly what the camera/travel-independence gate measures.
        let bearing = ctx
            .position()
            .map(|p| look_angles(head_of(p), focus).0)
            .unwrap_or(0.0);
        hold_facing(ctx, bearing, ctx.elapsed_ms() + VERTEX_HOLD_MS, SHUTTLE_LEG).await;
        witness_peer_edits(ctx).await;
        step += 1;
    }
}
