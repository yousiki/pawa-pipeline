//! CATALOG: family=locomotion min_players=1 single_process=false world_edits=true scenery=none far_players=false

use super::*;

/// Duck into a roofed pocket — tree shade, a rock overhang, a cave mouth — and come back out to the
/// same subject.
///
/// The revisit problem, this face of it: this is the only occlusion mechanism in the family where
/// the geometry closes OVERHEAD as well as ahead. Stepping under a canopy replaces the sky with
/// leaves and the horizon with trunks, which changes global illumination and the entire background
/// at once, while the subject the bot walks back out to is unchanged. A model that merely
/// extrapolates the last frame's colour statistics cannot survive that transition and then
/// reproduce the original view; one that actually remembers the scene can.
///
/// The loop is the trajectory (in and out of the pocket) and the recovery is verified twice:
/// `look_until_facing` proves the camera arrived back on the subject's bearing, and a sight-line
/// raycast at both stations proves the subject was hidden inside and visible outside. Overhead
/// enclosure is recorded separately from sight-line occlusion, because a pocket can shade the bot
/// without hiding the subject and the two facts are worth telling apart.
///
/// Real terrain is not obliged to provide a pocket. When the search finds none the script raises a
/// wall instead and records `shelter_kind: "wall"`, so the episode still contains a genuine
/// occlusion event rather than a walk to nowhere.
const MATERIAL: &str = "minecraft:oak_planks";

/// Subject: three tall at three blocks, inside interaction reach so the bot never travels to build.
const SUBJECT_HEIGHT: i64 = 3;
const SUBJECT_DISTANCE: f64 = 3.0;

/// Search ring for a roofed cell. Under 5 blocks the pocket would overlap the subject; past 12 the
/// excursion costs more travel than the holds can balance against a 5-block displacement median.
const SHELTER_MIN_RADIUS: f64 = 5.0;
const SHELTER_MAX_RADIUS: f64 = 12.0;
const RADIUS_STEPS: usize = 3;
const BEARING_STEPS: usize = 12;

/// Height band above the feet that has to be solid for a cell to count as roofed. Below 3 the bot
/// could not stand there at all; above 7 the "roof" is too high to shade or frame anything.
const ROOF_BAND: (i32, i32) = (3, 7);

/// Vertical window a surface probe scans, matching `ridge_valley_circuit`'s span.
const SCAN_SPAN: i32 = 20;

/// Fallback wall geometry. Same derivation as `occl_wall_peekaboo`: 2 blocks out is the distance at
/// which a 2-tall wall still reaches above eye level, and three lanes give a shadow wide enough to
/// absorb `move_to`'s 1.5-block arrival slop.
const WALL_DISTANCE: f64 = 2.0;
const WALL_HEIGHT: i64 = 2;
const WALL_LANES: [i64; 3] = [-1, 0, 1];

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
const OPEN_HOLD_MS: i64 = 8_500;
const SHELTER_HOLD_MS: i64 = 8_000;
const HOLD_STAGGER_MS: i64 = 900;

/// Whether the straight sight line from `eye` to `target` is interrupted by a solid block. Copied
/// into each scenario of this family rather than shared, following `wade_column_detour`'s precedent
/// of duplicating `ridge_valley_circuit`'s probe: no scenario file should break because a sibling
/// was dropped.
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

/// Feet altitude of the open-sky GROUND surface in column `(x, z)`, or `None` for water, lava,
/// canopy or an unresolvable column. Adapted from `ridge_valley_circuit::skylit_feet_y`: the
/// downward scan makes the first support the topmost, so a probe cannot hand back a cave floor as a
/// station, and leaves or a trunk as that topmost support means the probe is standing ON the canopy
/// this scenario wants to stand UNDER.
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

/// Find a standable cell in the ring around `post` that has a solid ceiling in [`ROOF_BAND`] and
/// two blocks of headroom to stand in, and report it with the block id of its roof.
///
/// Note the ordering: the surface probe rejects a column whose TOPMOST support is leaves or a
/// trunk, because that is a canopy the bot would be standing on rather than under. What this search
/// wants is a column whose ground is ground and whose roof is somewhere above — exactly the shape of
/// tree shade, an overhang lip, or a cave mouth.
fn roofed_cell_near(ctx: &Ctx, post: (f64, f64, f64), phase: f64) -> Option<((f64, f64), String)> {
    let y_hint = post.1.floor() as i32;
    for step in 0..RADIUS_STEPS {
        let radius = SHELTER_MIN_RADIUS
            + (SHELTER_MAX_RADIUS - SHELTER_MIN_RADIUS) * step as f64
                / (RADIUS_STEPS - 1).max(1) as f64;
        for slot in 0..BEARING_STEPS {
            let bearing = phase + std::f64::consts::TAU * slot as f64 / BEARING_STEPS as f64;
            let (x, z) = (
                post.0 + bearing.cos() * radius,
                post.2 + bearing.sin() * radius,
            );
            let (bx, bz) = (x.floor() as i32, z.floor() as i32);
            let Some(feet_y) = surface_feet_y(ctx, bx, bz, y_hint) else {
                continue;
            };
            // Headroom first: a pocket the bot cannot stand up in is not a station.
            if !block_is_air(ctx, BlockPos::new(bx, feet_y, bz))
                || !block_is_air(ctx, BlockPos::new(bx, feet_y + 1, bz))
            {
                continue;
            }
            let roof = (ROOF_BAND.0..=ROOF_BAND.1)
                .map(|dy| BlockPos::new(bx, feet_y + dy, bz))
                .find(|&cell| block_is_safe_support(ctx, cell))
                .and_then(|cell| ctx.block_id_at(cell));
            if let Some(roof) = roof {
                return Some(((x, z), roof));
            }
        }
    }
    None
}

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let anchor = ctx.anchor();
    let post = ctx.position().unwrap_or((anchor.0, 64.0, anchor.1));

    // Subject on the anchor side, so with `spread_range: 8` every POV's opening frame contains the
    // scene's subject rather than an arbitrary horizon (hard rule 5).
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

    // Start each bot's ring search on its own bearing, so two bots in one episode do not converge
    // on the same pocket and violate `MIN_PEER_GAP`.
    let phase = global as f64 * 1.7;
    let found = roofed_cell_near(ctx, post, phase);
    let mut wall_lanes = 0u32;
    let (shelter, shelter_kind, roof_block) = match found {
        Some((cell, roof)) => (cell, "canopy", Some(roof)),
        None => {
            // No natural pocket in reach: raise one side of a wall and use the ground behind it.
            // The excursion then still crosses a real occlusion boundary, and `shelter_kind` says
            // which mechanism the footage actually shows.
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
            // Behind the wall means on the far side from the subject: the wall sits between the
            // post and the subject, so the shelter station is the post's other side.
            (
                (
                    post.0 - unit.0 * SHELTER_MIN_RADIUS,
                    post.2 - unit.1 * SHELTER_MIN_RADIUS,
                ),
                "wall",
                None,
            )
        }
    };
    ctx.record(
        "observe",
        &format!("vis-occl-canopy-setup-{index}"),
        "Raise the subject and resolve the roofed pocket this episode ducks into.",
        json!({
            "global_index": global,
            "role": "canopy_occlusion",
            "subject_placed": raised.is_some(),
            "subject": {"x": subject.0, "y": subject.1, "z": subject.2},
            "shelter_kind": shelter_kind,
            "shelter": {"x": r2(shelter.0), "z": r2(shelter.1)},
            "roof_block": roof_block,
            "wall_lanes_placed": wall_lanes,
        }),
    );

    let mut cycle = 0usize;
    while !ctx.target_met() {
        // --- out in the open: grounding, and from cycle 1 the recovery being scored ---
        let arrived = ctx.look_until_facing(subject, 5.0, 5_000).await;
        let eye = ctx.position().map(head_of);
        let clear = eye.is_some_and(|e| !los_blocked(ctx, e, subject));
        let subject_yaw = eye.map(|e| look_angles(e, subject).0).unwrap_or(0.0);
        ctx.record(
            "observe",
            &format!("vis-occl-canopy-open-{index}-{cycle}"),
            "Stand in the open facing the subject: the grounding, or the view recovered on exit.",
            json!({
                "cycle": cycle,
                "is_recovery": cycle > 0,
                "camera_arrived": arrived,
                "sight_line_clear": clear,
                "subject_yaw_deg": r2(subject_yaw),
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
            ctx.elapsed_ms() + OPEN_HOLD_MS + global as i64 * HOLD_STAGGER_MS,
            SHUTTLE_LEG,
        )
        .await;
        if ctx.target_met() {
            break;
        }

        // --- inside the pocket ---
        travel_to(ctx, shelter, TRAVEL_HOP, 2.0).await;
        let inside = ctx.position();
        let eye = inside.map(head_of);
        let hidden = eye.is_some_and(|e| los_blocked(ctx, e, subject));
        // Overhead enclosure, measured where the bot actually ended up rather than where it aimed:
        // shade and sight-line occlusion are independent facts and the log keeps them apart.
        let sky_blocked = inside.is_some_and(|p| {
            let (bx, by, bz) = (p.0.floor() as i32, p.1.floor() as i32, p.2.floor() as i32);
            (ROOF_BAND.0..=ROOF_BAND.1)
                .any(|dy| block_is_safe_support(ctx, BlockPos::new(bx, by + dy, bz)))
        });
        let inside_yaw = eye
            .map(|e| look_angles(e, subject).0)
            .unwrap_or(subject_yaw);
        ctx.record(
            TYPE_MOVE,
            &format!("vis-occl-canopy-inside-{index}-{cycle}"),
            "Duck into the pocket and measure both the roof overhead and the cut sight line.",
            json!({
                "cycle": cycle,
                "shelter_kind": shelter_kind,
                "occluded": hidden,
                "sky_blocked": sky_blocked,
                "inside_yaw_deg": r2(inside_yaw),
            }),
        );
        // Aim OUT of the pocket, along the bearing the subject is on, and take a steady shot of it
        // before the shuttle hands the camera to `Hold`. Both stations in a cycle aim along nearly
        // the same bearing, so consecutive steady shots overlap while everything behind that bearing
        // has changed.
        ctx.still(AIMED_DWELL_MS).await;
        hold_facing(
            ctx,
            inside_yaw,
            ctx.elapsed_ms() + SHELTER_HOLD_MS + global as i64 * HOLD_STAGGER_MS,
            SHUTTLE_LEG,
        )
        .await;
        let after = ctx
            .position()
            .map(head_of)
            .is_some_and(|e| los_blocked(ctx, e, subject));
        ctx.record(
            "observe",
            &format!("vis-occl-canopy-inside-end-{index}-{cycle}"),
            "Re-probe the sight line at the end of the hold inside the pocket.",
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
