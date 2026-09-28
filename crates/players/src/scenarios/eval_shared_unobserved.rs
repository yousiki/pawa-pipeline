use super::*;

/// EVAL-ONLY family. Every bot opens on a shared subject, then all of them turn together toward the
/// SAME unobserved region, from different positions.
///
/// This is the one eval family that does not need ground truth at all. What the bots turn toward is
/// genuinely unpredictable — a real-terrain region nobody has seen — so no pixel metric applies. But
/// the generations must AGREE WITH EACH OTHER: two cameras looking at one region from two positions
/// constrain each other geometrically. That is the GT-free self-consistency class (Solaris
/// Consistency, MIND's `L_gsc`), and it is structurally out of reach for a two-player hard-coded
/// pipeline while being native to our N=1..32.
///
/// The shared target must be identical for every bot with no cross-process messaging, so it is
/// derived from `native_waypoints`, which the engine hands every bot the same copy of.
const MATERIAL: &str = "minecraft:stone_bricks";
const SUBJECT_HEIGHT: i64 = 2;
const SUBJECT_HOLD_MS: i64 = 3_000;
const SHARED_HOLD_MS: i64 = 4_000;
/// One shuttle leg, in blocks. 4 clears the key-chatter floor at sprint speed and matches
/// Solaris's 3-4 block random-walk primitive; see `hold_facing`.
const SHUTTLE_LEG: f64 = 4.0;
/// Far enough that the sight lines from bots spread over ~8 blocks are near-parallel, so all of
/// them really do look at one region rather than at their own separate patches of ground.
const SHARED_TARGET_DISTANCE: f64 = 48.0;
/// Per-bot delay before leaving the subject. Without it the bots issue identical turns at identical
/// frames and the pairwise `near-clones` QC check fires (measured 80.2% against an 80% ceiling).
/// It only staggers WHEN each bot turns, not WHERE: at 0.7 s per bot against a 4 s shared hold,
/// every pair still overlaps by seconds on the shared target, which is what consistency scoring
/// needs.
const SUBJECT_HOLD_STAGGER_MS: i64 = 700;

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let anchor = ctx.anchor();
    let start = ctx.position().unwrap_or((anchor.0, 64.0, anchor.1));

    // Shared, derived, no messaging: every bot reads the same waypoint list, so taking the same
    // entry gives one direction the whole swarm agrees on. Bot 0 alone raises the subject; the
    // others find the cluster already there and simply face it.
    let waypoint = {
        let shared = &ctx.engine().shared;
        shared
            .native_waypoints
            .first()
            .copied()
            .unwrap_or((1.0, 0.0))
    };
    let span = waypoint.0.hypot(waypoint.1).max(0.001);
    let shared_target = (
        anchor.0 + waypoint.0 / span * SHARED_TARGET_DISTANCE,
        start.1 + 2.0,
        anchor.1 + waypoint.1 / span * SHARED_TARGET_DISTANCE,
    );

    let desired = (
        anchor.0.round() as i64,
        start.1.round() as i64,
        anchor.1.round() as i64,
    );
    // Only bot 0 raises it. Every other bot finds the pillar already standing and just faces it —
    // building costs a stretch of standing still (turn to the cell, place, verify), and on the
    // builder that stretch is what pushed `moving frames` under the participation floor.
    let raised = if global == 0 {
        raise_landmark(ctx, desired, SUBJECT_HEIGHT, MATERIAL).await
    } else {
        None
    };
    let subject = raised
        .map(|(_, point)| point)
        .unwrap_or((anchor.0, start.1 + 2.0, anchor.1));
    ctx.record(
        "observe",
        &format!("vis-eval-consistency-setup-{index}"),
        "Open on the shared subject, with the shared unobserved target resolved.",
        json!({
            "global_index": global,
            "role": "consistency",
            "subject": {"x": subject.0, "y": subject.1, "z": subject.2},
            "shared_target": {"x": shared_target.0, "y": shared_target.1, "z": shared_target.2},
            "shared_target_distance": SHARED_TARGET_DISTANCE,
            "contributed": raised.is_some(),
            "eval_only": true,
        }),
    );

    let mut cycle = 0usize;
    while !ctx.target_met() {
        // Phase 1: everyone on the shared subject. This is the context frame's content, and it is
        // the same content for every POV — the alignment that makes cross-POV comparison legal.
        let on_subject = ctx.look_until_facing(subject, 5.0, 5_000).await;
        let subject_yaw = ctx
            .position()
            .map(|position| look_angles(head_of(position), subject).0)
            .unwrap_or(0.0);
        ctx.record(
            "observe",
            &format!("vis-eval-consistency-subject-{index}-{cycle}"),
            "Face the shared subject so every POV opens on the same content.",
            json!({
                "cycle": cycle,
                "arrived": on_subject,
                "is_return": cycle > 0,
                "elapsed_ms": ctx.elapsed_ms(),
            }),
        );
        hold_facing(
            ctx,
            subject_yaw,
            ctx.elapsed_ms() + SUBJECT_HOLD_MS + global as i64 * SUBJECT_HOLD_STAGGER_MS,
            SHUTTLE_LEG,
        )
        .await;
        if ctx.target_met() {
            break;
        }

        // Phase 2: all bots onto the shared unobserved region. Aim at the POINT, not at a bearing,
        // so bots at different positions converge on one region instead of running parallel rays.
        let arrived = ctx.look_until_facing(shared_target, 6.0, 6_000).await;
        let target_yaw = ctx
            .position()
            .map(|position| look_angles(head_of(position), shared_target).0)
            .unwrap_or(0.0);
        ctx.record(
            "observe",
            &format!("vis-eval-consistency-shared-{index}-{cycle}"),
            "Turn onto the shared unobserved region; generations must agree across POVs.",
            json!({
                "cycle": cycle,
                "arrived": arrived,
                "target_yaw_deg": target_yaw,
                "shared_target": {"x": shared_target.0, "y": shared_target.1, "z": shared_target.2},
                "gt_free_consistency": true,
            }),
        );
        hold_facing(
            ctx,
            target_yaw,
            ctx.elapsed_ms() + SHARED_HOLD_MS,
            SHUTTLE_LEG,
        )
        .await;
        cycle += 1;
    }
}
