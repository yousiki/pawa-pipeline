//! CATALOG: family=group_motion min_players=2 single_process=true world_edits=true scenery=none far_players=false
//!
//! Two players watch ONE subject from near-opposite sides, each sliding along its own arc around it,
//! so every frame from either camera holds the same pillar AND the other player standing behind it.
//!
//! This is the multi-view-consistency face of the revisit problem. Two cameras aimed at one subject from two
//! positions constrain each other geometrically, which is the supervision signal a two-player
//! hard-coded corpus cannot produce and our N-player swarm can. The angular separation is held in a
//! band around 130 deg for a non-obvious reason worth writing down: with both cameras aimed at the
//! subject, the partner's offset from the line of sight is `90 - separation/2` degrees, so a SMALL
//! separation pushes the partner out of the 102.5 deg field of view and a LARGE one pulls it in
//! behind the subject. 70 deg would put the teammate 55 deg off-axis and lose it; 130 deg puts it
//! 25 deg off-axis, comfortably framed, which is what makes the teammate present in the context
//! frame instead of walking into it later (the 22.4% v5 failure).
//!
//! The gaze loop is per beat: confirmed re-acquire of the subject, a leg under a pinned view, a
//! re-acquire. The trajectory loop is the band controller — each bot's arc slide reverses whenever
//! the separation leaves the band, so the pair oscillates around 130 deg forever and path length runs
//! several times net displacement.
//!
//! Not `group_orbit` or `totem_orbit_film`: those track their subject WHILE walking, which is the
//! documented 96-100% failure of the camera/travel-independence gate. Here the camera only ever turns
//! while the body is standing, and the arc legs run with the view pinned.

use super::*;

const MATERIAL: &str = "minecraft:stone_bricks";

/// Subject height. Three blocks is tall enough to read as a landmark from 6-8 blocks out at a level
/// camera, and short enough that a standing partner behind it is still visible above and beside it.
const SUBJECT_HEIGHT: i64 = 3;

/// Arc radius per side. The two differ on purpose: the same 4-block leg is 38 deg of arc at 6.0 and
/// 30 deg at 7.5, so the two bots' band-controller reversals fall at different times and their key
/// streams separate structurally rather than by jitter alone (jitter alone measured 80.2% pair
/// agreement against the 80% near-clones ceiling in `eval_shared_unobserved`).
const RADII: [f64; 2] = [6.0, 7.5];

/// The separation the pair opens on, and the band it is steered back into.
///
/// The floor is where the partner starts leaving the field of view (at 100 deg it sits 40 deg
/// off-axis, near the frame edge). The ceiling stops short of 180 deg, where the subject would stand
/// directly between the two cameras and occlude the partner outright — that shot is
/// `duet_screen_peek`'s subject, not this one's.
///
/// The band is only ~50 deg wide while one arc leg is ~30 deg of arc, so only ONE of the two bots may
/// move tangentially on any given beat — see the tangential/radial alternation in the loop. With both
/// sliding at once a single beat could swing the separation 60-70 deg and the controller would spend
/// the episode chasing its own overshoot out past the field of view.
const TARGET_SEP_DEG: f64 = 130.0;
const MIN_SEP_DEG: f64 = 108.0;
const MAX_SEP_DEG: f64 = 158.0;

/// How near and how far a bot may drift from the subject on its radial beats. Bounded because the
/// subject has to stay a readable size in frame and the pair has to stay clear of [`MIN_PEER_GAP`]
/// even at the narrow end of the separation band.
const MIN_RADIUS: f64 = 5.0;
const MAX_RADIUS: f64 = 9.0;

/// Tolerance for "the subject is centred". Tighter than the peer tolerance because this is the frame
/// two POVs are supposed to agree on.
const SUBJECT_TOL_DEG: f64 = 6.0;

/// One arc leg. 850 ms clears the key-chatter floor (over 35% of movement presses under 10 ticks =
/// 0.5 s fails the episode) with margin for the 50 ms control tick, and at walking pace covers ~3.7
/// blocks — under one radius, so the subject never swings outside the frame mid-leg.
const LEG_MS: u64 = 1_400;
const MIN_PRESS_MS: u64 = 700;

/// Draw in `[lo, hi)` from the per-bot stream, so a number this scenario could have hard-coded varies
/// per bot and per beat and still replays from the seed.
fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

/// The pair's shared bearing, agreed with NO messaging: the engine hands every bot the same
/// `native_waypoints` list, so its first entry gives both sides the same arc origin.
fn shared_axis(ctx: &Ctx) -> (f64, f64) {
    let waypoint = ctx
        .engine()
        .shared
        .native_waypoints
        .first()
        .copied()
        .unwrap_or((1.0, 0.0));
    let span = waypoint.0.hypot(waypoint.1).max(0.001);
    // Keep the two roles of each pair aligned while rotating each pair's workspace for N=4+.
    let angle = ctx.index().saturating_div(2) as f64 * 0.83;
    let (sin, cos) = angle.sin_cos();
    (
        waypoint.0 / span * cos - waypoint.1 / span * sin,
        waypoint.0 / span * sin + waypoint.1 / span * cos,
    )
}

/// Angular separation (degrees, 0..180) between the two bots as seen FROM the subject. This is the
/// controlled variable: it decides both whether the partner is in frame and how much the two views
/// constrain each other.
fn separation_deg(ctx: &Ctx, partner: Option<usize>, subject: (f64, f64)) -> Option<f64> {
    let own = ctx.position()?;
    let peer = ctx.peer_position(partner?)?;
    let a = (own.2 - subject.1).atan2(own.0 - subject.0);
    let b = (peer.2 - subject.1).atan2(peer.0 - subject.0);
    let d = (a - b).to_degrees().rem_euclid(360.0);
    Some(if d > 180.0 { 360.0 - d } else { d })
}

/// How far off the camera axis the partner sits, in degrees — the direct measure of "is the teammate
/// in frame", recorded per beat so the audit does not have to reconstruct it from geometry.
fn peer_off_axis_deg(ctx: &Ctx, partner: Option<usize>, subject: (f64, f64, f64)) -> Option<f64> {
    let own = ctx.position()?;
    let peer = ctx.peer_position(partner?)?;
    let eye = head_of(own);
    let to_subject = look_angles(eye, (subject.0, eye.1, subject.2)).0;
    let to_peer = look_angles(eye, head_of(peer)).0;
    Some(((to_peer - to_subject + 180.0).rem_euclid(360.0) - 180.0).abs())
}

fn flip(key: WalkDirection) -> WalkDirection {
    match key {
        WalkDirection::Left => WalkDirection::Right,
        _ => WalkDirection::Left,
    }
}

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let bot_count = ctx.player_count().max(1);
    let partner = Some(index ^ 1).filter(|&j| j < bot_count);
    let slot = global % 2;
    let radius = RADII[slot];
    // Opposite initial slide directions, so the pair starts by widening and narrowing the separation
    // out of phase rather than sliding in convoy.
    let mut key = if slot == 0 {
        WalkDirection::Left
    } else {
        WalkDirection::Right
    };

    let anchor = ctx.anchor();
    let axis = shared_axis(ctx);
    let base_angle = axis.1.atan2(axis.0);
    // Station bearing around the subject: slot 0 at the shared bearing, slot 1 a target separation
    // away from it. Derived, not negotiated, so both sides agree without any messaging.
    let station_angle = base_angle + slot as f64 * TARGET_SEP_DEG.to_radians();

    // The opening shot, before any travel: at `spread_range: 8` the partner is within a dozen blocks
    // at spawn, so the first recorded view already contains the teammate.
    let opening_confirmed = match partner.and_then(|j| ctx.peer_position(j)).map(head_of) {
        Some(target) => ctx.look_until_facing(target, 8.0, 4_000).await,
        None => false,
    };

    // Slot 0 alone raises the subject, and walks in to do it: `marker_base_near` searches only two
    // blocks out and `place_block` needs the cell in reach, so raising it from a spawn position eight
    // blocks off the anchor would silently place nothing. Building also costs a stretch of standing
    // still, which is why it is one bot's job rather than both (the participation floor is per POV).
    let raised = if slot == 0 {
        travel_to(ctx, anchor, TRAVEL_HOP, 2.5).await;
        let ground = ctx.position().map(|p| p.1).unwrap_or(64.0).round() as i64;
        raise_landmark(
            ctx,
            (anchor.0.round() as i64, ground, anchor.1.round() as i64),
            SUBJECT_HEIGHT,
            MATERIAL,
        )
        .await
    } else {
        None
    };

    let station = (
        anchor.0 + station_angle.cos() * radius,
        anchor.1 + station_angle.sin() * radius,
    );
    travel_to(ctx, station, TRAVEL_HOP, 1.5).await;
    let start = ctx.position().unwrap_or((station.0, 64.0, station.1));
    // Aim at the top of the subject when this bot raised it, otherwise at the anchor column at head
    // height: both sides then aim at the same physical thing, which is the premise of the whole shot.
    let subject = raised.map(|(_, point)| point).unwrap_or((
        anchor.0,
        start.1 + SUBJECT_HEIGHT as f64 - 0.5,
        anchor.1,
    ));
    ctx.record(
        "observe",
        &format!("vis-duet-sightline-setup-{index}"),
        "Take an arc station and open on the shared subject with the partner behind it.",
        json!({
            "global_index": global,
            "role": "twin_sightline",
            "partner": partner,
            "slot": slot,
            "radius": r2(radius),
            "target_separation_deg": TARGET_SEP_DEG,
            "subject": {"x": r2(subject.0), "y": r2(subject.1), "z": r2(subject.2)},
            "raised_subject": raised.is_some(),
            "opening_shot_confirmed": opening_confirmed,
        }),
    );

    let mut beat = 0u64;
    while !ctx.target_met() {
        // Wide turns happen HERE, standing: the camera/travel gate only weighs frames where the
        // camera turned while the body was moving, so a confirmed re-acquire between legs is free.
        let centred = ctx.look_until_facing(subject, SUBJECT_TOL_DEG, 5_000).await;
        let before = separation_deg(ctx, partner, (anchor.0, anchor.1));
        // Exactly one bot slides around the arc on any beat; the other walks a RADIAL leg, which
        // changes how large the subject sits in frame without touching the separation. The parity
        // gives each bot half the tangential beats, keeps the separation inside a band only 50 deg
        // wide, and — because the two are running different keys on every single beat — is the
        // strongest near-clones separation available here.
        let tangential = (beat + slot as u64).is_multiple_of(2);
        let own_radius = ctx
            .position()
            .map(|p| (p.0 - anchor.0).hypot(p.2 - anchor.1))
            .unwrap_or(radius);
        let leg_key = if tangential {
            key
        } else if own_radius > MAX_RADIUS {
            // The camera is on the subject, so Forward closes on it and Backward opens away from it.
            WalkDirection::Forward
        } else if own_radius < MIN_RADIUS {
            WalkDirection::Backward
        } else if beat.is_multiple_of(4) {
            WalkDirection::Forward
        } else {
            WalkDirection::Backward
        };
        // The leg. `hold_visual` pins the camera (`CameraIntent::LookAt`) and drives one key with no
        // pathfinder waypoint, so the subject and the partner stay where the re-acquire put them for
        // the whole leg, and the leg contributes nothing to the camera/travel gate.
        let leg_ms = (LEG_MS as f64 * vary(ctx, 0.9, 1.2)) as u64;
        let ran = ctx.hold_visual(leg_key, false, leg_ms, 0).await;
        let after = separation_deg(ctx, partner, (anchor.0, anchor.1));

        // Band controller instead of deriving the sign of "which strafe key runs which way around
        // the arc" from the yaw convention: measure the separation across the leg and reverse when it
        // left the band in the direction the leg was pushing it. Closed loop, so it stays correct
        // whichever way the strafe sign resolves, and it is what turns the arc into a shuttle rather
        // than a one-way orbit.
        if let (Some(before), Some(after)) = (before, after) {
            let widening = after > before;
            if (after > MAX_SEP_DEG && widening) || (after < MIN_SEP_DEG && !widening) {
                key = flip(key);
            }
        }
        if !ran {
            // The stall guard cut the press against terrain. Re-pressing is what turned every step
            // into a sub-0.5 s press in `mirror_dance`'s round-2 key-chatter failure (37% against a
            // 35% ceiling), so recover the arc station with ONE pathfinder leg instead.
            let want = (
                anchor.0 + station_angle.cos() * radius,
                anchor.1 + station_angle.sin() * radius,
            );
            let feet_y = ctx.position().map(|p| p.1).unwrap_or(start.1).floor() as i32;
            let target = walkable_xz_near(ctx, want, feet_y).unwrap_or(want);
            ctx.move_to(target.0, target.1, 6_000, 0.8).await;
            key = flip(key);
        }

        ctx.record(
            TYPE_MOVE,
            &format!("vis-duet-sightline-beat-{index}-{beat}"),
            "Slide one leg along the arc with the shared subject and the partner both in frame.",
            json!({
                "beat": beat,
                "slot": slot,
                "radius": r2(radius),
                "subject_centred": centred,
                "tangential": tangential,
                "own_radius": r2(own_radius),
                "separation_deg": after.map(r2),
                "peer_off_axis_deg": peer_off_axis_deg(ctx, partner, subject).map(r2),
                "leg_completed": ran,
            }),
        );

        // Every fourth beat, the revisit reversal: turn past the field of view, spend a leg there,
        // and come back onto the subject with the return CONFIRMED. Away-and-back is ~3.1 s of turn
        // at the configured 72 deg/s plus one leg, so the whole reversal fits inside a single 4.65 s
        // training window — which is the only way a window's `net_over_sweep` can come out low.
        if beat.is_multiple_of(4)
            && let Some(own) = ctx.position()
        {
            let base_yaw = look_angles(head_of(own), subject).0;
            let away_deg = 110.0 + slot as f64 * 20.0;
            let away = point_at_yaw(head_of(own), base_yaw + away_deg, 24.0);
            let left = ctx.look_until_facing(away, 8.0, 5_000).await;
            ctx.hold_visual(key, false, LEG_MS.max(MIN_PRESS_MS), 0)
                .await;
            let returned = ctx.look_until_facing(subject, SUBJECT_TOL_DEG, 5_000).await;
            witness_peer_edits(ctx).await;
            ctx.record(
                TYPE_LOOK,
                &format!("vis-duet-sightline-away-{index}-{beat}"),
                "Turn off the shared subject past the field of view, then confirm the return.",
                json!({
                    "beat": beat,
                    "away_deg": r2(away_deg),
                    "exceeds_horizontal_fov": true,
                    "left_view": left,
                    "return_confirmed": returned,
                }),
            );
        }
        beat += 1;
        // Per-bot pause so the two arcs never step on the same frame.
        sleep_ms(vary(ctx, 150.0, 550.0) as u64).await;
    }
}
