//! CATALOG: family=locomotion min_players=1 single_process=false world_edits=true scenery=none far_players=false

use super::*;

/// Drop below a terrain shoulder until it cuts the subject off, then climb back up to it.
///
/// The revisit problem, this face of it: the occluder is the EDGE OF THE GROUND THE BOT IS STANDING
/// ON, and the sight line is broken by losing altitude rather than by putting an object in the way.
/// That makes it the one occlusion mechanism in the family where the camera's height changes with
/// the occlusion, so a model cannot recover the view by re-rendering the same frustum from the same
/// eye — it has to actually place the geometry it saw before.
///
/// The loop is the trajectory: down past the shoulder, back up to the post. The recovery is verified
/// by `look_until_facing` (the camera provably arrived, rather than running out of deadline) plus a
/// sight-line raycast at both stations.
///
/// Two constraints make the station selectable at all, and both are measured rather than assumed:
///
/// - The descent has to be WALKABLE, or the climb back never happens and the episode has no
///   recovery in it. So the profile scan refuses any bearing that steps more than
///   [`WALKABLE_STEP`] blocks between adjacent columns — a sheer cliff is a trap, not a scenario.
/// - The descent has to be CONVEX. On a perfectly planar slope the line from a low eye to a high
///   subject passes above the ground the whole way and nothing is occluded at all; occlusion needs
///   a shoulder that stands above that line. The scan scores exactly that deviation, so a bearing
///   that merely descends loses to one that descends behind a lip.
const MATERIAL: &str = "minecraft:cobblestone";

/// Subject: three tall at three blocks, inside interaction reach so the bot never travels to build.
const SUBJECT_HEIGHT: i64 = 3;
const SUBJECT_DISTANCE: f64 = 3.0;

/// Eye height above the feet, and the height of the subject's top block above the post's ground.
/// Both feed the sight line the shoulder has to stand above, so neither is a free constant.
const EYE_HEIGHT: f64 = 1.62;
const SUBJECT_TOP: f64 = SUBJECT_HEIGHT as f64 - 0.5;

/// Profile scan: how far out to walk the columns, how far out the station may be, and how much
/// altitude the descent has to lose. The station stays short because `displacement` has to stay
/// under ~5 blocks and the holds have to outweigh the travel.
const PROFILE_MAX: usize = 14;
const STATION_MIN: usize = 5;
const DROP_MIN: i32 = 3;

/// Altitude a single block of horizontal travel may change and still be walked back up. Two is a
/// vanilla jump plus slack; three is where the pathfinder starts needing terrain it may not have.
const WALKABLE_STEP: i32 = 2;

/// Bearings sampled around the post. Twelve is 30 deg apart — fine enough to catch a shoulder on
/// ordinary overworld relief without turning the scan into a chunk-read storm.
const BEARING_STEPS: usize = 12;

/// Vertical window a surface probe scans, matching `ridge_valley_circuit`'s span.
const SCAN_SPAN: i32 = 64;

/// Fallback wall geometry, same derivation as `occl_wall_peekaboo`: 2 blocks out is the distance at
/// which a 2-tall wall still reaches above eye level, and three lanes give a shadow wide enough to
/// absorb `move_to`'s 1.5-block arrival slop.
const WALL_DISTANCE: f64 = 2.0;
const WALL_HEIGHT: i64 = 2;
const WALL_LANES: [i64; 3] = [-1, 0, 1];
const WALL_STATION: f64 = 5.0;

/// One shuttle leg, in blocks. 4 clears the key-chatter floor at sprint speed and matches
/// Solaris's 3-4 block random-walk primitive; see `hold_facing`.
const SHUTTLE_LEG: f64 = 4.0;

/// A steady shot on the aimed bearing before the shuttle starts.
///
/// `hold_facing` hands the camera to `CameraIntent::Hold` the moment its first leg begins, and
/// `Hold` faces the direction of travel — so an aimed bearing survives only while the body is
/// standing. Without a standing beat the recovery would be recorded during a shuttle that is
/// swinging the camera through a fresh random heading every leg, and no training window would ever
/// contain a readable view of it. Four seconds is just under one 93-frame window at 20 fps, so a
/// whole window fits inside a steady shot, and it is far inside the 20 s of stillness the frozen
/// gate allows.
const AIMED_DWELL_MS: u64 = 4_000;

/// Holds, with the per-bot stagger that keeps two bots on the same script off each other's phase.
const HIGH_HOLD_MS: i64 = 8_500;
const LOW_HOLD_MS: i64 = 8_000;
const HOLD_STAGGER_MS: i64 = 900;

/// Whether the straight sight line from `eye` to `target` is interrupted by a solid block. Copied
/// into each scenario of this family rather than shared, following `wade_column_detour`'s precedent
/// of duplicating `ridge_valley_circuit`'s probe.
fn los_blocked(ctx: &Ctx, eye: (f64, f64, f64), target: (f64, f64, f64)) -> bool {
    let (dx, dy, dz) = (target.0 - eye.0, target.1 - eye.1, target.2 - eye.2);
    let span = (dx * dx + dy * dy + dz * dz).sqrt();
    let steps = (span * 2.0).ceil() as i32;
    (1..steps).any(|step| {
        let t = step as f64 / steps as f64;
        block_is_safe_support(
            ctx,
            BlockPos::new(
                (eye.0 + dx * t).floor() as i32,
                (eye.1 + dy * t).floor() as i32,
                (eye.2 + dz * t).floor() as i32,
            ),
        )
    })
}

/// Feet altitude of the open-sky ground surface in column `(x, z)`. Adapted from
/// `ridge_valley_circuit::skylit_feet_y`: the downward scan makes the first support the topmost, so
/// a probe cannot hand back a cave floor, and canopy is refused because it is not a route.
fn surface_feet_y(ctx: &Ctx, x: i32, z: i32, y_hint: i32) -> Option<i32> {
    for y in ((y_hint - SCAN_SPAN)..=(y_hint + SCAN_SPAN)).rev() {
        let cell = BlockPos::new(x, y, z);
        let id = ctx.block_id_at(cell)?;
        if id.contains("water") || id.contains("lava") {
            return None;
        }
        if block_is_safe_support(ctx, cell) {
            if id.contains("leaves") || id.ends_with("log") {
                return None;
            }
            return Some(y + 1);
        }
    }
    None
}

/// One resolved descent: where to stand, how far below the post it is, and how far the shoulder
/// stands above the sight line back to the subject.
struct Descent {
    bearing: f64,
    distance: f64,
    drop: i32,
    shoulder: f64,
}

/// Walk the terrain profile along every candidate bearing and keep the descent whose shoulder rises
/// furthest above the low station's line of sight to the subject.
///
/// The profile is truncated at the first unwalkable step, so a station is only ever proposed on
/// ground the bot can climb back up. Within the walkable prefix, every candidate station is scored
/// by the largest amount any intermediate column rises above the straight line from the station's
/// eye to the subject's top block — the geometric definition of "this shoulder hides it".
fn best_descent(ctx: &Ctx, post: (f64, f64, f64), phase: f64) -> Option<Descent> {
    let y_hint = post.1.floor() as i32;
    let post_y =
        surface_feet_y(ctx, post.0.floor() as i32, post.2.floor() as i32, y_hint).unwrap_or(y_hint);
    let mut best: Option<Descent> = None;
    for slot in 0..BEARING_STEPS {
        let bearing = phase + std::f64::consts::TAU * slot as f64 / BEARING_STEPS as f64;
        // Walkable prefix of the profile, one column per block of horizontal distance.
        let mut profile: Vec<i32> = Vec::with_capacity(PROFILE_MAX);
        let mut previous = post_y;
        for distance in 1..=PROFILE_MAX {
            let d = distance as f64;
            let Some(y) = surface_feet_y(
                ctx,
                (post.0 + bearing.cos() * d).floor() as i32,
                (post.2 + bearing.sin() * d).floor() as i32,
                y_hint,
            ) else {
                break;
            };
            if (y - previous).abs() > WALKABLE_STEP {
                break;
            }
            profile.push(y);
            previous = y;
        }
        for station in STATION_MIN..=profile.len() {
            let station_y = profile[station - 1];
            let drop = post_y - station_y;
            if drop < DROP_MIN {
                continue;
            }
            // Sight line from the low eye back up to the subject's top block, and the largest
            // amount any intermediate column stands above it.
            let from = (station as f64, station_y as f64 + EYE_HEIGHT);
            let to = (0.0, post_y as f64 + SUBJECT_TOP);
            let shoulder = (1..station)
                .map(|d| {
                    let t = (from.0 - d as f64) / from.0;
                    profile[d - 1] as f64 - (from.1 + (to.1 - from.1) * t)
                })
                .fold(f64::NEG_INFINITY, f64::max);
            if shoulder <= 0.0 {
                continue; // planar slope: the subject stays in view the whole way down.
            }
            if best.as_ref().is_none_or(|b| shoulder > b.shoulder) {
                best = Some(Descent {
                    bearing,
                    distance: station as f64,
                    drop,
                    shoulder,
                });
            }
        }
    }
    best
}

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let anchor = ctx.anchor();
    let post = ctx.position().unwrap_or((anchor.0, 64.0, anchor.1));

    // Subject on the anchor side so every POV's opening frame contains the scene's subject with the
    // swarm pulled in to `spread_range: 8` (hard rule 5).
    let toward = (anchor.0 - post.0, anchor.1 - post.2);
    let span = toward.0.hypot(toward.1);
    let unit = if span > 1.0 {
        (toward.0 / span, toward.1 / span)
    } else {
        (0.0, 1.0)
    };
    let desired = (
        (post.0 + unit.0 * SUBJECT_DISTANCE).round() as i64,
        post.1.round() as i64,
        (post.2 + unit.1 * SUBJECT_DISTANCE).round() as i64,
    );
    let raised = raise_landmark(ctx, desired, SUBJECT_HEIGHT, MATERIAL).await;
    let subject = raised.map(|(_, point)| point).unwrap_or((
        desired.0 as f64 + 0.5,
        post.1 + SUBJECT_TOP,
        desired.2 as f64 + 0.5,
    ));

    // Resolved once, not per cycle: the profile scan reads a few hundred columns, and the terrain
    // does not change underneath the episode. Each bot starts its sweep on its own bearing so two
    // bots in one episode do not descend onto the same station.
    let descent = best_descent(ctx, post, global as f64 * 1.7);
    let mut wall_lanes = 0u32;
    let (station, occluder_kind, drop, shoulder) = match &descent {
        Some(d) => (
            (
                post.0 + d.bearing.cos() * d.distance,
                post.2 + d.bearing.sin() * d.distance,
            ),
            "terrain_shoulder",
            d.drop,
            r2(d.shoulder),
        ),
        None => {
            // Flat ground, or every descent planar: raise a wall so the episode still contains a
            // real occlusion event, and say in the log which mechanism the footage shows.
            let perp = (-unit.1, unit.0);
            for lane in WALL_LANES {
                let cell = (
                    (post.0 + unit.0 * WALL_DISTANCE + perp.0 * lane as f64).round() as i64,
                    post.1.round() as i64,
                    (post.2 + unit.1 * WALL_DISTANCE + perp.1 * lane as f64).round() as i64,
                );
                if raise_landmark(ctx, cell, WALL_HEIGHT, MATERIAL)
                    .await
                    .is_some()
                {
                    wall_lanes += 1;
                }
            }
            (
                (
                    post.0 - unit.0 * WALL_STATION,
                    post.2 - unit.1 * WALL_STATION,
                ),
                "wall",
                0,
                0.0,
            )
        }
    };
    ctx.record(
        "observe",
        &format!("vis-occl-dropoff-setup-{index}"),
        "Raise the subject and resolve the descent whose shoulder will cut its sight line.",
        json!({
            "global_index": global,
            "role": "dropoff_occlusion",
            "subject_placed": raised.is_some(),
            "subject": {"x": subject.0, "y": subject.1, "z": subject.2},
            "occluder_kind": occluder_kind,
            "station": {"x": r2(station.0), "z": r2(station.1)},
            "planned_drop": drop,
            "planned_shoulder": shoulder,
            "wall_lanes_placed": wall_lanes,
        }),
    );

    let mut cycle = 0usize;
    while !ctx.target_met() {
        // --- up at the post: grounding, and from cycle 1 the recovery after the climb ---
        let arrived = ctx.look_until_facing(subject, 5.0, 5_000).await;
        let high = ctx.position();
        let eye = high.map(head_of);
        let clear = eye.is_some_and(|e| !los_blocked(ctx, e, subject));
        let subject_yaw = eye.map(|e| look_angles(e, subject).0).unwrap_or(0.0);
        ctx.record(
            "observe",
            &format!("vis-occl-dropoff-high-{index}-{cycle}"),
            "Stand at the high post facing the subject: the grounding, or the view regained by climbing back.",
            json!({
                "cycle": cycle,
                "is_recovery": cycle > 0,
                "camera_arrived": arrived,
                "sight_line_clear": clear,
                "subject_yaw_deg": r2(subject_yaw),
                "feet_y": high.map(|p| r2(p.1)),
                "elapsed_ms": ctx.elapsed_ms(),
            }),
        );
        // A steady shot before the shuttle takes the camera: `hold_facing`'s first leg hands it to
        // `CameraIntent::Hold`, which faces travel, so this is the only stretch that actually holds
        // the aimed bearing.
        ctx.still(AIMED_DWELL_MS).await;
        hold_facing(
            ctx,
            subject_yaw,
            ctx.elapsed_ms() + HIGH_HOLD_MS + global as i64 * HOLD_STAGGER_MS,
            SHUTTLE_LEG,
        )
        .await;
        if ctx.target_met() {
            break;
        }

        // --- down below the shoulder ---
        travel_to(ctx, station, TRAVEL_HOP, 2.0).await;
        let low = ctx.position();
        let eye = low.map(head_of);
        let hidden = eye.is_some_and(|e| los_blocked(ctx, e, subject));
        let low_yaw = eye
            .map(|e| look_angles(e, subject).0)
            .unwrap_or(subject_yaw);
        ctx.record(
            TYPE_MOVE,
            &format!("vis-occl-dropoff-low-{index}-{cycle}"),
            "Descend past the shoulder and measure the altitude lost and the sight line cut.",
            json!({
                "cycle": cycle,
                "occluder_kind": occluder_kind,
                "occluded": hidden,
                "feet_y": low.map(|p| r2(p.1)),
                "measured_drop": match (high, low) { (Some(h), Some(l)) => Some(r2(h.1 - l.1)), _ => None },
                "low_yaw_deg": r2(low_yaw),
            }),
        );
        // Aim back UP the slope, on the bearing the subject sits on, and take a steady shot of it
        // before the shuttle hands the camera to `Hold`. Both stations in a cycle aim along nearly
        // the same bearing, so consecutive steady shots overlap while the pixels behind that bearing
        // change completely.
        ctx.still(AIMED_DWELL_MS).await;
        hold_facing(
            ctx,
            low_yaw,
            ctx.elapsed_ms() + LOW_HOLD_MS + global as i64 * HOLD_STAGGER_MS,
            SHUTTLE_LEG,
        )
        .await;
        let after = ctx
            .position()
            .map(head_of)
            .is_some_and(|e| los_blocked(ctx, e, subject));
        ctx.record(
            "observe",
            &format!("vis-occl-dropoff-low-end-{index}-{cycle}"),
            "Re-probe the sight line at the end of the hold below the shoulder.",
            json!({
                "cycle": cycle,
                "occluded_on_arrival": hidden,
                "occluded_at_hold_end": after,
                "flickered": hidden != after,
            }),
        );

        travel_to(ctx, (post.0, post.2), TRAVEL_HOP, 2.0).await;
        cycle += 1;
    }
}
