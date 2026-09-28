//! CATALOG: family=group_motion min_players=2 single_process=true world_edits=true scenery=none far_players=false

use super::*;

/// Sweep a shared visual target sideways so the two fixed cameras' co-visible region blooms and
/// contracts without changing their physical baseline.
///
/// The revisit problem's sight line is a closed target loop: the target offset schedule is
/// palindromic, and every offset is acquired with `look_until_facing` before a held observation.
/// The trajectory is a short lateral out-and-back shuffle at fixed stations, so movement closes
/// without making the stereo rig itself drift. A small landmark gives the target sweep a real
/// observed centre while the surrounding terrain stays untouched and natural. Each event records
/// baseline, target distance, relative bearing and the measured angular co-visibility width.
///
/// This differs from `stereo_convergence_fan`, which changes camera gaze around a fixed mutual line.
/// Here both cameras aim at the same sequence of world points around one physical landmark; the
/// changing point, not a prescribed yaw fan, is the independent variable. It also differs from a
/// baseline ladder because the camera separation is held fixed throughout.
const MATERIAL: &str = "minecraft:stone_bricks";
const SUBJECT_HEIGHT: i64 = 3;
const HALF_FOV_DEG: f64 = 51.25;
const STANDOFF: f64 = 12.0;
const BASELINE: f64 = 18.0;
const TARGET_OFFSETS: [f64; 7] = [-8.0, -4.0, 0.0, 4.0, 8.0, 4.0, -4.0];
const STEP_MS: i64 = 7_500;
const PRESS_MS: u64 = 1_350;
const _: () = assert!(BASELINE - 2.0 >= MIN_PEER_GAP);

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

/// Resolve a nearby open standing column before sending the pathfinder to a fixed stereo station.
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

fn geometry(ctx: &Ctx, peer: Option<usize>, target: (f64, f64, f64), facing: f64) -> Value {
    let Some(own) = ctx.position() else {
        return json!({"peer_visible": false});
    };
    let eye = head_of(own);
    let own_target = look_angles(eye, target).0;
    let mut out = json!({
        "peer_visible": false,
        "target_distance": r2((target.0 - own.0).hypot(target.2 - own.2)),
        "relative_bearing_deg": r2(delta_deg(facing, own_target)),
        "facing_deg": r2(facing),
    });
    let Some(peer_head) = peer.and_then(|j| ctx.peer_position(j)).map(head_of) else {
        return out;
    };
    let peer_target = look_angles(peer_head, target).0;
    let target_separation = delta_deg(own_target, peer_target).abs();
    let own_peer = delta_deg(facing, look_angles(eye, peer_head).0);
    out["peer_visible"] = json!(true);
    out["baseline"] = json!(r2((peer_head.0 - own.0).hypot(peer_head.2 - own.2)));
    out["peer_relative_bearing_deg"] = json!(r2(own_peer));
    out["target_bearing_separation_deg"] = json!(r2(target_separation));
    out["covisibility_width_deg"] = json!(r2((2.0 * HALF_FOV_DEG - target_separation).max(0.0)));
    out["covisible"] = json!(target_separation <= 2.0 * HALF_FOV_DEG);
    out
}

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
    let (px, pz) = (-uz, ux);
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
        subject.0 - ux * STANDOFF + ux * side * BASELINE * 0.5,
        subject.2 - uz * STANDOFF + uz * side * BASELINE * 0.5,
    );
    let hint = ctx.position().map(|p| p.1.floor() as i32).unwrap_or(64);
    let Some(station) = open_station(ctx, desired_station, hint) else {
        ctx.pause().await;
        return;
    };
    ctx.record(
        TYPE_LOOK,
        &format!("vis-stereo-covis-setup-{index}"),
        "Resolve fixed stereo stations and the target-offset bloom schedule.",
        json!({"global_index": global, "axis_deg": r2(axis.to_degrees()), "baseline": BASELINE,
            "standoff": STANDOFF, "target_offsets": TARGET_OFFSETS,
            "subject": {"x": r2(subject.0), "y": r2(subject.1), "z": r2(subject.2)},
            "subject_placed": raised.is_some()}),
    );
    sleep_ms(global as u64 * 900).await;

    let mut step = 0usize;
    while !ctx.target_met() {
        travel_to(ctx, station, TRAVEL_HOP, 1.5).await;
        let offset = TARGET_OFFSETS[step % TARGET_OFFSETS.len()];
        let target = (subject.0 + px * offset, subject.1, subject.2 + pz * offset);
        let faced = ctx.look_until_facing(target, 6.0, 5_000).await;
        let facing = ctx
            .position()
            .map(|p| look_angles(head_of(p), target).0)
            .unwrap_or(0.0);
        let mut data = geometry(ctx, peer, target, facing);
        data["step"] = json!(step);
        data["target_offset"] = json!(offset);
        data["blooming"] = json!(offset.abs() > 0.0 && step <= 3);
        data["contracting"] = json!(step >= 4);
        data["target_return"] = json!(step > 0 && offset == TARGET_OFFSETS[0]);
        data["faced"] = json!(faced);
        ctx.record(
            TYPE_LOOK,
            &format!("vis-stereo-covis-target-{index}-{step}"),
            "Aim both views at the next real-world target offset and measure co-visibility.",
            data,
        );
        shuffle(ctx, facing, ctx.elapsed_ms() + STEP_MS, global + step).await;
        step += 1;
    }
}
