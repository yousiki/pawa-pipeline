//! CATALOG: family=group_motion min_players=2 single_process=true world_edits=true scenery=none far_players=false

use super::*;

/// Half the horizontal field of view (102.5 deg at `fov_deg=70`, 16:9).
const HALF_FOV_DEG: f64 = 51.25;

const MATERIAL: &str = "minecraft:stone_bricks";
/// Tall enough to read as the shared subject from either end of the diameter, small enough that the
/// standing time it costs the one bot that builds it does not threaten the participation floor.
const SUBJECT_HEIGHT: i64 = 3;

/// Half the crossing, in blocks: each viewpoint sits this far from the subject on the shared axis,
/// so a swap is a ~24-block walk — long enough to be several honest legs, short enough that both
/// ends of the diameter stay well inside render range of each other.
const VIEWPOINT_RANGE: f64 = 12.0;

/// How far off the diameter each bot swerves on its way across. The two swerve to OPPOSITE sides,
/// so the closest approach mid-swap is `2 * SWERVE` — the one moment in this episode where the pair
/// could otherwise walk through each other.
const SWERVE: f64 = 5.0;
const _: () = assert!(2.0 * SWERVE >= MIN_PEER_GAP);

/// Wall clock one occupancy of a viewpoint gets, swap included. Read off `elapsed_ms`, which starts
/// at the shared `go` handshake for every bot in the process, so both bots swap on the same beat
/// with nothing exchanged; a pair of local counters would drift the moment one crossing took longer
/// and could leave both bots walking to the same end.
const ROUND_MS: i64 = 26_000;

/// The shared crossing axis. Off `engine().seed`, which every bot in the process reads identically;
/// `rng_next` is a PER-BOT stream and would have given the two bots two different diameters while
/// every QC gate still passed.
fn axis_rad(ctx: &Ctx) -> f64 {
    // Pair 0 keeps the N=2 geometry; at N=4+ rotate each pair's workspace so paired roles do not
    // land on the same stations while preserving one shared axis inside each pair.
    f64::from(ctx.engine().seed % 360).to_radians() + ctx.index().saturating_div(2) as f64 * 0.83
}

/// Signed shortest angle from `from` to `to`, in degrees.
fn delta_deg(from: f64, to: f64) -> f64 {
    (to - from + 540.0) % 360.0 - 180.0
}

/// MEASURED geometry of the handover: which side of the subject each camera is on, how far apart
/// they are, and whether the teammate is genuinely in frame.
///
/// Azimuths are computed in the math convention the stations are built in (`atan2(dz, dx)`) and
/// never mixed with the notchian yaw the look APIs speak; the two differ by a sign and 90 deg.
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
    let azimuth = (own.2 - subject.2).atan2(own.0 - subject.0).to_degrees();
    let subject_off = delta_deg(facing_deg, look_angles(eye, subject).0);
    let mut out = json!({
        "peer_visible": false,
        "own_azimuth_deg": r2(azimuth),
        "subject_distance": r2((subject.0 - own.0).hypot(subject.2 - own.2)),
        "subject_off_axis_deg": r2(subject_off),
        "subject_in_frame": subject_off.abs() <= HALF_FOV_DEG,
        "facing_deg": r2(facing_deg),
    });
    let Some(head) = peer.and_then(|j| ctx.peer_position(j)).map(head_of) else {
        return out;
    };
    let peer_azimuth = (head.2 - subject.2).atan2(head.0 - subject.0).to_degrees();
    let off = delta_deg(facing_deg, look_angles(eye, head).0);
    out["peer_visible"] = json!(true);
    out["peer_azimuth_deg"] = json!(r2(peer_azimuth));
    // ~180 deg for as long as the two hold opposite ends: the pair covers the subject's front and
    // back at once, so each POV is the other's missing side.
    out["azimuth_separation_deg"] = json!(r2((peer_azimuth - azimuth + 540.0) % 360.0 - 180.0));
    out["baseline"] = json!(r2((head.0 - own.0).hypot(head.2 - own.2)));
    out["peer_off_axis_deg"] = json!(r2(off));
    out["peer_in_frame"] = json!(off.abs() <= HALF_FOV_DEG);
    out
}

/// Cover ground with the camera PINNED on `bearing_deg`, out and back on one key pair.
///
/// `hold_visual` is the only primitive that decouples camera from body: it pins
/// `CameraIntent::LookAt` at the current angle and presses one movement key for the whole duration,
/// where `move_to` sets `CameraIntent::Hold`, which steers the camera onto the travel bearing
/// (`control.rs:762`) and would throw away the framing. One long press is also the opposite of key
/// chatter, and a body moving under a still camera adds nothing to the camera/travel-independence
/// denominator.
///
/// `lateral` picks the key pair. Sideways breaks the subject-behind-subject alignment and lets the
/// teammate come out from behind the pillar; along the sight line keeps the alignment and changes
/// only scale. The two bots are given OPPOSITE pairs in any one round (and swap between rounds), so
/// the frame-by-frame key tuples the near-clones gate compares are not even from the same alphabet.
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
        // A refused press is the stall guard — the body is leaning on terrain, and more presses
        // there are the short ones the chatter gate counts. Break loose, then re-pin the bearing
        // (`shake_loose` looks somewhere random on its way out).
        shake_loose(ctx).await;
        if let Some(p) = ctx.position() {
            ctx.look_default(point_at_yaw(head_of(p), bearing_deg, 24.0))
                .await;
        }
    }
}

/// Two cameras hold opposite ends of a diameter through one subject and then trade ends, each
/// taking over the view the other just had.
///
/// The revisit problem: the TRAJECTORY closes over two rounds — a swap and a swap back returns each
/// bot to its own end — and the SIGHT LINE closes at a longer range than any single window, because
/// the view a camera holds in round 2 is the view its teammate held in round 1. Every arrival
/// re-acquires the subject through `look_until_facing`, which CONFIRMS the turn instead of trusting
/// a deadline. The crossing itself is walked on real terrain, so whatever hill, tree or water lies
/// on the diameter occludes the subject partway through, and the occlusion resolves as the walk
/// continues.
///
/// What it gives the world model: a supervised viewpoint handover. Two POVs of the same subject
/// separated by ~180 deg (each is literally the other's missing side), and then the same two POVs
/// with the cameras exchanged — so a generator's consistency can be checked against footage of the
/// exact viewpoint it is being asked to imagine, recorded by the other player minutes earlier. The
/// event stream carries both azimuths and their separation, which is the key a sample selector
/// needs to pair the two halves.
///
/// Distinct from `ring_slot_exchange`, which exchanges slots on a crowd ring as a group formation
/// beat: here the exchange IS the episode, it is a two-body diameter, both bots keep the subject and
/// each other in frame throughout, and the swerve that keeps them from colliding is what makes the
/// two paths asymmetric.
pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let peer = Some(index ^ 1).filter(|&j| j < ctx.player_count());
    let anchor = ctx.anchor();
    let start = ctx.position().unwrap_or((anchor.0, 64.0, anchor.1));
    let axis = axis_rad(ctx);
    let (dir, perp) = ((axis.cos(), axis.sin()), (-axis.sin(), axis.cos()));
    // Home end of the diameter, and the side of it this bot swerves to when crossing. Both off the
    // GLOBAL index so the two ends are assigned the same way however the swarm is enumerated.
    let home = if global.is_multiple_of(2) { 1.0 } else { -1.0 };
    let press_ms = 1_300 + (global % 3) as u64 * 250;

    // One bot builds. Placement is a stretch of standing still (turn to the cell, place, verify) and
    // on the builder that stretch is what pushed `moving frames` under the participation floor in
    // the eval family; the other bot finds the pillar already standing.
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
        &format!("vis-swap-setup-{index}"),
        "Resolve the shared diameter, the home viewpoint and the subject being handed over.",
        json!({
            "global_index": global,
            "home_viewpoint": if home > 0.0 { "plus" } else { "minus" },
            "axis_deg": r2(axis.to_degrees()),
            "viewpoint_range": VIEWPOINT_RANGE,
            "swerve": SWERVE,
            "subject": {"x": r2(subject.0), "y": r2(subject.1), "z": r2(subject.2)},
            "subject_placed": raised.is_some(),
            "press_ms": press_ms,
        }),
    );
    sleep_ms(global as u64 * 900).await;

    while !ctx.target_met() {
        let round = (ctx.elapsed_ms() / ROUND_MS).max(0);
        let deadline = (round + 1) * ROUND_MS;
        // Even rounds at home, odd rounds at the teammate's end. Two rounds is a closed loop.
        let swapped = round % 2 != 0;
        let side = if swapped { -home } else { home };
        let station = (
            subject.0 + dir.0 * VIEWPOINT_RANGE * side,
            subject.2 + dir.1 * VIEWPOINT_RANGE * side,
        );
        let far = ctx
            .position()
            .map(|p| (station.0 - p.0).hypot(station.1 - p.2) > 4.0)
            .unwrap_or(true);
        if far {
            // Cross via a waypoint swerved to THIS bot's own side of the diameter. Both bots cross
            // at once and the two swerves have opposite signs, so the closest approach is
            // `2 * SWERVE` rather than a head-on walk through each other.
            let via = (
                subject.0 + perp.0 * SWERVE * home,
                subject.2 + perp.1 * SWERVE * home,
            );
            travel_to(ctx, via, TRAVEL_HOP, 2.5).await;
            travel_to(ctx, station, TRAVEL_HOP, 1.5).await;
        }

        // Re-acquire the subject from this end. The teammate is at the far end, so it sits behind
        // the subject in the same frame: both POVs open on the subject AND on the other player,
        // which is the fix for the 22.4% of v5 multiplayer windows whose teammate only walks in
        // partway through.
        let faced = ctx.look_until_facing(subject, 6.0, 5_000).await;
        let facing = ctx
            .position()
            .map(|p| look_angles(head_of(p), subject).0)
            .unwrap_or(0.0);
        // Opposite key pairs within a round, swapped between rounds: one bot steps along the sight
        // line (scale changes, the alignment holds) while the other shuffles sideways (the teammate
        // comes out from behind the pillar).
        let lateral = (global + round as usize).is_multiple_of(2);
        let mut extras = pair_geometry(ctx, peer, subject, facing);
        extras["round"] = json!(round);
        extras["viewpoint"] = json!(if side > 0.0 { "plus" } else { "minus" });
        extras["swapped"] = json!(swapped);
        extras["handover_from_peer"] = json!(peer);
        extras["dwell_lateral"] = json!(lateral);
        extras["faced"] = json!(faced);
        ctx.record(
            "observe",
            &format!("vis-swap-hold-{index}-{round}"),
            "Hold a viewpoint on the shared subject; on odd rounds it is the teammate's own.",
            extras,
        );
        pinned_dwell(ctx, facing, deadline, press_ms, lateral).await;
    }
}
