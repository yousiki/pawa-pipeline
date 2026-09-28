//! CATALOG: family=construction min_players=2 single_process=true world_edits=true scenery=none far_players=false
//!
//! Two players lay a causeway toward each other from opposite ends of one span, a cell per beat,
//! until the two halves nearly meet — then they dig it back out to the ends and start again.
//!
//! The structure lies ON the pair's sight line, which is what this scenario contributes to the revisit problem:
//! each bot's frame contains its own cursor, the partner's cursor, and the partner. To know where to
//! put the next cell you have to see how far the other side has got, so the shot the script needs and
//! the shot the task needs are the same shot — there is no beat in which losing the teammate would be
//! harmless. Each beat also PROBES the far half and records the mismatch, so the footage carries a
//! measured claim about what was visible rather than an assumed one.
//!
//! The trajectory loop is the closure-and-reopening: every cell is walked to twice, once to lay it
//! and once to take it back, and each bot's cursor sweeps its own half of the span repeatedly, so path
//! length is a large multiple of net displacement. The gaze loop is per cell: off the partner onto the
//! cursor cell, back onto the partner.
//!
//! Not `duet_face_off_wall`: that wall runs ACROSS the sight line and the two builders work its two
//! faces from fixed stations. Here the structure runs ALONG the sight line and both builders travel
//! its length. Not `column_relay_wall`: no crews, no observation arc, and the reversal (digging the
//! span back out) is half the script rather than an afterthought.

use super::*;

/// One material per side, so the two halves of the span are visibly two hands' work and the seam
/// where they meet is readable in a single frame.
const SIDE_MATERIALS: [&str; 2] = ["minecraft:oak_planks", "minecraft:stone_bricks"];

/// Kit block ids as `block_id_at` reports them (no namespace). Both the dig beat and the far-half
/// probe are gated on this list: digging anything else would open natural terrain, whose occlusion should
/// stay intact.
const KIT_IDS: [&str; 6] = [
    "stone_bricks",
    "oak_planks",
    "oak_log",
    "cobblestone",
    "bricks",
    "stone",
];

/// Outermost cell of each half, in blocks from the middle. Eight cells per side gives a 16-block span
/// — long enough that a full sweep takes most of an episode, short enough that the partner is 8-19
/// blocks away and always legible in frame.
const SPAN_HALF: i64 = 8;

/// Innermost cell the closing half stops at. The builder stands ~1.5 blocks behind its cursor, so
/// stopping at 3 leaves the two bodies 9 blocks apart — clear of [`MIN_PEER_GAP`] with room for
/// pathfinder overshoot. The two sides use 3 and 4 so their cursors are never exact mirrors of each
/// other at the same moment, which is the structural half of the near-clones defence.
const CLOSE_STOP: i64 = 3;

/// How far behind its cursor cell a builder stands: inside placement reach, outside the cell itself.
const CURSOR_STAND: f64 = 1.5;

/// Tolerance for "the partner is in frame" — well inside the 51 deg half-FOV.
const PEER_TOL_DEG: f64 = 8.0;

/// Shortest movement key press. The key-chatter gate fails an episode when over 35% of movement
/// presses run under 10 ticks (0.5 s); 700 ms leaves margin for the 50 ms control tick.
const MIN_PRESS_MS: u64 = 700;

/// Gap below which a framing leg must open the pair up, and above which it must close in.
const GAP_TIGHT: f64 = 6.5;
const GAP_LOOSE: f64 = 20.0;

/// Draw in `[lo, hi)` from the per-bot stream, so a number this scenario could have hard-coded varies
/// per bot and per beat and still replays from the seed.
fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

/// The span's bearing, agreed with NO messaging: the engine hands every bot the same
/// `native_waypoints` list, so its first entry is a direction both sides derive identically.
fn shared_axis(ctx: &Ctx) -> (f64, f64) {
    let waypoint = ctx
        .engine()
        .shared
        .native_waypoints
        .first()
        .copied()
        .unwrap_or((1.0, 0.0));
    let span = waypoint.0.hypot(waypoint.1).max(0.001);
    // Keep the two roles of each pair aligned while rotating each pair's workspace for N=4+.
    let angle = ctx.index().saturating_div(2) as f64 * 0.83;
    let (sin, cos) = angle.sin_cos();
    (
        waypoint.0 / span * cos - waypoint.1 / span * sin,
        waypoint.0 / span * sin + waypoint.1 / span * cos,
    )
}

/// 2D distance to the partner, or `None` when its position has not replicated yet.
fn peer_gap(ctx: &Ctx, partner: Option<usize>) -> Option<f64> {
    let own = ctx.position()?;
    let peer = ctx.peer_position(partner?)?;
    Some((peer.0 - own.0).hypot(peer.2 - own.2))
}

/// Turn onto the partner's head and CONFIRM the view arrived. `look` only waits out a deadline, so a
/// wide turn that timed out would be recorded as a re-acquire that never happened.
async fn face_partner(ctx: &Ctx, partner: Option<usize>, fallback: (f64, f64, f64)) -> bool {
    let target = partner
        .and_then(|j| ctx.peer_position(j))
        .map(head_of)
        .unwrap_or(fallback);
    ctx.look_until_facing(target, PEER_TOL_DEG, 4_000).await
}

/// One leg of travel that leaves the partner where the camera already put it. `hold_visual` pins the
/// camera (`CameraIntent::LookAt`) and drives one key with no pathfinder waypoint, so the view is
/// still for the whole leg — which keeps the beat off the camera/travel-independence gate, where a
/// tracked look while walking measured 96-100% agreement. Direction comes from the LIVE gap.
async fn framing_leg(
    ctx: &mut Ctx,
    partner: Option<usize>,
    side_key: WalkDirection,
    ms: u64,
) -> bool {
    let dir = match peer_gap(ctx, partner) {
        Some(gap) if gap < GAP_TIGHT => WalkDirection::Backward,
        Some(gap) if gap > GAP_LOOSE => WalkDirection::Forward,
        _ => side_key,
    };
    if ctx.hold_visual(dir, false, ms.max(MIN_PRESS_MS), 0).await {
        return true;
    }
    // The stall guard cut the press against terrain. Re-pressing is what turned every step into a
    // sub-0.5 s press in `mirror_dance`'s round-2 key-chatter failure (37% against a 35% ceiling), so
    // bail to ONE pathfinder leg toward solvable ground instead.
    if let Some(own) = ctx.position() {
        let bearing = vary(ctx, 0.0, std::f64::consts::TAU);
        let desired = (own.0 + bearing.cos() * 4.0, own.2 + bearing.sin() * 4.0);
        if let Some(target) = walkable_xz_near(ctx, desired, own.1.floor() as i32) {
            ctx.move_to(target.0, target.1, 5_000, 0.8).await;
        }
    }
    false
}

/// Turn PAST the field of view, spend one leg there, then turn back onto the partner and confirm it.
///
/// The `net_over_sweep` beat. `away_deg` exceeds the 102.5 deg horizontal FOV so the away view shares
/// no pixels with the partner view, and away-plus-back fits inside one 4.65 s training window
/// (2 x ~110 deg at the configured 72 deg/s is ~3.1 s, plus one 0.7-0.9 s leg).
async fn away_and_back(
    ctx: &mut Ctx,
    partner: Option<usize>,
    fallback: (f64, f64, f64),
    away_deg: f64,
    side_key: WalkDirection,
) {
    let Some(own) = ctx.position() else { return };
    let target = partner
        .and_then(|j| ctx.peer_position(j))
        .map(head_of)
        .unwrap_or(fallback);
    let base_yaw = look_angles(head_of(own), target).0;
    let away = point_at_yaw(head_of(own), base_yaw + away_deg, 24.0);
    let left = ctx.look_until_facing(away, PEER_TOL_DEG, 5_000).await;
    // Strafe, not forward/back: with the camera 110 deg off the partner those keys walk the pair
    // together or apart along an unpredictable bearing, while a strafe leaves the gap roughly alone.
    let dwell_ms = vary(ctx, 700.0, 950.0) as u64;
    ctx.hold_visual(side_key, false, dwell_ms.max(MIN_PRESS_MS), 0)
        .await;
    let returned = face_partner(ctx, partner, fallback).await;
    ctx.record(
        TYPE_LOOK,
        &format!("vis-duet-span-away-{}-{}", ctx.index(), ctx.elapsed_ms()),
        "Turn off the span past the field of view, then confirm the return onto the partner.",
        json!({
            "away_deg": r2(away_deg),
            "exceeds_horizontal_fov": true,
            "left_view": left,
            "return_confirmed": returned,
        }),
    );
}

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let bot_count = ctx.player_count().max(1);
    let partner = Some(index ^ 1).filter(|&j| j < bot_count);
    let slot = global % 2;
    let side = if slot == 0 { 1.0 } else { -1.0 };
    let material = SIDE_MATERIALS[slot];
    // Opposite strafe keys per side: the two halves are worked SIMULTANEOUSLY, so their key streams
    // have to differ by construction, not by jitter (jitter alone measured 80.2% pair agreement
    // against the 80% near-clones ceiling in `eval_shared_unobserved`).
    let side_key = if slot == 0 {
        WalkDirection::Left
    } else {
        WalkDirection::Right
    };
    let away_deg = 110.0 + slot as f64 * 20.0;
    // Asymmetric stopping cells, so the two cursors are never exact mirrors at the same moment.
    let stop = CLOSE_STOP + slot as i64;

    let anchor = ctx.anchor();
    let axis = shared_axis(ctx);
    let cell_xz = |cells: i64| {
        (
            anchor.0 + axis.0 * side * cells as f64,
            anchor.1 + axis.1 * side * cells as f64,
        )
    };

    // The opening shot, before any travel: at `spread_range: 8` the partner is within a dozen blocks
    // at spawn, so the first recorded view already contains the teammate.
    let opening = face_partner(ctx, partner, (anchor.0, 64.0, anchor.1)).await;
    let far_end = cell_xz(SPAN_HALF + 1);
    travel_to(ctx, far_end, TRAVEL_HOP, 1.5).await;
    let start = ctx.position().unwrap_or((far_end.0, 64.0, far_end.1));
    ctx.record(
        "observe",
        &format!("vis-duet-span-setup-{index}"),
        "Take the far end of this half of the span, facing the partner down its length.",
        json!({
            "global_index": global,
            "role": "span_closer",
            "partner": partner,
            "slot": slot,
            "material": material,
            "span_half": SPAN_HALF,
            "close_stop": stop,
            "opening_shot_confirmed": opening,
        }),
    );

    let mut cursor = SPAN_HALF;
    let mut closing = true;
    while !ctx.target_met() {
        let target_cell = cell_xz(cursor);
        let stand = (
            target_cell.0 + axis.0 * side * CURSOR_STAND,
            target_cell.1 + axis.1 * side * CURSOR_STAND,
        );
        ctx.move_to(stand.0, stand.1, 6_000, 1.5).await;
        let feet_y = ctx.position().map(|p| p.1).unwrap_or(start.1).round() as i32;
        let exact = BlockPos::new(
            target_cell.0.round() as i32,
            feet_y,
            target_cell.1.round() as i32,
        );

        // Read the FAR half before touching this one: how many of the partner's cells are already
        // standing. This is the alignment the script is named for, and it is genuine observation —
        // block probes over the same neighbourhood a placer could have used, no shared state.
        let peer_cells = (stop..=SPAN_HALF)
            .filter(|&c| {
                let far = (
                    anchor.0 - axis.0 * side * c as f64,
                    anchor.1 - axis.1 * side * c as f64,
                );
                area_find_block(
                    ctx,
                    (far.0.round() as i32, far.1.round() as i32),
                    (feet_y - 2, feet_y + 2),
                    SIDE_MATERIALS[1 - slot],
                )
                .is_some()
            })
            .count();

        let mut changed = false;
        let mut observed = ctx.block_id_at(exact);
        if closing {
            // Lay this cell. `marker_base_near` resolves the placeable cell per column rather than
            // trusting a fixed Y: on natural terrain the ground under cell 8 is not the ground under
            // cell 3, and it also refuses cells the placer's own body occupies.
            if block_is_air(ctx, exact) {
                let base =
                    marker_base_near(ctx, (exact.x as i64, exact.y as i64, exact.z as i64), 1);
                let equipped = ctx.equip(material).await;
                // `place_block` runs its own confirmed look at the target face, so the camera leaves
                // the partner here on purpose — the outbound half of the gaze loop.
                changed = match base {
                    Some(cell) if equipped => ctx.place_block(cell, material).await,
                    _ => false,
                };
                observed = ctx.block_id_at(exact);
            }
        } else {
            // Take it back out — but only if it is a kit block this pair laid. An ungated dig would
            // punch holes in the natural terrain the rest of the corpus needs intact.
            let is_kit = observed.as_deref().is_some_and(|id| KIT_IDS.contains(&id));
            if is_kit {
                changed = ctx.dig(exact).await;
                observed = ctx.block_id_at(exact);
            }
        }
        ctx.record(
            if closing { TYPE_PLACE } else { TYPE_MINE },
            &format!("vis-duet-span-cell-{index}-{cursor}-{}", ctx.elapsed_ms()),
            if closing {
                "Lay the next cell of this half of the span, toward the partner."
            } else {
                "Take one cell of this half back out, away from the partner."
            },
            json!({
                "cursor": cursor,
                "closing": closing,
                "material": material,
                "changed": changed,
                "observed": observed,
                "cell": {"x": exact.x, "y": exact.y, "z": exact.z},
                // The alignment reading: how far the OTHER half has got, probed not assumed.
                "peer_half_cells": peer_cells,
                "own_half_target": SPAN_HALF - stop + 1,
            }),
        );

        // Back onto the partner, then walk under a pinned view: the return halves of the gaze loop
        // and of this beat's travel are the same beat, so the teammate is framed while the body moves.
        face_partner(ctx, partner, (anchor.0, start.1 + 1.0, anchor.1)).await;
        let leg_ms = vary(ctx, 800.0, 1_150.0) as u64;
        framing_leg(ctx, partner, side_key, leg_ms).await;
        witness_peer_edits(ctx).await;

        // Reverse at the ends. The reversal is also where the away-and-back reversal beat goes: once
        // per sweep is often enough that most windows contain one, rare enough that the span still
        // gets built.
        if closing {
            cursor -= 1;
            if cursor < stop {
                cursor = stop;
                closing = false;
                away_and_back(
                    ctx,
                    partner,
                    (anchor.0, start.1 + 1.0, anchor.1),
                    away_deg,
                    side_key,
                )
                .await;
            }
        } else {
            cursor += 1;
            if cursor > SPAN_HALF {
                cursor = SPAN_HALF;
                closing = true;
                away_and_back(
                    ctx,
                    partner,
                    (anchor.0, start.1 + 1.0, anchor.1),
                    away_deg,
                    side_key,
                )
                .await;
            }
        }
        // Per-bot pause so the two halves never step on the same frame.
        sleep_ms(vary(ctx, 150.0, 600.0) as u64).await;
    }
}
