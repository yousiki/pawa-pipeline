//! CATALOG: family=group_motion min_players=2 single_process=true world_edits=true scenery=none far_players=false

use super::*;

/// Half the horizontal field of view (102.5 deg at `fov_deg=70`, 16:9).
const HALF_FOV_DEG: f64 = 51.25;

const MATERIAL: &str = "minecraft:stone_bricks";
/// Tall enough to stay above ground clutter at the orbit radius, cheap enough that the standing time
/// it costs the one bot that builds it does not threaten the participation floor.
const SUBJECT_HEIGHT: i64 = 3;

/// Orbit radius, in blocks.
///
/// A 45 deg step is an 8.4-block chord (`2*11*sin(22.5 deg)`), which is one honest walking leg — an
/// orbit walked as continuous short chords is the key-chatter failure the QC notes measure at
/// 95-100%. The radius also has to survive both bots stepping inward at once during a dwell.
const ORBIT_RADIUS: f64 = 11.0;

/// Most one open-loop press can cover: 1.85 s at ~4.3 blocks/s.
const MAX_PRESS_BLOCKS: f64 = 8.0;
const _: () = assert!(2.0 * (ORBIT_RADIUS - MAX_PRESS_BLOCKS) >= MIN_PEER_GAP);

/// Degrees of orbit per station. Eight of them is a full turn, so a bot is back where it started
/// after eight and standing on its teammate's opening viewpoint after four.
const STEP_DEG: f64 = 45.0;

/// Wall clock one station gets. Read off `elapsed_ms`, which starts at the shared `go` handshake for
/// every bot in the process, so the two stay antipodal with nothing exchanged; two local counters
/// drift the moment one bot's leg takes longer, and a drifted pair stops being antipodal at all
/// while every QC gate still passes.
const STEP_MS: i64 = 9_000;

/// Within this many degrees of the subject's bearing, the teammate is behind the subject and the
/// pillar occludes part of it. Worth a flag: an occlusion that resolves as the pair orbits is one of
/// the few things a single-view corpus cannot teach.
const BEHIND_SUBJECT_DEG: f64 = 6.0;

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

/// Resolve orbit targets to nearby open ground. A nominal point can fall inside a normal-world
/// embankment or cave, so require a supported two-block column with lateral clearance.
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

/// Azimuth zero for the orbit. Off `engine().seed`, which every bot in the process reads identically;
/// `rng_next` is a PER-BOT stream and would have put the two bots on two different orbits.
fn axis_rad(ctx: &Ctx) -> f64 {
    // Pair 0 keeps the N=2 geometry; at N=4+ rotate each pair's workspace so paired roles do not
    // land on the same stations while preserving one shared axis inside each pair.
    f64::from(ctx.engine().seed % 360).to_radians() + ctx.index().saturating_div(2) as f64 * 0.83
}

/// Signed shortest angle from `from` to `to`, in degrees.
fn delta_deg(from: f64, to: f64) -> f64 {
    (to - from + 540.0) % 360.0 - 180.0
}

/// MEASURED orbit geometry. Azimuths are taken in the math convention the stations are built in
/// (`atan2(dz, dx)`) and never mixed with the notchian yaw the look APIs speak — the two differ by a
/// sign and 90 deg, and mixing them puts the orbit on the wrong quarter of the world.
fn pair_geometry(
    ctx: &Ctx,
    peer: Option<usize>,
    subject: (f64, f64, f64),
    facing_deg: f64,
) -> Value {
    let Some(own) = ctx.position() else {
        return json!({"peer_visible": false});
    };
    let eye = head_of(own);
    let subject_bearing = look_angles(eye, subject).0;
    let mut out = json!({
        "peer_visible": false,
        "own_azimuth_deg": r2((own.2 - subject.2).atan2(own.0 - subject.0).to_degrees()),
        "subject_distance": r2((subject.0 - own.0).hypot(subject.2 - own.2)),
        "subject_off_axis_deg": r2(delta_deg(facing_deg, subject_bearing)),
        "facing_deg": r2(facing_deg),
    });
    let Some(head) = peer.and_then(|j| ctx.peer_position(j)).map(head_of) else {
        return out;
    };
    let peer_bearing = look_angles(eye, head).0;
    let off = delta_deg(facing_deg, peer_bearing);
    let own_azimuth = (own.2 - subject.2).atan2(own.0 - subject.0).to_degrees();
    let peer_azimuth = (head.2 - subject.2).atan2(head.0 - subject.0).to_degrees();
    out["peer_visible"] = json!(true);
    out["peer_azimuth_deg"] = json!(r2(peer_azimuth));
    // The invariant this episode maintains: the two cameras hold opposite sides of the subject for
    // the whole episode, so each POV is the other's missing half.
    out["azimuth_separation_deg"] = json!(r2((peer_azimuth - own_azimuth + 540.0) % 360.0 - 180.0));
    out["baseline"] = json!(r2((head.0 - own.0).hypot(head.2 - own.2)));
    out["peer_off_axis_deg"] = json!(r2(off));
    out["peer_in_frame"] = json!(off.abs() <= HALF_FOV_DEG);
    out["peer_behind_subject"] =
        json!(delta_deg(subject_bearing, peer_bearing).abs() <= BEHIND_SUBJECT_DEG);
    out
}

/// Cover ground with the camera PINNED on `bearing_deg`, out and back on one key pair.
///
/// `hold_visual` is the only primitive that decouples camera from body: it pins
/// `CameraIntent::LookAt` at the current angle and holds one movement key, where `move_to` sets
/// `CameraIntent::Hold`, which faces the direction of travel while moving (`control.rs`) — so an
/// orbit walked with `move_to` while "watching the centre" is the canonical
/// camera/travel-independence failure, its `d_yaw` sign locked to the orbit direction. Here the
/// camera is genuinely still while the body moves, so those frames are not evidence at all.
///
/// `lateral` picks the key pair and alternates per bot per station: sideways is a short tangential
/// arc that keeps the subject distance, along the sight line is a radial in-and-out. Two different
/// key streams on the same frames is what keeps an intentionally symmetric pair off the near-clones
/// ceiling — that gate compares packed key tuples frame by frame, and a synchronized pair measured
/// 80.2% against an 80% ceiling. The radial pair retreats FIRST, so the first press opens the gap.
async fn pinned_dwell(
    ctx: &mut Ctx,
    bearing_deg: f64,
    deadline_ms: i64,
    press_ms: u64,
    lateral: bool,
) {
    let pair = if lateral {
        [WalkDirection::Left, WalkDirection::Right]
    } else {
        [WalkDirection::Backward, WalkDirection::Forward]
    };
    let mut phase = 0usize;
    while !ctx.target_met() && ctx.elapsed_ms() + press_ms as i64 + 600 < deadline_ms {
        let dir = pair[phase % 2];
        phase += 1;
        if ctx.hold_visual(dir, false, press_ms, 0).await {
            ctx.pause().await;
            continue;
        }
        // A refused press is the stall guard: the body is against terrain, and more presses there
        // are the short ones the chatter gate counts. Break loose, then re-pin the bearing.
        shake_loose(ctx).await;
        if let Some(p) = ctx.position() {
            ctx.look_default(point_at_yaw(head_of(p), bearing_deg, 24.0))
                .await;
        }
    }
}

/// Two cameras walk one orbit around a shared subject while holding OPPOSITE sides of it, so between
/// them they cover the subject's front and back at every instant.
///
/// The revisit problem: the TRAJECTORY closes exactly — eight 45 deg stations return each bot to
/// the station it started on, and at the fourth it is standing on its teammate's opening viewpoint.
/// The SIGHT LINE closes at every station, because each one is a fresh `look_until_facing` back
/// onto the same subject from a new bearing (a plain `look` returns on its deadline, so a wide turn
/// that timed out would be recorded as an acquisition that never happened). Occlusion is deliberate
/// and doubled: the terrain the orbit crosses breaks the sight line, and for as long as the pair is
/// antipodal the subject itself sits between the two players, so each POV holds a teammate partly
/// hidden behind the pillar and watches it emerge — the `peer_behind_subject` flag marks exactly
/// those windows.
///
/// What it gives the world model: a viewpoint traverse with a guaranteed complementary view. Every
/// frame one camera records has a simultaneous frame from the opposite side, so "what is behind the
/// subject" is never a guess the corpus cannot check — it is recorded by the other player. That
/// pairing is the differentiator against a hard-coded two-player pipeline, where the second camera's
/// placement is not a controlled variable.
///
/// Distinct from `group_orbit` (a swarm circling a point at assorted radii, cameras free) and from
/// `orbit_lane_swap` (one-way concentric traffic lanes whose event is the merge): here there are
/// exactly two cameras, they are held 180 deg apart by construction, the stations are discrete and
/// long enough to be samples, and both cameras stay on the subject throughout.
pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let peer = Some(index ^ 1).filter(|&j| j < ctx.player_count());
    let anchor = ctx.anchor();
    let start = ctx.position().unwrap_or((anchor.0, 64.0, anchor.1));
    let axis = axis_rad(ctx);
    // Which side of the orbit this bot opens on, off the GLOBAL index so the assignment is the same
    // however the swarm is enumerated. The half-turn offset is what keeps the pair antipodal: both
    // bots advance the same number of stations in the same sense, so the offset never changes.
    let half_turn = if global.is_multiple_of(2) {
        0.0
    } else {
        std::f64::consts::PI
    };
    let press_ms = 1_300 + (global % 3) as u64 * 250;

    // One bot builds; placement is a stretch of standing still (turn to the cell, place, verify) and
    // on the builder that stretch is what pushed `moving frames` under the participation floor in the
    // eval family. The other bot finds the pillar already standing.
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
        .map(|(_, point)| point)
        .unwrap_or((anchor.0, start.1 + 2.0, anchor.1));
    ctx.record(
        "observe",
        &format!("vis-antipodal-setup-{index}"),
        "Resolve the orbit axis, this camera's side of it and the shared subject.",
        json!({
            "global_index": global,
            "side": if global.is_multiple_of(2) { "near_axis" } else { "far_axis" },
            "axis_deg": r2(axis.to_degrees()),
            "orbit_radius": ORBIT_RADIUS,
            "step_deg": STEP_DEG,
            "subject": {"x": r2(subject.0), "y": r2(subject.1), "z": r2(subject.2)},
            "subject_placed": raised.is_some(),
            "press_ms": press_ms,
        }),
    );
    sleep_ms(global as u64 * 900).await;

    while !ctx.target_met() {
        let step = (ctx.elapsed_ms() / STEP_MS).max(0);
        let deadline = (step + 1) * STEP_MS;
        let azimuth = axis + half_turn + (step as f64) * STEP_DEG.to_radians();
        let desired_station = (
            subject.0 + azimuth.cos() * ORBIT_RADIUS,
            subject.2 + azimuth.sin() * ORBIT_RADIUS,
        );
        let hint = ctx.position().map(|p| p.1.floor() as i32).unwrap_or(64);
        let alternate_angle = azimuth + std::f64::consts::FRAC_PI_2;
        let alternate = (
            subject.0 + alternate_angle.cos() * ORBIT_RADIUS,
            subject.2 + alternate_angle.sin() * ORBIT_RADIUS,
        );
        let Some(mut station) =
            open_station(ctx, desired_station, hint).or_else(|| open_station(ctx, alternate, hint))
        else {
            ctx.pause().await;
            continue;
        };
        let residual = travel_to(ctx, station, TRAVEL_HOP, 1.5).await;
        if residual > 3.0
            && let Some(recovery) = open_station(ctx, alternate, hint)
        {
            station = recovery;
            travel_to(ctx, station, TRAVEL_HOP, 1.5).await;
        }

        // Face the subject: the teammate is on the far side of it, so this one frame holds the
        // subject AND the other player. That is the fix for the 22.4% of v5 multiplayer windows whose
        // teammate only walks into frame partway through.
        let faced = ctx.look_until_facing(subject, 6.0, 5_000).await;
        let facing = ctx
            .position()
            .map(|p| look_angles(head_of(p), subject).0)
            .unwrap_or(0.0);
        let lateral = (global + step as usize).is_multiple_of(2);
        let mut extras = pair_geometry(ctx, peer, subject, facing);
        extras["step"] = json!(step);
        extras["azimuth_nominal_deg"] = json!(r2(azimuth.to_degrees()));
        extras["station"] = json!({"x": r2(station.0), "z": r2(station.1)});
        // Four stations in, this camera stands where its teammate opened the episode; eight in, both
        // are back on their own opening stations and the loop has closed.
        extras["on_peer_opening_viewpoint"] = json!((step as usize) % 8 == 4);
        extras["orbit_closed"] = json!(step > 0 && (step as usize).is_multiple_of(8));
        extras["dwell_lateral"] = json!(lateral);
        extras["faced"] = json!(faced);
        ctx.record(
            "observe",
            &format!("vis-antipodal-station-{index}-{step}"),
            "Hold this side of the orbit with the subject framed and the teammate behind it.",
            extras,
        );
        pinned_dwell(ctx, facing, deadline, press_ms, lateral).await;
    }
}
