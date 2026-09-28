use super::*;

/// EVAL-ONLY family. Turn away from a SHARED landmark, dwell for a per-bot duration, turn back.
///
/// Same return structure as `eval_grounding_turn_back`, with the dwell length as the independent
/// variable: bots take 1.5 / 3 / 6 / 12 s away, so a run yields a memory-decay curve rather than a
/// single number. This mirrors WorldMem's protocol, which keeps reporting pixel metrics across a
/// 600-frame gap precisely because what it scores is the reconstruction of an observed region.
///
/// The landmark is SHARED — every bot contributes a block to one cluster at the anchor and all of
/// them open facing it. That gives two things a per-bot landmark cannot: the opening frame contains
/// the same subject for every POV, and while one bot is away the others keep editing the cluster,
/// so the return is scored against a subject that may have genuinely changed. `witness_peer_edits`
/// records those edits, so a changed subject is distinguishable from a failed reconstruction.
const MATERIAL: &str = "minecraft:stone_bricks";
const CLUSTER_HEIGHT: i64 = 2;
const FACE_HOLD_MS: i64 = 2_500;
/// One shuttle leg, in blocks. 4 clears the key-chatter floor at sprint speed and matches
/// Solaris's 3-4 block random-walk primitive; see `hold_facing`.
const SHUTTLE_LEG: f64 = 4.0;
/// The independent variable. Spread over bots rather than over time so a single episode carries
/// several points of the curve, and every POV keeps one fixed dwell for its whole recording.
const DWELL_MS: [i64; 4] = [1_500, 3_000, 6_000, 12_000];
/// Past the 102.5 deg horizontal FOV, so the dwell view shares no pixels with the landmark view.
const TURN_AWAY_DEG: f64 = 150.0;

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let anchor = ctx.anchor();
    let start = ctx.position().unwrap_or((anchor.0, 64.0, anchor.1));
    let dwell_ms = DWELL_MS[global % DWELL_MS.len()];

    // One cluster at the anchor: each bot adds its own pillar in the anchor's neighbourhood, so the
    // subject is shared without needing cross-process coordination — `marker_base_near` walks
    // outward from the same desired cell and lands every bot on a free neighbour.
    let desired = (
        anchor.0.round() as i64,
        start.1.round() as i64,
        anchor.1.round() as i64,
    );
    let raised = raise_landmark(ctx, desired, CLUSTER_HEIGHT, MATERIAL).await;
    let focus = raised
        .map(|(_, point)| point)
        .unwrap_or((anchor.0, start.1 + 2.0, anchor.1));
    ctx.record(
        "observe",
        &format!("vis-eval-memory-setup-{index}"),
        "Contribute to the shared landmark cluster and open facing it.",
        json!({
            "global_index": global,
            "role": "memory",
            "dwell_ms": dwell_ms,
            "contributed": raised.is_some(),
            "cluster_focus": {"x": focus.0, "y": focus.1, "z": focus.2},
            "eval_only": true,
        }),
    );

    let mut cycle = 0usize;
    while !ctx.target_met() {
        let arrived = ctx.look_until_facing(focus, 5.0, 5_000).await;
        let landmark_yaw = ctx
            .position()
            .map(|position| look_angles(head_of(position), focus).0)
            .unwrap_or(0.0);
        // Read the bot's own pillar on every pass: comparing the two readings across a dwell tells
        // a peer's edit apart from a model's hallucination when the footage is scored later.
        let own_block = raised.and_then(|(base, _)| ctx.block_id_at(base));
        ctx.record(
            "observe",
            &format!("vis-eval-memory-face-{index}-{cycle}"),
            "Face the shared cluster: the opening grounding, or the return after a dwell.",
            json!({
                "cycle": cycle,
                "is_return": cycle > 0,
                "arrived": arrived,
                "dwell_ms": dwell_ms,
                "landmark_yaw_deg": landmark_yaw,
                "own_pillar_block": own_block,
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

        let away_yaw =
            landmark_yaw + TURN_AWAY_DEG * if cycle.is_multiple_of(2) { 1.0 } else { -1.0 };
        ctx.record(
            "observe",
            &format!("vis-eval-memory-away-{index}-{cycle}"),
            "Turn away and dwell for this bot's fixed memory interval.",
            json!({
                "cycle": cycle,
                "dwell_ms": dwell_ms,
                "away_yaw_deg": away_yaw,
                "exceeds_horizontal_fov": true,
            }),
        );
        hold_facing(ctx, away_yaw, ctx.elapsed_ms() + dwell_ms, SHUTTLE_LEG).await;
        // Account for what changed on the cluster while this bot was looking away.
        witness_peer_edits(ctx).await;
        cycle += 1;
    }
}
