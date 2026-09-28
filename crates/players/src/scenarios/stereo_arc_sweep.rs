//! CATALOG: family=group_motion min_players=2 single_process=true world_edits=false scenery=none far_players=false

use super::*;

/// Half the horizontal field of view (102.5 deg at `fov_deg=70`, 16:9): the window inside which the
/// teammate is actually IN FRAME rather than merely nearby.
const HALF_FOV_DEG: f64 = 51.25;

/// Azimuths (degrees off the episode's shared axis) the moving camera visits around the standing
/// one. Palindromic on purpose: the walker returns along the arc it came out on, so the relative
/// geometry retraces itself and every window in the second half has a matching one in the first.
const SWEEP: [f64; 8] = [0.0, 45.0, 90.0, 135.0, 180.0, 135.0, 90.0, 45.0];

/// Most one open-loop press can cover: 1.85 s at ~4.3 blocks/s.
const MAX_PRESS_BLOCKS: f64 = 8.0;

/// Radius of the arc, in blocks.
///
/// 45 deg of it is a 10-block chord (`2*13*sin(22.5 deg)`), which is one honest walking leg — an arc
/// walked as continuous short chords is the key-chatter failure the QC notes measure at 95-100%. The
/// radius itself is set by the worst case of the dwell: only the walker steps along the line joining
/// the pair (the pivot strafes), so one full press toward the pivot has to leave the two clear of
/// [`MIN_PEER_GAP`].
const ARC_RADIUS: f64 = 13.0;
const _: () = assert!(ARC_RADIUS - MAX_PRESS_BLOCKS >= MIN_PEER_GAP);

/// Wall clock one azimuth station gets. Derived from `elapsed_ms`, which starts at the shared `go`
/// handshake for every bot in the process, so both bots agree on which station is current (and on
/// who is walking) without exchanging anything. The few hundred milliseconds of handshake skew are
/// nothing against 8 seconds.
const STATION_MS: i64 = 8_000;

/// How far the pivot may drift from its post before it walks back. The pivot has to keep moving —
/// the frozen gate fails 20 s of a body under half a block per second — but a pivot that wanders is
/// no longer the fixed vertex the recorded azimuths are measured from.
const PIVOT_LEASH: f64 = 6.0;

/// Resolve only exterior ground: a cave floor has air at the feet but not a continuous sky ray.
fn open_station(ctx: &Ctx, desired: (f64, f64), hint: i32) -> Option<(f64, f64)> {
    let bx = desired.0.floor() as i32;
    let bz = desired.1.floor() as i32;
    for radius in 0i32..=8 {
        for dx in -radius..=radius {
            let dz = radius - dx.abs();
            for dz in [dz, -dz] {
                for delta in 0..=32 {
                    for y in [hint + delta, hint - delta] {
                        let feet = BlockPos::new(bx + dx, y, bz + dz);
                        if (feet.y - hint).abs() > 2 {
                            continue;
                        }
                        let support = feet.down(1);
                        let Some(id) = ctx.block_id_at(support) else {
                            continue;
                        };
                        let id = id.strip_prefix("minecraft:").unwrap_or(&id);
                        let artificial = id.contains("brick")
                            || id.contains("plank")
                            || id.contains("concrete")
                            || id.contains("terracotta")
                            || id.contains("glass")
                            || id.contains("wool")
                            || id.contains("slab")
                            || id.contains("stairs")
                            || id.contains("fence")
                            || id.contains("wall")
                            || id.contains("quartz")
                            || id.contains("prismarine");
                        if artificial
                            || id.contains("water")
                            || id.contains("lava")
                            || id.contains("leaves")
                            || id.ends_with("log")
                            || !block_is_safe_support(ctx, support)
                            || !(feet.y..=319).all(|sky_y| {
                                block_is_air(ctx, BlockPos::new(feet.x, sky_y, feet.z))
                            })
                        {
                            continue;
                        }
                        let clear = [(0, 0), (1, 0), (-1, 0), (0, 1), (0, -1)].into_iter().all(
                            |(sx, sz)| {
                                let cell = BlockPos::new(feet.x + sx, feet.y, feet.z + sz);
                                block_is_air(ctx, cell) && block_is_air(ctx, cell.up(1))
                            },
                        );
                        if clear {
                            return Some((feet.x as f64 + 0.5, feet.z as f64 + 0.5));
                        }
                    }
                }
            }
        }
    }
    None
}

/// The pair's shared axis, azimuth zero for the whole episode. Read off `engine().seed`, which is
/// identical for every bot in the process; `rng_next` is a PER-BOT stream and would have given the
/// two bots two different arcs while every QC gate still passed.
fn axis_rad(ctx: &Ctx) -> f64 {
    // Pair 0 keeps the N=2 geometry; at N=4+ rotate each pair's workspace so paired roles do not
    // land on the same stations while preserving one shared axis inside each pair.
    f64::from(ctx.engine().seed % 360).to_radians() + ctx.index().saturating_div(2) as f64 * 0.83
}

/// Signed shortest angle from `from` to `to`, in degrees.
fn delta_deg(from: f64, to: f64) -> f64 {
    (to - from + 540.0) % 360.0 - 180.0
}

/// MEASURED relative geometry of the pair, in the event stream so windows can be selected by it.
///
/// Azimuth is computed in the same math convention the stations are built in (`atan2(dz, dx)`,
/// zero on the shared axis) and never mixed with the notchian yaw the look APIs speak — the two
/// differ by a sign and 90 deg, and mixing them is how an arc ends up on the wrong quarter.
fn pair_geometry(ctx: &Ctx, peer: Option<usize>, facing_deg: f64, axis: f64, pivot: bool) -> Value {
    let (Some(own), Some(head)) = (
        ctx.position(),
        peer.and_then(|j| ctx.peer_position(j)).map(head_of),
    ) else {
        return json!({"peer_visible": false, "is_pivot": pivot});
    };
    let (hub, rim) = if pivot { (own, head) } else { (head, own) };
    let azimuth = (rim.2 - hub.2)
        .atan2(rim.0 - hub.0)
        .to_degrees()
        .rem_euclid(360.0)
        - axis.to_degrees();
    let off = delta_deg(facing_deg, look_angles(head_of(own), head).0);
    json!({
        "is_pivot": pivot,
        "peer_visible": true,
        "baseline": r2((head.0 - own.0).hypot(head.2 - own.2)),
        "azimuth_deg": r2((azimuth + 540.0) % 360.0 - 180.0),
        "peer_off_axis_deg": r2(off),
        "peer_in_frame": off.abs() <= HALF_FOV_DEG,
        "facing_deg": r2(facing_deg),
    })
}

/// Cover ground with the camera PINNED on `bearing_deg`: out and back on one key pair.
///
/// `hold_visual` is the only primitive that decouples the two — it pins `CameraIntent::LookAt` at
/// the current angle and presses one movement key for the whole duration, while `move_to` sets
/// `CameraIntent::Hold`, which steers the camera onto the travel bearing (`control.rs:762`) and
/// would discard the framing this episode exists to record. One long press is also the opposite of
/// key chatter, and a body that moves under a still camera contributes nothing to the
/// camera/travel-independence denominator.
///
/// `lateral` picks the key pair, and that is what keeps the two POVs off each other: the pivot
/// shuffles sideways while the walker steps along its sight line, so the frame-by-frame key tuples
/// the near-clones gate compares are not even drawn from the same alphabet. Roles swap between
/// sweeps, so both bots still exercise both.
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
        // A refused press is the stall guard: the body is leaning on terrain, and further presses
        // there are the short ones the chatter gate counts. Break the footing loose, then re-pin —
        // `shake_loose` looks somewhere random on its way out.
        shake_loose(ctx).await;
        if let Some(p) = ctx.position() {
            ctx.look_default(point_at_yaw(head_of(p), bearing_deg, 24.0))
                .await;
        }
    }
}

/// One camera stands still while the other walks to a series of azimuths around it, and the two
/// swap those parts every sweep.
///
/// The revisit problem: the walker's TRAJECTORY closes because [`SWEEP`] is a palindrome, and the
/// pivot's SIGHT LINE closes because it must keep re-acquiring a teammate that has moved 45 deg
/// around it — out to the far side and back again, every turn confirmed with `look_until_facing`
/// rather than assumed (a plain `look` returns on its deadline, so a wide turn that timed out would
/// be recorded as an acquisition that never happened). Occlusion is whatever the real terrain on
/// the arc puts between them, which is also why the arc is walked rather than teleported through.
///
/// What it gives the world model: relative azimuth as a continuous, labelled sweep over one pair of
/// cameras — the same two bodies seen at 0, 45, 90, 135 and 180 deg of separation about a fixed
/// vertex, with the measured azimuth, baseline and in-frame flag on every station. That is the
/// conditioning axis a multi-view generator needs and the one thing a corpus of hard-coded
/// two-player scenes cannot vary.
///
/// Distinct from `group_orbit` and `orbit_lane_swap`, which circulate continuously on fixed radii:
/// here the arc is a set of DISCRETE stations held long enough to be a sample, and the pivot is a
/// bot rather than a point.
pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let peer = Some(index ^ 1).filter(|&j| j < ctx.player_count());
    let anchor = ctx.anchor();
    let axis = axis_rad(ctx);
    // Per-bot press length: the near-clones gate compares packed key tuples FRAME BY FRAME, and a
    // pair pressing together measured 80.2% against an 80% ceiling. Role asymmetry does most of the
    // work here, this covers the beats where both happen to hold the same part.
    let press_ms = 1_300 + (global % 3) as u64 * 250;
    ctx.record(
        "observe",
        &format!("vis-arc-setup-{index}"),
        "Resolve the shared arc axis and the pivot/walker rotation.",
        json!({
            "global_index": global,
            "axis_deg": r2(axis.to_degrees()),
            "arc_radius": ARC_RADIUS,
            "sweep_deg": SWEEP,
            "station_ms": STATION_MS,
            "press_ms": press_ms,
        }),
    );
    sleep_ms(global as u64 * 900).await;

    while !ctx.target_met() {
        // Station and role both come off the shared clock, never off a local counter: two local
        // counters drift apart the moment one bot's leg takes longer, and a drifted pair can put
        // both bots in the pivot part with nothing to sweep — the geometry would quietly stop
        // existing while QC still passed.
        let leg = (ctx.elapsed_ms() / STATION_MS).max(0);
        let deadline = (leg + 1) * STATION_MS;
        let turn = (leg as usize) / SWEEP.len();
        let pivot = (global + turn).is_multiple_of(2);
        let azimuth = axis + SWEEP[(leg as usize) % SWEEP.len()].to_radians();

        let target = if pivot {
            // The pivot only has to stay near its post; its work is the camera. Walk back when the
            // dwell has carried it off the vertex the azimuths are measured from.
            let drifted = ctx
                .position()
                .map(|p| (anchor.0 - p.0).hypot(anchor.1 - p.2) > PIVOT_LEASH)
                .unwrap_or(false);
            if drifted {
                travel_to(ctx, anchor, TRAVEL_HOP, 1.5).await;
            }
            None
        } else {
            // The walker's station is measured off the pivot's LIVE position, so the arc stays
            // centred on the body it is supposed to be centred on rather than on a stale point.
            let hub = peer
                .and_then(|j| ctx.peer_position(j))
                .map(|p| (p.0, p.2))
                .unwrap_or(anchor);
            let desired = (
                hub.0 + azimuth.cos() * ARC_RADIUS,
                hub.1 + azimuth.sin() * ARC_RADIUS,
            );
            let hint = ctx.position().map(|p| p.1.floor() as i32).unwrap_or(64);
            let alternate_angle = azimuth + std::f64::consts::FRAC_PI_2;
            let alternate = (
                hub.0 + alternate_angle.cos() * ARC_RADIUS,
                hub.1 + alternate_angle.sin() * ARC_RADIUS,
            );
            let Some(mut station) =
                open_station(ctx, desired, hint).or_else(|| open_station(ctx, alternate, hint))
            else {
                // Never route to an unverified point: keep the camera alive and try the next beat.
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
            Some(station)
        };

        // Both parts frame the teammate, so both POVs open on a frame that contains the other
        // player — the 22.4% of v5 multiplayer windows whose teammate only appears mid-window is
        // the defect this addresses.
        let focus = peer
            .and_then(|j| ctx.peer_position(j))
            .map(head_of)
            .unwrap_or((anchor.0, 66.0, anchor.1));
        let faced = ctx.look_until_facing(focus, 6.0, 5_000).await;
        let facing = ctx
            .position()
            .map(|p| look_angles(head_of(p), focus).0)
            .unwrap_or(0.0);
        let mut extras = pair_geometry(ctx, peer, facing, axis, pivot);
        extras["leg"] = json!(leg);
        extras["sweep_index"] = json!((leg as usize) % SWEEP.len());
        extras["azimuth_nominal_deg"] = json!(SWEEP[(leg as usize) % SWEEP.len()]);
        extras["role_turn"] = json!(turn);
        extras["faced"] = json!(faced);
        extras["station"] = json!(target.map(|s| json!({"x": r2(s.0), "z": r2(s.1)})));
        ctx.record(
            "observe",
            &format!("vis-arc-station-{index}-{leg}"),
            if pivot {
                "Pivot: re-acquire the teammate that has moved a step around the arc."
            } else {
                "Walker: hold the next azimuth station and frame the pivot."
            },
            extras,
        );
        // The pivot shuffles sideways under a camera locked on the walker; the walker steps along
        // its sight line. Different key pairs on the same frames, which is what keeps the pair off
        // the near-clones ceiling while both views stay on each other.
        pinned_dwell(ctx, facing, deadline, press_ms, pivot).await;
    }
}
