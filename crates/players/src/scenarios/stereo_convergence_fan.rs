//! CATALOG: family=group_motion min_players=2 single_process=true world_edits=false scenery=none far_players=false

use super::*;

/// Half the horizontal field of view (102.5 deg at `fov_deg=70`, 16:9). This is the threshold the
/// whole episode is built to cross: the teammate leaves the frame when the fan opens past it and
/// re-enters when the fan closes back under it.
const HALF_FOV_DEG: f64 = 51.25;

/// The fan, in degrees off the line joining the two cameras. Both bots rotate by the SAME amount in
/// the SAME world sense, so the pair stays antiparallel and each loses sight of the other at the
/// same angle — the co-visible region between them opens and closes symmetrically.
///
/// Palindromic and capped at 75: the return leg retraces the outward one (so every window has a
/// counterpart it can be checked against), 75 straddles [`HALF_FOV_DEG`] with margin on both sides,
/// and a wider fan would start eating the per-window overlap the corpus is short of (v5's
/// `overlap_mean` median is 0.58 and v6 wants 0.65).
const FAN: [f64; 6] = [0.0, 25.0, 50.0, 75.0, 50.0, 25.0];
const _: () = assert!(FAN[3] > HALF_FOV_DEG && FAN[2] < HALF_FOV_DEG);

/// Most one open-loop press can cover: 1.85 s at ~4.3 blocks/s.
const MAX_PRESS_BLOCKS: f64 = 8.0;

/// The pair's separation at its stations, in blocks.
///
/// Set by the worst case of the dwell rather than by taste: at any one step only one of the two bots
/// works the sight line (the other strafes, see `pinned_dwell`), and its retreat-then-return pair
/// leaves it at most one excursion off station if the retreat stalls — so the gap has to survive a
/// full press. Close enough at 13 that each bot still fills a useful part of the other's frame at
/// fan 0 rather than being a speck.
const STATION_GAP: f64 = 13.0;
const _: () = assert!(STATION_GAP - MAX_PRESS_BLOCKS >= MIN_PEER_GAP);

/// Below this the planner walks the bot back to its station instead of pressing again — the recovery
/// path for drift the excursion argument above does not cover (a stalled press, or a `shake_loose`
/// hop that threw the body several blocks).
const CLOSE_GUARD: f64 = 8.0;

/// Wall clock one fan angle gets. Read off `elapsed_ms`, which starts at the shared `go` handshake
/// for every bot in the process, so both bots are on the same fan step with nothing exchanged; a
/// pair of local counters drifts the moment one bot's leg takes longer, and a drifted pair would
/// record a symmetric fan that never happened.
const STEP_MS: i64 = 7_000;

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

/// Select a nearby supported, open column for each endpoint. This prevents fixed fan stations from
/// putting a player into a normal-world slope, wall, or cave while keeping the baseline geometry.
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

/// The line the two stations sit on. Off `engine().seed`, which every bot in the process reads
/// identically; `rng_next` is a PER-BOT stream and would have given the two bots two different lines.
fn axis_rad(ctx: &Ctx) -> f64 {
    // Pair 0 keeps the N=2 geometry; at N=4+ rotate each pair's workspace so paired roles do not
    // land on the same stations while preserving one shared axis inside each pair.
    f64::from(ctx.engine().seed % 360).to_radians() + ctx.index().saturating_div(2) as f64 * 0.83
}

/// Signed shortest angle from `from` to `to`, in degrees.
fn delta_deg(from: f64, to: f64) -> f64 {
    (to - from + 540.0) % 360.0 - 180.0
}

/// MEASURED fan geometry. The nominal angle is what the script asked for; this is what the two
/// bodies and cameras actually held, which is what a sample selector has to filter on.
fn pair_geometry(ctx: &Ctx, peer: Option<usize>, facing_deg: f64) -> Value {
    let (Some(own), Some(head)) = (
        ctx.position(),
        peer.and_then(|j| ctx.peer_position(j)).map(head_of),
    ) else {
        return json!({"peer_visible": false});
    };
    let off = delta_deg(facing_deg, look_angles(head_of(own), head).0);
    json!({
        "peer_visible": true,
        "baseline": r2((head.0 - own.0).hypot(head.2 - own.2)),
        "fan_measured_deg": r2(off),
        "peer_in_frame": off.abs() <= HALF_FOV_DEG,
        // How far this camera's axis is from the line joining the pair, i.e. how much of the space
        // between the two cameras is still covered by both frusta.
        "covisible": off.abs() <= HALF_FOV_DEG,
        "facing_deg": r2(facing_deg),
    })
}

/// Cover ground with the camera PINNED on `bearing_deg`, out and back on one key pair.
///
/// `hold_visual` is the only primitive that decouples camera from body: it pins
/// `CameraIntent::LookAt` at the current angle and holds one movement key, where `move_to` sets
/// `CameraIntent::Hold`, which steers the camera onto the travel bearing (`control.rs:762`) and
/// would throw the fan angle away. One long press is also the opposite of key chatter, and a body
/// moving under a still camera adds nothing to the camera/travel-independence denominator.
///
/// `lateral` picks the key pair and alternates per bot per step, so the two POVs are not pressing
/// the same key on the same frame even though the fan is deliberately symmetric — the near-clones
/// gate compares exactly that, and a mirror-symmetric pair can fail it while looking perfectly
/// asymmetric to a human. The sight-line pair retreats FIRST, so at fan 0 (the two facing each
/// other) the first press opens the gap rather than closing it.
async fn pinned_dwell(
    ctx: &mut Ctx,
    bearing_deg: f64,
    station: (f64, f64),
    peer: Option<usize>,
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
    while !ctx.target_met() && ctx.elapsed_ms() + press_ms as i64 + 900 < deadline_ms {
        let close = match (ctx.position(), peer.and_then(|j| ctx.peer_position(j))) {
            (Some(own), Some(p)) => (p.0 - own.0).hypot(p.2 - own.2) < CLOSE_GUARD,
            _ => false,
        };
        if close {
            ctx.move_to(station.0, station.1, 6_000, 1.5).await;
            if let Some(p) = ctx.position() {
                ctx.look_default(point_at_yaw(head_of(p), bearing_deg, 24.0))
                    .await;
            }
            continue;
        }
        let dir = pair[phase % 2];
        phase += 1;
        if ctx.hold_visual(dir, false, press_ms, 0).await {
            ctx.pause().await;
            continue;
        }
        // A refused press is the stall guard: the body is against terrain, and more presses there
        // are the short ones the chatter gate counts. Break loose, then re-pin the fan angle.
        shake_loose(ctx).await;
        if let Some(p) = ctx.position() {
            ctx.look_default(point_at_yaw(head_of(p), bearing_deg, 24.0))
                .await;
        }
    }
}

/// Two cameras facing each other open their gaze outward in steps until neither is in the other's
/// frame, then close it back — the co-visible region between them expanding and contracting on a
/// labelled schedule.
///
/// The revisit problem: the SIGHT LINE closes by construction — [`FAN`] is a palindrome that
/// returns to the mutual line, and the return is confirmed with `look_until_facing` on the
/// teammate's head rather than trusted to a deadline (a plain `look` returns when its timer
/// expires, so a wide turn that timed out would be recorded as a re-acquisition that never
/// happened). The TRAJECTORY closes because every dwell is an out-and-back press pair around a
/// fixed station, and each step re-walks to that station. Occlusion is whatever the real terrain
/// between the two stations provides, and the fan sweeps the cameras across it; the level type is
/// never overridden.
///
/// What it gives the world model: the moment a teammate crosses the frame edge, from both sides, at
/// a known angle. The pair steps through fans of 0, 25, 50, 75 deg with the frame edge at 51.25, so
/// the corpus contains matched windows where the other player is centred, near the edge, just gone,
/// and just returning — with the measured angle on every event, which is what makes "generate the
/// player entering frame" a supervisable behaviour instead of an accident.
///
/// Distinct from `walk_look_away`, which turns away from a heading and keeps walking, and from
/// `stereo_antipodal_orbit`, where the pair's relative geometry is fixed at 180 deg and the BODIES
/// traverse viewpoints: here the bodies hold still on a fixed baseline and the relative GAZE is the
/// variable.
pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let peer = Some(index ^ 1).filter(|&j| j < ctx.player_count());
    let anchor = ctx.anchor();
    let axis = axis_rad(ctx);
    // Which end of the line this bot holds, off the GLOBAL index so the two ends are assigned the
    // same way however the swarm is enumerated.
    let end = if global.is_multiple_of(2) { 1.0 } else { -1.0 };
    let desired_station = (
        anchor.0 + axis.cos() * STATION_GAP * 0.5 * end,
        anchor.1 + axis.sin() * STATION_GAP * 0.5 * end,
    );
    let hint = ctx.position().map(|p| p.1.floor() as i32).unwrap_or(64);
    let Some(station) = open_station(ctx, desired_station, hint) else {
        ctx.pause().await;
        return;
    };
    // Per-bot press length. The fan is symmetric on purpose, so the near-clones defence rests on
    // press length, phase and the per-step key-pair alternation below.
    let press_ms = 1_300 + (global % 3) as u64 * 250;
    ctx.record(
        "observe",
        &format!("vis-fan-setup-{index}"),
        "Resolve the shared line, this camera's station and the fan schedule.",
        json!({
            "global_index": global,
            "axis_deg": r2(axis.to_degrees()),
            "station": {"x": r2(station.0), "z": r2(station.1)},
            "station_gap": STATION_GAP,
            "fan_deg": FAN,
            "half_fov_deg": HALF_FOV_DEG,
            "press_ms": press_ms,
        }),
    );
    sleep_ms(global as u64 * 900).await;

    while !ctx.target_met() {
        let step = (ctx.elapsed_ms() / STEP_MS).max(0);
        let deadline = (step + 1) * STEP_MS;
        let fan_index = (step as usize) % FAN.len();
        let fan = FAN[fan_index];
        travel_to(ctx, station, TRAVEL_HOP, 1.5).await;

        // Fan 0 is aimed at the teammate's head, so the acquisition is confirmed against the body
        // itself; every other step is that bearing plus the fan, in ONE world sense for both bots so
        // the pair stays antiparallel and the two losses of sight happen at the same angle.
        let head = peer.and_then(|j| ctx.peer_position(j)).map(head_of);
        let mutual = ctx
            .position()
            .zip(head)
            .map(|(p, head)| look_angles(head_of(p), head).0);
        let facing = mutual.unwrap_or(0.0) + fan;
        // Float patterns are not allowed in a match, and comparing the angle would be the wrong
        // question anyway: step 0 of the palindrome IS the mutual line, by index.
        let faced = match head.filter(|_| fan_index == 0) {
            Some(head) => ctx.look_until_facing(head, 6.0, 5_000).await,
            None => {
                let Some(p) = ctx.position() else {
                    ctx.pause().await;
                    continue;
                };
                ctx.look_until_facing(point_at_yaw(head_of(p), facing, 32.0), 6.0, 5_000)
                    .await
            }
        };
        // Alternate the key pair per bot and per step: same fan, different keys, so the frame-by-
        // frame tuples the near-clones gate compares diverge even while the geometry is symmetric.
        let lateral = (global + step as usize).is_multiple_of(2);
        let mut extras = pair_geometry(ctx, peer, facing);
        extras["step"] = json!(step);
        extras["fan_nominal_deg"] = json!(fan);
        extras["fan_index"] = json!(fan_index);
        extras["opening"] = json!(fan_index < 3);
        extras["expected_in_frame"] = json!(fan <= HALF_FOV_DEG);
        extras["dwell_lateral"] = json!(lateral);
        extras["faced"] = json!(faced);
        ctx.record(
            "observe",
            &format!("vis-fan-step-{index}-{step}"),
            "Hold this fan angle off the mutual line; the teammate is in frame or just past it.",
            extras,
        );
        pinned_dwell(ctx, facing, station, peer, deadline, press_ms, lateral).await;
    }
}
