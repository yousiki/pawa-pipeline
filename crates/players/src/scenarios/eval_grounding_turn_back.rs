use super::*;

/// EVAL-ONLY family. Look at a landmark, turn away past the field of view, turn back.
///
/// The point is a window whose ground truth is an ALREADY OBSERVED region, so a pixel-wise
/// comparison against it is interpretable. Every training scenario we ship mixes "turn away and
/// never come back" into the same 4.65 s window, which asks the model to invent what is behind it
/// and then scores the invention against pixels. This script isolates the return.
///
/// Design constraints, all deliberate:
/// - Displacement stays inside ~1.2 blocks. The family exists to isolate CAMERA motion, so the
///   translation term has to be negligible rather than merely small.
/// - The turn-away angle exceeds the 102.5 deg horizontal FOV, so the away view and the landmark
///   view share no pixels at all. A smaller turn would leave partial overlap and blur the contrast.
/// - The hold is a shuffle, never a stand: standing trips the frozen/stuck QC gate.
/// - Run on real terrain (the manifest leaves `world.level_type` at `minecraft:normal`) so the
///   landmark's occlusion and the return sightline test actual memory rather than a uniform plane.
const MATERIAL: &str = "minecraft:stone_bricks";
const LANDMARK_HEIGHT: i64 = 3;
const LANDMARK_DISTANCE: f64 = 5.0;
const FACE_HOLD_MS: i64 = 6_000;
const AWAY_HOLD_MS: i64 = 7_000;
/// One shuttle leg, in blocks. 4 clears the key-chatter floor at sprint speed and matches
/// Solaris's 3-4 block random-walk primitive; see `hold_facing`.
const SHUTTLE_LEG: f64 = 4.0;
/// Both exceed the horizontal FOV (102.5 deg at `fov_deg=70`, 16:9), which is what makes the away
/// view genuinely unobserved rather than partially overlapping.
const TURN_AWAY_DEG: [f64; 2] = [120.0, 180.0];

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let anchor = ctx.anchor();
    let start = ctx.position().unwrap_or((anchor.0, 64.0, anchor.1));

    // Put the landmark on the anchor side, so in a multi-bot episode every view points inward and
    // the opening frame contains the scene's subject — the alignment Solaris gets from a forced
    // `lookAt` with `randomized:false`.
    let toward = (anchor.0 - start.0, anchor.1 - start.2);
    let span = toward.0.hypot(toward.1);
    let bearing = if span > 1.0 {
        (toward.0 / span, toward.1 / span)
    } else {
        (0.0, 1.0)
    };
    let desired = (
        (start.0 + bearing.0 * LANDMARK_DISTANCE).round() as i64,
        start.1.round() as i64,
        (start.2 + bearing.1 * LANDMARK_DISTANCE).round() as i64,
    );
    let raised = raise_landmark(ctx, desired, LANDMARK_HEIGHT, MATERIAL).await;
    let focus = raised.map(|(_, point)| point).unwrap_or((
        desired.0 as f64 + 0.5,
        start.1 + 2.0,
        desired.2 as f64 + 0.5,
    ));
    ctx.record(
        "observe",
        &format!("vis-eval-grounding-setup-{index}"),
        "Raise the landmark this episode grounds every return on.",
        json!({
            "global_index": global,
            "role": "grounding",
            "landmark_placed": raised.is_some(),
            "landmark_focus": {"x": focus.0, "y": focus.1, "z": focus.2},
            "landmark_height": LANDMARK_HEIGHT,
            "eval_only": true,
        }),
    );

    let mut cycle = 0usize;
    while !ctx.target_met() {
        // Facing the landmark is both the opening grounding and, from cycle 1 on, the RETURN whose
        // fidelity this family exists to score. `look_until_facing` is what makes it evidence: a
        // plain `look` returns on a deadline, so a wide turn that timed out would be recorded as a
        // return that never happened.
        let arrived = ctx.look_until_facing(focus, 5.0, 5_000).await;
        let landmark_yaw = ctx
            .position()
            .map(|position| look_angles(head_of(position), focus).0)
            .unwrap_or(0.0);
        let standing = raised.map(|(base, _)| ctx.block_id_at(base));
        ctx.record(
            "observe",
            &format!("vis-eval-grounding-face-{index}-{cycle}"),
            "Face the landmark: the opening grounding, or the return being scored.",
            json!({
                "cycle": cycle,
                "is_return": cycle > 0,
                "arrived": arrived,
                "landmark_yaw_deg": landmark_yaw,
                "landmark_block": standing.flatten(),
                "elapsed_ms": ctx.elapsed_ms(),
            }),
        );
        hold_facing(
            ctx,
            landmark_yaw,
            ctx.elapsed_ms() + FACE_HOLD_MS,
            SHUTTLE_LEG,
        )
        .await;
        if ctx.target_met() {
            break;
        }

        // Alternate magnitude AND sign so the episode does not degenerate into one rotation
        // direction, which is the "turn away, never come back" shape the training set already has.
        let away_deg = TURN_AWAY_DEG[cycle % TURN_AWAY_DEG.len()];
        let away_yaw = landmark_yaw + away_deg * if cycle % 4 < 2 { 1.0 } else { -1.0 };
        ctx.record(
            "observe",
            &format!("vis-eval-grounding-away-{index}-{cycle}"),
            "Turn away from the landmark by more than the horizontal field of view.",
            json!({
                "cycle": cycle,
                "turn_away_deg": away_deg,
                "away_yaw_deg": away_yaw,
                "exceeds_horizontal_fov": true,
            }),
        );
        hold_facing(ctx, away_yaw, ctx.elapsed_ms() + AWAY_HOLD_MS, SHUTTLE_LEG).await;
        cycle += 1;
    }
}
