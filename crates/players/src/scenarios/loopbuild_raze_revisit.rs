//! CATALOG: family=construction min_players=1 single_process=true world_edits=true scenery=none far_players=false
//!
//! Build a cluster, walk out to a review stand and frame it, walk back and tear PART of it down,
//! walk out to the SAME stand and frame it again, walk back and rebuild. Four verified sight-line
//! returns and four out-and-back walks per round.
//!
//! Which face of the revisit problem this answers: the revisit whose CHECK is a world change. A return is only
//! useful training signal if it is distinguishable from a hallucination, and a return to unchanged
//! pixels is not — a model that memorised the frame scores the same as one that understood it. Here
//! the second review of each round is deliberately framed from the identical stand as the first, and
//! between them the bot has removed named cells. So the pair of frames differs in exactly the way
//! the recorded block probes say it should: the demolition is ground truth for the difference.
//!
//! Both loops are present. Trajectory: four `travel_to` legs of `REVIEW_DISTANCE` per round, always
//! returning, so `path_length` is roughly eight times the net `displacement`. Sight line: the
//! review view (9 blocks back) and the work view (arm's length) alternate, each confirmed with
//! `look_until_facing` rather than a `look` that would report a timed-out turn as an arrival.
//! Occlusion is real terrain between plot and stand, and the cluster's own top course hiding its
//! base from the low work view.
use super::*;

/// Cluster material. One material throughout, so a probe of any cell is unambiguous about whether
/// the cell is standing or razed.
const MATERIAL: &str = "minecraft:stone_bricks";

/// The cluster footprint, relative to its resolved base: an L two blocks tall. Five cells is small
/// enough that a full rebuild is one short burst — and short bursts are what keeps `build_blocks`'
/// own nav taps a minority of movement presses (the key-chatter gate caps short presses at 35%).
const CLUSTER_CELLS: [(i64, i64, i64); 5] = [(0, 0, 0), (0, 1, 0), (1, 0, 0), (0, 0, 1), (1, 1, 0)];

/// Which cells the raze takes out each round. Two of five: enough that the change is unmistakable
/// at 9 blocks, few enough that the review frame still shows a recognisably persistent structure
/// rather than empty ground.
const RAZE_CELLS: [usize; 2] = [1, 4];

/// Distance from the cluster to the review stand, in blocks. 9 is `build_tower`'s measured retreat:
/// full-height cues without pushing a single 4.65 s window past the audit's 5-block displacement
/// median.
const REVIEW_DISTANCE: f64 = 9.0;

/// Stand-off while working the cluster. Inside `place_reach` for every cell, so both the build and
/// the dig phases run from where the body already stands.
const WORK_STANDOFF: f64 = 2.5;

/// Centre-to-centre distance between two bots' plots, in blocks. Copied from
/// `parallel_plot_remodel`: 12 minus twice the work stand-off still leaves the bodies past the
/// 4.5-block peer gap even when both are on the sides facing each other.
const PLOT_PITCH: f64 = 12.0;

/// Per-bot rotation of the review stand, in degrees. Distinct bearings mean two bots' review stands
/// are never the same cell and their camera work never lines up, which is what the near-clones check
/// reads.
const BOT_BEARING_SPREAD_DEG: f64 = 137.5;

/// Dwells at the review stand and at the work face, in ms, per bot. Both clear `hold_facing`'s 2.2 s
/// leg budget; a shorter dwell would fit no leg at all and become a stand, which the frozen gate
/// fails outright.
const REVIEW_HOLD_MS: [i64; 3] = [5_000, 6_000, 7_000];
const WORK_HOLD_MS: [i64; 3] = [4_500, 5_500, 4_000];

/// One shuttle leg, in blocks. 4 clears the key-chatter floor at sprint speed; see `hold_facing`.
///
/// The bearing handed to `hold_facing` alongside it is only the OPENING aim. `move_to` overwrites
/// `camera_intent` with `CameraIntent::Hold`, which points the camera along the path whenever the
/// body is moving, so from the first leg onward a hold does NOT keep a subject framed. Every sight
/// line in this script is carried by a STATIONARY `look_until_facing` — walk, stop, look, walk. Do
/// not "fix" a hold by re-issuing `LookAt` inside it: that fights `Hold` for the camera and was
/// measured pushing three already-passing scripts to 96-100% on the camera/travel gate.
const SHUTTLE_LEG: f64 = 4.0;

/// This bot's plot centre on an anchor-centred grid — same layout as `parallel_plot_remodel`, which
/// keeps every plot compact and inside the staged arena at the swarm sizes we record.
fn plot_center(anchor: (f64, f64), index: usize, bot_count: usize) -> (f64, f64) {
    let cols = (bot_count.max(1) as f64).sqrt().ceil() as usize;
    let rows = bot_count.max(1).div_ceil(cols);
    let (col, row) = (index % cols, index / cols);
    (
        anchor.0 + (col as f64 - (cols as f64 - 1.0) / 2.0) * PLOT_PITCH,
        anchor.1 + (row as f64 - (rows as f64 - 1.0) / 2.0) * PLOT_PITCH,
    )
}

/// World point `radius` blocks from `origin` on compass `bearing_deg` (0 = +Z, matching
/// `point_at_yaw` so every recorded bearing means one thing).
fn ring_point(origin: (f64, f64), bearing_deg: f64, radius: f64) -> (f64, f64) {
    let a = bearing_deg.to_radians();
    (origin.0 - a.sin() * radius, origin.1 + a.cos() * radius)
}

/// Absolute cells of the cluster given its resolved base.
fn cluster_cells(base: BlockPos) -> Vec<BlockPos> {
    CLUSTER_CELLS
        .iter()
        .map(|&(dx, dy, dz)| {
            BlockPos::new(base.x + dx as i32, base.y + dy as i32, base.z + dz as i32)
        })
        .collect()
}

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let slot = global % REVIEW_HOLD_MS.len();
    let bot_count = ctx.player_count().max(1);
    let anchor = ctx.anchor();
    let (px, pz) = plot_center(anchor, index, bot_count);
    let bearing = BOT_BEARING_SPREAD_DEG * global as f64;
    // The review stand is resolved ONCE and reused for both reviews of every round. That identity is
    // the whole design: two frames from the same cell, before and after a named demolition.
    let stand = ring_point((px, pz), bearing, REVIEW_DISTANCE);
    let work_stand = ring_point((px, pz), bearing, WORK_STANDOFF);

    travel_to(ctx, work_stand, TRAVEL_HOP, 1.5).await;
    let feet_y = ctx.position().map(|p| p.1.round() as i64).unwrap_or(64);
    let Some(base) = marker_base_near(ctx, (px.round() as i64, feet_y, pz.round() as i64), 2)
    else {
        // No placeable ground under this plot. Report it rather than pretending, and fall back to the
        // review loop alone so the POV still contributes a bounded, non-frozen recording.
        ctx.record(
            "observe",
            &format!("vis-loopbuild-raze-unplaceable-{index}"),
            "No placeable cluster base in this plot; running the review loop without a subject.",
            json!({"plot": {"x": r2(px), "z": r2(pz)}, "feet_y": feet_y}),
        );
        let fallback = (px, feet_y as f64 + 1.0, pz);
        while !ctx.target_met() {
            // Still a loop, still verified, with the plot centre standing in for the structure that
            // could not be placed. The bearing is recomputed at each end because it is measured from
            // wherever the body actually is.
            for (target, hold_ms) in [
                (stand, REVIEW_HOLD_MS[slot]),
                (work_stand, WORK_HOLD_MS[slot]),
            ] {
                travel_to(ctx, target, TRAVEL_HOP, 1.5).await;
                ctx.look_until_facing(fallback, 8.0, 5_000).await;
                let bearing = ctx
                    .position()
                    .map(|p| look_angles(head_of(p), fallback).0)
                    .unwrap_or(0.0);
                hold_facing(ctx, bearing, ctx.elapsed_ms() + hold_ms, SHUTTLE_LEG).await;
            }
        }
        return;
    };
    let cells = cluster_cells(base);
    let focus = (
        base.x as f64 + 0.5,
        base.y as f64 + 1.0,
        base.z as f64 + 0.5,
    );
    let expected = MATERIAL.strip_prefix("minecraft:").unwrap_or(MATERIAL);
    ctx.record(
        "observe",
        &format!("vis-loopbuild-raze-claim-{index}"),
        "Claim the plot and resolve the cluster base the whole episode revolves around.",
        json!({
            "global_index": global,
            "plot": {"x": r2(px), "z": r2(pz)},
            "base": {"x": base.x, "y": base.y, "z": base.z},
            "review_stand": {"x": r2(stand.0), "z": r2(stand.1)},
            "bearing_deg": r2(bearing % 360.0),
        }),
    );

    /// One out-and-back review: walk to the fixed stand, confirm the cluster is framed from a
    /// standstill, then shuttle out the dwell. Two calls per round bracket the demolition, and the
    /// standing-cell counts they record are what makes the two frames comparable.
    async fn review(
        ctx: &mut Ctx,
        stand: (f64, f64),
        focus: (f64, f64, f64),
        hold_ms: i64,
        vis_id: &str,
        note: &str,
        extra: Value,
    ) -> bool {
        travel_to(ctx, stand, TRAVEL_HOP, 1.5).await;
        let faced = ctx.look_until_facing(focus, 6.0, 5_000).await;
        ctx.record(
            "observe",
            vis_id,
            note,
            merged(
                &extra,
                json!({"faced": faced, "review_stand": {"x": r2(stand.0), "z": r2(stand.1)},
                    "elapsed_ms": ctx.elapsed_ms()}),
            ),
        );
        let b = ctx
            .position()
            .map(|p| look_angles(head_of(p), focus).0)
            .unwrap_or(0.0);
        hold_facing(ctx, b, ctx.elapsed_ms() + hold_ms, SHUTTLE_LEG).await;
        faced
    }

    let mut round = 0usize;
    while !ctx.target_met() {
        // ---- build (round 0) or rebuild (later rounds) whatever is missing ----
        travel_to(ctx, work_stand, TRAVEL_HOP, 1.5).await;
        ctx.look_until_facing(focus, 8.0, 4_000).await;
        let missing: Vec<((i64, i64, i64), String)> = cells
            .iter()
            .filter(|c| ctx.block_id_at(**c).as_deref() != Some(expected))
            .map(|c| ((c.x as i64, c.y as i64, c.z as i64), MATERIAL.to_string()))
            .collect();
        let total = missing.len();
        let (placed, attempted) = ctx
            .build_blocks(&missing, |b, _target, _material| {
                (
                    format!("vis-loopbuild-raze-build-{index}-r{round}-{b}"),
                    format!("Place cluster cell {}/{total} for round {round}.", b + 1),
                    json!({"round": round, "cell_index": b, "rebuild": round > 0}),
                )
            })
            .await;
        ctx.record(
            TYPE_PLACE,
            &format!("vis-loopbuild-raze-built-{index}-r{round}"),
            "Cluster complete for this round; next it gets reviewed from 9 blocks back.",
            json!({"round": round, "placed": placed, "attempted": attempted,
                "expected_cells": total, "rebuild": round > 0}),
        );
        let work_bearing = ctx
            .position()
            .map(|p| look_angles(head_of(p), focus).0)
            .unwrap_or(0.0);
        hold_facing(
            ctx,
            work_bearing,
            ctx.elapsed_ms() + WORK_HOLD_MS[slot],
            SHUTTLE_LEG,
        )
        .await;
        if ctx.target_met() {
            break;
        }

        // ---- review #1: the intact cluster from the fixed stand ----
        let standing_before: Vec<bool> = cells
            .iter()
            .map(|c| ctx.block_id_at(*c).as_deref() == Some(expected))
            .collect();
        review(
            ctx,
            stand,
            focus,
            REVIEW_HOLD_MS[slot],
            &format!("vis-loopbuild-raze-before-{index}-r{round}"),
            "Review the intact cluster; this frame is the baseline the raze will be scored against.",
            json!({"round": round, "phase": "before_raze", "standing": standing_before}),
        )
        .await;
        if ctx.target_met() {
            break;
        }

        // ---- back in, tear out the named cells ----
        travel_to(ctx, work_stand, TRAVEL_HOP, 1.5).await;
        ctx.look_until_facing(focus, 8.0, 4_000).await;
        ctx.equip("diamond_pickaxe").await;
        let mut razed = Vec::new();
        for &k in &RAZE_CELLS {
            if ctx.target_met() {
                break;
            }
            let Some(cell) = cells.get(k).copied() else {
                continue;
            };
            let before = ctx.block_id_at(cell);
            let removed = ctx.dig(cell).await;
            ctx.record(
                TYPE_MINE,
                &format!("vis-loopbuild-raze-dig-{index}-r{round}-{k}"),
                "Tear one named cell out of the cluster, so the next review frame must differ.",
                json!({"round": round, "cell_index": k, "removed": removed,
                    "block_before": before, "block_after": ctx.block_id_at(cell),
                    "block_position": {"x": cell.x, "y": cell.y, "z": cell.z}}),
            );
            razed.push(k);
        }
        // Recomputed, not reused: `dig` aims the camera at each cell it breaks and the digging walked
        // the body around, so the pre-raze bearing no longer points at the cluster. It is the opening
        // aim of the hold — the frames right after the last break, which are the ones that should
        // show the gap.
        let razed_bearing = ctx
            .position()
            .map(|p| look_angles(head_of(p), focus).0)
            .unwrap_or(work_bearing);
        hold_facing(
            ctx,
            razed_bearing,
            ctx.elapsed_ms() + WORK_HOLD_MS[slot],
            SHUTTLE_LEG,
        )
        .await;
        if ctx.target_met() {
            break;
        }

        // ---- review #2: the SAME stand, the changed cluster ----
        let standing_after: Vec<bool> = cells
            .iter()
            .map(|c| ctx.block_id_at(*c).as_deref() == Some(expected))
            .collect();
        review(
            ctx,
            stand,
            focus,
            REVIEW_HOLD_MS[slot],
            &format!("vis-loopbuild-raze-after-{index}-r{round}"),
            "Review the razed cluster from the identical stand: same viewpoint, changed world.",
            json!({"round": round, "phase": "after_raze", "standing": standing_after,
                "razed_cells": razed}),
        )
        .await;
        witness_peer_edits(ctx).await;
        round += 1;
    }
}
