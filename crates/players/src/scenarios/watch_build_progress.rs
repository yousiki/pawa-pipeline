//! CATALOG: family=construction min_players=2 single_process=true world_edits=true scenery=none far_players=false
//!
//! One bot builds a site out in stages; the other watches it grow, turning away between stages and
//! back again to check what changed.
//!
//! The revisit problem. **The sightline loop is the whole point here, and it is verifiable against
//! the world**: the watcher looks at the site, turns past the horizontal FOV to a scripted bearing,
//! holds there while the builder adds a stage, and turns back. Every other v6 return is scored
//! against terrain that did not change; this one is scored against a subject that DID, and
//! `witness_peer_edits` records the edit with its age, so "the pillar grew" and "the model
//! hallucinated a pillar" are distinguishable in the footage. **The trajectory loop** belongs to
//! both roles: the builder alternates between its placement cell and a stand-back post 5 blocks
//! away — out and back, once per stage — and the watcher's `hold_facing` shuttles are bounded by
//! their own leg length.
//!
//! Only ONE bot builds. Building is a standing activity (turn, place, verify) and the participation
//! gate wants 200 moving frames per POV, so making both bots build starves both; making one build
//! and one watch gives the watcher a movement budget and the builder its stand-back legs.
//!
//! **How this clears the camera/travel independence gate.** The subject here is a FIXED WORLD POINT,
//! which is the single most dangerous case: tracking a fixed point while shuttling in and out from it
//! is the shape measured at 96-100% predictability, because the camera's turn to re-centre it is a
//! direct consequence of which way the body swung. So the watcher never tracks the site — strict
//! walk-stop-look-walk. Both of its aims, the one at the site and the scripted
//! turn-away, are STANDING turns, and `qc.rs:420` discards frames with no travel. `hold_facing` then
//! covers ground with the camera left to `CameraIntent::Hold`; it does NOT keep the view on the
//! bearing it was given, so what makes the return scoreable is that the gaze is re-aimed and
//! CONFIRMED at the start of each cycle, not that the camera was pinned throughout. The builder
//! travels on plain `move_to` legs.
//!
//! The gate is two-sided (`agreement()` is `max(p, 1-p)`), so no camera correction is layered on top
//! of travel in either direction.

use super::*;

/// Stage material. Must be one of the kit items the coordinator stocks (`EQUIP_ITEMS`), or `equip`
/// silently equips nothing and the whole site is lost.
const MATERIAL: &str = "minecraft:stone_bricks";
/// Blocks per stage. Two is deliberately small: a stage has to finish inside one turn-away hold, or
/// the watcher comes back to an unchanged site and the return has nothing to verify.
const STAGE_HEIGHT: i64 = 2;
/// How far the builder backs off to inspect its own work, in blocks. Over [`MIN_PEER_GAP`] so the
/// stand-back never walks into the watcher, and far enough that a 2-3 block stage fits in frame.
const STAND_BACK: f64 = 5.0;
/// How long the watcher faces the site before turning away.
const FACE_MS: i64 = 5_000;
/// How long it stays turned away — one stage's worth of building, so the return lands on a changed
/// subject rather than the same one.
const AWAY_MS: i64 = 7_000;
/// The scripted turn-away angle. Past the 102.5 deg horizontal FOV (vertical `fov_deg` 70 at 16:9),
/// so the away view and the site view share no pixels at all and the return is a real
/// reconstruction rather than a pan across partly-visible ground.
const TURN_AWAY_DEG: f64 = 140.0;
/// Vertical window the watcher searches for the builder's stack. Wide enough to cover the vertical
/// search `marker_base_near` does when the desired cell is unusable on natural terrain.
const PROBE_SPAN: i32 = 6;
/// Shuttle leg for `hold_facing`, in blocks. 4.0 is the validated value — see its doc comment.
const SHUTTLE_LEG: f64 = 4.0;

/// The bot this one is paired with; see the identical helper in the sibling `watch_*` scripts.
fn partner_of(index: usize, bot_count: usize) -> usize {
    let paired = index ^ 1;
    if paired < bot_count { paired } else { 0 }
}

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let bot_count = ctx.player_count().max(1);
    let partner = partner_of(index, bot_count);
    let anchor = ctx.anchor();
    // Even bots build, odd bots watch. At N=2 that is exactly one of each, which is what this
    // scenario needs; a larger swarm just gets more of both.
    let is_builder = index.is_multiple_of(2);

    if bot_count >= 2 {
        look_at_peer(ctx, partner, 800).await;
    }
    ctx.record(
        TYPE_LOOK,
        &format!("vis-progress-open-{index}"),
        &format!(
            "Open facing partner bot {partner}; role is {}.",
            if is_builder { "builder" } else { "watcher" }
        ),
        json!({
            "role": if is_builder { "builder" } else { "watcher" },
            "partner": partner, "stage_height": STAGE_HEIGHT,
            "turn_away_deg": TURN_AWAY_DEG,
        }),
    );

    if is_builder || bot_count < 2 {
        build(ctx, anchor).await;
    } else {
        watch(ctx, anchor).await;
    }
}

/// Raise the site one stage at a time, stepping back to inspect between stages.
async fn build(ctx: &mut Ctx, anchor: (f64, f64)) {
    let index = ctx.index();
    let start = ctx.position().unwrap_or((anchor.0, 64.0, anchor.1));
    let mut stage = 0usize;
    while !ctx.target_met() {
        // Every stage aims at the anchor column. `marker_base_near` walks outward from there over
        // `MARKER_OFFSETS` until it finds a free, supported cell, so successive stages land on
        // neighbouring columns and the site SPREADS as well as rises. That is what makes the
        // watcher's sightline move a little with the work instead of staying pinned to one pixel.
        let desired = (
            anchor.0.round() as i64,
            start.1.round() as i64,
            anchor.1.round() as i64,
        );
        let raised = raise_landmark(ctx, desired, STAGE_HEIGHT, MATERIAL).await;
        let focus = raised.map(|(_, point)| point);
        ctx.record(
            TYPE_PLACE,
            &format!("vis-progress-stage-{index}-{stage}"),
            &format!("Build stage {stage} of the site."),
            json!({
                "role": "builder", "stage": stage,
                "placed": raised.is_some(),
                "base": raised.map(|(base, _)| json!({"x": base.x, "y": base.y, "z": base.z})),
            }),
        );
        if ctx.target_met() {
            break;
        }

        // Stand back, look at the work, come back. The out-and-back is where the builder earns its
        // moving frames — placement itself is a standing action, and a builder that never walks
        // fails the participation gate on its own.
        let Some(own) = ctx.position() else {
            sleep_ms(300).await;
            continue;
        };
        let (ux, uz) = away_unit(Some(own), (anchor.0, own.1, anchor.1));
        let post = (own.0 + ux * STAND_BACK, own.2 + uz * STAND_BACK);
        ctx.move_to(post.0, post.1, 6_000, 1.2).await;
        if let Some(point) = focus {
            let confirmed = ctx.look_until_facing(point, 6.0, 4_000).await;
            ctx.record(
                TYPE_LOOK,
                &format!("vis-progress-inspect-{index}-{stage}"),
                &format!(
                    "Stand back {} blocks and inspect stage {stage}.",
                    r2(STAND_BACK)
                ),
                json!({"role": "builder", "stage": stage,
                    "post": {"x": r2(post.0), "z": r2(post.1)},
                    "facing_confirmed": confirmed}),
            );
        }
        // Back to the site: the return leg is what closes the builder's own trajectory loop.
        ctx.move_to(own.0, own.2, 6_000, 1.2).await;
        stage += 1;
    }
}

/// Face the site, turn away past the FOV, hold, turn back — and account for what changed.
async fn watch(ctx: &mut Ctx, anchor: (f64, f64)) {
    let index = ctx.index();
    let start = ctx.position().unwrap_or((anchor.0, 64.0, anchor.1));
    let mut cycle = 0usize;
    while !ctx.target_met() {
        let Some(own) = ctx.position() else {
            sleep_ms(300).await;
            continue;
        };
        // Find the builder's stack by searching the SAME neighbourhood the builder placed into —
        // `area_find_block` walks `MARKER_OFFSETS`, the very offsets `marker_base_near` uses, so
        // the search area can never drift from the placement area. No cross-process message needed.
        let feet_y = own.1.floor() as i32;
        let found = area_find_block(
            ctx,
            (anchor.0.floor() as i32, anchor.1.floor() as i32),
            (feet_y - PROBE_SPAN, feet_y + PROBE_SPAN),
            MATERIAL,
        );
        // Before the first stage lands there is nothing to find; aim at the anchor column at eye
        // height so the opening frames still contain the site's ground rather than the sky.
        let focus = found
            .map(|cell| {
                (
                    cell.x as f64 + 0.5,
                    cell.y as f64 + 0.5,
                    cell.z as f64 + 0.5,
                )
            })
            .unwrap_or((anchor.0, start.1 + 1.5, anchor.1));

        let confirmed = ctx.look_until_facing(focus, 6.0, 5_000).await;
        let site_yaw = look_angles(head_of(own), focus).0;
        ctx.record(
            TYPE_LOOK,
            &format!("vis-progress-face-{index}-{cycle}"),
            "Face the site: the opening look, or the return after a stage.",
            json!({
                "role": "watcher", "cycle": cycle, "is_return": cycle > 0,
                "site_found": found.is_some(),
                "site_yaw_deg": r2(site_yaw),
                "facing_confirmed": confirmed,
            }),
        );
        // Hold the site bearing as a FIXED number. Re-aiming at the site through the shuttle is the
        // measured 96-100% camera/travel failure, so the bearing is sampled here and left alone.
        hold_facing(ctx, site_yaw, ctx.elapsed_ms() + FACE_MS, SHUTTLE_LEG).await;
        if ctx.target_met() {
            break;
        }

        // Turn away by more than the FOV. Alternate the sign so the episode does not degenerate
        // into one rotation direction — that is the "turn away and never come back" shape the
        // training corpus already has too much of.
        let side = if cycle.is_multiple_of(2) { 1.0 } else { -1.0 };
        let away_yaw = site_yaw + TURN_AWAY_DEG * side;
        ctx.record(
            TYPE_LOOK,
            &format!("vis-progress-away-{index}-{cycle}"),
            "Turn away from the site by more than the horizontal FOV while the stage is built.",
            json!({
                "role": "watcher", "cycle": cycle,
                "away_yaw_deg": r2(away_yaw), "turn_away_deg": TURN_AWAY_DEG,
                "exceeds_horizontal_fov": true,
            }),
        );
        hold_facing(ctx, away_yaw, ctx.elapsed_ms() + AWAY_MS, SHUTTLE_LEG).await;
        // Account for what the builder changed while this bot was looking elsewhere: the gaze is
        // logged with the edit's age, which is what makes a changed subject separable from a
        // hallucinated one when the footage is scored.
        witness_peer_edits(ctx).await;
        cycle += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Named explicitly rather than left to the parent's glob, matching how
    // counter_ring_flow / orbit_lane_swap / satellite_escort import it in their tests.
    use crate::scenarios::common::MIN_PEER_GAP;

    /// The turn-away must exceed the horizontal FOV or the away view still overlaps the site, and
    /// the away hold must be long enough for a stage to actually land. Both are the reason this
    /// scenario's return is scoreable, so pin them.
    #[test]
    fn the_turn_away_clears_the_fov_and_outlasts_a_stage() {
        const { assert!(TURN_AWAY_DEG > 102.5) };
        const { assert!(AWAY_MS > FACE_MS) };
        const { assert!(STAND_BACK > MIN_PEER_GAP - 1.0) };
    }

    /// Nobody watches itself.
    #[test]
    fn partners_are_distinct() {
        for bots in 2..=16 {
            for index in 0..bots {
                assert_ne!(partner_of(index, bots), index, "bots={bots} index={index}");
                assert!(partner_of(index, bots) < bots);
            }
        }
    }
}
