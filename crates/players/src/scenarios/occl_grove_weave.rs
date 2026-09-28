//! CATALOG: family=locomotion min_players=1 single_process=false world_edits=true scenery=none far_players=false

use super::*;

const SURFACE_SCAN: i32 = 64;

/// Resolve an open-sky standing target instead of trusting the current feet Y. A bot can be
/// temporarily below a cave lip; probing from above makes the target recover the natural surface.
fn surface_y(ctx: &Ctx, x: i32, z: i32, hint: i32) -> Option<i32> {
    for y in ((hint - SURFACE_SCAN)..=(hint + SURFACE_SCAN)).rev() {
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

/// Snap a route to the topmost measured surface, with a small lateral retry when a column is
/// occupied or unloaded.
fn route_target(ctx: &Ctx, desired: (f64, f64), hint: i32) -> Option<(f64, f64)> {
    const OFFSETS: [(i32, i32); 9] = [
        (0, 0),
        (1, 0),
        (-1, 0),
        (0, 1),
        (0, -1),
        (1, 1),
        (1, -1),
        (-1, 1),
        (-1, -1),
    ];
    let bx = desired.0.floor() as i32;
    let bz = desired.1.floor() as i32;
    OFFSETS.iter().find_map(|(dx, dz)| {
        let x = bx + dx;
        let z = bz + dz;
        let y = surface_y(ctx, x, z, hint)?;
        walkable_xz_near(ctx, (x as f64 + 0.5, z as f64 + 0.5), y)
    })
}

/// Weave across a distant picket so the subject behind it flickers in and out of view.
///
/// The revisit problem, this face of it: every other occlusion in this family is a single
/// disappearance and a single recovery. Here the sight line is cut and restored REPEATEDLY inside
/// one episode, because the occluder is far from the eye and close to the subject — so a few blocks
/// of sideways walking swings the sight line right across it. That is the pillar-grove case: what
/// leaves the frustum is a small part of the view, several times, while the aimed bearing moves by
/// about 36 deg between the centre station and an outer one — a third of the 102.5 deg horizontal
/// field of view, so every station's view still overlaps every other station's heavily.
///
/// The loop is the trajectory, and it never leaves the lane: the stations are lateral offsets from
/// one spot, so `displacement` stays tiny while `path_length` keeps growing — the direct evidence of
/// a closed trajectory the audit looks for. Recovery is verified by `look_until_facing` at every
/// station plus a sight-line raycast, so a station labelled clear or occluded is measured.
///
/// The geometry is the whole design and none of it is arbitrary:
///
/// - The picket sits at [`PICKET_DISTANCE`] and the subject at [`SUBJECT_DISTANCE`], both measured
///   from the lane. A sight line leaving a station at lateral offset `s` is only
///   `s * (1 - PICKET_DISTANCE / SUBJECT_DISTANCE)` off centre by the time it reaches the picket's
///   plane, which at 6 and 9.5 is `0.37 s`. So the picket's 1.5-block half-width covers stations out
///   to 4.1 blocks either side — far wider than the 1.5-block arrival slop `move_to` floors its
///   radius at, which is what makes "occluded at the centre station" a robust fact rather than a coin
///   flip. Putting the occluder near the SUBJECT rather than near the eye is the whole trick: an
///   occluder near the eye would need to be walked around, one near the subject is walked ACROSS.
/// - [`STATION_OFFSET`] of 7 leaves the outer stations 2.9 blocks clear of that shadow — again past
///   the arrival slop — and makes every leg a 7-block walk: one long press, well over the key-chatter
///   floor of 10 ticks.
/// - Both structures are 2 tall. From the lane the subject subtends about -10 deg to +2 deg
///   vertically and the picket -15 deg to +4 deg, so the picket covers it. A 3-tall subject would
///   poke over the top, and raising the picket to match would cost three more placements of standing
///   still.
/// - The picket is three columns wide, not two. Two columns straddle the axis unevenly once their
///   centres are rounded to cells, which pushes one edge of the shadow to within the arrival slop of
///   the centre station; three columns centre it.
const PICKET_MATERIAL: &str = "minecraft:oak_log";
const SUBJECT_MATERIAL: &str = "minecraft:stone_bricks";

const PICKET_DISTANCE: f64 = 6.0;
const SUBJECT_DISTANCE: f64 = 9.5;
const PICKET_LANES: [f64; 3] = [-1.0, 0.0, 1.0];
const STRUCTURE_HEIGHT: i64 = 2;
const STATION_OFFSET: f64 = 7.0;

/// Where the bot stands to raise each structure: two blocks short of it, so the cells are inside
/// interaction reach and none of them overlaps the placer's own hitbox (which vanilla always refuses
/// to build into, and `marker_base_near` therefore skips).
const BUILD_SETBACK: f64 = 2.0;

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

/// Station hold, and the per-bot stagger. Four stations per cycle against a 9 s hold means the holds
/// dominate the travel, which is what keeps the window-median displacement inside 5 blocks.
const STATION_HOLD_MS: i64 = 9_000;
const HOLD_STAGGER_MS: i64 = 800;

/// Lateral offsets walked, in multiples of [`STATION_OFFSET`]: centre, out, centre, out the other
/// way, so every leg is a full offset and the sight line crosses the picket's shadow boundary on
/// each one. Kept as integers so "is this the centre station" is an integer comparison rather than a
/// float one.
const STATIONS: [i32; 4] = [0, 1, 0, -1];

/// How far each bot's build axis is rotated off the outward bearing, in radians. Facing away from
/// the anchor already separates two bots on opposite sides of it; the rotation is what keeps a
/// larger swarm's sites apart, since several bots on the same side of the anchor would otherwise
/// share one outward direction. At 9.5 blocks out, 0.35 rad puts two neighbouring sites 6.5 blocks
/// apart, past `MIN_PEER_GAP`.
const AXIS_SPREAD_RAD: f64 = 0.35;

/// Whether the straight sight line from `eye` to `target` is interrupted by a solid block. Copied
/// into each scenario of this family rather than shared, following `wade_column_detour`'s precedent
/// of duplicating `ridge_valley_circuit`'s probe: no scenario file should break because a sibling
/// was dropped.
///
/// Half-block strides cannot step over a full cube, and `block_is_safe_support` reports false for an
/// UNLOADED chunk — the honest default, since the opposite convention would call every station
/// occluded during the post-spawn window `settle_on_ground` waits out.
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
    let lane = ctx.position().unwrap_or((anchor.0, 64.0, anchor.1));
    let post_y = surface_y(
        ctx,
        lane.0.floor() as i32,
        lane.2.floor() as i32,
        lane.1.floor() as i32,
    )
    .unwrap_or(lane.1.floor() as i32);

    // Build AWAY from the anchor, with a per-bot rotation on top. The subject has to sit 9.5 blocks
    // out for the picket-to-subject ratio above to work, and at `spread_range: 8` two bots aiming
    // inward would raise their pickets and subjects through each other — `marker_base_near` would
    // absorb the collision by shifting cells, quietly corrupting the geometry the whole scenario is
    // built on. Facing away puts the two sites on opposite sides of the anchor; the rotation keeps
    // larger swarms apart too. Rule 5 is paid separately, by the peer glance below.
    let toward = (anchor.0 - lane.0, anchor.1 - lane.2);
    let span = toward.0.hypot(toward.1);
    let base = if span > 1.0 {
        (-toward.1).atan2(-toward.0)
    } else {
        std::f64::consts::FRAC_PI_2
    };
    let axis = base
        + if global.is_multiple_of(2) {
            AXIS_SPREAD_RAD
        } else {
            -AXIS_SPREAD_RAD
        };
    let unit = (axis.cos(), axis.sin());
    let perp = (-unit.1, unit.0);
    let at = |distance: f64, sideways: f64| {
        (
            lane.0 + unit.0 * distance + perp.0 * sideways,
            lane.2 + unit.1 * distance + perp.1 * sideways,
        )
    };

    // The peer in the opening frame, before the camera commits to the build axis for the rest of
    // the episode.
    glance_at_nearest_peer(ctx, None).await;

    // Then look down the axis: the shot the flicker is scored against is established before either
    // structure exists.
    let far = at(SUBJECT_DISTANCE, 0.0);
    ctx.look_until_facing((far.0, post_y as f64 + 1.5, far.1), 8.0, 5_000)
        .await;

    // Subject first, from two blocks short of it. Doing the far structure first means the walk back
    // to the lane passes the picket site, so the picket build costs no extra travel.
    travel_to(
        ctx,
        route_target(ctx, at(SUBJECT_DISTANCE - BUILD_SETBACK, 0.0), post_y)
            .unwrap_or((lane.0, lane.2)),
        TRAVEL_HOP,
        2.0,
    )
    .await;
    let subject_cell = at(SUBJECT_DISTANCE, 0.0);
    let feet_y = ctx.position().map(|p| p.1).unwrap_or(post_y as f64);
    let raised = raise_landmark(
        ctx,
        (
            subject_cell.0.round() as i64,
            feet_y.round() as i64,
            subject_cell.1.round() as i64,
        ),
        STRUCTURE_HEIGHT,
        SUBJECT_MATERIAL,
    )
    .await;
    let subject = raised.map(|(_, point)| point).unwrap_or((
        subject_cell.0.round() + 0.5,
        feet_y + STRUCTURE_HEIGHT as f64 - 0.5,
        subject_cell.1.round() + 0.5,
    ));

    // The picket: three columns centred on the axis, so the lane's centre station looks
    // straight into wood rather than through a gap between two pillars.
    travel_to(
        ctx,
        route_target(ctx, at(PICKET_DISTANCE - BUILD_SETBACK, 0.0), post_y)
            .unwrap_or((lane.0, lane.2)),
        TRAVEL_HOP,
        2.0,
    )
    .await;
    let picket_feet = ctx.position().map(|p| p.1).unwrap_or(post_y as f64);
    let mut picket_columns = 0u32;
    for sideways in PICKET_LANES {
        let cell = at(PICKET_DISTANCE, sideways);
        if raise_landmark(
            ctx,
            (
                cell.0.round() as i64,
                picket_feet.round() as i64,
                cell.1.round() as i64,
            ),
            STRUCTURE_HEIGHT,
            PICKET_MATERIAL,
        )
        .await
        .is_some()
        {
            picket_columns += 1;
        }
    }
    ctx.record(
        TYPE_PLACE,
        &format!("vis-occl-grove-setup-{index}"),
        "Raise the distant subject and the picket that will keep cutting across it.",
        json!({
            "global_index": global,
            "role": "grove_occlusion",
            "axis_rad": r2(axis),
            "subject_placed": raised.is_some(),
            "subject": {"x": subject.0, "y": subject.1, "z": subject.2},
            "picket_columns_placed": picket_columns,
            "picket_distance": PICKET_DISTANCE,
            "subject_distance": SUBJECT_DISTANCE,
        }),
    );

    travel_to(
        ctx,
        route_target(ctx, (lane.0, lane.2), post_y).unwrap_or((lane.0, lane.2)),
        TRAVEL_HOP,
        2.0,
    )
    .await;

    let mut step = 0usize;
    while !ctx.target_met() {
        // Start each bot at a different point in the station cycle, so two POVs in one episode run
        // the same structure out of phase — the pairwise near-clones check reads exactly that.
        let slot = STATIONS[(step + global) % STATIONS.len()];
        let sideways = slot as f64 * STATION_OFFSET;
        travel_to(
            ctx,
            route_target(ctx, at(0.0, sideways), post_y).unwrap_or((lane.0, lane.2)),
            TRAVEL_HOP,
            1.8,
        )
        .await;
        let arrived = ctx.look_until_facing(subject, 5.0, 5_000).await;
        let eye = ctx.position().map(head_of);
        let hidden = eye.is_some_and(|e| los_blocked(ctx, e, subject));
        let subject_yaw = eye.map(|e| look_angles(e, subject).0).unwrap_or(0.0);
        ctx.record(
            if slot == 0 { TYPE_MOVE } else { "observe" },
            &format!("vis-occl-grove-station-{index}-{step}"),
            "Take the next lateral station and measure whether the picket is across the subject from here.",
            json!({
                "step": step,
                "station_offset": r2(sideways),
                "expected_behind_picket": slot == 0,
                "occluded": hidden,
                "camera_arrived": arrived,
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
            ctx.elapsed_ms() + STATION_HOLD_MS + global as i64 * HOLD_STAGGER_MS,
            SHUTTLE_LEG,
        )
        .await;
        // The hold shuttles four blocks off station, which on this geometry can cross the shadow
        // boundary. Record the flicker rather than claim the station held one state throughout.
        let after = ctx
            .position()
            .map(head_of)
            .is_some_and(|e| los_blocked(ctx, e, subject));
        ctx.record(
            "observe",
            &format!("vis-occl-grove-station-end-{index}-{step}"),
            "Re-probe the sight line at the end of the station hold.",
            json!({
                "step": step,
                "occluded_on_arrival": hidden,
                "occluded_at_hold_end": after,
                "flickered": hidden != after,
            }),
        );
        step += 1;
    }
}
