//! CATALOG: family=construction min_players=2 single_process=true world_edits=true scenery=none far_players=false
//!
//! A single block is ferried back and forth between two players across a handoff cell that sits
//! exactly on the line between them: one side sets it down, the other picks it up and banks it in its
//! own tally column, and the turn passes.
//!
//! The revisit-problem face here is the teammate that is absent from the context frame (22.4% of v5's
//! multi-player windows) AND the edit that appears with no observed cause. The exchange point is on
//! the pair's sight line, so a frame containing the parcel contains the partner behind it: the model
//! sees WHO is about to change the block, from both points of view, before it changes. The handoff is
//! also the tightest form of the gaze loop this corpus can produce — off the partner, down onto the
//! cell, back onto the partner — repeated every turn.
//!
//! The trajectory loop is the ferry itself: each turn is a walk in to the midpoint and a walk back
//! out to the same station, so path length is roughly four times net displacement per turn.
//!
//! Not `site_handoff_build`: there hands ROTATE around a ring of separate sites and each visit
//! accretes a layer on whichever site the rotation handed it. Here the stations are fixed for the
//! whole episode so the sight line is a constant, and the passed object is a single block that is
//! really dug up and really re-placed, not a turn in a schedule.

use super::*;

/// One material per side, so the tally columns read as two distinct hands and the parcel's colour
/// says who set it down.
const SIDE_MATERIALS: [&str; 2] = ["minecraft:bricks", "minecraft:stone"];

/// Kit block ids as `block_id_at` reports them (no namespace). The dig beat is gated on this list:
/// digging anything else would open natural terrain, which should stay intact
/// because its occlusion is the point.
const KIT_IDS: [&str; 6] = [
    "stone_bricks",
    "oak_planks",
    "oak_log",
    "cobblestone",
    "bricks",
    "stone",
];

/// Half the distance between the two stations. 5.0 leaves the pair 10 blocks apart, so the hand can
/// close to reaching range of the midpoint (~2.5 blocks off it) with 7.5 blocks still between the two
/// bodies — clear of [`MIN_PEER_GAP`] even after a pathfinder overshoot.
const STATION_HALF: f64 = 5.0;

/// How close the hand stands to the handoff cell.
///
/// DERIVED from `place_reach`, which defaults to 4.0 blocks: `move_to` floors its arrival radius at
/// 1.5, so 2.0 leaves the hand at most 3.5 blocks out and the floor-level cell 3.7 blocks from the
/// eye — inside reach — while the two bodies stay 8 blocks apart.
const HANDOFF_STAND: f64 = 2.0;

/// Tallest the tally column is stacked before it wraps. The banker stands 1.5 blocks off it, so a
/// fourth layer would sit past `place_reach`; wrapping lets `marker_base_near` spread the surplus
/// sideways into a heap instead of recording an endless run of failed placements.
const TALLY_ROWS: i64 = 3;

/// Length of one turn. Has to cover the walk in (~1.5 s), a dig or a placement including
/// `place_block`'s own confirmed look (up to ~2.5 s), the bank placement, and the retreat.
const ROUND_MS: f64 = 8_500.0;

/// Tolerance for "the partner is in frame" — well inside the 51 deg half-FOV.
const PEER_TOL_DEG: f64 = 8.0;

/// Shortest movement key press. The key-chatter gate fails an episode when over 35% of movement
/// presses run under 10 ticks (0.5 s); 700 ms leaves margin for the 50 ms control tick.
const MIN_PRESS_MS: u64 = 700;

/// Gap below which a framing leg must open the pair up, and above which it must close in.
const GAP_TIGHT: f64 = 6.5;
const GAP_LOOSE: f64 = 11.0;

/// Draw in `[lo, hi)` from the per-bot stream, so a number this scenario could have hard-coded varies
/// per bot and per turn and still replays from the seed.
fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

/// The pair's shared axis, agreed with NO messaging: the engine hands every bot the same
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
/// still for the whole leg — which is what keeps this off the camera/travel-independence gate, where
/// a tracked look while walking measured 96-100% agreement. Direction comes from the LIVE gap.
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
    // Strafe, not forward/back: with the camera 110 deg off the partner those keys move the pair
    // together or apart on an unpredictable bearing, while a strafe leaves the gap roughly alone.
    let dwell_ms = vary(ctx, 700.0, 950.0) as u64;
    framing_leg(ctx, None, side_key, dwell_ms).await;
    let returned = face_partner(ctx, partner, fallback).await;
    ctx.record(
        TYPE_LOOK,
        &format!("vis-duet-ferry-away-{}-{}", ctx.index(), ctx.elapsed_ms()),
        "Turn off the exchange past the field of view, then confirm the return onto the partner.",
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
    // Opposite strafe keys per side: even in frames where both bots walk, their key tuples differ.
    // Per-bot jitter alone measured 80.2% pair agreement against the 80% near-clones ceiling.
    let side_key = if side > 0.0 {
        WalkDirection::Left
    } else {
        WalkDirection::Right
    };
    let away_deg = 110.0 + side * 15.0;

    let anchor = ctx.anchor();
    let axis = shared_axis(ctx);
    let perp = (-axis.1, axis.0);
    let station = (
        anchor.0 + axis.0 * side * STATION_HALF,
        anchor.1 + axis.1 * side * STATION_HALF,
    );

    // The opening shot, before any travel: at `spread_range: 8` the partner is within a dozen blocks
    // at spawn, so the first recorded view already contains the teammate.
    let opening = face_partner(ctx, partner, (anchor.0, 64.0, anchor.1)).await;
    travel_to(ctx, station, TRAVEL_HOP, 1.5).await;
    let start = ctx.position().unwrap_or((station.0, 64.0, station.1));
    let handoff_focus = (anchor.0, start.1 + 0.5, anchor.1);
    ctx.record(
        "observe",
        &format!("vis-duet-ferry-setup-{index}"),
        "Take the station facing the partner across the handoff cell.",
        json!({
            "global_index": global,
            "role": "ferry_hand",
            "partner": partner,
            "material": material,
            "side": side,
            "station": {"x": r2(station.0), "z": r2(station.1)},
            "handoff": {"x": r2(anchor.0), "z": r2(anchor.1)},
            "opening_shot_confirmed": opening,
        }),
    );

    let mut banked = 0i64;
    let mut last_round = i64::MIN;
    while !ctx.target_met() {
        // `phase_at` reads one process-wide epoch, so both sides agree on the turn number with no
        // barrier. Parity against `global` makes exactly one of the two the hand.
        let round = ctx.engine().shared.phase_at(ROUND_MS);
        let hand = (round + global as i64).rem_euclid(2) == 0;
        if round != last_round {
            last_round = round;
            ctx.record(
                TYPE_MOVE,
                &format!("vis-duet-ferry-turn-{index}-{round}"),
                if hand {
                    "This side's turn at the handoff cell."
                } else {
                    "The partner's turn: hold the station and keep the exchange in frame."
                },
                json!({"round": round, "hand": hand, "banked": banked}),
            );
        }

        if hand {
            let approach = (
                anchor.0 + axis.0 * side * HANDOFF_STAND,
                anchor.1 + axis.1 * side * HANDOFF_STAND,
            );
            ctx.move_to(approach.0, approach.1, 6_000, 1.5).await;
            let feet_y = ctx.position().map(|p| p.1).unwrap_or(start.1).round() as i32;
            // Search a short vertical window rather than one guessed cell: the parcel was placed by
            // the other side from its own footing, and natural terrain means its Y is not derivable
            // from ours.
            let parked = (feet_y - 2..=feet_y + 2).find_map(|y| {
                let cell = BlockPos::new(anchor.0.round() as i32, y, anchor.1.round() as i32);
                let id = ctx.block_id_at(cell)?;
                let is_kit = KIT_IDS.contains(&id.as_str());
                is_kit.then_some((cell, id))
            });
            match parked {
                // A parcel is waiting: pick it up. Gated on the kit list, so a miss can never turn
                // into a hole in the natural terrain.
                Some((cell, id)) => {
                    let dug = ctx.dig(cell).await;
                    ctx.record(
                        TYPE_MINE,
                        &format!("vis-duet-ferry-pickup-{index}-{round}"),
                        "Pick the parcel up off the handoff cell.",
                        json!({
                            "round": round,
                            "cell": {"x": cell.x, "y": cell.y, "z": cell.z},
                            "parcel_block": id,
                            "dug": dug,
                        }),
                    );
                    if dug {
                        // Bank it: walk back out and stack one block on this side's tally column, so
                        // a delivery leaves a countable trace the partner can see from its station.
                        ctx.move_to(station.0, station.1, 6_000, 1.5).await;
                        let base_y = ctx.position().map(|p| p.1).unwrap_or(start.1).round() as i64;
                        let desired = (
                            (station.0 + perp.0 * 1.5).round() as i64,
                            base_y + banked.rem_euclid(TALLY_ROWS),
                            (station.1 + perp.1 * 1.5).round() as i64,
                        );
                        let tally = marker_base_near(ctx, desired, 1);
                        let equipped = ctx.equip(material).await;
                        let stacked = match tally {
                            Some(cell) if equipped => ctx.place_block(cell, material).await,
                            _ => false,
                        };
                        if stacked {
                            banked += 1;
                        }
                        ctx.record(
                            TYPE_PLACE,
                            &format!("vis-duet-ferry-bank-{index}-{round}"),
                            "Bank the delivered parcel on this side's tally column.",
                            json!({
                                "round": round,
                                "banked": banked,
                                "material": material,
                                "stacked": stacked,
                                "equipped": equipped,
                                "cell": tally.map(|c| json!({"x": c.x, "y": c.y, "z": c.z})),
                            }),
                        );
                    }
                }
                // The cell is empty: set a parcel down for the other side to collect.
                None => {
                    let desired = (
                        anchor.0.round() as i64,
                        feet_y as i64,
                        anchor.1.round() as i64,
                    );
                    let cell = marker_base_near(ctx, desired, 1);
                    let equipped = ctx.equip(material).await;
                    let dropped = match cell {
                        Some(cell) if equipped => ctx.place_block(cell, material).await,
                        _ => false,
                    };
                    ctx.record(
                        TYPE_PLACE,
                        &format!("vis-duet-ferry-drop-{index}-{round}"),
                        "Set a parcel down on the handoff cell for the other side.",
                        json!({
                            "round": round,
                            "material": material,
                            "dropped": dropped,
                            "equipped": equipped,
                            "cell": cell.map(|c| json!({"x": c.x, "y": c.y, "z": c.z})),
                        }),
                    );
                }
            }
            // Retreat: camera back onto the partner FIRST, then the body walks under a pinned view,
            // so the return halves of the gaze loop and the trajectory loop are the same beat.
            face_partner(ctx, partner, handoff_focus).await;
            let leg_ms = vary(ctx, 800.0, 1_100.0) as u64;
            framing_leg(ctx, partner, side_key, leg_ms).await;
        } else {
            // The receiving side. It never bare-stands — the frozen gate fails 20 s of sub-0.5-block
            // motion, and the waiting role is where those stretches historically came from.
            face_partner(ctx, partner, handoff_focus).await;
            let leg_ms = vary(ctx, 800.0, 1_200.0) as u64;
            framing_leg(ctx, partner, side_key, leg_ms).await;
            // Look at what the hand just changed: the cross-view record that an edit made by one
            // player showed up in the other player's footage.
            witness_peer_edits(ctx).await;
            away_and_back(ctx, partner, handoff_focus, away_deg, side_key).await;
        }
        // Per-bot pause so the two sides never issue their beats on the same frame.
        sleep_ms(vary(ctx, 200.0, 700.0) as u64).await;
    }
}
