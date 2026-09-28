//! CATALOG: family=group_motion min_players=2 single_process=true world_edits=true scenery=none far_players=false

use super::*;

/// Two opposed cameras share one physical landmark while each sees the side the other cannot.
///
/// The revisit problem's multi-view face is explicit here: the same block-built landmark is
/// observed from opposing bearings, then each bot briefly checks the other player and returns to
/// the landmark. That produces a closed gaze loop without asking either camera to invent a hidden
/// side. The bodies perform short lateral out-and-back bouts around fixed stations, so the
/// trajectory also closes and still contributes moving frames. Natural terrain is never replaced;
/// it supplies the background and any incidental occlusion. The event records carry baseline,
/// subject distance, relative bearing and co-visibility for pairing the opposing views.
///
/// Unlike `stereo_antipodal_orbit`, the bodies do not orbit: the opposing baseline is fixed and only
/// the small body shuffle changes. Unlike `stereo_baseline_pulse`, the baseline is deliberately
/// constant, making this a clean front/back landmark pair rather than a distance sweep.
const MATERIAL: &str = "minecraft:stone_bricks";
const SUBJECT_HEIGHT: i64 = 3;
const HALF_FOV_DEG: f64 = 51.25;
const STANDOFF: f64 = 11.0;
const BASELINE: f64 = 22.0;
const HOLD_MS: i64 = 8_500;
const PRESS_MS: u64 = 1_350;
const CHECK_EVERY: usize = 2;
const _: () = assert!(BASELINE - 2.0 * PRESS_MS as f64 / 650.0 >= MIN_PEER_GAP);

/// A standing column is exterior only when its support is natural and the vertical ray reaches sky.
fn exterior(ctx: &Ctx, feet: BlockPos) -> bool {
    let support = feet.down(1);
    let Some(id) = ctx.block_id_at(support) else {
        return false;
    };
    let id = id.strip_prefix("minecraft:").unwrap_or(&id);
    let artificial = [
        "brick",
        "plank",
        "concrete",
        "terracotta",
        "glass",
        "wool",
        "slab",
        "stairs",
        "fence",
        "wall",
        "quartz",
        "prismarine",
    ]
    .iter()
    .any(|part| id.contains(part));
    !artificial
        && !id.contains("water")
        && !id.contains("lava")
        && !id.contains("leaves")
        && !id.ends_with("log")
        && block_is_safe_support(ctx, support)
        && (feet.y..=319).all(|y| block_is_air(ctx, BlockPos::new(feet.x, y, feet.z)))
}

/// Resolve the requested bearing onto nearby open ground before routing. This rejects stations
/// embedded in a normal-world slope, wall, or cave while preserving the opposing-view geometry.
fn open_station(ctx: &Ctx, desired: (f64, f64), hint: i32) -> Option<(f64, f64)> {
    const OFFSETS: [(i32, i32); 13] = [
        (0, 0),
        (1, 0),
        (-1, 0),
        (0, 1),
        (0, -1),
        (1, 1),
        (1, -1),
        (-1, 1),
        (-1, -1),
        (2, 0),
        (-2, 0),
        (0, 2),
        (0, -2),
    ];
    let bx = desired.0.floor() as i32;
    let bz = desired.1.floor() as i32;
    for (dx, dz) in OFFSETS {
        for delta in 0..=18 {
            for y in [hint + delta, hint - delta] {
                let feet = BlockPos::new(bx + dx, y, bz + dz);
                if (feet.y - hint).abs() > 2 {
                    continue;
                }
                let clear =
                    [(0, 0), (1, 0), (-1, 0), (0, 1), (0, -1)]
                        .into_iter()
                        .all(|(sx, sz)| {
                            let cell = BlockPos::new(feet.x + sx, feet.y, feet.z + sz);
                            block_is_air(ctx, cell) && block_is_air(ctx, cell.up(1))
                        });
                if clear && exterior(ctx, feet) {
                    return Some((feet.x as f64 + 0.5, feet.z as f64 + 0.5));
                }
            }
        }
    }
    None
}

fn axis_rad(ctx: &Ctx) -> f64 {
    // Pair 0 keeps the N=2 geometry; at N=4+ rotate each pair's workspace so paired roles do not
    // land on the same stations while preserving one shared axis inside each pair.
    f64::from(ctx.engine().seed % 360).to_radians() + ctx.index().saturating_div(2) as f64 * 0.83
}

fn delta_deg(from: f64, to: f64) -> f64 {
    (to - from + 540.0) % 360.0 - 180.0
}

fn geometry(ctx: &Ctx, peer: Option<usize>, subject: (f64, f64, f64), facing: f64) -> Value {
    let Some(own) = ctx.position() else {
        return json!({"peer_visible": false});
    };
    let eye = head_of(own);
    let subject_bearing = look_angles(eye, subject).0;
    let mut out = json!({
        "peer_visible": false,
        "subject_distance": r2((subject.0 - own.0).hypot(subject.2 - own.2)),
        "relative_bearing_deg": r2(delta_deg(facing, subject_bearing)),
        "facing_deg": r2(facing),
        "opposed_view": true,
    });
    let Some(head) = peer.and_then(|j| ctx.peer_position(j)).map(head_of) else {
        return out;
    };
    let peer_bearing = look_angles(eye, head).0;
    let own_az = (own.2 - subject.2).atan2(own.0 - subject.0).to_degrees();
    let peer_az = (head.2 - subject.2).atan2(head.0 - subject.0).to_degrees();
    let subject_off = delta_deg(facing, subject_bearing);
    let peer_off = delta_deg(facing, peer_bearing);
    out["peer_visible"] = json!(true);
    out["baseline"] = json!(r2((head.0 - own.0).hypot(head.2 - own.2)));
    out["subject_in_frame"] = json!(subject_off.abs() <= HALF_FOV_DEG);
    out["peer_in_frame"] = json!(peer_off.abs() <= HALF_FOV_DEG);
    out["covisibility"] =
        json!(subject_off.abs() <= HALF_FOV_DEG && peer_off.abs() <= HALF_FOV_DEG);
    out["azimuth_separation_deg"] = json!(r2(delta_deg(own_az, peer_az).abs()));
    out
}

/// Keep the camera pinned while the body shuffles sideways. The two global roles start on opposite
/// key phases, which preserves the geometry while avoiding synchronized action tuples.
async fn shuffle(ctx: &mut Ctx, facing: f64, deadline: i64, phase: usize) {
    let dirs = if phase.is_multiple_of(2) {
        [WalkDirection::Left, WalkDirection::Right]
    } else {
        [WalkDirection::Right, WalkDirection::Left]
    };
    let mut n = 0usize;
    while !ctx.target_met() && ctx.elapsed_ms() + PRESS_MS as i64 + 700 < deadline {
        if ctx.hold_visual(dirs[n % 2], false, PRESS_MS, 0).await {
            ctx.pause().await;
        } else {
            shake_loose(ctx).await;
            if let Some(p) = ctx.position() {
                ctx.look_default(point_at_yaw(head_of(p), facing, 24.0))
                    .await;
            }
        }
        n += 1;
    }
}

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let peer = Some(index ^ 1).filter(|&j| j < ctx.player_count());
    let anchor = ctx.anchor();
    let start = ctx.position().unwrap_or((anchor.0, 64.0, anchor.1));
    let axis = axis_rad(ctx);
    let (ux, uz) = (axis.cos(), axis.sin());
    let side = if global.is_multiple_of(2) { 1.0 } else { -1.0 };
    let desired = (
        anchor.0.round() as i64,
        start.1.round() as i64,
        anchor.1.round() as i64,
    );
    let raised = if global == 0 {
        raise_landmark(ctx, desired, SUBJECT_HEIGHT, MATERIAL).await
    } else {
        None
    };
    let subject = raised
        .map(|(_, p)| p)
        .unwrap_or((anchor.0, start.1 + 2.0, anchor.1));
    let desired_station = (
        subject.0 + ux * STANDOFF * side,
        subject.2 + uz * STANDOFF * side,
    );
    let hint = ctx.position().map(|p| p.1.floor() as i32).unwrap_or(64);
    let Some(station) = open_station(ctx, desired_station, hint) else {
        ctx.pause().await;
        return;
    };
    ctx.record(
        TYPE_LOOK,
        &format!("vis-stereo-opposed-setup-{index}"),
        "Resolve the fixed opposed stations around one shared landmark.",
        json!({"global_index": global, "axis_deg": r2(axis.to_degrees()), "baseline": BASELINE,
            "standoff": STANDOFF, "subject": {"x": r2(subject.0), "y": r2(subject.1), "z": r2(subject.2)},
            "subject_placed": raised.is_some()}),
    );
    sleep_ms(global as u64 * 950).await;

    let mut round = 0usize;
    while !ctx.target_met() {
        travel_to(ctx, station, TRAVEL_HOP, 1.5).await;
        let faced = ctx.look_until_facing(subject, 6.0, 5_000).await;
        let facing = ctx
            .position()
            .map(|p| look_angles(head_of(p), subject).0)
            .unwrap_or(0.0);
        let mut data = geometry(ctx, peer, subject, facing);
        data["round"] = json!(round);
        data["is_revisit"] = json!(round > 0);
        data["faced"] = json!(faced);
        ctx.record(
            TYPE_LOOK,
            &format!("vis-stereo-opposed-landmark-{index}-{round}"),
            "Frame the shared landmark from this camera's opposing station.",
            data,
        );
        shuffle(ctx, facing, ctx.elapsed_ms() + HOLD_MS, global + round).await;
        if ctx.target_met() {
            break;
        }

        // The rear-side view is sampled without moving the station. A confirmed peer check is a
        // labelled detour, then the camera returns to the already observed landmark.
        if round.is_multiple_of(CHECK_EVERY) {
            let peer_target = peer.and_then(|j| ctx.peer_position(j)).map(head_of);
            let checked = match peer_target {
                Some(target) => ctx.look_until_facing(target, 8.0, 4_000).await,
                None => false,
            };
            let peer_yaw = ctx
                .position()
                .zip(peer_target)
                .map(|(p, target)| look_angles(head_of(p), target).0)
                .unwrap_or(facing + 140.0);
            ctx.record(
                TYPE_LOOK,
                &format!("vis-stereo-opposed-peer-check-{index}-{round}"),
                "Check the opposite camera, then reacquire the shared landmark.",
                json!({"round": round, "checked": checked, "peer_yaw_deg": r2(peer_yaw),
                    "turn_back_required": true, "baseline": geometry(ctx, peer, subject, peer_yaw)["baseline"]}),
            );
            ctx.look_until_facing(subject, 6.0, 5_000).await;
        }
        round += 1;
    }
}
