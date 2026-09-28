//! CATALOG: family=construction min_players=1 single_process=false world_edits=true scenery=none far_players=false

use super::*;

/// Build a wall across your own sight line, then step out from behind it and back.
///
/// The revisit problem, this face of it: the occluder is BUILT, so the occlusion event is
/// guaranteed on any terrain and its geometry is known to the frame that recorded it — unlike a
/// hill, a wall this script raised has a position, a width and a height in the action log. What
/// loops is the trajectory: a lateral step out to the peek station where the subject is visible,
/// and back behind the wall where it is not. The camera bearing differs by only ~30 deg between the
/// two stations, so the model is asked the sharp question — the view barely turned, yet the subject
/// vanished and came back — rather than the easy one, where a 150 deg turn explains everything.
///
/// The revisit is verified by `look_until_facing` (the camera provably arrived, not merely ran out
/// of deadline) plus a sight-line raycast at both stations, so "hidden here, visible there" is
/// recorded as measurement rather than intent.
const MATERIAL: &str = "minecraft:stone_bricks";

/// Subject: three tall, eight blocks out. Eight is far enough that the wall's angular size dwarfs
/// it, and the subject's bearing shifts by only ~32 deg across the peek step.
const SUBJECT_HEIGHT: i64 = 3;
const SUBJECT_DISTANCE: f64 = 8.0;

/// Wall geometry, all three numbers derived from the occlusion the wall has to actually achieve.
///
/// At 2 blocks out, a 2-tall wall spans about -39 deg to +11 deg vertically from an eye 1.62 above
/// its feet, which covers the 3-tall subject's -11.5 deg to +10 deg at 8 blocks. Pushed to 3 blocks
/// out the same wall only reaches +7 deg and the subject's top peeks over it, so the distance is not
/// a free parameter. Three lanes wide spans +/-37 deg horizontally, which absorbs the 1.5-block
/// arrival slop `move_to` floors its radius at — a one-lane wall would be lost with half a block of
/// drift.
const WALL_DISTANCE: f64 = 2.0;
const WALL_HEIGHT: i64 = 2;
const WALL_LANES: [i64; 3] = [-1, 0, 1];

/// Lateral offset of the peek station. From 5 blocks aside, the line to a subject 8 blocks ahead
/// crosses the wall's plane 3.75 blocks off centre — well clear of the 1.5-block half-width, so the
/// peek is robust rather than marginal.
const PEEK_OFFSET: f64 = 5.0;

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

/// Holds, and the per-bot stagger that keeps two bots running this script off each other's phase —
/// `eval_shared_unobserved` measured 80.2% against the 80% near-clones ceiling without one.
const PEEK_HOLD_MS: i64 = 8_500;
const BEHIND_HOLD_MS: i64 = 8_000;
const HOLD_STAGGER_MS: i64 = 900;

/// Whether the straight sight line from `eye` to `target` is interrupted by a solid block. Adapted
/// per-file rather than shared, following `wade_column_detour`'s precedent of copying
/// `ridge_valley_circuit`'s probe so each scenario file stays independently droppable.
///
/// Half-block strides cannot step over a full cube, and `block_is_safe_support` is false for an
/// UNLOADED chunk — the honest default here, since the opposite convention would report every
/// station as occluded during the post-spawn window `settle_on_ground` waits out.
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

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let anchor = ctx.anchor();
    let post = ctx.position().unwrap_or((anchor.0, 64.0, anchor.1));

    // Build OUTWARD, away from the anchor. Every other scenario in this family puts its subject a
    // few blocks off the post, where two bots' structures merge into one shared cluster the way
    // `eval_memory_dwell` intends. This one needs its subject eight blocks out for the wall's
    // angular size to cover it, and at `spread_range: 8` two bots aiming inward would then be
    // building through each other — one bot's wall landing between the other and its subject, with
    // `marker_base_near` quietly shifting cells to absorb the collision. Facing away puts the two
    // sites on opposite sides of the anchor instead. Rule 5 is paid separately, by the peer glance
    // below.
    let toward = (anchor.0 - post.0, anchor.1 - post.2);
    let span = toward.0.hypot(toward.1);
    let unit = if span > 1.0 {
        (-toward.0 / span, -toward.1 / span)
    } else {
        (0.0, 1.0)
    };
    let perp = (-unit.1, unit.0);

    // The peer in the opening frame, before the camera commits to the build axis for the rest of
    // the episode.
    glance_at_nearest_peer(ctx, None).await;
    let desired = (
        (post.0 + unit.0 * SUBJECT_DISTANCE).round() as i64,
        post.1.round() as i64,
        (post.2 + unit.1 * SUBJECT_DISTANCE).round() as i64,
    );

    // Establish the shot BEFORE building anything: the bearing the recovery is scored on has to be
    // on record from before the wall existed, and once the wall is up it is the one thing hidden.
    let subject_guess = (
        desired.0 as f64 + 0.5,
        post.1 + SUBJECT_HEIGHT as f64 - 0.5,
        desired.2 as f64 + 0.5,
    );
    ctx.look_until_facing(subject_guess, 6.0, 5_000).await;

    let raised = raise_landmark(ctx, desired, SUBJECT_HEIGHT, MATERIAL).await;
    let subject = raised.map(|(_, point)| point).unwrap_or(subject_guess);

    // The wall, one lane at a time from where the bot stands: 2 blocks out is inside interaction
    // reach, so no lane needs a walk. Six placements run ~10 s of standing, comfortably inside the
    // 20 s frozen window, and every excursion after this is movement.
    let mut lanes_up = 0u32;
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
            lanes_up += 1;
        }
    }
    ctx.record(
        TYPE_PLACE,
        &format!("vis-occl-wall-setup-{index}"),
        "Raise the wall this episode's sight line is cut by, across the subject's bearing.",
        json!({
            "global_index": global,
            "role": "wall_occlusion",
            "subject_placed": raised.is_some(),
            "subject": {"x": subject.0, "y": subject.1, "z": subject.2},
            "wall_lanes_placed": lanes_up,
            "wall_distance": WALL_DISTANCE,
            "wall_height": WALL_HEIGHT,
        }),
    );

    let mut cycle = 0usize;
    while !ctx.target_met() {
        // Alternate which side the peek happens on, phased by bot, so neither the episode nor the
        // pair of POVs degenerates into one repeated sidestep.
        let side = if (cycle + global).is_multiple_of(2) {
            1.0
        } else {
            -1.0
        };
        let peek = (
            post.0 + perp.0 * PEEK_OFFSET * side,
            post.2 + perp.1 * PEEK_OFFSET * side,
        );

        // --- peek station: step out from behind the wall and re-acquire the subject ---
        travel_to(ctx, peek, TRAVEL_HOP, 1.8).await;
        let arrived = ctx.look_until_facing(subject, 5.0, 5_000).await;
        let eye = ctx.position().map(head_of);
        let peek_clear = eye.is_some_and(|e| !los_blocked(ctx, e, subject));
        let peek_yaw = eye.map(|e| look_angles(e, subject).0).unwrap_or(0.0);
        ctx.record(
            "observe",
            &format!("vis-occl-wall-peek-{index}-{cycle}"),
            "Step clear of the wall and re-acquire the subject it was hiding.",
            json!({
                "cycle": cycle,
                "is_recovery": cycle > 0,
                "side": side,
                "camera_arrived": arrived,
                "sight_line_clear": peek_clear,
                "subject_yaw_deg": r2(peek_yaw),
                "elapsed_ms": ctx.elapsed_ms(),
            }),
        );
        // A steady shot before the shuttle takes the camera: `hold_facing`'s first leg hands it to
        // `CameraIntent::Hold`, which faces travel, so this is the only stretch that actually holds
        // the aimed bearing.
        ctx.still(AIMED_DWELL_MS).await;
        hold_facing(
            ctx,
            peek_yaw,
            ctx.elapsed_ms() + PEEK_HOLD_MS + global as i64 * HOLD_STAGGER_MS,
            SHUTTLE_LEG,
        )
        .await;
        if ctx.target_met() {
            break;
        }

        // --- back behind the wall: same bearing, no subject ---
        travel_to(ctx, (post.0, post.2), TRAVEL_HOP, 1.8).await;
        let eye = ctx.position().map(head_of);
        let hidden = eye.is_some_and(|e| los_blocked(ctx, e, subject));
        let behind_yaw = eye.map(|e| look_angles(e, subject).0).unwrap_or(peek_yaw);
        ctx.record(
            TYPE_MOVE,
            &format!("vis-occl-wall-behind-{index}-{cycle}"),
            "Return behind the wall and check the subject's sight line is really cut.",
            json!({
                "cycle": cycle,
                "occluded": hidden,
                "behind_yaw_deg": r2(behind_yaw),
                "bearing_shift_deg": r2((behind_yaw - peek_yaw + 540.0) % 360.0 - 180.0),
            }),
        );
        // A steady shot before the shuttle takes the camera: `hold_facing`'s first leg hands it to
        // `CameraIntent::Hold`, which faces travel, so this is the only stretch that actually holds
        // the aimed bearing.
        ctx.still(AIMED_DWELL_MS).await;
        hold_facing(
            ctx,
            behind_yaw,
            ctx.elapsed_ms() + BEHIND_HOLD_MS + global as i64 * HOLD_STAGGER_MS,
            SHUTTLE_LEG,
        )
        .await;
        // A four-block shuttle can leave the wall's shadow, which is a narrow one at this width.
        // Re-probe instead of asserting: a recorded flicker is usable, a false claim of continuous
        // occlusion is not.
        let still_hidden = ctx
            .position()
            .map(head_of)
            .is_some_and(|e| los_blocked(ctx, e, subject));
        ctx.record(
            "observe",
            &format!("vis-occl-wall-behind-end-{index}-{cycle}"),
            "Re-probe the sight line at the end of the hold behind the wall.",
            json!({
                "cycle": cycle,
                "occluded_on_arrival": hidden,
                "occluded_at_hold_end": still_hidden,
                "flickered": hidden != still_hidden,
            }),
        );
        cycle += 1;
    }
}
