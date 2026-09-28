//! CATALOG: family=construction min_players=2 single_process=true world_edits=true scenery=none far_players=false
//!
//! Two players recover the same landmark from opposite reveal sides of a short screen. Only one peeks
//! during each shared beat, so the event stream records who saw the landmark first and who followed;
//! the next beat reverses the order. This addresses the revisit problem's temporal handoff face: both players are
//! grounded in the opening frame, the occluder is known, and the return view is a confirmed re-acquisition.
//!
//! The revealer travels shadow-to-side-to-shadow; the waiting player makes an unequal pinned-camera
//! shuttle instead of standing. Opposite sides, reversed order, and different leg durations prevent
//! near-clones. Placement probes real terrain and may fail or shift; the script records those outcomes
//! and still performs the reveal route. All large turns happen while stopped with `look_until_facing`.

use super::*;

const SUBJECT_MATERIAL: &str = "minecraft:bricks";
const SCREEN_MATERIAL: &str = "minecraft:stone_bricks";
const SUBJECT_HEIGHT: i64 = 3;
const SCREEN_HEIGHT: i64 = 4;
const HOME_DEPTH: f64 = 6.0;
const REVEAL_SIDE: f64 = 4.0;
const ROLE_MS: f64 = 8_000.0;
const FACE_TOL_DEG: f64 = 8.0;
const WAIT_LEG_MS: [u64; 2] = [1_300, 1_500];

fn shared_axis(ctx: &Ctx) -> (f64, f64) {
    let v = ctx
        .engine()
        .shared
        .native_waypoints
        .first()
        .copied()
        .unwrap_or((1.0, 0.0));
    let d = v.0.hypot(v.1).max(0.001);
    // Pair roles remain slot 0/1, but each pair gets its own rotated axis at N=4+ so pairs do
    // not converge on the same homes, screens or workstations. N=2 keeps the original axis.
    let angle = ctx.index().saturating_div(2) as f64 * 0.83;
    let (sin, cos) = angle.sin_cos();
    (v.0 / d * cos - v.1 / d * sin, v.0 / d * sin + v.1 / d * cos)
}

async fn face_peer(ctx: &Ctx, peer: Option<usize>, fallback: (f64, f64, f64)) -> bool {
    let target = peer
        .and_then(|j| ctx.peer_position(j))
        .map(head_of)
        .unwrap_or(fallback);
    ctx.look_until_facing(target, FACE_TOL_DEG, 5_000).await
}

fn screen_on_sightline(
    eye: (f64, f64, f64),
    subject: (f64, f64, f64),
    screen: BlockPos,
    height: i64,
) -> bool {
    let line = (subject.0 - eye.0, subject.1 - eye.1, subject.2 - eye.2);
    let rel = (
        screen.x as f64 + 0.5 - eye.0,
        screen.y as f64 + 0.5 - eye.1,
        screen.z as f64 + 0.5 - eye.2,
    );
    let len2 = line.0 * line.0 + line.1 * line.1 + line.2 * line.2;
    if len2 < 0.01 {
        return false;
    }
    let t = (rel.0 * line.0 + rel.1 * line.1 + rel.2 * line.2) / len2;
    if !(0.05..0.95).contains(&t) {
        return false;
    }
    let x = eye.0 + line.0 * t;
    let z = eye.2 + line.2 * t;
    let y = eye.1 + line.1 * t;
    (x - screen.x as f64 - 0.5).abs() <= 1.6
        && (z - screen.z as f64 - 0.5).abs() <= 1.6
        && y >= screen.y as f64
        && y <= screen.y as f64 + height as f64
}

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let slot = global % 2;
    let peer = Some(index ^ 1).filter(|&j| j < ctx.player_count());
    let anchor = ctx.anchor();
    let axis = shared_axis(ctx);
    let perp = (-axis.1, axis.0);
    let side = if slot == 0 { 1.0 } else { -1.0 };

    let opening = face_peer(ctx, peer, (anchor.0, 64.0, anchor.1)).await;
    let raised_subject = if slot == 0 {
        travel_to(
            ctx,
            (anchor.0 + axis.0 * 3.0, anchor.1 + axis.1 * 3.0),
            TRAVEL_HOP,
            2.0,
        )
        .await;
        let y = ctx.position().map(|p| p.1).unwrap_or(64.0).round() as i64;
        raise_landmark(
            ctx,
            (
                (anchor.0 + axis.0 * 3.0).round() as i64,
                y,
                (anchor.1 + axis.1 * 3.0).round() as i64,
            ),
            SUBJECT_HEIGHT,
            SUBJECT_MATERIAL,
        )
        .await
    } else {
        None
    };
    let raised_screen = if slot == 0 {
        let y = ctx.position().map(|p| p.1).unwrap_or(64.0).round() as i64;
        raise_landmark(
            ctx,
            (anchor.0.round() as i64, y, anchor.1.round() as i64),
            SCREEN_HEIGHT,
            SCREEN_MATERIAL,
        )
        .await
    } else {
        None
    };

    let home = (
        anchor.0 - axis.0 * HOME_DEPTH + perp.0 * side * 2.0,
        anchor.1 - axis.1 * HOME_DEPTH + perp.1 * side * 2.0,
    );
    travel_to(ctx, home, TRAVEL_HOP, 1.2).await;
    let start = ctx.position().unwrap_or((home.0, 64.0, home.1));
    let screen_cell = raised_screen.map(|(base, _)| base).or_else(|| {
        area_find_block(
            ctx,
            (anchor.0.round() as i32, anchor.1.round() as i32),
            (
                start.1.round() as i32 - 2,
                start.1.round() as i32 + SCREEN_HEIGHT as i32 + 2,
            ),
            SCREEN_MATERIAL,
        )
    });
    let subject_cell = raised_subject.map(|(base, _)| base).or_else(|| {
        area_find_block(
            ctx,
            (
                (anchor.0 + axis.0 * 3.0).round() as i32,
                (anchor.1 + axis.1 * 3.0).round() as i32,
            ),
            (
                start.1.round() as i32 - 2,
                start.1.round() as i32 + SUBJECT_HEIGHT as i32 + 2,
            ),
            SUBJECT_MATERIAL,
        )
    });
    let subject = raised_subject
        .map(|(_, p)| p)
        .or_else(|| {
            subject_cell.map(|base| {
                (
                    base.x as f64 + 0.5,
                    base.y as f64 + SUBJECT_HEIGHT as f64 - 0.5,
                    base.z as f64 + 0.5,
                )
            })
        })
        .unwrap_or((
            anchor.0 + axis.0 * 3.0,
            start.1 + SUBJECT_HEIGHT as f64 - 0.5,
            anchor.1 + axis.1 * 3.0,
        ));
    let shadow = (
        anchor.0 - axis.0 * 2.5 + perp.0 * side * 0.8,
        anchor.1 - axis.1 * 2.5 + perp.1 * side * 0.8,
    );
    let reveal = (
        shadow.0 + perp.0 * side * REVEAL_SIDE,
        shadow.1 + perp.1 * side * REVEAL_SIDE,
    );
    ctx.record(
        "observe",
        &format!("vis-handoff-reveal-setup-{index}"),
        "Open on the partner before alternating the two sides of the landmark reveal.",
        json!({
            "global_index": global,
            "slot": slot,
            "peer": peer,
            "opening_confirmed": opening,
            "subject_placed_by_this_bot": raised_subject.is_some(),
            "subject_cell": subject_cell.map(|cell| json!({"x": cell.x, "y": cell.y, "z": cell.z})),
            "screen_placed_by_this_bot": raised_screen.is_some(),
            "shadow": {"x": r2(shadow.0), "z": r2(shadow.1)},
            "reveal": {"x": r2(reveal.0), "z": r2(reveal.1)},
        }),
    );

    let mut beat = 0u64;
    while !ctx.target_met() {
        let phase = ctx.engine().shared.phase_at(ROLE_MS);
        // The active role and reveal order are separate: one player reveals in each phase, while
        // phase parity chooses whether that player is first or second across alternating phases.
        let first_slot = phase.rem_euclid(2) as usize;
        let revealer = slot == first_slot;
        if revealer {
            let shadow_in = ctx.move_to(shadow.0, shadow.1, 7_000, 1.0).await;
            let hidden_view = ctx.look_until_facing(subject, FACE_TOL_DEG, 5_000).await;
            let hidden_geometry = screen_cell
                .zip(ctx.position())
                .is_some_and(|(screen, eye)| {
                    screen_on_sightline(head_of(eye), subject, screen, SCREEN_HEIGHT)
                });
            let side_out = ctx.move_to(reveal.0, reveal.1, 7_000, 1.0).await;
            let revealed_view = ctx.look_until_facing(subject, FACE_TOL_DEG, 5_000).await;
            let revealed_geometry = screen_cell
                .zip(ctx.position())
                .is_some_and(|(screen, eye)| {
                    screen_on_sightline(head_of(eye), subject, screen, SCREEN_HEIGHT)
                });
            let shadow_back = ctx.move_to(shadow.0, shadow.1, 7_000, 1.0).await;
            let home_ok = ctx.move_to(home.0, home.1, 7_000, 1.0).await;
            let peer_confirmed = face_peer(ctx, peer, subject).await;
            ctx.record(
                TYPE_LOOK,
                &format!("vis-handoff-reveal-{index}-{beat}"),
                "Reveal the landmark from this side, close the loop, and report the first sighting.",
                json!({
                    "phase": phase,
                    "reveal_order": "first",
                    "shadow_arrived": shadow_in,
                    "hidden_view_confirmed": hidden_view,
                    "screen_geometry_blocks_hidden_view": hidden_geometry,
                    "reveal_arrived": side_out,
                    "revealed_view_confirmed": revealed_view,
                    "screen_geometry_blocks_revealed_view": revealed_geometry,
                    "screen_cell": screen_cell.map(|cell| json!({"x": cell.x, "y": cell.y, "z": cell.z})),
                    "shadow_returned": shadow_back,
                    "home_arrived": home_ok,
                    "peer_confirmed": peer_confirmed,
                }),
            );
        } else {
            let before = ctx.look_until_facing(subject, FACE_TOL_DEG, 5_000).await;
            let key = if slot == 0 {
                WalkDirection::Left
            } else {
                WalkDirection::Right
            };
            let ran = ctx.hold_visual(key, false, WAIT_LEG_MS[slot], 0).await;
            let after = ctx.look_until_facing(subject, FACE_TOL_DEG, 5_000).await;
            let home_ok = ctx.move_to(home.0, home.1, 7_000, 1.0).await;
            ctx.record(
                TYPE_LOOK,
                &format!("vis-handoff-reveal-follow-{index}-{beat}"),
                "Keep the landmark context alive, then confirm the partner's later reveal.",
                json!({
                    "phase": phase,
                    "reveal_order": "second",
                    "before_reveal_faced": before,
                    "waiting_leg_completed": ran,
                    "after_reveal_faced": after,
                    "home_arrived": home_ok,
                }),
            );
        }
        beat += 1;
    }
}
