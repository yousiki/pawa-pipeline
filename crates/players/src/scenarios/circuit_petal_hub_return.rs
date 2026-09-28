//! CATALOG: family=locomotion min_players=1 single_process=false world_edits=true scenery=none far_players=false

use super::*;

const TAU: f64 = std::f64::consts::TAU;

/// Petals per lap. Three keeps every petal's outbound and inbound edges more than the 102.5 deg
/// horizontal FOV apart (120 deg between petal axes), so a petal is genuinely a different piece of
/// ground rather than a re-walk of the last one — while the HUB view they all come home to is the
/// same every time.
const PETALS: usize = 3;
/// Distance from the hub to a petal corner. Two `TRAVEL_HOP` legs, and short enough that the hub
/// pillar is still a readable subject from the corner (it is three blocks tall).
const REACH: f64 = 7.0;
/// Half-angle between a petal's two corners, in radians. ~29 deg either side of the petal axis
/// makes the cross edge (~0.96 * REACH) about as long as the two radial edges, so no edge of the
/// triangle is a shuffle and none is a march.
const SPLAY: f64 = 0.5;
/// Hub pillar height. Three puts the aiming point above head height, so it clears grass and a
/// shallow rise from a corner seven blocks out.
const PILLAR_HEIGHT: i64 = 3;
/// A kit item (`EQUIP_ITEMS`).
const MATERIAL: &str = "minecraft:stone_bricks";
/// Legs of a dolly dwell, in blocks.
const DOLLY_LEG: f64 = 4.0;

/// Draw in `[lo, hi)` from the per-bot stream, so a number this scenario could have hard-coded
/// varies per bot and per petal and still replays from the seed.
fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

/// Corner `side` (0 or 1) of petal `petal`, `reach` blocks out from `hub`.
///
/// Derived from one angle rather than written as six offsets: the petal axis is per-bot, and
/// hand-rotated offsets are where a sign error hides in a way no runtime check would catch.
fn petal_corner(hub: (f64, f64), axis: f64, reach: f64, petal: usize, side: usize) -> (f64, f64) {
    let base = axis + petal as f64 * TAU / PETALS as f64;
    let theta = base + if side == 0 { -SPLAY } else { SPLAY };
    (hub.0 + theta.cos() * reach, hub.1 + theta.sin() * reach)
}

/// Dwell on the CURRENT view by dollying: walk `leg` blocks along the facing, then the same
/// distance backward, leaving the camera exactly where the caller pointed it, until `deadline_ms`.
/// Returns how many halves actually displaced the body.
///
/// `hold_visual` pins the camera to the angle it finds, so across a dwell the view only
/// TRANSLATES along the sight line. That is what the frustum audit rewards: `overlap_mean` is
/// measured against the window's FIRST frame and a window with no rotation scores 1.0, while
/// `net_over_sweep` is 0.0 whenever the yaw range is empty. A dwell longer than one window
/// (93 frames = 4.65 s) therefore contributes whole windows of pure revisit — and a dolly toward
/// and away from a landmark is the most informative form of that, because the subject changes
/// scale without leaving the frame.
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

/// Split one big circuit into three small closed petals that all come home to the SAME hub pillar,
/// and dwell on that pillar at every corner and every homecoming.
///
/// The revisit problem, the shared-subject face of a closed trajectory. A single wide loop passes
/// each piece of ground once per lap, which at our episode lengths is once or twice; three petals
/// off one hub bring the bot back to the same cell three times as often, and because the hub
/// carries a placed pillar the return has a SUBJECT rather than just a coordinate. Every dwell in
/// the episode — at the two corners of a petal and at the hub between petals — is a frozen-camera
/// dolly with that pillar in frame, so the ground truth of those windows is a region the POV has
/// rendered from a nearby cell several times already.
///
/// Petal chirality alternates: even petals walk corner 0 then corner 1, odd petals the reverse. The
/// two corner turns therefore change sign from petal to petal, which is what keeps a window's net
/// rotation from growing as fast as its sweep — the 0.80 ratio the v5 audit measured came from
/// exactly the opposite, every turn sharing a sign.
///
/// Distinct from `converge_scatter_pulse`, which pulses in and out along ONE radial spoke per bot
/// (out and back on the same line, with the camera on peers and free sweeps), and from
/// `spiral_march`, whose private parametric curves never return to a fixed point at all: here the
/// return path is a different edge from the outbound one, so the hub is re-approached from three
/// distinct bearings while the subject stays put.
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

    // Hub between the bot and the anchor: at N=2 both hubs are pulled toward the shared anchor, so
    // the two bots' petals interleave and a teammate is in frame from the FIRST frame instead of
    // walking into it mid-window (the 22.4% barge-in defect in the v5 audit).
    let hub_xz = walkable_xz_near(
        ctx,
        ((start.0 + anchor.0) * 0.5, (start.2 + anchor.1) * 0.5),
        start.1.round() as i32,
    )
    .unwrap_or((start.0, start.2));
    // Per-bot petal orientation: two bots tracing congruent rosettes produce near-identical action
    // streams, which is what the `near-clones` QC check reads.
    let axis = vary(ctx, 0.0, TAU);
    let reach = vary(ctx, REACH - 1.0, REACH + 1.0);
    let hub_dwell_ms = vary(ctx, 7_500.0, 10_000.0) as i64;
    let corner_dwell_ms = vary(ctx, 5_500.0, 7_000.0) as i64;

    travel_to(ctx, hub_xz, TRAVEL_HOP, 1.6).await;
    let feet = ctx.position().map(|p| p.1).unwrap_or(start.1);
    let raised = raise_landmark(
        ctx,
        (
            hub_xz.0.round() as i64,
            feet.round() as i64,
            hub_xz.1.round() as i64,
        ),
        PILLAR_HEIGHT,
        MATERIAL,
    )
    .await;
    // Without a pillar the hub is still a place, just an unmarked one — aim at head height over the
    // hub cell so the script degrades to a coordinate return instead of losing its camera target.
    let focus = raised
        .map(|(_, point)| point)
        .unwrap_or((hub_xz.0, feet + 1.6, hub_xz.1));
    ctx.record(
        "observe",
        &format!("vis-petal-hub-setup-{index}"),
        "Raise the hub pillar every petal comes home to.",
        json!({
            "global_index": global,
            "role": "petal_hub",
            "opened_on_anchor": opened,
            "hub": {"x": r2(hub_xz.0), "z": r2(hub_xz.1)},
            "pillar_placed": raised.is_some(),
            "focus": {"x": r2(focus.0), "y": r2(focus.1), "z": r2(focus.2)},
            "axis_deg": r2(axis.to_degrees()),
            "reach": r2(reach),
            "petals": PETALS,
            "hub_dwell_ms": hub_dwell_ms,
            "corner_dwell_ms": corner_dwell_ms,
        }),
    );

    let mut petal = 0usize;
    while !ctx.target_met() {
        // Alternate which corner leads, so the petal's two turns change sign every petal.
        let order = if petal.is_multiple_of(2) {
            [0usize, 1]
        } else {
            [1usize, 0]
        };
        for (step, &side) in order.iter().enumerate() {
            if ctx.target_met() {
                break;
            }
            let target = petal_corner(hub_xz, axis, reach, petal % PETALS, side);
            let gap = travel_to(ctx, target, TRAVEL_HOP, 1.6).await;
            // Turn back onto the hub pillar and CONFIRM it: a plain `look` returns on its deadline,
            // so a wide turn that timed out would be recorded as a sighting that never happened.
            let confirmed = ctx.look_until_facing(focus, 6.0, 5_000).await;
            let leg = vary(ctx, DOLLY_LEG - 0.5, DOLLY_LEG + 0.8);
            let halves = dolly_dwell(ctx, leg, ctx.elapsed_ms() + corner_dwell_ms).await;
            ctx.record(
                TYPE_LOOK,
                &format!("vis-petal-corner-{index}-{petal}-{step}"),
                &format!(
                    "Petal {} corner {side}: sight the hub pillar and dolly on it.",
                    petal % PETALS
                ),
                json!({
                    "petal": petal % PETALS,
                    "lap": petal / PETALS,
                    "corner": side,
                    "leads": order[0],
                    "arrival_gap": r2(gap),
                    "facing_confirmed": confirmed,
                    "dolly_halves_moved": halves,
                    "elapsed_ms": ctx.elapsed_ms(),
                }),
            );
        }
        if ctx.target_met() {
            break;
        }

        // HOMECOMING. The petal closes on the hub, and the pillar is probed as well as looked at:
        // comparing the reading across petals tells a peer's edit apart from a reconstruction
        // failure when the footage is scored later.
        let gap = travel_to(ctx, hub_xz, TRAVEL_HOP, 1.6).await;
        let confirmed = ctx.look_until_facing(focus, 6.0, 5_000).await;
        let standing = raised.and_then(|(base, _)| ctx.block_id_at(base));
        let leg = vary(ctx, DOLLY_LEG - 0.5, DOLLY_LEG + 0.8);
        let halves = dolly_dwell(ctx, leg, ctx.elapsed_ms() + hub_dwell_ms).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-petal-home-{index}-{petal}"),
            "Petal closed: home on the hub pillar and dolly on it.",
            json!({
                "petal": petal % PETALS,
                "lap": petal / PETALS,
                "arrival_gap": r2(gap),
                "facing_confirmed": confirmed,
                "pillar_block": standing,
                "dolly_halves_moved": halves,
                "elapsed_ms": ctx.elapsed_ms(),
            }),
        );
        petal += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::{PETALS, REACH, SPLAY, petal_corner};

    /// The bug this pins: a petal whose corners are not both exactly `reach` from the hub, or whose
    /// axes are not evenly spaced, turns "three petals off one hub" into three unrelated
    /// excursions — the hub stops being a common return and the whole design claim evaporates.
    #[test]
    fn petals_are_congruent_and_evenly_spaced_about_the_hub() {
        let hub = (5.0, -18.0);
        for &axis in &[0.0_f64, 0.9, 3.8] {
            let cross = 2.0 * REACH * SPLAY.sin();
            for petal in 0..PETALS {
                let a = petal_corner(hub, axis, REACH, petal, 0);
                let b = petal_corner(hub, axis, REACH, petal, 1);
                for corner in [a, b] {
                    let radius = (corner.0 - hub.0).hypot(corner.1 - hub.1);
                    assert!((radius - REACH).abs() < 1e-9, "petal={petal} r={radius}");
                }
                let edge = (a.0 - b.0).hypot(a.1 - b.1);
                assert!((edge - cross).abs() < 1e-9, "petal={petal} cross={edge}");
                // Neighbouring petals are one full 1/PETALS turn apart, so a petal never re-walks
                // the last one's ground.
                let next = petal_corner(hub, axis, REACH, (petal + 1) % PETALS, 0);
                let want = petal_corner(
                    hub,
                    axis + std::f64::consts::TAU / PETALS as f64,
                    REACH,
                    petal,
                    0,
                );
                assert!((next.0 - want.0).abs() < 1e-9 && (next.1 - want.1).abs() < 1e-9);
            }
        }
    }
}
