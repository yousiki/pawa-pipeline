//! CATALOG: family=locomotion min_players=1 single_process=false world_edits=true scenery=none far_players=false

use super::*;

/// Spokes in the ladder. Eight steps of 45 deg tile the full turn around the hub, and 45 deg is
/// under half the 102.5 deg horizontal FOV, so consecutive spokes' inbound views OVERLAP — the
/// ladder is a continuous sweep of viewpoints on one subject, not eight disjoint ones.
const SPOKES: i64 = 8;
/// Degrees between consecutive spokes. `SPOKES * STEP_DEG` must be a full turn.
const STEP_DEG: f64 = 45.0;
/// Spoke length. A traverse at ~4.5 blocks/s takes under two seconds, which keeps a single audit
/// window's displacement near the 5-block target even when the window lands wholly on a traverse;
/// and at seven blocks a three-tall hub post is still a substantial subject in the frame.
const SPOKE: f64 = 7.0;
/// Hub post height. Three puts the aiming point above head height so it clears grass and a shallow
/// rise from the far end of a spoke.
const POST_HEIGHT: i64 = 3;
/// A kit item (`EQUIP_ITEMS`).
const MATERIAL: &str = "minecraft:stone_bricks";
/// Legs of a dolly dwell, in blocks.
const DOLLY_LEG: f64 = 4.0;

/// Draw in `[lo, hi)` from the per-bot stream, so a number this scenario could have hard-coded
/// varies per bot and per spoke and still replays from the seed.
fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

/// Turn to a world bearing WHILE STANDING STILL.
///
/// Every scripted turn in this family is stationary on purpose. `qc.rs`'s camera/travel gate is
/// TWO-SIDED — `agreement()` is `max(toward, away) / total` — so a turn taken while the body
/// translates is evidence against the episode whichever way it points, while a turn taken below
/// `CAMERA_SPEED_FLOOR` (0.05 blocks per frame) is not evidence at all.
async fn face_bearing(ctx: &Ctx, bearing_deg: f64) {
    if let Some(p) = ctx.position() {
        ctx.look_default(point_at_yaw(head_of(p), bearing_deg, 24.0))
            .await;
    }
}

/// Dwell on the CURRENT view by dollying: walk `leg` blocks along the facing, then the same
/// distance backward, leaving the camera exactly where the caller pointed it, until `deadline_ms`.
/// Returns how many halves actually displaced the body.
///
/// `hold_visual` pins the camera to the angle it finds, so across a dwell the view only
/// TRANSLATES along the sight line. That is what the frustum audit rewards: `overlap_mean` is
/// measured against the window's FIRST frame and a window with no rotation scores 1.0, while
/// `net_over_sweep` is 0.0 whenever the yaw range is empty. A dwell longer than one window
/// (93 frames = 4.65 s) therefore contributes whole windows of pure revisit — and along a spoke the
/// dolly is a pure dolly toward and away from the hub post, the cleanest translation-only motion
/// the runtime can produce.
///
/// Each half is ONE press of 0.9-1.4 s, over the `SHORT_RUN_FRAMES` (0.5 s) chatter floor, and a
/// blocked half is cut by `hold_visual`'s stall guard only after 1.2 s, so even walking into a tree
/// cannot produce a short press. The two halves cancel, so the dwell keeps the body moving for the
/// frozen gate without inflating the window displacement median.
async fn dolly_dwell(ctx: &mut Ctx, leg: f64, deadline_ms: i64) -> u32 {
    let hold_ms = ((leg / 4.3 * 1_000.0) as u64).clamp(900, 2_600);
    let sprint_prob = ctx.cfg().sprint_bout_prob;
    let mut moved = 0u32;
    let mut outbound = true;
    while !ctx.target_met() && ctx.elapsed_ms() + hold_ms as i64 + 200 < deadline_ms {
        let before = ctx.position();
        // Sprint only on forward halves (it does nothing backward) and only sometimes: `qc.rs`
        // fails an episode whose sustained forward runs all agree on the sprint bit.
        let sprint = outbound && ctx.rng_next() < sprint_prob;
        let dir = if outbound {
            WalkDirection::Forward
        } else {
            WalkDirection::Backward
        };
        ctx.hold_visual(dir, sprint, hold_ms, 0).await;
        if let (Some(a), Some(b)) = (before, ctx.position())
            && (b.0 - a.0).hypot(b.2 - a.2) > 1.0
        {
            moved += 1;
        }
        outbound = !outbound;
    }
    moved
}

/// Walk out and back along a hub's spokes in a fixed 45 deg ladder, scanning the ladder up to the
/// last spoke and then back down it, dwelling on the hub post from the far end of every spoke.
///
/// The revisit problem, the viewpoint-coverage face. `circuit_shuttle_gate` drills two views
/// deeply; this script does the opposite — it tiles the whole circle of approach directions to ONE
/// subject with overlapping 45 deg steps, so an episode carries a continuum of viewpoints on the
/// same three blocks. Each round is itself closed (out N blocks, always straight back down the same
/// spoke), so displacement over any window stays inside one spoke while `path_length` is twice the
/// ladder.
///
/// Two things make the returns evidence rather than assumption. The far-end turn onto the hub is a
/// ~180 deg reversal — far past the 102.5 deg FOV, so the outbound view and the hub view share no
/// pixels — and it is taken with `look_until_facing`, because a plain `look` returns on its deadline
/// and a wide turn that timed out would be recorded as a return that never happened. And the ladder
/// SCANS: it climbs to the last spoke, then walks back down, so every spoke except the two ends is
/// visited twice and the ladder's net rotation reverses instead of accumulating — a monotone ladder
/// would spin one way all episode, which is the v5 defect in a different costume.
///
/// Distinct from `landmark_tour`, which visits several different landmarks once each, and from
/// `totem_orbit_film`, which circles a subject at constant radius so the camera never stops turning:
/// here the radius is what changes and the bearing is held still, one spoke at a time.
pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let anchor = ctx.anchor();
    let start = ctx.position().unwrap_or((anchor.0, 64.0, anchor.1));

    // Brief rule 5: open facing the SHARED subject. Every bot in the episode turns onto the anchor
    // before it does anything else, so the opening frame — the only context frame a 93-frame
    // training window gets — already contains the anchor's neighbourhood and whoever else is
    // standing in it. Solaris gets the same effect from a forced `lookAt` with `randomized:false`.
    // The turn is stationary, so it is invisible to the camera/travel gate.
    let opened = ctx
        .look_until_facing((anchor.0, start.1 + 1.4, anchor.1), 10.0, 4_000)
        .await;

    // Hub between the bot and the anchor: in the first two-player tier the two hubs sit close enough that the other bot's
    // spokes cross this one's, so a teammate is in frame from the FIRST frame instead of walking
    // into it mid-window (the 22.4% barge-in defect in the v5 audit).
    let hub = walkable_xz_near(
        ctx,
        ((start.0 + anchor.0) * 0.5, (start.2 + anchor.1) * 0.5),
        start.1.round() as i32,
    )
    .unwrap_or((start.0, start.2));
    // Per-bot ladder phase and pacing: two bots scanning the same bearings in the same order produce
    // near-identical action streams, which is what the `near-clones` QC check reads.
    let base_deg = vary(ctx, -180.0, 180.0);
    let spoke_len = vary(ctx, SPOKE - 1.0, SPOKE + 1.0);
    let far_dwell_ms = vary(ctx, 7_000.0, 9_500.0) as i64;
    let hub_dwell_ms = vary(ctx, 5_000.0, 6_500.0) as i64;

    travel_to(ctx, hub, TRAVEL_HOP, 1.6).await;
    let feet = ctx.position().map(|p| p.1).unwrap_or(start.1);
    let raised = raise_landmark(
        ctx,
        (
            hub.0.round() as i64,
            feet.round() as i64,
            hub.1.round() as i64,
        ),
        POST_HEIGHT,
        MATERIAL,
    )
    .await;
    // Without the post the hub is still a place, just an unmarked one — aim over the hub cell so the
    // script degrades to a coordinate return rather than losing its camera target.
    let focus = raised
        .map(|(_, point)| point)
        .unwrap_or((hub.0, feet + 2.0, hub.1));
    ctx.record(
        "observe",
        &format!("vis-ladder-setup-{index}"),
        "Raise the hub post every spoke of the ladder looks back at.",
        json!({
            "global_index": global,
            "role": "bearing_ladder",
            "opened_on_anchor": opened,
            "hub": {"x": r2(hub.0), "z": r2(hub.1)},
            "post_placed": raised.is_some(),
            "focus": {"x": r2(focus.0), "y": r2(focus.1), "z": r2(focus.2)},
            "base_deg": r2(base_deg),
            "spoke_blocks": r2(spoke_len),
            "spokes": SPOKES,
            "step_deg": STEP_DEG,
            "far_dwell_ms": far_dwell_ms,
            "hub_dwell_ms": hub_dwell_ms,
        }),
    );

    // The scan: climb 0..SPOKES-1, then descend. Reversing at the ends is what stops the ladder
    // from becoming one continuous rotation over the episode.
    let mut spoke: i64 = 0;
    let mut step: i64 = 1;
    let mut round = 0u32;
    while !ctx.target_met() {
        let bearing = base_deg + spoke as f64 * STEP_DEG;

        // Face the spoke and dwell at the hub before leaving. This is the OUTBOUND view of this
        // spoke, and the ladder brings the bot back to it on the descent — so it, too, is a frustum
        // the episode re-enters.
        face_bearing(ctx, bearing).await;
        let leg = vary(ctx, DOLLY_LEG - 0.5, DOLLY_LEG + 0.8);
        let hub_halves = dolly_dwell(ctx, leg, ctx.elapsed_ms() + hub_dwell_ms).await;
        if ctx.target_met() {
            break;
        }

        // OUT. `travel_to`'s legs steer the camera onto the travel bearing, which is the bearing
        // just faced, so the traverse adds no camera turn of its own.
        let out = point_at_yaw((hub.0, feet, hub.1), bearing, spoke_len);
        let gap = travel_to(ctx, (out.0, out.2), TRAVEL_HOP, 1.6).await;
        if ctx.target_met() {
            break;
        }

        // BACK-SIGHT: the confirmed ~180 deg reversal onto the hub post, then the round's main
        // dwell — a dolly straight down the spoke, so the post grows and shrinks in a frame that
        // never rotates.
        let confirmed = ctx.look_until_facing(focus, 6.0, 5_000).await;
        let leg = vary(ctx, DOLLY_LEG - 0.5, DOLLY_LEG + 0.8);
        let far_halves = dolly_dwell(ctx, leg, ctx.elapsed_ms() + far_dwell_ms).await;
        // HOME along the same spoke. The camera is already on the hub, and travel is toward it.
        travel_to(ctx, hub, TRAVEL_HOP, 1.8).await;

        ctx.record(
            TYPE_LOOK,
            &format!("vis-ladder-spoke-{index}-{round}"),
            &format!(
                "Spoke {spoke} at {bearing:.0} deg: out, look back at the hub post, dolly, home."
            ),
            json!({
                "round": round,
                "spoke": spoke,
                "scan_step": step,
                "bearing_deg": r2(bearing),
                "spoke_blocks": r2(spoke_len),
                "far_gap": r2(gap),
                "facing_confirmed": confirmed,
                "hub_dolly_halves": hub_halves,
                "far_dolly_halves": far_halves,
                "elapsed_ms": ctx.elapsed_ms(),
            }),
        );

        // Reverse at either end of the ladder rather than wrapping around: wrapping would make the
        // bearing advance monotonically for the whole episode.
        if (spoke + step) < 0 || (spoke + step) >= SPOKES {
            step = -step;
        }
        spoke += step;
        round += 1;
    }
}
