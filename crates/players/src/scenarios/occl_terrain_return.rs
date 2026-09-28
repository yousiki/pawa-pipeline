//! CATALOG: family=locomotion min_players=1 single_process=false world_edits=true scenery=none far_players=false

use super::*;

/// Walk into the shadow of a natural rise until the subject is genuinely hidden, then come back
/// and re-face it.
///
/// The revisit problem, this face of it: the loop is the TRAJECTORY and the occluder is REAL
/// TERRAIN. The generation-5 eval scripts ran on Superflat, where an infinite plane hides nothing,
/// so "reconstruct the view you lost" was never actually asked of the model — the only thing that
/// ever left the frustum there was whatever the camera turned off. Here the camera barely turns:
/// the subject leaves the view because a crest of ground moves between the eye and it, and comes
/// back because the body walks back out. That is the occlusion-and-recovery event the corpus has
/// none of.
///
/// The revisit is checked two ways, so a failed return is never recorded as a success:
/// `look_until_facing` proves the camera really arrived on the subject's bearing (a plain `look`
/// only waits out a deadline), and a sight-line raycast recorded at every phase proves the subject
/// was actually hidden in between rather than merely off-centre.
///
/// Overlap is protected by WHERE THE STANDING SHOTS POINT, not by the shuttle. `hold_facing` gives
/// the camera to `CameraIntent::Hold` as soon as its first leg starts, and `Hold` faces travel — so
/// the aimed bearing only lasts while the bot is still. Both aimed shots in a cycle therefore point
/// at the same thing: the bearing to where the subject IS, hidden or not. Consecutive stations look
/// in nearly the same direction while the pixels behind that direction change completely, which is
/// exactly the "GT is an observed region" property that makes a pixel metric interpretable.
const MATERIAL: &str = "minecraft:stone_bricks";

/// Three blocks of subject: tall enough that the crest has to genuinely rise to cover it, short
/// enough to be one `raise_landmark` stand well inside the 20 s frozen window.
const SUBJECT_HEIGHT: i64 = 3;

/// How far out the subject goes. Inside interaction reach from the post, so the bot never has to
/// travel to build it, and far enough that its bearing barely moves as the body shuffles.
const SUBJECT_DISTANCE: f64 = 3.0;

/// One shuttle leg, in blocks. 4 clears the key-chatter floor at sprint speed and matches
/// Solaris's 3-4 block random-walk primitive; see `hold_facing`.
const SHUTTLE_LEG: f64 = 4.0;

/// Vertical window a surface probe scans around the bot's own altitude. Same span
/// `ridge_valley_circuit` uses: wide enough for real relief, narrow enough that the probe cannot
/// wander into a cave system below.
const SCAN_SPAN: i32 = 20;

/// Distance out to the shadow station. Deliberately short: `displacement` has to stay under ~5
/// blocks and the holds have to outweigh the travel, so a 10-block excursion against two 8 s holds
/// is roughly the longest trip this budget allows.
const STATION_RADIUS: f64 = 10.0;

/// Where along a candidate bearing the crest is looked for. A crest closer than this is inside the
/// subject's own footprint; further out and the station would have to be beyond it.
const CREST_PROBES: [f64; 3] = [4.0, 6.0, 8.0];

/// Candidate bearings sampled per excursion, spread over `±BEARING_SPAN` radians around the
/// direction away from the subject. Seven is enough to find a usable rise on ordinary overworld
/// terrain without turning the probe into a chunk-read storm.
const BEARING_SAMPLES: usize = 7;
const BEARING_SPAN: f64 = 1.2;

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

/// Holds, and the per-bot stagger on them. The stagger is what keeps two bots running the same
/// script off each other's phase: `eval_shared_unobserved` measured 80.2% action-stream agreement
/// against an 80% near-clones ceiling until it staggered exactly like this.
const FACE_HOLD_MS: i64 = 8_500;
const SHADOW_HOLD_MS: i64 = 8_000;
const HOLD_STAGGER_MS: i64 = 900;

/// Radians the search arc rotates per cycle, so a cycle that found no real shadow tries different
/// ground next time instead of re-walking the same failure.
const ARC_STEP: f64 = 0.9;

/// Whether the straight sight line from `eye` to `target` is interrupted by a solid block.
///
/// Sampled at half-block strides rather than a true DDA: the only question is "is the subject
/// hidden", and a stride that short cannot step over a full cube. Occlusion is judged by
/// `block_is_safe_support` (a full collision cube), which is also the honest answer for an
/// UNLOADED chunk — it reports false, so an unloaded reading counts as not blocking. The opposite
/// convention would make every candidate station trivially "occluded" during the post-spawn window
/// `settle_on_ground` exists to wait out.
fn los_blocked(ctx: &Ctx, eye: (f64, f64, f64), target: (f64, f64, f64)) -> bool {
    let (dx, dy, dz) = (target.0 - eye.0, target.1 - eye.1, target.2 - eye.2);
    let span = (dx * dx + dy * dy + dz * dz).sqrt();
    let steps = (span * 2.0).ceil() as i32;
    // Skip the endpoints: the eye's own cell and the subject's own block are not occluders.
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

/// Feet altitude of the open-sky surface in column `(x, z)`, or `None` when the column is water,
/// lava or unresolvable. Adapted from `ridge_valley_circuit::skylit_feet_y`: scanning DOWNWARD from
/// above the bot's own altitude makes the first support the topmost one, so a probe can never
/// resolve to a cave floor and hand back a station under the ground.
fn surface_feet_y(ctx: &Ctx, x: i32, z: i32, y_hint: i32) -> Option<i32> {
    for y in ((y_hint - SCAN_SPAN)..=(y_hint + SCAN_SPAN)).rev() {
        let cell = BlockPos::new(x, y, z);
        let id = ctx.block_id_at(cell)?;
        if id.contains("water") || id.contains("lava") {
            return None;
        }
        if block_is_safe_support(ctx, cell) {
            return Some(y + 1);
        }
    }
    None
}

/// Pick the bearing whose terrain profile is a RISE FOLLOWED BY A DROP, and report the relief it
/// found: `(bearing, crest_above_post, station_above_post)`.
///
/// Only that shape puts geometry between the station and the subject. A bearing that merely climbs
/// leaves the bot on top of the rise looking DOWN at the subject — nothing is occluded — so the
/// score is how far the crest clears the HIGHER of the two endpoints, which is the surface the
/// sight line actually has to graze. A flat arc scores zero and the caller records the excursion as
/// unoccluded rather than pretending.
fn shadow_bearing(ctx: &Ctx, post: (f64, f64, f64), centre_rad: f64) -> (f64, i32, i32) {
    let y_hint = post.1.floor() as i32;
    let post_y =
        surface_feet_y(ctx, post.0.floor() as i32, post.2.floor() as i32, y_hint).unwrap_or(y_hint);
    let mut best = (centre_rad, i32::MIN, 0, 0);
    for sample in 0..BEARING_SAMPLES {
        let bearing = centre_rad - BEARING_SPAN
            + 2.0 * BEARING_SPAN * sample as f64 / (BEARING_SAMPLES - 1) as f64;
        let column = |distance: f64| {
            surface_feet_y(
                ctx,
                (post.0 + bearing.cos() * distance).floor() as i32,
                (post.2 + bearing.sin() * distance).floor() as i32,
                y_hint,
            )
        };
        let Some(station_y) = column(STATION_RADIUS) else {
            continue; // water, lava, or unloaded: not a station.
        };
        let Some(crest_y) = CREST_PROBES.iter().filter_map(|&d| column(d)).max() else {
            continue;
        };
        let score = crest_y - post_y.max(station_y);
        if score > best.1 {
            best = (bearing, score, crest_y - post_y, station_y - post_y);
        }
    }
    (best.0, best.2, best.3)
}

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let anchor = ctx.anchor();
    let post = ctx.position().unwrap_or((anchor.0, 64.0, anchor.1));

    // Subject on the ANCHOR side, as in `eval_grounding_turn_back`: with the swarm's spread pulled
    // in to 8 blocks, pointing every bot inward is what puts the scene's subject in every POV's
    // opening frame instead of an arbitrary horizon (hard rule 5).
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
    // Excursions leave on the far side of the subject, so the subject is never between the bot and
    // the ground it walks over — the crest has to be.
    // Math bearing, matching `ridge_valley_circuit`'s convention (x = cos, z = sin) that every
    // terrain probe in this file samples with; `+PI` turns it away from the subject.
    let away = unit.1.atan2(unit.0) + std::f64::consts::PI;
    ctx.record(
        "observe",
        &format!("vis-occl-terrain-setup-{index}"),
        "Raise the subject every sight-line recovery in this episode is scored against.",
        json!({
            "global_index": global,
            "role": "terrain_occlusion",
            "subject_placed": raised.is_some(),
            "subject": {"x": subject.0, "y": subject.1, "z": subject.2},
            "station_radius": STATION_RADIUS,
        }),
    );

    let mut cycle = 0usize;
    while !ctx.target_met() {
        // --- grounding, and from cycle 1 on the RECOVERY this scenario exists to produce ---
        let arrived = ctx.look_until_facing(subject, 5.0, 5_000).await;
        let eye = ctx.position().map(head_of);
        let visible = eye.is_some_and(|e| !los_blocked(ctx, e, subject));
        let subject_yaw = eye.map(|e| look_angles(e, subject).0).unwrap_or(0.0);
        ctx.record(
            "observe",
            &format!("vis-occl-terrain-face-{index}-{cycle}"),
            "Face the subject: the opening grounding, or the sight line recovered after occlusion.",
            json!({
                "cycle": cycle,
                "is_recovery": cycle > 0,
                "camera_arrived": arrived,
                "sight_line_clear": visible,
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
            ctx.elapsed_ms() + FACE_HOLD_MS + global as i64 * HOLD_STAGGER_MS,
            SHUTTLE_LEG,
        )
        .await;
        if ctx.target_met() {
            break;
        }

        // --- excursion into the crest's shadow ---
        let arc = away + cycle as f64 * ARC_STEP + global as f64 * 0.4;
        let (bearing, crest_relief, station_relief) = shadow_bearing(ctx, post, arc);
        let station = (
            post.0 + bearing.cos() * STATION_RADIUS,
            post.2 + bearing.sin() * STATION_RADIUS,
        );
        let remaining = travel_to(ctx, station, TRAVEL_HOP, 2.5).await;
        let eye = ctx.position().map(head_of);
        let hidden = eye.is_some_and(|e| los_blocked(ctx, e, subject));
        // The bearing to where the subject IS, occluded or not. Aiming there is the honest framing
        // of the event — the camera looks at the ground that took the subject away — and it is what
        // makes the two steady shots in a cycle point almost the same way.
        let hidden_yaw = eye
            .map(|e| look_angles(e, subject).0)
            .unwrap_or(subject_yaw);
        ctx.record(
            TYPE_MOVE,
            &format!("vis-occl-terrain-shadow-{index}-{cycle}"),
            "Walk into the shadow of the sampled crest and check the sight line is really cut.",
            json!({
                "cycle": cycle,
                "bearing_rad": r2(bearing),
                "crest_above_post": crest_relief,
                "station_above_post": station_relief,
                "occluded": hidden,
                "station_miss": r2(remaining),
            }),
        );
        // A steady shot before the shuttle takes the camera: `hold_facing`'s first leg hands it to
        // `CameraIntent::Hold`, which faces travel, so this is the only stretch that actually holds
        // the aimed bearing.
        ctx.still(AIMED_DWELL_MS).await;
        hold_facing(
            ctx,
            hidden_yaw,
            ctx.elapsed_ms() + SHADOW_HOLD_MS + global as i64 * HOLD_STAGGER_MS,
            SHUTTLE_LEG,
        )
        .await;
        // `hold_facing` shuttles four blocks off the station and back, which on a narrow shadow can
        // step out of it. Re-probe rather than assert: a flicker is a fact about the episode, and a
        // recorded flicker is usable where a false "occluded the whole time" claim is not.
        let still_hidden = ctx
            .position()
            .map(head_of)
            .is_some_and(|e| los_blocked(ctx, e, subject));
        ctx.record(
            "observe",
            &format!("vis-occl-terrain-shadow-end-{index}-{cycle}"),
            "Re-probe the sight line at the end of the shadow hold.",
            json!({
                "cycle": cycle,
                "occluded_on_arrival": hidden,
                "occluded_at_hold_end": still_hidden,
                "flickered": hidden != still_hidden,
            }),
        );

        travel_to(ctx, (post.0, post.2), TRAVEL_HOP, 2.0).await;
        cycle += 1;
    }
}
