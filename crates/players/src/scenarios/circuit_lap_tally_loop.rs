//! CATALOG: family=construction min_players=1 single_process=false world_edits=true scenery=none far_players=false

use super::*;

const TAU: f64 = std::f64::consts::TAU;

/// Stations on the ring. Five gives a ~7-block side at the radius below (one or two `TRAVEL_HOP`
/// legs each) and an odd count, so a station is never diametrically opposite another one — the
/// camera bearing to the hub is different at every station of a lap.
const STATIONS: i64 = 5;
/// Ring radius. Close enough that a two-block post at the hub fills a useful part of the frame from
/// every station, far enough that walking the ring is travel rather than a shuffle.
const RING_RADIUS: f64 = 6.0;
/// Height of each tally post. Two blocks reads clearly from six blocks out and, unlike a taller
/// stack, its top cell is always inside placement reach from the ground beside it.
const POST_HEIGHT: i64 = 2;
/// Distance from the hub at which each lap's tally post is planted. Two blocks keeps every post
/// inside the frame that already contains the reference post, and inside reach of one standing spot.
const TALLY_RADIUS: f64 = 2.0;
/// A kit item (`EQUIP_ITEMS`).
const MATERIAL: &str = "minecraft:stone_bricks";
/// Legs of a dolly dwell, in blocks.
const DOLLY_LEG: f64 = 4.0;

/// Draw in `[lo, hi)` from the per-bot stream, so a number this scenario could have hard-coded
/// varies per bot and per station and still replays from the seed.
fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

/// Station `slot` of the ring around `hub`. One angle rather than five written-out offsets: the
/// ring's orientation is per-bot, and hand-rotated offsets are where a sign error hides silently.
fn station(hub: (f64, f64), axis: f64, radius: f64, slot: i64) -> (f64, f64) {
    let theta = axis + slot.rem_euclid(STATIONS) as f64 * TAU / STATIONS as f64;
    (hub.0 + theta.cos() * radius, hub.1 + theta.sin() * radius)
}

/// Dwell on the CURRENT view by dollying: walk `leg` blocks along the facing, then the same
/// distance backward, leaving the camera exactly where the caller pointed it, until `deadline_ms`.
/// Returns how many halves actually displaced the body.
///
/// `hold_visual` pins the camera to the angle it finds, so across a dwell the view only
/// TRANSLATES along the sight line. That is what the frustum audit rewards: `overlap_mean` is
/// measured against the window's FIRST frame and a window with no rotation scores 1.0, while
/// `net_over_sweep` is 0.0 whenever the yaw range is empty. A dwell longer than one window
/// (93 frames = 4.65 s) therefore contributes whole windows of pure revisit.
///
/// Each half is ONE press of 0.9-1.4 s, over the `SHORT_RUN_FRAMES` (0.5 s) chatter floor, and a
/// blocked half is cut by `hold_visual`'s stall guard only after 1.2 s, so even walking into a tree
/// cannot produce a short press. The two halves cancel, so the dwell keeps the body moving for the
/// frozen gate without inflating the window displacement median.
///
/// Turns are never taken during a dolly. `qc.rs`'s camera/travel gate is TWO-SIDED (`agreement()`
/// is `max(toward, away) / total`), so a turn while translating is evidence against the episode
/// whichever way it points, while a turn below `CAMERA_SPEED_FLOOR` (0.05 blocks/frame) is not
/// evidence at all.
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

/// Circle a hub with the camera locked on it, and add one tally post at the hub every time the lap
/// closes — so the revisited subject demonstrably CHANGES between visits.
///
/// The revisit problem, the verifiability face. Every other way of closing a loop leaves an
/// ambiguity when the footage is scored: if the model's prediction of a revisited region differs
/// from the ground truth, nothing in the episode says whether the model hallucinated or the world
/// moved. Here the world moves on a schedule the episode records — exactly one two-block post
/// appears at the hub per lap, nowhere else, and the camera is on the hub at every single station —
/// so a difference between consecutive laps is attributable. The trajectory closes by construction
/// (a fixed ring walked repeatedly, `path_length` climbing while displacement stays inside one
/// side), and the gaze never leaves the subject: the aiming point is the FIRST post's top for the
/// whole episode, so each station's frustum is re-entered once per lap with a known increment
/// inside it.
///
/// Lap chirality alternates. A one-way ring makes every station turn share a sign, which is how v5
/// ended up with a 0.80 net/sweep ratio; reversing the lap inverts them.
///
/// Distinct from `circuit_petal_hub_return`, which walks THROUGH its hub three times a lap and
/// never edits it, and from the build scenarios (`radial_trail_build`, `structure_metronome`,
/// `rosette_tower_ring`), which are placement scripts whose camera follows the work: here the
/// building is one post per lap — a beat, not the activity — and it exists only to date-stamp the
/// revisit.
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

    // Hub between the bot and the anchor, so in the first two-player tier the two rings interleave and a teammate is in
    // frame from the FIRST frame instead of walking into it mid-window (the v5 barge-in defect).
    let hub = walkable_xz_near(
        ctx,
        ((start.0 + anchor.0) * 0.5, (start.2 + anchor.1) * 0.5),
        start.1.round() as i32,
    )
    .unwrap_or((start.0, start.2));
    // Per-bot ring orientation and pacing: two bots tracing congruent rings produce near-identical
    // action streams, which is what the `near-clones` QC check reads.
    let axis = vary(ctx, 0.0, TAU);
    let radius = vary(ctx, RING_RADIUS - 0.5, RING_RADIUS + 1.5);
    let dwell_ms = vary(ctx, 7_000.0, 9_500.0) as i64;

    travel_to(ctx, hub, TRAVEL_HOP, 1.6).await;
    let feet = ctx.position().map(|p| p.1).unwrap_or(start.1);
    let reference = raise_landmark(
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
    // Without the reference post the hub is still a place, just an unmarked one — aim over the hub
    // cell so the script degrades to a coordinate return rather than losing its camera target.
    let focus = reference
        .map(|(_, point)| point)
        .unwrap_or((hub.0, feet + 1.4, hub.1));
    ctx.record(
        "observe",
        &format!("vis-tally-setup-{index}"),
        "Raise the reference post the whole episode aims at, before any tally is added.",
        json!({
            "global_index": global,
            "role": "tally_patrol",
            "opened_on_anchor": opened,
            "hub": {"x": r2(hub.0), "z": r2(hub.1)},
            "reference_placed": reference.is_some(),
            "focus": {"x": r2(focus.0), "y": r2(focus.1), "z": r2(focus.2)},
            "axis_deg": r2(axis.to_degrees()),
            "radius": r2(radius),
            "stations": STATIONS,
            "dwell_ms": dwell_ms,
        }),
    );

    let mut slot: i64 = 0;
    let mut visits = 0u32;
    // +1 walks the ring counter-clockwise, -1 clockwise; flipped when a lap closes.
    let mut turn: i64 = 1;
    let mut lap = 0u32;
    while !ctx.target_met() {
        slot += turn;
        let target = station(hub, axis, radius, slot);
        let gap = travel_to(ctx, target, TRAVEL_HOP, 1.6).await;
        if ctx.target_met() {
            break;
        }
        // Turn onto the reference post and CONFIRM it: a plain `look` returns on its deadline, so a
        // wide turn that timed out would be recorded as a sighting that never happened.
        let confirmed = ctx.look_until_facing(focus, 6.0, 5_000).await;
        // Read the post from here. Two laps' readings of the same cell are what makes a difference
        // in the rendered frames attributable to the tally rather than to the model.
        let seen = reference.and_then(|(base, _)| ctx.block_id_at(base));
        let leg = vary(ctx, DOLLY_LEG - 0.5, DOLLY_LEG + 0.8);
        let halves = dolly_dwell(ctx, leg, ctx.elapsed_ms() + dwell_ms).await;
        ctx.record(
            TYPE_LOOK,
            &format!("vis-tally-station-{index}-{visits}"),
            &format!(
                "Station {}: hold the hub post and dolly along the sight line.",
                slot.rem_euclid(STATIONS)
            ),
            json!({
                "visit": visits,
                "lap": lap,
                "chirality": turn,
                "at_slot": slot.rem_euclid(STATIONS),
                "tally_posts": lap,
                "arrival_gap": r2(gap),
                "facing_confirmed": confirmed,
                "reference_block": seen,
                "dolly_halves_moved": halves,
                "elapsed_ms": ctx.elapsed_ms(),
            }),
        );
        visits += 1;
        if !visits.is_multiple_of(STATIONS as u32) {
            continue;
        }

        // LAP CLOSED. Step into the hub, plant this lap's tally post, and reverse the ring.
        travel_to(ctx, hub, TRAVEL_HOP, 2.2).await;
        let stand = ctx.position().map(|p| p.1).unwrap_or(feet);
        // Each lap's post goes on its own bearing around the hub, so posts never contend for a cell
        // and the cluster grows outward-visible rather than stacking out of reach.
        let bearing = axis + lap as f64 * TAU / STATIONS as f64;
        let desired = (
            (hub.0 + bearing.cos() * TALLY_RADIUS).round() as i64,
            stand.round() as i64,
            (hub.1 + bearing.sin() * TALLY_RADIUS).round() as i64,
        );
        let planted = raise_landmark(ctx, desired, POST_HEIGHT, MATERIAL).await;
        // Back onto the reference post: the tally is a beat inside the patrol, not a new subject.
        let back = ctx.look_until_facing(focus, 6.0, 5_000).await;
        let leg = vary(ctx, DOLLY_LEG - 0.5, DOLLY_LEG + 0.8);
        let halves = dolly_dwell(ctx, leg, ctx.elapsed_ms() + dwell_ms / 2).await;
        lap += 1;
        turn = -turn;
        ctx.record(
            TYPE_PLACE,
            &format!("vis-tally-mark-{index}-{lap}"),
            "Lap closed: plant this lap's tally post at the hub and reverse the ring.",
            json!({
                "lap": lap,
                "chirality": turn,
                "desired_cell": {"x": desired.0, "y": desired.1, "z": desired.2},
                "planted": planted.is_some(),
                "planted_base": planted
                    .map(|(b, _)| json!({"x": b.x, "y": b.y, "z": b.z})),
                "post_height": POST_HEIGHT,
                "facing_confirmed": back,
                "dolly_halves_moved": halves,
                "elapsed_ms": ctx.elapsed_ms(),
            }),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{RING_RADIUS, STATIONS, station};

    /// The bug this pins: an off-by-one in the slot-to-angle map (or a radius applied before the
    /// rotation) makes the "ring" an ellipse or leaves a short side, and the leg budget the dolly
    /// and the arrival tolerance are sized against no longer holds at every station.
    #[test]
    fn stations_are_a_closed_regular_ring_about_the_hub() {
        let hub = (-4.0, 62.0);
        let side = 2.0 * RING_RADIUS * (std::f64::consts::PI / STATIONS as f64).sin();
        for &axis in &[0.0_f64, 1.4, 5.2] {
            for slot in 0..STATIONS {
                let a = station(hub, axis, RING_RADIUS, slot);
                let b = station(hub, axis, RING_RADIUS, slot + 1);
                let radius = (a.0 - hub.0).hypot(a.1 - hub.1);
                assert!(
                    (radius - RING_RADIUS).abs() < 1e-9,
                    "slot={slot} r={radius}"
                );
                let chord = (a.0 - b.0).hypot(a.1 - b.1);
                assert!((chord - side).abs() < 1e-9, "slot={slot} chord={chord}");
            }
            let zero = station(hub, axis, RING_RADIUS, 0);
            let wrapped = station(hub, axis, RING_RADIUS, STATIONS);
            assert!((zero.0 - wrapped.0).abs() < 1e-9 && (zero.1 - wrapped.1).abs() < 1e-9);
        }
    }
}
