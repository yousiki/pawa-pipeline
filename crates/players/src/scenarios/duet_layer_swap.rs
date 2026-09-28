//! CATALOG: family=construction min_players=2 single_process=true world_edits=true scenery=none far_players=false
//!
//! Two players face each other across ONE shared column and take turns adding a layer to it, each
//! in its own material, so the finished stack reads as alternating stripes — the visible record of
//! whose turn it was.
//!
//! Against the revisit problem this closes the PEER half. Generation-5 data measured 22.4% of
//! multi-player windows as having the teammate walk INTO frame (invisible in the context frame),
//! which asks the model to invent a player, its pose and the blocks it is changing out of nothing.
//! Here the two stations sit on opposite sides of the work column, so the partner is on the far side
//! of the very thing being edited: any frame that contains the work also contains the partner's
//! body. The GAZE loop is the turn itself — camera off the partner onto the column, back onto the
//! partner — and the TRAJECTORY loop is the actor stepping in to reach and retreating to its station
//! every round, which is why path length far exceeds net displacement.
//!
//! Not `column_relay_wall`: there each crew stacks its OWN column and the watchers sit on an arc
//! BEHIND the worker, so the worker is between them and the wall and the two never face each other.
//! Not `pillar_rise_and_collapse`: nothing is ever dug here — a capped column is abandoned for a
//! fresh one beside it, so the site accretes instead of resetting.
//! Not `site_handoff_build`: no station rotation, no ring; the two stations are fixed for the whole
//! episode precisely so the sight line between them is a constant.

use super::*;

/// One material per side, so a layer's owner is readable from the footage alone.
const SIDE_MATERIALS: [&str; 2] = ["minecraft:stone_bricks", "minecraft:oak_planks"];

/// Half the distance between the two stations. 5.0 puts the pair 10 blocks apart, so the actor can
/// close to reaching range of the mid-column (~2.5 blocks off it) and still leave 7.5 blocks to the
/// partner — clear of [`MIN_PEER_GAP`] with margin for the pathfinder overshooting its radius.
const STATION_HALF: f64 = 5.0;

/// How high one column is taken before the pair starts a fresh one beside it.
///
/// DERIVED from `place_reach`, which defaults to 4.0 blocks. `move_to` floors its arrival radius at
/// 1.5, so a builder aiming for [`APPROACH`] can end up 3.7 blocks out; the top cell's centre then
/// sits `hypot(3.7, 0.88)` = 3.8 blocks from the eye at three layers and 4.7 at six, which is why the
/// first draft's six-layer column would simply have stopped growing past layer three with every
/// further placement recorded as a failure. Three layers also still fires the abandon-and-restart
/// beat several times in a 120 s episode, so nothing is lost.
const COLUMN_CAP: i32 = 3;

/// How far off the work column the actor stands. Close enough that every layer is inside
/// `place_reach` even at the far edge of `move_to`'s arrival radius, far enough that the two bodies
/// stay ~7 blocks apart.
const APPROACH: f64 = 2.2;

/// Length of one turn. It has to cover: the actor's walk in (~1.5 s), a placement including
/// `place_block`'s own confirmed look (up to ~2.5 s), the retreat, and the watcher's away-glance
/// beat — with slack, because a round that expires mid-placement wastes the layer.
const ROUND_MS: f64 = 9_000.0;

/// Tolerance for "the partner is in frame". Well inside the 51 deg half-FOV, so a confirmed
/// re-acquire really does put the body near the centre of the picture rather than at its edge.
const PEER_TOL_DEG: f64 = 8.0;

/// Shortest movement key press this script will issue. The key-chatter gate fails an episode when
/// over 35% of movement presses run under 10 ticks (0.5 s); 700 ms leaves margin for the 50 ms
/// control tick and for a press that gets clipped by the round boundary.
const MIN_PRESS_MS: u64 = 700;

/// Gap below which a framing leg must open the pair up rather than close it, and above which it must
/// close in. Between them the leg strafes and the gap is left alone.
const GAP_TIGHT: f64 = 6.5;
const GAP_LOOSE: f64 = 11.0;

/// Draw in `[lo, hi)` from the per-bot stream, so a number this scenario could have hard-coded
/// varies per bot and per round and still replays from the seed.
fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

/// The pair's shared axis, agreed with NO messaging: the engine hands every bot the same
/// `native_waypoints` list, so its first entry is a direction both sides derive identically. Using
/// a world axis instead would put every episode of every seed on the same bearing.
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
/// wide turn that timed out would be recorded as a re-acquire that never happened — the exact defect
/// the compact-staging rule exists for. Falls back to the work column when there is no partner (N=1).
async fn face_partner(ctx: &Ctx, partner: Option<usize>, fallback: (f64, f64, f64)) -> bool {
    let target = partner
        .and_then(|j| ctx.peer_position(j))
        .map(head_of)
        .unwrap_or(fallback);
    ctx.look_until_facing(target, PEER_TOL_DEG, 4_000).await
}

/// One leg of travel that leaves the partner where the camera already put it.
///
/// `hold_visual` pins the camera at its current angles (`CameraIntent::LookAt`) and drives one key
/// directly, with no pathfinder waypoint, so the view does not move for the whole leg. That is what
/// keeps this beat off the camera/travel-independence gate: the gate only weighs frames in which the
/// camera turned WHILE the body was travelling, and here it never turns. Tracking the partner
/// instead — `move_to_watching_peer` — is the measured 96-100% failure mode.
///
/// The direction is chosen from the LIVE gap, never fixed, because a script that always walks
/// forward toward a facing partner ends up inside its head box.
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
    // The hold's stall guard cut the press short against terrain. Re-pressing here is what turned
    // every step into a sub-0.5 s press in `mirror_dance`'s round-2 key-chatter failure (37% against
    // a 35% ceiling), so bail out to ONE pathfinder leg toward ground the planner can actually
    // solve. 0.8 blocks of arrival tolerance against a 4-block leg keeps the leg from being
    // satisfied before the first step.
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
/// This is the `net_over_sweep` beat. A duet whose camera rests on one subject sweeps almost
/// nothing, and a window holding a single slow drift reads as a ratio of 1.0 — the "turn away and
/// never come back" shape v5 is full of. `away_deg` exceeds the 102.5 deg horizontal FOV so the away
/// view genuinely shares no pixels with the partner view, and the whole away-and-back fits inside one
/// 4.65 s window: 2 x 110 deg at the configured 72 deg/s is ~3.1 s plus one 0.7-0.9 s leg.
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
    // together or apart on an unpredictable bearing, while a strafe leaves the gap roughly alone.
    let dwell_ms = vary(ctx, 700.0, 950.0) as u64;
    framing_leg(ctx, None, side_key, dwell_ms).await;
    let returned = face_partner(ctx, partner, fallback).await;
    ctx.record(
        TYPE_LOOK,
        &format!("vis-duet-layer-away-{}-{}", ctx.index(), ctx.elapsed_ms()),
        "Turn past the field of view and confirm the return onto the partner.",
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
    // Local index pairing: `single_process` is declared for exactly this, and at the N=2 tier v6
    // records there is only ever one pair.
    let partner = Some(index ^ 1).filter(|&j| j < bot_count);
    let side = if global.is_multiple_of(2) { 1.0 } else { -1.0 };
    let material = SIDE_MATERIALS[global % SIDE_MATERIALS.len()];
    // Opposite strafe keys per side. This is the STRUCTURAL half of the near-clones defence: even in
    // the frames where both bots happen to be walking, their key tuples differ. Per-bot jitter alone
    // measured 80.2% agreement against the 80% ceiling in `eval_shared_unobserved`.
    let side_key = if side > 0.0 {
        WalkDirection::Left
    } else {
        WalkDirection::Right
    };
    let away_deg = 110.0 + side * 15.0;

    let anchor = ctx.anchor();
    let axis = shared_axis(ctx);
    let station = (
        anchor.0 + axis.0 * side * STATION_HALF,
        anchor.1 + axis.1 * side * STATION_HALF,
    );

    // The opening shot, before any travel: the partner is 8-or-so blocks away at spawn (the manifest
    // gives the swarm `spread_range: 8`), so the very first thing recorded is a confirmed view with
    // the teammate in it.
    let opening = face_partner(ctx, partner, (anchor.0, 64.0, anchor.1)).await;
    travel_to(ctx, station, TRAVEL_HOP, 1.5).await;
    let start = ctx.position().unwrap_or((station.0, 64.0, station.1));
    ctx.record(
        "observe",
        &format!("vis-duet-layer-setup-{index}"),
        "Take the station facing the partner across the shared work column.",
        json!({
            "global_index": global,
            "role": "layer_swap",
            "partner": partner,
            "material": material,
            "side": side,
            "station": {"x": r2(station.0), "z": r2(station.1)},
            "opening_shot_confirmed": opening,
        }),
    );

    // Column cursor. Both sides derive it from the same anchor, and a capped column is abandoned for
    // a fresh base one cell along the axis, so the pair keeps working the midpoint between them
    // instead of drifting apart.
    let mut column = 0i64;
    let mut layers = 0i32;
    let mut last_round = i64::MIN;
    while !ctx.target_met() {
        // `phase_at` reads one process-wide epoch, so both sides agree on the round number with no
        // barrier and no waiting. Parity against `global` makes exactly one of the two the actor.
        let round = ctx.engine().shared.phase_at(ROUND_MS);
        let acting = (round + global as i64).rem_euclid(2) == 0;
        if round != last_round {
            last_round = round;
            ctx.record(
                TYPE_MOVE,
                &format!("vis-duet-layer-turn-{index}-{round}"),
                if acting {
                    "This side's turn: step in and add a layer to the shared column."
                } else {
                    "The partner's turn: hold the station and keep it in frame."
                },
                json!({
                    "round": round,
                    "acting": acting,
                    "column": column,
                    "layers": layers,
                }),
            );
        }

        if acting {
            let base_desired = (
                (anchor.0 + axis.0 * column as f64).round() as i64,
                start.1.round() as i64 + layers as i64,
                (anchor.1 + axis.1 * column as f64).round() as i64,
            );
            // Step in to within reach of the column, not onto it. `move_to`'s own 1.5-block floor is
            // the tightest arrival radius available, and from ~2 blocks off the column the partner is
            // still ~7 blocks away.
            let approach = (
                anchor.0 + axis.0 * (column as f64 + side * APPROACH),
                anchor.1 + axis.1 * (column as f64 + side * APPROACH),
            );
            ctx.move_to(approach.0, approach.1, 6_000, 1.5).await;
            // Resolve the cell only AFTER arriving. `marker_base_near` rejects cells the placer's own
            // body occupies, and `place_block` needs the target in reach — both are judged from where
            // the body stands, so a cell resolved before the walk is answering the wrong question. A
            // fixed Y row would be wrong for a second reason: on natural terrain the ground under one
            // column is not the ground under the next.
            let cell = marker_base_near(ctx, base_desired, 1);
            let equipped = ctx.equip(material).await;
            // `place_block` runs its own confirmed look at the target face, so the camera leaves the
            // partner here on purpose: this is the gaze loop's outbound half.
            let placed = match cell {
                Some(cell) if equipped => ctx.place_block(cell, material).await,
                _ => false,
            };
            if placed {
                layers += 1;
                if layers >= COLUMN_CAP {
                    // Capped: start a fresh column one cell along the axis. Nothing is dug — the
                    // site accretes, which is what keeps a revisit checkable against a growing
                    // structure rather than a reverted one.
                    column += 1;
                    layers = 0;
                }
            }
            ctx.record(
                TYPE_PLACE,
                &format!("vis-duet-layer-place-{index}-{round}"),
                "Add this side's striped layer to the shared column.",
                json!({
                    "round": round,
                    "column": column,
                    "layer": layers,
                    "material": material,
                    "placed": placed,
                    "equipped": equipped,
                    "cell": cell.map(|c| json!({"x": c.x, "y": c.y, "z": c.z})),
                }),
            );
            // Retreat: the camera goes back onto the partner FIRST, then the body walks the retreat
            // under a pinned view, so the return half of the gaze loop and the return half of the
            // trajectory loop are the same beat and the partner is framed throughout.
            face_partner(ctx, partner, (approach.0, start.1 + 1.0, approach.1)).await;
            let leg_ms = vary(ctx, 800.0, 1_100.0) as u64;
            framing_leg(ctx, partner, side_key, leg_ms).await;
        } else {
            // The watching side. It never bare-stands: the frozen gate fails 20 s of sub-0.5-block
            // motion, and a watcher is the role that historically produced those stretches.
            face_partner(ctx, partner, (anchor.0, start.1 + 1.0, anchor.1)).await;
            let leg_ms = vary(ctx, 800.0, 1_200.0) as u64;
            framing_leg(ctx, partner, side_key, leg_ms).await;
            // Account for what the actor changed while this side was framing it — the cross-view
            // world-synchronisation record.
            witness_peer_edits(ctx).await;
            away_and_back(
                ctx,
                partner,
                (anchor.0, start.1 + 1.0, anchor.1),
                away_deg,
                side_key,
            )
            .await;
        }
        // Per-bot pause so the two sides never issue their beats on the same frame.
        sleep_ms(vary(ctx, 200.0, 700.0) as u64).await;
    }
}
