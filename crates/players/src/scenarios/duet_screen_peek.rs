//! CATALOG: family=group_motion min_players=2 single_process=true world_edits=true scenery=none far_players=false
//!
//! Two players hold each other in frame across a free-standing screen and slide along opposite arcs
//! around it, so the partner is briefly hidden behind the screen and then recovered, over and over.
//!
//! This is the OCCLUSION face of the revisit problem (occlusion is a feature, not a defect). Every other
//! duet in this family keeps the teammate continuously visible, which is the right default — 22.4% of
//! v5's multi-player windows never showed the teammate in the context frame at all. But a model that
//! only ever sees uninterrupted bodies has no reason to learn that a player behind an obstacle still
//! exists. Here the teammate is visible in the context frame, disappears behind a known, visible
//! occluder, and comes back on the far side: the disappearance has a cause the frame contains, which
//! is exactly what makes it learnable instead of noise.
//!
//! The occlusion intervals are kept SHORT on purpose — a leg at a time, roughly a second — so the
//! episode is overwhelmingly "teammate visible" with brief, explained interruptions.
//!
//! The gaze loop is per beat: confirmed re-acquire of the partner, one leg with the view pinned, a
//! re-acquire. The trajectory loop is the arc shuttle, reversed by the same closed-loop band
//! controller `duet_twin_sightline` uses, so each bot sweeps back and forth across the screen's
//! shadow rather than orbiting away.
//!
//! Not `group_orbit` or `totem_orbit_film`: those track their subject WHILE walking, which is the
//! documented 96-100% failure of the camera/travel-independence gate. Every leg here runs with the
//! camera pinned, and every turn happens standing.

use super::*;

const MATERIAL: &str = "minecraft:stone_bricks";

/// The screen: `SCREEN_WIDTH` columns across the pair's sight line, `SCREEN_HEIGHT` tall.
///
/// A one-column pillar barely occludes a player at all — from 5.5 blocks out it subtends 10 deg and
/// the sight line slips past it inside a third of a leg — so the width is what decides how long the
/// shadow holds: three columns subtend ~31 deg, about one leg of arc. Two layers is the minimum that
/// covers a standing body's eye (1.62 blocks above its feet) rather than only its legs. Six cells
/// total, and the build costs roughly a third of an episode at one cell per beat, which is why it is
/// not made larger.
const SCREEN_WIDTH: i64 = 3;
const SCREEN_HEIGHT: i64 = 2;

/// Arc radius per side. Different radii mean the same leg length is a different arc angle, so the two
/// bots cross the screen's shadow at different times and their key streams separate structurally
/// rather than by jitter (jitter alone measured 80.2% pair agreement against the 80% near-clones
/// ceiling in `eval_shared_unobserved`).
const RADII: [f64; 2] = [5.5, 7.0];

/// Band the pair's angular separation (as seen from the screen) is steered back into. Centred on
/// 180 deg, where the screen sits squarely between the two cameras: that is the configuration whose
/// sight line the screen can actually interrupt. The band is wide enough that each bot spends most of
/// its legs OFF the shadow and only sweeps through it.
const MIN_SEP_DEG: f64 = 120.0;
const MAX_SEP_DEG: f64 = 240.0;

/// Tolerance for "the partner is in frame" — well inside the 51 deg half-FOV.
const PEER_TOL_DEG: f64 = 8.0;

/// One arc leg. 850 ms clears the key-chatter floor (over 35% of movement presses shorter than 10
/// ticks = 0.5 s fails the episode) with margin for the 50 ms control tick, and at walking pace covers
/// ~3.7 blocks — about 38 deg of arc at radius 5.5, so a crossing of the screen's shadow takes about
/// one leg.
const LEG_MS: u64 = 1_400;
const MIN_PRESS_MS: u64 = 700;

/// Draw in `[lo, hi)` from the per-bot stream, so a number this scenario could have hard-coded varies
/// per bot and per beat and still replays from the seed.
fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

/// The screen's bearing, agreed with NO messaging: the engine hands every bot the same
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

/// Signed area of the triangle `a b c`, i.e. which side of `a -> b` the point `c` falls on.
fn orient(a: (f64, f64), b: (f64, f64), c: (f64, f64)) -> f64 {
    (b.0 - a.0) * (c.1 - a.1) - (b.1 - a.1) * (c.0 - a.0)
}

/// Whether the open segments `a b` and `c d` cross in the horizontal plane.
///
/// Hand-derived geometry, pinned by a test below: this predicate is the scenario's only claim about
/// whether the teammate was occluded, and a sign error in it would label every frame wrongly while
/// the footage looked fine. Collinear and touching cases are reported as NOT crossing — the screen's
/// shadow is a solid band several degrees wide, so a grazing sight line is not what this measures.
fn segments_cross(a: (f64, f64), b: (f64, f64), c: (f64, f64), d: (f64, f64)) -> bool {
    let (d1, d2) = (orient(a, b, c), orient(a, b, d));
    let (d3, d4) = (orient(c, d, a), orient(c, d, b));
    ((d1 > 0.0) != (d2 > 0.0)) && ((d3 > 0.0) != (d4 > 0.0)) && d1 != 0.0 && d2 != 0.0
}

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let bot_count = ctx.player_count().max(1);
    let partner = Some(index ^ 1).filter(|&j| j < bot_count);
    let slot = global % 2;
    let radius = RADII[slot];
    // Opposite slide directions, so the pair sweeps into and out of the shadow out of phase.
    let mut key = if slot == 0 {
        WalkDirection::Left
    } else {
        WalkDirection::Right
    };

    let anchor = ctx.anchor();
    let axis = shared_axis(ctx);
    // The screen stands ACROSS the sight line: the axis is the line between the two arcs' opening
    // stations, the perpendicular is the screen's face.
    let face = (-axis.1, axis.0);
    let base_angle = axis.1.atan2(axis.0);
    // Opening stations 180 deg apart, so the screen starts squarely between the two cameras.
    let station_angle = base_angle + slot as f64 * std::f64::consts::PI;

    // The opening shot, before any travel: at `spread_range: 8` the partner is within a dozen blocks
    // at spawn, so the first recorded view already contains the teammate — unoccluded, because the
    // screen has not been built yet.
    let opening = match partner.and_then(|j| ctx.peer_position(j)).map(head_of) {
        Some(target) => ctx.look_until_facing(target, PEER_TOL_DEG, 4_000).await,
        None => false,
    };

    let station = (
        anchor.0 + station_angle.cos() * radius,
        anchor.1 + station_angle.sin() * radius,
    );
    travel_to(ctx, station, TRAVEL_HOP, 1.5).await;
    let start = ctx.position().unwrap_or((station.0, 64.0, station.1));
    // The screen's footprint as a 2D segment, shared by both bots because both derive it from the
    // anchor and the same axis. Half a block of slack at each end so the segment matches the block
    // cells rather than their centres.
    let half = SCREEN_WIDTH as f64 / 2.0;
    let screen_ends = (
        (anchor.0 - face.0 * half, anchor.1 - face.1 * half),
        (anchor.0 + face.0 * half, anchor.1 + face.1 * half),
    );
    ctx.record(
        "observe",
        &format!("vis-duet-peek-setup-{index}"),
        "Take an arc station opposite the partner, with the screen's line between them.",
        json!({
            "global_index": global,
            "role": "screen_peek",
            "partner": partner,
            "slot": slot,
            "radius": r2(radius),
            "screen_width": SCREEN_WIDTH,
            "screen_height": SCREEN_HEIGHT,
            "opening_shot_confirmed": opening,
        }),
    );

    // Slot 0 raises the screen ONE BLOCK PER BEAT rather than all six at once. Two reasons: a
    // six-placement burst is a long stretch of standing still, which is what pushed a builder under
    // the per-POV participation floor in `eval_shared_unobserved`; and a screen that grows during the
    // episode gives the recording both the unoccluded early windows and the occluded later ones,
    // with the occluder's construction visible in between.
    let mut screen_cursor = 0i64;
    let screen_cells = SCREEN_WIDTH * SCREEN_HEIGHT;
    let mut beat = 0u64;
    while !ctx.target_met() {
        // Wide turns happen HERE, standing: the camera/travel gate only weighs frames where the
        // camera turned while the body was moving, so a confirmed re-acquire between legs is free.
        let framed = match partner.and_then(|j| ctx.peer_position(j)).map(head_of) {
            Some(target) => ctx.look_until_facing(target, PEER_TOL_DEG, 4_000).await,
            None => false,
        };

        // Does a screen actually stand yet? PROBED, not counted: slot 1 never places a block, so its
        // own cursor would say "no screen" while it stares straight at one. An occlusion label that
        // came out true for the builder and false for the watcher looking at the same wall would be
        // worse than no label. `area_find_block` sweeps the same small neighbourhood `marker_base_near`
        // may have shifted a cell into, so the two sides agree.
        let feet_y = ctx.position().map(|p| p.1).unwrap_or(start.1).round() as i32;
        let screen_standing = area_find_block(
            ctx,
            (anchor.0.round() as i32, anchor.1.round() as i32),
            (feet_y - 2, feet_y + 3),
            MATERIAL,
        )
        .is_some();

        // Is the sight line to the partner crossing the screen right now? Measured, not assumed: this
        // is the label that makes an occluded frame distinguishable from a lost teammate.
        let occluded = match (ctx.position(), partner.and_then(|j| ctx.peer_position(j))) {
            (Some(own), Some(peer)) if screen_standing => segments_cross(
                (own.0, own.2),
                (peer.0, peer.2),
                screen_ends.0,
                screen_ends.1,
            ),
            _ => false,
        };

        // One block of screen per beat, by slot 0 only, and only while the partner is NOT hidden —
        // so the placement itself is always witnessable from the other camera.
        if slot == 0 && screen_cursor < screen_cells && !occluded {
            let column = screen_cursor / SCREEN_HEIGHT;
            let layer = screen_cursor % SCREEN_HEIGHT;
            let offset = column as f64 - (SCREEN_WIDTH - 1) as f64 / 2.0;
            let column_xz = (anchor.0 + face.0 * offset, anchor.1 + face.1 * offset);
            // Stand OFF the column, on this bot's own side of it, rather than aiming `move_to` at
            // the cell: a builder standing in its own target cell makes `marker_base_near` shift the
            // block sideways, which would scatter the screen instead of building it. 2.0 blocks out
            // keeps both rows inside the 4.0-block default `place_reach` even at the far edge of
            // `move_to`'s 1.5-block arrival floor. Walking in and back out is also a there-and-back
            // trip rather than an excursion that leaves the builder parked at the anchor.
            let build_stand = (
                column_xz.0 + station_angle.cos() * 2.0,
                column_xz.1 + station_angle.sin() * 2.0,
            );
            ctx.move_to(build_stand.0, build_stand.1, 6_000, 1.5).await;
            let ground = ctx.position().map(|p| p.1).unwrap_or(start.1).round() as i64;
            let desired = (
                column_xz.0.round() as i64,
                ground + layer,
                column_xz.1.round() as i64,
            );
            let cell = marker_base_near(ctx, desired, 1);
            let equipped = ctx.equip(MATERIAL).await;
            // `place_block` runs its own confirmed look at the target face, so the camera leaves the
            // partner here on purpose.
            let placed = match cell {
                Some(cell) if equipped => ctx.place_block(cell, MATERIAL).await,
                _ => false,
            };
            // Advance regardless: a cell the terrain will not accept must not stall the whole screen.
            screen_cursor += 1;
            ctx.record(
                TYPE_PLACE,
                &format!("vis-duet-peek-screen-{index}-{screen_cursor}"),
                "Add one block to the screen the pair will peek around.",
                json!({
                    "cell_index": screen_cursor,
                    "cells_total": screen_cells,
                    "placed": placed,
                    "equipped": equipped,
                    "cell": cell.map(|c| json!({"x": c.x, "y": c.y, "z": c.z})),
                }),
            );
            let back = (
                anchor.0 + station_angle.cos() * radius,
                anchor.1 + station_angle.sin() * radius,
            );
            ctx.move_to(back.0, back.1, 6_000, 1.5).await;
            continue;
        }

        let before = separation_deg(ctx, partner, anchor);
        // The arc leg. `hold_visual` pins the camera (`CameraIntent::LookAt`) and drives one key with
        // no pathfinder waypoint, so the partner stays where the re-acquire put it — and when the
        // screen slides in front of it, the disappearance is the SCREEN's doing rather than a camera
        // turn. That distinction is the whole point of the scenario.
        let leg_ms = (LEG_MS as f64 * vary(ctx, 0.9, 1.2)) as u64;
        let ran = ctx.hold_visual(key, false, leg_ms, 0).await;
        let after = separation_deg(ctx, partner, anchor);

        // Closed-loop band controller rather than deriving which strafe key runs which way around the
        // arc from the yaw convention: measure the separation across the leg and reverse when it left
        // the band in the direction the leg was pushing it. This is also what makes the arc a shuttle
        // that keeps re-crossing the screen's shadow instead of a one-way orbit.
        if let (Some(before), Some(after)) = (before, after) {
            let widening = after > before;
            if (after > MAX_SEP_DEG && widening) || (after < MIN_SEP_DEG && !widening) {
                key = match key {
                    WalkDirection::Left => WalkDirection::Right,
                    _ => WalkDirection::Left,
                };
            }
        }
        if !ran {
            // The stall guard cut the press against terrain. Re-pressing is what turned every step
            // into a sub-0.5 s press in `mirror_dance`'s round-2 key-chatter failure (37% against a
            // 35% ceiling), so recover the arc with ONE pathfinder leg instead.
            let want = (
                anchor.0 + station_angle.cos() * radius,
                anchor.1 + station_angle.sin() * radius,
            );
            let feet_y = ctx.position().map(|p| p.1).unwrap_or(start.1).floor() as i32;
            let target = walkable_xz_near(ctx, want, feet_y).unwrap_or(want);
            ctx.move_to(target.0, target.1, 6_000, 0.8).await;
            key = match key {
                WalkDirection::Left => WalkDirection::Right,
                _ => WalkDirection::Left,
            };
        }

        ctx.record(
            TYPE_MOVE,
            &format!("vis-duet-peek-beat-{index}-{beat}"),
            "Slide one leg around the screen, holding the partner in frame or losing it behind it.",
            json!({
                "beat": beat,
                "slot": slot,
                "peer_framed": framed,
                "peer_occluded_by_screen": occluded,
                "screen_standing": screen_standing,
                "separation_deg": after.map(r2),
                "screen_cells_placed": screen_cursor,
                "leg_completed": ran,
            }),
        );

        // Every fourth beat, the revisit reversal: turn past the field of view, spend a leg there,
        // then come back onto the partner with the return CONFIRMED. Away-and-back is ~3.1 s of turn
        // at the configured 72 deg/s plus one leg, so the reversal fits inside a single 4.65 s
        // training window — the only way a window's `net_over_sweep` can come out low.
        if beat.is_multiple_of(4)
            && let Some(own) = ctx.position()
            && let Some(peer) = partner.and_then(|j| ctx.peer_position(j))
        {
            let base_yaw = look_angles(head_of(own), head_of(peer)).0;
            let away_deg = 110.0 + slot as f64 * 20.0;
            let away = point_at_yaw(head_of(own), base_yaw + away_deg, 24.0);
            let left = ctx.look_until_facing(away, PEER_TOL_DEG, 5_000).await;
            ctx.hold_visual(key, false, LEG_MS.max(MIN_PRESS_MS), 0)
                .await;
            let returned = match partner.and_then(|j| ctx.peer_position(j)).map(head_of) {
                Some(target) => ctx.look_until_facing(target, PEER_TOL_DEG, 5_000).await,
                None => false,
            };
            witness_peer_edits(ctx).await;
            ctx.record(
                TYPE_LOOK,
                &format!("vis-duet-peek-away-{index}-{beat}"),
                "Turn off the partner past the field of view, then confirm the return onto it.",
                json!({
                    "beat": beat,
                    "away_deg": r2(away_deg),
                    "exceeds_horizontal_fov": true,
                    "left_view": left,
                    "return_confirmed": returned,
                }),
            );
        }
        beat += 1;
        // Per-bot pause so the two arcs never step on the same frame.
        sleep_ms(vary(ctx, 150.0, 550.0) as u64).await;
    }
}

/// Angular separation between the two bots as seen from the screen, in degrees over the FULL turn
/// (0..360) rather than folded to 0..180.
///
/// The fold is deliberately absent. This scenario's band is centred on 180 deg — the configuration
/// where the screen sits squarely between the two cameras — and folding to 0..180 makes "swung just
/// past the screen" and "not yet at the screen" the same number, which would leave the band
/// controller unable to tell which way its last leg pushed the pair. Each bot computes the
/// complement of the other's reading (`360 - d`), and the band is symmetric about 180, so both sides
/// steer to the same configuration without exchanging anything.
fn separation_deg(ctx: &Ctx, partner: Option<usize>, screen: (f64, f64)) -> Option<f64> {
    let own = ctx.position()?;
    let peer = ctx.peer_position(partner?)?;
    let a = (own.2 - screen.1).atan2(own.0 - screen.0);
    let b = (peer.2 - screen.1).atan2(peer.0 - screen.0);
    Some((a - b).to_degrees().rem_euclid(360.0))
}

#[cfg(test)]
mod tests {
    use super::segments_cross;

    /// The occlusion label is only as good as this predicate, and a sign error in `orient` would flip
    /// every label while the footage still looked correct. Pin the four cases that matter: a sight
    /// line straight through the screen, one clearly past its end, one parallel to it, and one whose
    /// INFINITE line would cross but whose segment stops short.
    #[test]
    fn a_sight_line_crosses_the_screen_only_when_it_really_does() {
        let screen = ((-1.5, 0.0), (1.5, 0.0));
        // Two bots on opposite sides, screen between them.
        assert!(segments_cross((0.0, -5.0), (0.0, 5.0), screen.0, screen.1));
        // Same geometry, slid past the screen's end.
        assert!(!segments_cross((4.0, -5.0), (4.0, 5.0), screen.0, screen.1));
        // Both bots on the same side: the screen is not between them at all.
        assert!(!segments_cross((0.0, 2.0), (3.0, 5.0), screen.0, screen.1));
        // Parallel to the screen.
        assert!(!segments_cross((-5.0, 1.0), (5.0, 1.0), screen.0, screen.1));
        // Crossing at a diagonal, still through the screen.
        assert!(segments_cross((-3.0, -3.0), (3.0, 3.0), screen.0, screen.1));
    }
}
