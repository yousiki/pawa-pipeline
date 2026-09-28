//! CATALOG: family=construction min_players=2 single_process=true world_edits=true scenery=none far_players=false
//!
//! Two players stand on opposite sides of ONE low wall and extend it outward from the middle, each
//! along its own half, so every placement is made on the face the other one is looking at.
//!
//! The revisit-problem face this covers is the teammate that is never in the context frame (22.4% of v5's
//! multi-player windows). Here the geometry forbids losing them: the pair works a common surface from
//! 6-10 blocks apart with the wall between, so the partner is directly behind the work. The wall is
//! kept low and crenellated on purpose — it cuts the partner's legs and lower body while leaving the
//! head clear, which is the cheap, honest source of PARTIAL occlusion these scenarios want:
//! the sight line is interrupted and recovered by geometry rather than by the camera turning away.
//!
//! The gaze loop is per placement: off the partner onto the wall cell, back onto the partner. The
//! trajectory loop is the cursor patrol — out along the bot's own half of the wall, then back to the
//! middle — so path length is several times net displacement.
//!
//! Not `reflection_build_pairs`: there each partner works a PRIVATE 4-block-deep strip at least 9
//! blocks off a symmetry axis, facing its own cells, and the two builds only relate through a shared
//! hash. Here there is one physical wall, both halves of it, and each bot's frame is filled by the
//! other's face across it.

use super::*;

/// One material per side, so a finished stretch shows which half of the wall each hand laid.
const SIDE_MATERIALS: [&str; 2] = ["minecraft:cobblestone", "minecraft:oak_log"];

/// How far off the wall a builder stands.
///
/// DERIVED from `place_reach`, which defaults to 4.0 blocks: `move_to` floors its arrival radius at
/// 1.5, so a 2.5-block stand-off can leave the builder up to 4.0 blocks out, and the upper row's
/// centre is then `hypot(4.0, 0.88)` = 4.1 blocks from the eye — just past reach. 2.5 keeps the
/// typical case comfortably inside it, and still leaves two builders working the same cursor 5.0
/// blocks apart, clear of [`MIN_PEER_GAP`].
const STAND_OFF: f64 = 2.5;

/// Cells of wall each side is responsible for, measured from the middle. Four keeps the pair's
/// separation inside 10 blocks at full extension, so the partner never leaves the frame, and keeps a
/// there-and-back patrol short enough to complete several times in a 120 s episode.
const WALL_HALF: i64 = 4;

/// Tallest a wall cell is taken. Two layers is enough to cut a standing body at the waist while
/// leaving the head clear; a third would close the sight line the scenario is built around.
const WALL_ROWS: i64 = 2;

/// Tolerance for "the partner is in frame" — well inside the 51 deg half-FOV, so a confirmed
/// re-acquire puts the body near the centre of the picture rather than at its edge.
const PEER_TOL_DEG: f64 = 8.0;

/// Shortest movement key press this script issues. The key-chatter gate fails an episode when over
/// 35% of movement presses run under 10 ticks (0.5 s); 700 ms leaves margin for the control tick.
const MIN_PRESS_MS: u64 = 700;

/// Gap below which a framing leg must open the pair up, and above which it must close in. Between
/// them the leg strafes and the gap is left alone.
const GAP_TIGHT: f64 = 5.0;
const GAP_LOOSE: f64 = 11.0;

/// Draw in `[lo, hi)` from the per-bot stream, so a number this scenario could have hard-coded varies
/// per bot and per beat and still replays from the seed.
fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

/// The pair's shared axis, agreed with NO messaging: the engine hands every bot the same
/// `native_waypoints` list, so its first entry is a direction both sides derive identically. A world
/// axis would instead put every episode of every seed on the same bearing.
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
/// camera (`CameraIntent::LookAt`) and drives one key with no pathfinder waypoint, so the view does
/// not move for the whole leg — which is what keeps the beat off the camera/travel-independence gate,
/// where a tracked look while walking measured 96-100% agreement. Direction comes from the LIVE gap,
/// because always walking forward toward a facing partner ends up inside its head box.
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
    // The hold's stall guard cut the press against terrain. Re-pressing is what turned every step
    // into a sub-0.5 s press in `mirror_dance`'s round-2 key-chatter failure (37% against a 35%
    // ceiling), so bail to ONE pathfinder leg toward solvable ground instead.
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
/// The `net_over_sweep` beat: a duet resting its camera on one subject sweeps almost nothing, and a
/// window holding one slow drift reads as a ratio of 1.0 — the "turn away and never come back" shape
/// v5 is full of. `away_deg` exceeds the 102.5 deg horizontal FOV, so the away view shares no pixels
/// with the partner view, and away-plus-back fits inside one 4.65 s window (2 x ~110 deg at the
/// configured 72 deg/s is ~3.1 s, plus one 0.7-0.9 s leg).
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
    // Strafe, not forward/back: with the camera 110 deg off the partner those keys move the pair
    // together or apart on an unpredictable bearing, while a strafe leaves the gap roughly alone.
    let dwell_ms = vary(ctx, 700.0, 950.0) as u64;
    framing_leg(ctx, None, side_key, dwell_ms).await;
    let returned = face_partner(ctx, partner, fallback).await;
    ctx.record(
        TYPE_LOOK,
        &format!("vis-duet-wall-away-{}-{}", ctx.index(), ctx.elapsed_ms()),
        "Turn past the field of view over the wall, then confirm the return onto the partner.",
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
    let side = if global.is_multiple_of(2) { 1.0 } else { -1.0 };
    let material = SIDE_MATERIALS[global % SIDE_MATERIALS.len()];
    // Opposite strafe keys per side: the STRUCTURAL half of the near-clones defence. The two sides
    // build SIMULTANEOUSLY here, so per-bot jitter alone would not separate their key streams — that
    // is exactly how `eval_shared_unobserved` measured 80.2% against the 80% ceiling.
    let side_key = if side > 0.0 {
        WalkDirection::Left
    } else {
        WalkDirection::Right
    };
    let away_deg = 110.0 + side * 15.0;

    let anchor = ctx.anchor();
    // The wall runs PERPENDICULAR to the pair axis: the axis is the sight line between the two
    // builders, the perpendicular is the surface they share.
    let axis = shared_axis(ctx);
    let wall = (-axis.1, axis.0);

    // The opening shot, before any travel: at `spread_range: 8` the partner is already within a
    // dozen blocks at spawn, so the first thing recorded is a confirmed view containing the teammate.
    let opening = face_partner(ctx, partner, (anchor.0, 64.0, anchor.1)).await;
    let home = (
        anchor.0 + axis.0 * side * STAND_OFF,
        anchor.1 + axis.1 * side * STAND_OFF,
    );
    travel_to(ctx, home, TRAVEL_HOP, 1.5).await;
    let start = ctx.position().unwrap_or((home.0, 64.0, home.1));
    ctx.record(
        "observe",
        &format!("vis-duet-wall-setup-{index}"),
        "Take the near side of the shared wall, facing the partner across it.",
        json!({
            "global_index": global,
            "role": "face_off_builder",
            "partner": partner,
            "material": material,
            "side": side,
            "wall_bearing": {"x": r2(wall.0), "z": r2(wall.1)},
            "opening_shot_confirmed": opening,
        }),
    );

    // Cursor patrol along this side's half of the wall: out to `WALL_HALF`, then back to the middle.
    // The return leg is the trajectory loop, and it doubles as a revisit — cells already filled on
    // the way out are re-probed rather than re-placed, which is what makes persistence checkable.
    let mut cursor = 1i64;
    let mut outbound = true;
    while !ctx.target_met() {
        // The bot's own half only: the two sides never contend for a cell, so nothing here needs a
        // barrier, and their travel directions along the wall are opposite by construction.
        let along = cursor as f64 * side;
        let wall_xz = (anchor.0 + wall.0 * along, anchor.1 + wall.1 * along);
        let stand = (
            wall_xz.0 + axis.0 * side * STAND_OFF,
            wall_xz.1 + axis.1 * side * STAND_OFF,
        );
        ctx.move_to(stand.0, stand.1, 6_000, 1.5).await;

        // Lowest free layer of this wall cell. Probed rather than counted: on natural terrain the
        // ground under cell 4 is not the ground under cell 1, and `marker_base_near` also refuses
        // cells the placer's own body occupies (vanilla rejects those regardless).
        let feet_y = ctx.position().map(|p| p.1).unwrap_or(start.1).round() as i64;
        let mut placed = false;
        let mut standing = 0i64;
        let mut cell_used = None;
        for row in 0..WALL_ROWS {
            let exact = BlockPos::new(
                wall_xz.0.round() as i32,
                (feet_y + row) as i32,
                wall_xz.1.round() as i32,
            );
            // Test the EXACT column cell before asking for a placeable one. `marker_base_near` walks
            // outward when its desired cell is taken, which is right for a free-standing marker and
            // wrong for a wall: on the return pass every cell is already filled, and letting it drift
            // would smear the wall two blocks wide instead of leaving it verified.
            if !block_is_air(ctx, exact) {
                standing += 1;
                continue;
            }
            let Some(cell) =
                marker_base_near(ctx, (exact.x as i64, exact.y as i64, exact.z as i64), 1)
            else {
                continue;
            };
            if !ctx.equip(material).await {
                break;
            }
            // `place_block` runs its own confirmed look at the target face, so the camera leaves the
            // partner here on purpose — this is the outbound half of the gaze loop.
            if ctx.place_block(cell, material).await {
                placed = true;
                cell_used = Some(cell);
                break;
            }
        }
        ctx.record(
            TYPE_PLACE,
            &format!("vis-duet-wall-lay-{index}-{cursor}-{}", ctx.elapsed_ms()),
            "Lay this side's cell of the shared wall, or verify the one already standing.",
            json!({
                "cursor": cursor,
                "outbound": outbound,
                "material": material,
                "placed": placed,
                "rows_already_standing": standing,
                "cell": cell_used.map(|c| json!({"x": c.x, "y": c.y, "z": c.z})),
            }),
        );

        // Back onto the partner, then walk under a pinned view: the return half of the gaze loop and
        // a stretch of travel are the same beat, so the teammate is framed while the body moves.
        face_partner(ctx, partner, (wall_xz.0, start.1 + 1.0, wall_xz.1)).await;
        let leg_ms = vary(ctx, 800.0, 1_150.0) as u64;
        framing_leg(ctx, partner, side_key, leg_ms).await;
        witness_peer_edits(ctx).await;
        // Once per patrol turn, not per cell: often enough that most windows contain a reversal,
        // rare enough that the wall keeps getting built.
        if cursor == WALL_HALF || cursor == 1 {
            away_and_back(
                ctx,
                partner,
                (wall_xz.0, start.1 + 1.0, wall_xz.1),
                away_deg,
                side_key,
            )
            .await;
        }

        if outbound {
            cursor += 1;
            if cursor > WALL_HALF {
                cursor = WALL_HALF;
                outbound = false;
            }
        } else {
            cursor -= 1;
            if cursor < 1 {
                cursor = 1;
                outbound = true;
            }
        }
        // Per-bot pause so the two simultaneous builders never step on the same frame.
        sleep_ms(vary(ctx, 150.0, 600.0) as u64).await;
    }
}
