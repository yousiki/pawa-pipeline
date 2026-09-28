//! CATALOG: family=locomotion min_players=1 single_process=false world_edits=true scenery=none far_players=false

use super::*;

/// Wade off the shore until the water line takes the subject, then come back up onto dry ground.
///
/// The revisit problem, this face of it: wading is the only mechanism in the family that moves the
/// EYE vertically without the camera doing anything. A player who steps off a bank drops from
/// standing eye height to swimming eye height in under a second, the shore lip rises through the
/// frame, and the subject on the bank goes behind it — then the same walk in reverse brings it
/// back. Nothing in the corpus contains that: `wade_column_detour` exists specifically to route
/// AROUND water, because the pathfinder will not plan through it.
///
/// Which is also why the entry is open-loop. `move_to` cannot be given a water target — that is the
/// whole reason `wade_column_detour` skirts ponds — so the wade is short `hold_visual` bouts toward
/// the water, checked between bouts and stopped the moment the feet are wet. The bouts are 900 ms
/// each: long enough to clear the key-chatter floor (10 ticks = 500 ms) and short enough that the
/// bot never swims out past ground it can walk back over.
///
/// Real terrain is not obliged to provide water. When the ring search finds none the script raises a
/// wall instead and records `occluder_kind: "wall"`, so the episode still contains an occlusion
/// event rather than a walk to nowhere.
const MATERIAL: &str = "minecraft:stone_bricks";

/// Subject: three tall at three blocks, inside interaction reach so the bot never travels to build.
const SUBJECT_HEIGHT: i64 = 3;
const SUBJECT_DISTANCE: f64 = 3.0;

/// Ring searched for a water column, and how far back from it the dry standing cell is looked for.
/// The far edge stays short because `displacement` has to hold under ~5 blocks and the dry holds
/// have to outweigh the travel.
const WATER_MIN_RADIUS: f64 = 5.0;
const WATER_MAX_RADIUS: f64 = 13.0;
const RADIUS_STEP: f64 = 2.0;
const BEARING_STEPS: usize = 12;
const SHORE_SETBACK: [f64; 3] = [2.0, 3.0, 4.0];

/// Open-loop wade: at most this many bouts of this length, checked between each. Two bouts of
/// walking pace put the bot a few blocks into the shallows — far enough for the water line to rise
/// through the frame, not far enough to be swimming somewhere the planner cannot return from.
const WADE_BOUTS: usize = 2;
const WADE_BOUT_MS: u64 = 1_400;

/// Vertical window a surface probe scans, matching `ridge_valley_circuit`'s span.
const SCAN_SPAN: i32 = 64;

/// Fallback wall geometry, same derivation as `occl_wall_peekaboo`: 2 blocks out is where a 2-tall
/// wall still reaches above eye level, and three lanes give a shadow wide enough to absorb
/// `move_to`'s 1.5-block arrival slop.
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
/// Only the DRY hold is long: a hold in the water would shuttle with `move_to`, which is exactly the
/// call that cannot plan there.
const SHORE_HOLD_MS: i64 = 9_000;
const WET_DWELL_MS: u64 = 2_500;
const HOLD_STAGGER_MS: i64 = 900;

/// Whether the straight sight line from `eye` to `target` is interrupted by a solid block. Copied
/// into each scenario of this family rather than shared, following `wade_column_detour`'s precedent
/// of duplicating `ridge_valley_circuit`'s probe.
///
/// Water is deliberately NOT an occluder here: it is not a full collision cube, so the raycast walks
/// straight through it. Being underwater is recorded separately as `eye_submerged`, because "the
/// view is green" and "the bank is in the way" are different events and the log keeps them apart.
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

/// Feet altitude of the open-sky dry-land surface in column `(x, z)`. Adapted from
/// `wade_column_detour::land_feet_y`, which adapted it from `ridge_valley_circuit`: scanning
/// downward makes the first support the topmost, and water, lava and canopy are refused because
/// none of them is a place to stand.
fn land_feet_y(ctx: &Ctx, x: i32, z: i32, y_hint: i32) -> Option<i32> {
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

/// Whether the topmost non-air block of column `(x, z)` is water — i.e. this column is open water
/// rather than land with a puddle buried under it.
fn is_open_water(ctx: &Ctx, x: i32, z: i32, y_hint: i32) -> bool {
    for y in ((y_hint - SCAN_SPAN)..=(y_hint + SCAN_SPAN)).rev() {
        let cell = BlockPos::new(x, y, z);
        let Some(id) = ctx.block_id_at(cell) else {
            return false; // unloaded: not evidence of anything
        };
        if block_is_air(ctx, cell) {
            continue;
        }
        return id.contains("water");
    }
    false
}

/// Whether the bot's feet are in water, and whether its eyes are. Adapted from
/// `wade_column_detour::in_water`, extended with the eye cell because the eye going under is the
/// visual event this scenario is built around.
fn wetness(ctx: &Ctx) -> (bool, bool) {
    let Some(p) = ctx.position() else {
        return (false, false);
    };
    let wet = |y: f64| {
        ctx.block_id_at(BlockPos::new(
            p.0.floor() as i32,
            y.floor() as i32,
            p.2.floor() as i32,
        ))
        .is_some_and(|id| id.contains("water"))
    };
    (wet(p.1), wet(p.1 + 1.62))
}

/// Find open water in the ring around `post` and the dry cell to wade in from: `(water, shore)`.
fn water_and_shore(
    ctx: &Ctx,
    post: (f64, f64, f64),
    phase: f64,
) -> Option<((f64, f64), (f64, f64))> {
    let y_hint = post.1.floor() as i32;
    let mut radius = WATER_MIN_RADIUS;
    while radius <= WATER_MAX_RADIUS {
        for slot in 0..BEARING_STEPS {
            let bearing = phase + std::f64::consts::TAU * slot as f64 / BEARING_STEPS as f64;
            let water = (
                post.0 + bearing.cos() * radius,
                post.2 + bearing.sin() * radius,
            );
            if !is_open_water(ctx, water.0.floor() as i32, water.1.floor() as i32, y_hint) {
                continue;
            }
            // Step back along the same bearing until the column is land the bot can stand on. A
            // shore cell further back than this is not a shore, it is the middle of a field.
            // A water column with no dry cell behind it is a pond seen across another pond: keep
            // scanning rather than abandoning the search, which an early `?` here would do.
            let shore = SHORE_SETBACK.iter().find_map(|&back| {
                let cell = (
                    post.0 + bearing.cos() * (radius - back),
                    post.2 + bearing.sin() * (radius - back),
                );
                land_feet_y(ctx, cell.0.floor() as i32, cell.1.floor() as i32, y_hint).map(|_| cell)
            });
            if let Some(shore) = shore {
                return Some((water, shore));
            }
        }
        radius += RADIUS_STEP;
    }
    None
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
        post.1 + 2.0,
        desired.2 as f64 + 0.5,
    ));

    // Resolved once: the ring scan reads a few hundred columns and the shoreline does not move.
    // Each bot starts its sweep on its own bearing so two bots do not wade in at the same cell.
    let found = water_and_shore(ctx, post, global as f64 * 1.7);
    let mut wall_lanes = 0u32;
    let (shore, water, occluder_kind) = match found {
        Some((water, shore)) => (shore, Some(water), "water"),
        None => {
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
                None,
                "wall",
            )
        }
    };
    ctx.record(
        "observe",
        &format!("vis-occl-wade-setup-{index}"),
        "Raise the subject and resolve the shore this episode wades off.",
        json!({
            "global_index": global,
            "role": "wade_occlusion",
            "subject_placed": raised.is_some(),
            "subject": {"x": subject.0, "y": subject.1, "z": subject.2},
            "occluder_kind": occluder_kind,
            "shore": {"x": r2(shore.0), "z": r2(shore.1)},
            "water": water.map(|w| json!({"x": r2(w.0), "z": r2(w.1)})),
            "wall_lanes_placed": wall_lanes,
        }),
    );

    let mut cycle = 0usize;
    while !ctx.target_met() {
        // --- dry ground: grounding, and from cycle 1 the view regained on climbing back out ---
        let arrived = ctx.look_until_facing(subject, 5.0, 5_000).await;
        let dry = ctx.position();
        let eye = dry.map(head_of);
        let clear = eye.is_some_and(|e| !los_blocked(ctx, e, subject));
        let subject_yaw = eye.map(|e| look_angles(e, subject).0).unwrap_or(0.0);
        ctx.record(
            "observe",
            &format!("vis-occl-wade-dry-{index}-{cycle}"),
            "Stand on dry ground facing the subject: the grounding, or the view regained after wading back.",
            json!({
                "cycle": cycle,
                "is_recovery": cycle > 0,
                "camera_arrived": arrived,
                "sight_line_clear": clear,
                "subject_yaw_deg": r2(subject_yaw),
                "eye_y": eye.map(|e| r2(e.1)),
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
            ctx.elapsed_ms() + SHORE_HOLD_MS + global as i64 * HOLD_STAGGER_MS,
            SHUTTLE_LEG,
        )
        .await;
        if ctx.target_met() {
            break;
        }

        // --- out to the shore, then off it ---
        travel_to(ctx, shore, TRAVEL_HOP, 2.0).await;
        let mut bouts = 0usize;
        if let Some(water) = water {
            // Aim at the water surface, then walk in open-loop: the planner has no route into
            // water, so this is the only way in. Checked between bouts so the wade stops at the
            // first wet frame instead of running its whole budget.
            if let Some(p) = ctx.position() {
                ctx.look(
                    (water.0, p.1 + 0.2, water.1),
                    ctx.cfg().look_settle_ms.max(600),
                )
                .await;
            }
            while bouts < WADE_BOUTS && !ctx.target_met() {
                ctx.hold_visual(WalkDirection::Forward, false, WADE_BOUT_MS, 0)
                    .await;
                bouts += 1;
                if wetness(ctx).0 {
                    break;
                }
            }
        }
        let wet = ctx.position();
        let (feet_wet, eye_submerged) = wetness(ctx);
        let eye = wet.map(head_of);
        let hidden = eye.is_some_and(|e| los_blocked(ctx, e, subject));
        let wet_yaw = eye
            .map(|e| look_angles(e, subject).0)
            .unwrap_or(subject_yaw);
        ctx.record(
            TYPE_MOVE,
            &format!("vis-occl-wade-wet-{index}-{cycle}"),
            "Wade off the bank and measure how far the eye dropped and whether the subject went with it.",
            json!({
                "cycle": cycle,
                "occluder_kind": occluder_kind,
                "wade_bouts": bouts,
                "feet_in_water": feet_wet,
                "eye_submerged": eye_submerged,
                "occluded": hidden,
                "eye_y": eye.map(|e| r2(e.1)),
                "eye_drop": match (dry, wet) { (Some(d), Some(w)) => Some(r2(d.1 - w.1)), _ => None },
                "wet_yaw_deg": r2(wet_yaw),
            }),
        );
        // Face back at the bank the subject stands on, and dwell WITHOUT shuttling: `hold_facing`
        // drives `move_to`, and `move_to` is precisely the call that has no route in water.
        ctx.look_until_facing(subject, 8.0, 4_000).await;
        sleep_ms(WET_DWELL_MS).await;

        // Climb back out. From dry ground the planner handles it; from the water it needs a shove
        // in the right direction first, which is the same open-loop trick that got the bot in.
        if feet_wet {
            if let Some(p) = ctx.position() {
                ctx.look(
                    (
                        p.0 + (shore.0 - p.0) * 4.0,
                        p.1 + 1.4,
                        p.2 + (shore.1 - p.2) * 4.0,
                    ),
                    ctx.cfg().look_settle_ms.max(500),
                )
                .await;
            }
            ctx.hold_visual(WalkDirection::Forward, false, WADE_BOUT_MS, 0)
                .await;
        }
        travel_to(ctx, (post.0, post.2), TRAVEL_HOP, 2.0).await;
        cycle += 1;
    }
}
