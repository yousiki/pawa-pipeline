//! CATALOG: family=construction min_players=2 single_process=true world_edits=true scenery=none far_players=false
//!
//! An alternating calibration job around one shared cairn: slot 0 adds one block, slot 1 measures it,
//! then the roles swap. This addresses the revisit problem's missing causal link between a peer and a world edit;
//! the first stationary gaze faces the peer before either bot travels, so the context frame contains
//! both actors. Each worker returns from its work point to its viewing point, making a trajectory loop.
//! Near-clones are avoided by opposite stations, unequal work/watch durations, and alternating roles;
//! if a marker cannot be placed on natural terrain, the episode degrades to a recorded observation and
//! still keeps the movement and confirmed re-acquisition beats.
//!
//! The shared cairn is deliberately only three blocks high: it is readable at the eight-block spread,
//! but remains inside placement reach. All turns happen after stopping; no peer is tracked while moving.

use super::*;

const MATERIALS: [&str; 2] = ["minecraft:stone_bricks", "minecraft:oak_planks"];
const CAIRN_HEIGHT: i64 = 3;
const STATION_HALF: f64 = 5.0;
const APPROACH: f64 = 2.2;
const ROUND_MS: f64 = 8_500.0;
const PEER_TOL_DEG: f64 = 8.0;
const MIN_PRESS_MS: u64 = 700;
const GAP_TIGHT: f64 = 6.5;
const GAP_LOOSE: f64 = 11.0;

fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

fn axis(ctx: &Ctx) -> (f64, f64) {
    let (x, z) = ctx
        .engine()
        .shared
        .native_waypoints
        .first()
        .copied()
        .unwrap_or((1.0, 0.0));
    let d = x.hypot(z).max(0.001);
    // Keep roles aligned within each pair, but rotate pair workspaces for N=4+.
    let angle = ctx.index().saturating_div(2) as f64 * 0.83;
    let (sin, cos) = angle.sin_cos();
    (x / d * cos - z / d * sin, x / d * sin + z / d * cos)
}

fn peer_gap(ctx: &Ctx, peer: Option<usize>) -> Option<f64> {
    Some({
        let a = ctx.position()?;
        let b = ctx.peer_position(peer?)?;
        (a.0 - b.0).hypot(a.2 - b.2)
    })
}

async fn face_peer(ctx: &Ctx, peer: Option<usize>, fallback: (f64, f64, f64)) -> bool {
    let target = peer
        .and_then(|j| ctx.peer_position(j))
        .map(head_of)
        .unwrap_or(fallback);
    ctx.look_until_facing(target, PEER_TOL_DEG, 4_000).await
}

async fn frame_leg(ctx: &mut Ctx, peer: Option<usize>, side: WalkDirection, ms: u64) {
    let dir = match peer_gap(ctx, peer) {
        Some(g) if g < GAP_TIGHT => WalkDirection::Backward,
        Some(g) if g > GAP_LOOSE => WalkDirection::Forward,
        _ => side,
    };
    if !ctx.hold_visual(dir, false, ms.max(MIN_PRESS_MS), 0).await
        && let Some(p) = ctx.position()
    {
        let a = vary(ctx, 0.0, std::f64::consts::TAU);
        if let Some(t) = walkable_xz_near(
            ctx,
            (p.0 + a.cos() * 4.0, p.2 + a.sin() * 4.0),
            p.1.floor() as i32,
        ) {
            ctx.move_to(t.0, t.1, 5_000, 0.8).await;
        }
    }
}

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let peer = Some(index ^ 1).filter(|&j| j < ctx.player_count());
    let slot = global % 2;
    let side = if slot == 0 { 1.0 } else { -1.0 };
    let side_key = if slot == 0 {
        WalkDirection::Left
    } else {
        WalkDirection::Right
    };
    let anchor = ctx.anchor();
    let (ax, az) = axis(ctx);
    let station = (
        anchor.0 + ax * side * STATION_HALF,
        anchor.1 + az * side * STATION_HALF,
    );
    let opening = face_peer(ctx, peer, (anchor.0, 64.0, anchor.1)).await;
    travel_to(ctx, station, TRAVEL_HOP, 1.5).await;
    let start = ctx.position().unwrap_or((station.0, 64.0, station.1));
    let focus = (anchor.0, start.1 + 1.5, anchor.1);
    ctx.record("observe", &format!("vis-duet-calibration-setup-{index}"), "Open on the peer before the alternating cairn calibration.", json!({
        "global_index": global, "role": if slot == 0 { "setter" } else { "reader" },
        "peer": peer, "opening_confirmed": opening, "station": {"x": r2(station.0), "z": r2(station.1)},
    }));

    let mut last_round = i64::MIN;
    let mut layer = 0i64;
    while !ctx.target_met() {
        let round = ctx.engine().shared.phase_at(ROUND_MS);
        let acting = (round + global as i64).rem_euclid(2) == 0;
        if round != last_round {
            last_round = round;
            ctx.record(
                TYPE_MOVE,
                &format!("vis-duet-calibration-round-{index}-{round}"),
                if acting {
                    "Step in to calibrate the shared cairn."
                } else {
                    "Watch the peer calibrate, then measure the result."
                },
                json!({"round": round, "acting": acting, "layer": layer}),
            );
        }
        let work = (
            anchor.0 + ax * side * APPROACH,
            anchor.1 + az * side * APPROACH,
        );
        if acting {
            ctx.move_to(work.0, work.1, 6_000, 1.5).await;
            let feet = ctx
                .position()
                .map(|p| p.1.round() as i64)
                .unwrap_or(start.1.round() as i64);
            let desired = (
                anchor.0.round() as i64,
                feet + layer,
                anchor.1.round() as i64,
            );
            let cell = marker_base_near(ctx, desired, 1);
            let equipped = ctx.equip(MATERIALS[slot]).await;
            let placed = match cell {
                Some(c) if equipped => ctx.place_block(c, MATERIALS[slot]).await,
                _ => false,
            };
            if placed {
                layer = (layer + 1) % CAIRN_HEIGHT;
            }
            ctx.record(
                TYPE_PLACE,
                &format!("vis-duet-calibration-place-{index}-{round}"),
                "Add one calibration layer while the peer watches.",
                json!({
                    "round": round, "placed": placed, "equipped": equipped, "layer": layer,
                    "cell": cell.map(|c| json!({"x": c.x, "y": c.y, "z": c.z})),
                }),
            );
            face_peer(ctx, peer, focus).await;
            let leg_ms = vary(ctx, 850.0, 1_150.0) as u64;
            frame_leg(ctx, peer, side_key, leg_ms).await;
        } else {
            face_peer(ctx, peer, focus).await;
            let leg_ms = vary(ctx, 900.0, 1_350.0) as u64;
            frame_leg(ctx, peer, side_key, leg_ms).await;
            let observed = area_find_block(
                ctx,
                (anchor.0.round() as i32, anchor.1.round() as i32),
                (start.1.floor() as i32 - 2, start.1.floor() as i32 + 4),
                MATERIALS[1 - slot],
            );
            ctx.record(
                "observe",
                &format!("vis-duet-calibration-read-{index}-{round}"),
                "Measure the layer the peer just added.",
                json!({
                    "round": round, "peer_edit_visible": observed.is_some(),
                    "observed_cell": observed.map(|c| json!({"x": c.x, "y": c.y, "z": c.z})),
                }),
            );
            witness_peer_edits(ctx).await;
        }
        sleep_ms(vary(ctx, 200.0, 650.0) as u64).await;
    }
}
