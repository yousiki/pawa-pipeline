//! CATALOG: family=group_motion min_players=2 single_process=true world_edits=true scenery=none far_players=false

use super::*;

/// Use two sampled terrain shelves as a stereo pair with a deliberate vertical baseline.
///
/// The revisit problem's trajectory loop is a repeated out-and-back between two sampled stations,
/// while the sight line loop re-acquires the same landmark after each return. The stations are
/// chosen from real terrain rather than a forced level type: the pair sees the same subject from
/// different heights, making vertical parallax and ground occlusion observable. `height_baseline`,
/// horizontal baseline, subject distance, relative bearing and co-visibility are measured from live
/// positions. If no relief is available, both bots fall back to the closest resolved shelves and
/// the record marks the reduced disparity instead of pretending a stair exists.
///
/// This is distinct from `stereo_inline_depth_relay`, whose baseline is longitudinal and nearly
/// co-axial. Here the useful variable is the Y separation of two cameras, while each station keeps a
/// lateral offset from the subject.
const MATERIAL: &str = "minecraft:stone_bricks";
const SUBJECT_HEIGHT: i64 = 3;
const HALF_FOV_DEG: f64 = 51.25;
const STANDOFF: f64 = 10.0;
const STATION_RADIUS: [f64; 4] = [8.0, 10.0, 12.0, 14.0];
const SCAN_SPAN: i32 = 18;
const HOLD_MS: i64 = 8_000;
const PRESS_MS: u64 = 1_350;

fn axis_rad(ctx: &Ctx) -> f64 {
    // Pair 0 keeps the N=2 geometry; at N=4+ rotate each pair's workspace so paired roles do not
    // land on the same stations while preserving one shared axis inside each pair.
    f64::from(ctx.engine().seed % 360).to_radians() + ctx.index().saturating_div(2) as f64 * 0.83
}

fn delta_deg(from: f64, to: f64) -> f64 {
    (to - from + 540.0) % 360.0 - 180.0
}

fn exterior_surface(ctx: &Ctx, support: BlockPos) -> bool {
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
        && (support.y + 1..=319).all(|y| block_is_air(ctx, BlockPos::new(support.x, y, support.z)))
}

fn surface_y(ctx: &Ctx, x: i32, z: i32, hint: i32) -> Option<i32> {
    for y in ((hint - SCAN_SPAN)..=(hint + SCAN_SPAN)).rev() {
        let support = BlockPos::new(x, y, z);
        let id = ctx.block_id_at(support)?;
        if id.contains("water")
            || id.contains("lava")
            || id.contains("leaves")
            || id.ends_with("log")
        {
            continue;
        }
        let feet = support.up(1);
        let clear = [(0, 0), (1, 0), (-1, 0), (0, 1), (0, -1)]
            .into_iter()
            .all(|(dx, dz)| {
                let cell = BlockPos::new(x + dx, feet.y, z + dz);
                block_is_air(ctx, cell) && block_is_air(ctx, cell.up(1))
            });
        if (support.y + 1 - hint).abs() <= 2 && clear && exterior_surface(ctx, support) {
            return Some(y + 1);
        }
    }
    None
}

fn station(ctx: &Ctx, subject: (f64, f64, f64), axis: f64, side: f64) -> Option<(f64, f64, i32)> {
    let (ux, uz) = (axis.cos() * side, axis.sin() * side);
    let hint = ctx.position().map(|p| p.1.floor() as i32).unwrap_or(64);
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
    for radius in STATION_RADIUS {
        let desired_x = subject.0 - axis.cos() * STANDOFF + ux * radius;
        let desired_z = subject.2 - axis.sin() * STANDOFF + uz * radius;
        for (dx, dz) in OFFSETS {
            let x = desired_x + dx as f64;
            let z = desired_z + dz as f64;
            if let Some(y) = surface_y(ctx, x.floor() as i32, z.floor() as i32, hint) {
                return Some((x, z, y));
            }
        }
    }
    None
}

fn geometry(ctx: &Ctx, peer: Option<usize>, subject: (f64, f64, f64), facing: f64) -> Value {
    let Some(own) = ctx.position() else {
        return json!({"peer_visible": false});
    };
    let eye = head_of(own);
    let sb = look_angles(eye, subject).0;
    let mut out = json!({
        "peer_visible": false,
        "subject_distance": r2((subject.0 - own.0).hypot(subject.2 - own.2)),
        "relative_bearing_deg": r2(delta_deg(facing, sb)),
        "facing_deg": r2(facing),
        "own_feet_y": r2(own.1),
    });
    let Some(peer_pos) = peer.and_then(|j| ctx.peer_position(j)) else {
        return out;
    };
    let peer_eye = head_of(peer_pos);
    let po = delta_deg(facing, look_angles(eye, peer_eye).0);
    let so = delta_deg(facing, sb);
    out["peer_visible"] = json!(true);
    out["baseline"] = json!(r2((peer_pos.0 - own.0).hypot(peer_pos.2 - own.2)));
    out["horizontal_baseline"] = json!(r2((peer_pos.0 - own.0).hypot(peer_pos.2 - own.2)));
    out["height_baseline"] = json!(r2(peer_pos.1 - own.1));
    out["peer_feet_y"] = json!(r2(peer_pos.1));
    out["peer_relative_bearing_deg"] = json!(r2(po));
    out["subject_in_frame"] = json!(so.abs() <= HALF_FOV_DEG);
    out["peer_in_frame"] = json!(po.abs() <= HALF_FOV_DEG);
    out["covisibility"] = json!(so.abs() <= HALF_FOV_DEG && po.abs() <= HALF_FOV_DEG);
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
    let Some((station_x, station_z, sampled_y)) = station(ctx, subject, axis, side) else {
        ctx.pause().await;
        return;
    };
    ctx.record(
        TYPE_LOOK,
        &format!("vis-stereo-stair-setup-{index}"),
        "Resolve the two natural shelves and their intended vertical parallax.",
        json!({"global_index": global, "axis_deg": r2(axis.to_degrees()), "standoff": STANDOFF,
            "station_radius": STATION_RADIUS, "sampled_feet_y": sampled_y,
            "subject": {"x": r2(subject.0), "y": r2(subject.1), "z": r2(subject.2)},
            "subject_placed": raised.is_some()}),
    );
    sleep_ms(global as u64 * 900).await;

    let mut round = 0usize;
    while !ctx.target_met() {
        travel_to(ctx, (station_x, station_z), TRAVEL_HOP, 1.5).await;
        let faced = ctx.look_until_facing(subject, 6.0, 5_000).await;
        let facing = ctx
            .position()
            .map(|p| look_angles(head_of(p), subject).0)
            .unwrap_or(0.0);
        let mut data = geometry(ctx, peer, subject, facing);
        data["round"] = json!(round);
        data["sampled_feet_y"] = json!(sampled_y);
        data["is_revisit"] = json!(round > 0);
        data["faced"] = json!(faced);
        ctx.record(
            TYPE_LOOK,
            &format!("vis-stereo-stair-hold-{index}-{round}"),
            "Hold the shared landmark from the sampled height shelf.",
            data,
        );
        shuffle(ctx, facing, ctx.elapsed_ms() + HOLD_MS, global + round).await;
        if ctx.target_met() {
            break;
        }
        let away = facing
            + if round.is_multiple_of(2) {
                145.0
            } else {
                -145.0
            };
        let turned = ctx
            .look_until_facing(
                ctx.position()
                    .map(|p| point_at_yaw(head_of(p), away, 24.0))
                    .unwrap_or((subject.0, subject.1, subject.2)),
                6.0,
                5_000,
            )
            .await;
        ctx.record(
            TYPE_LOOK,
            &format!("vis-stereo-stair-away-{index}-{round}"),
            "Turn away from the elevated landmark, then re-acquire it on the next cycle.",
            json!({"round": round, "away_deg": r2(away), "reached_away": turned,
                "exceeds_horizontal_fov": true}),
        );
        round += 1;
    }
}
