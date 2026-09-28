//! CATALOG: family=group_motion min_players=2 single_process=true world_edits=true scenery=none far_players=false

use super::*;

/// Half the horizontal field of view (102.5 deg at `fov_deg=70`, 16:9).
const HALF_FOV_DEG: f64 = 51.25;

const MATERIAL: &str = "minecraft:stone_bricks";
/// Tall enough that the near camera does not hide the whole subject from the far one — a partial
/// occlusion is the signal; a total one is just a wall.
const SUBJECT_HEIGHT: i64 = 3;

/// The two depth slots, in blocks from the subject. The ratio (22/8 = 2.75) is the scale factor
/// between the two views, which is what an in-line baseline delivers instead of lateral parallax.
const NEAR_RANGE: f64 = 8.0;
const FAR_RANGE: f64 = 22.0;

/// Most one open-loop press can cover: 1.85 s at ~4.3 blocks/s. Only the near camera dwells along
/// the sight line (the far one strafes), so this is the whole budget by which the gap can close.
const MAX_PRESS_BLOCKS: f64 = 8.0;
const _: () = assert!(FAR_RANGE - NEAR_RANGE - MAX_PRESS_BLOCKS >= MIN_PEER_GAP);

/// How far off the shared ray each bot swings while the two trade slots. The two swerve to OPPOSITE
/// sides, so the closest approach during a relay is `2 * SWERVE` — this is the one moment in the
/// episode where two bodies on one line would otherwise walk through each other.
const SWERVE: f64 = 4.0;
const _: () = assert!(2.0 * SWERVE >= MIN_PEER_GAP);

/// Wall clock one slot occupancy gets, relay included. Read off `elapsed_ms`, which starts at the
/// shared `go` handshake for every bot in the process, so the two trade slots on the same beat with
/// nothing exchanged; two local counters drift the moment one bot's leg takes longer and could leave
/// both bots walking to the same slot.
const ROUND_MS: i64 = 24_000;

/// Past the frame edge, so the near camera's check on its teammate genuinely leaves the subject and
/// comes back to it — the reversal v5 windows almost never contain (net-turn / sweep ratio 0.80).
const SHOULDER_DEG: f64 = 160.0;

/// The shared ray the pair lines up on. Off `engine().seed`, which every bot in the process reads
/// identically; `rng_next` is a PER-BOT stream and would have put the two bots on two different rays
/// while every QC gate still passed and the recorded geometry described something that never
/// happened.
fn axis_rad(ctx: &Ctx) -> f64 {
    // Pair 0 keeps the N=2 geometry; at N=4+ rotate each pair's workspace so paired roles do not
    // land on the same stations while preserving one shared axis inside each pair.
    f64::from(ctx.engine().seed % 360).to_radians() + ctx.index().saturating_div(2) as f64 * 0.83
}

/// Signed shortest angle from `from` to `to`, in degrees.
fn delta_deg(from: f64, to: f64) -> f64 {
    (to - from + 540.0) % 360.0 - 180.0
}

/// MEASURED in-line geometry. `inline_error` is the honest one: the pair is only a longitudinal
/// stereo pair to the extent that both bodies really are on one ray through the subject, and a dwell
/// or a terrain detour pushes them off it. Downstream selection needs to know by how much.
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
    let own_range = (subject.0 - own.0).hypot(subject.2 - own.2);
    let mut out = json!({
        "peer_visible": false,
        "own_subject_distance": r2(own_range),
        "subject_off_axis_deg": r2(delta_deg(facing_deg, look_angles(eye, subject).0)),
        "facing_deg": r2(facing_deg),
    });
    let Some(head) = peer.and_then(|j| ctx.peer_position(j)).map(head_of) else {
        return out;
    };
    let peer_range = (subject.0 - head.0).hypot(subject.2 - head.2);
    let off = delta_deg(facing_deg, look_angles(eye, head).0);
    // Perpendicular distance from this bot to the subject-to-teammate line: zero when the three are
    // exactly collinear, which is the configuration the slots describe.
    let (lx, lz) = (head.0 - subject.0, head.2 - subject.2);
    let (px, pz) = (own.0 - subject.0, own.2 - subject.2);
    let inline_error = (lx * pz - lz * px).abs() / lx.hypot(lz).max(1e-6);
    out["peer_visible"] = json!(true);
    out["peer_subject_distance"] = json!(r2(peer_range));
    out["baseline"] = json!(r2((head.0 - own.0).hypot(head.2 - own.2)));
    // Scale factor between the two views of the subject: the whole point of a longitudinal baseline.
    out["depth_ratio"] = json!(r2(
        own_range.max(peer_range) / own_range.min(peer_range).max(1e-6)
    ));
    out["inline_error"] = json!(r2(inline_error));
    out["peer_off_axis_deg"] = json!(r2(off));
    out["peer_in_frame"] = json!(off.abs() <= HALF_FOV_DEG);
    // Which camera is in front, i.e. whose body is partly covering the subject for the other.
    out["occluder"] = json!(if own_range < peer_range {
        "self"
    } else {
        "peer"
    });
    out
}

/// Cover ground with the camera PINNED on `bearing_deg`, out and back on one key pair.
///
/// `hold_visual` is the only primitive that decouples camera from body: it pins
/// `CameraIntent::LookAt` at the current angle and holds one movement key, where `move_to` sets
/// `CameraIntent::Hold`, which faces the direction of travel while moving (`control.rs`) and would
/// throw the framing away. One long press is also the opposite of key chatter, and a body moving
/// under a still camera adds nothing to the camera/travel-independence denominator.
///
/// `lateral` is decided by the slot, not drawn: the far camera strafes (its body swings the near
/// camera off the subject and back, so the occlusion opens and closes) while the near camera works
/// its sight line (the subject grows and shrinks). Two different key streams on the same frames is
/// what keeps the pair off the near-clones ceiling — that gate compares packed key tuples frame by
/// frame — and it is also what bounds the closing: only one of the two ever moves along the line
/// joining them.
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
        // A refused press is the stall guard: the body is against terrain, and more presses there are
        // the short ones the chatter gate counts. Break loose, then re-pin the bearing.
        shake_loose(ctx).await;
        if let Some(p) = ctx.position() {
            ctx.look_default(point_at_yaw(head_of(p), bearing_deg, 24.0))
                .await;
        }
    }
}

/// Two cameras line up on ONE ray through a shared subject — one close, one far — and then trade
/// places.
///
/// The baseline here is LONGITUDINAL, not lateral: the two frusta are nearly co-axial, the epipole
/// sits in the middle of the frame, and the difference between the two views is scale rather than
/// sideways shift. That is the complement of every other pair geometry in this family, and it is the
/// configuration where one player physically occludes the subject for the other — the far camera sees
/// its teammate's body in front of the pillar, and the near camera cannot see its teammate at all.
///
/// The revisit problem: the TRAJECTORY closes over two rounds, because a relay and a relay back
/// puts each bot on its own opening slot. The SIGHT LINE closes twice — every arrival re-acquires
/// the subject through `look_until_facing` (a plain `look` returns on its deadline, so a turn that
/// timed out would be recorded as an acquisition that never happened), and the near camera, which
/// has its teammate squarely behind it, spends one beat per round turned 160 deg round to check on
/// it before confirming its way back onto the subject. Occlusion is designed in twice over: the
/// teammate's body on the subject, and whatever the real terrain along the ray puts in the way.
///
/// What it gives the world model: matched pairs of views of one subject at a 2.75x scale ratio, with
/// the occluding player labelled, and then the same pair with the cameras exchanged — so "what the
/// subject looks like from three times closer" and "what is behind the player in front of me" are
/// both recorded rather than inferred.
///
/// Distinct from `follow_cam_chain` and `satellite_escort`, where one bot's camera tracks another
/// while both travel: here neither camera tracks a body, both hold the same subject, the depth
/// ordering is the controlled variable, and the exchange is the episode.
pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let peer = Some(index ^ 1).filter(|&j| j < ctx.player_count());
    let anchor = ctx.anchor();
    let start = ctx.position().unwrap_or((anchor.0, 64.0, anchor.1));
    let axis = axis_rad(ctx);
    let (ray, perp) = ((axis.cos(), axis.sin()), (-axis.sin(), axis.cos()));
    // Opening slot, and the side of the ray this bot always swerves to when relaying. Both off the
    // GLOBAL index so the two bots make opposite choices however the swarm is enumerated.
    let starts_near = global.is_multiple_of(2);
    let swerve_side = if starts_near { 1.0 } else { -1.0 };
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
        &format!("vis-inline-setup-{index}"),
        "Resolve the shared ray, this camera's opening depth slot and the subject.",
        json!({
            "global_index": global,
            "opening_slot": if starts_near { "near" } else { "far" },
            "axis_deg": r2(axis.to_degrees()),
            "near_range": NEAR_RANGE,
            "far_range": FAR_RANGE,
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
        // Slots alternate every round, so two rounds is a closed loop and each camera has recorded
        // both depths of the same subject.
        let near = starts_near == (round % 2 == 0);
        let range = if near { NEAR_RANGE } else { FAR_RANGE };
        let station = (subject.0 + ray.0 * range, subject.2 + ray.1 * range);
        let far_from_station = ctx
            .position()
            .map(|p| (station.0 - p.0).hypot(station.1 - p.2) > 4.0)
            .unwrap_or(true);
        if far_from_station {
            // Relay via a waypoint swung to THIS bot's own side of the ray. Both bots move at once
            // and the two swerves have opposite signs, so the closest approach is `2 * SWERVE`
            // instead of a head-on pass along a shared line.
            let mid = (NEAR_RANGE + FAR_RANGE) * 0.5;
            let via = (
                subject.0 + ray.0 * mid + perp.0 * SWERVE * swerve_side,
                subject.2 + ray.1 * mid + perp.1 * SWERVE * swerve_side,
            );
            travel_to(ctx, via, TRAVEL_HOP, 2.5).await;
            travel_to(ctx, station, TRAVEL_HOP, 1.5).await;
        }

        let faced = ctx.look_until_facing(subject, 6.0, 5_000).await;
        let facing = ctx
            .position()
            .map(|p| look_angles(head_of(p), subject).0)
            .unwrap_or(0.0);
        let mut extras = pair_geometry(ctx, peer, subject, facing);
        extras["round"] = json!(round);
        extras["depth_slot"] = json!(if near { "near" } else { "far" });
        extras["relayed"] = json!(near != starts_near);
        extras["faced"] = json!(faced);
        ctx.record(
            "observe",
            &format!("vis-inline-hold-{index}-{round}"),
            "Hold this depth slot on the shared ray; the pair is co-axial on one subject.",
            extras,
        );

        // The near camera has its teammate directly behind it and cannot see it at all, which would
        // otherwise leave half this episode's windows without the other player in frame. One beat per
        // round is spent turned round to it, then confirmed back onto the subject.
        if near {
            let head = peer.and_then(|j| ctx.peer_position(j)).map(head_of);
            let shoulder = facing + SHOULDER_DEG;
            let reached = match head {
                Some(head) => ctx.look_until_facing(head, 8.0, 4_000).await,
                None => {
                    if let Some(p) = ctx.position() {
                        ctx.look_default(point_at_yaw(head_of(p), shoulder, 24.0))
                            .await;
                    }
                    false
                }
            };
            let bearing = ctx
                .position()
                .zip(head)
                .map(|(p, head)| look_angles(head_of(p), head).0)
                .unwrap_or(shoulder);
            let mut extras = pair_geometry(ctx, peer, subject, bearing);
            extras["round"] = json!(round);
            extras["shoulder_check"] = json!(true);
            extras["turn_deg"] = json!(SHOULDER_DEG);
            extras["exceeds_horizontal_fov"] = json!(SHOULDER_DEG > HALF_FOV_DEG);
            extras["reached_peer"] = json!(reached);
            ctx.record(
                "observe",
                &format!("vis-inline-shoulder-{index}-{round}"),
                "Near camera turns past the frame edge onto the teammate behind it, then back.",
                extras,
            );
            // Strafe, not the sight line: facing the teammate, a backward press would walk this bot
            // straight into the pillar it is standing 8 blocks from.
            let until = (ctx.elapsed_ms() + press_ms as i64 * 2 + 900).min(deadline);
            pinned_dwell(ctx, bearing, until, press_ms, true).await;
            ctx.look_until_facing(subject, 6.0, 5_000).await;
        }

        let bearing = ctx
            .position()
            .map(|p| look_angles(head_of(p), subject).0)
            .unwrap_or(facing);
        // The far camera strafes and the near one works its sight line: see `pinned_dwell`.
        pinned_dwell(ctx, bearing, deadline, press_ms, !near).await;
    }
}
