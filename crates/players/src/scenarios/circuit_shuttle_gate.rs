//! CATALOG: family=locomotion min_players=1 single_process=false world_edits=true scenery=none far_players=false

use super::*;

/// Half the corridor's length. The audit reads 93-frame (4.65 s) windows and wants the median
/// window displacement under 5 blocks, so a 10-block corridor — one traverse in ~2.5 s — is the
/// longest shuttle that still fits a single window, and the dwells at either end pull the median
/// far below it while `path_length` keeps climbing.
const CORRIDOR_HALF: f64 = 5.0;
/// Gate posts are three tall so the aiming point sits above head height and stays visible over
/// grass, flowers and a shallow rise; taller wastes placement attempts for no extra visibility.
const POST_HEIGHT: i64 = 3;
/// A kit item (`EQUIP_ITEMS`), and the one every other landmark script uses, so the posts read as
/// the same kind of infrastructure across the corpus.
const MATERIAL: &str = "minecraft:stone_bricks";
/// Legs of a dolly dwell, in blocks. Long enough that one press clears the 0.5 s key-chatter floor
/// at sprint speed, short enough that the bot never dollies out of its own corridor.
const DOLLY_LEG: f64 = 4.5;

/// Draw in `[lo, hi)` from the per-bot stream, so a number this scenario could have hard-coded
/// varies per bot and per round and still replays from the seed.
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
/// This is the shape the whole `circuit_*` family is built on, and it is what makes a revisit
/// measurable. `hold_visual` pins the camera to the angle it finds (`CameraIntent::LookAt` at the
/// current yaw), so across a dwell the view only TRANSLATES along the sight line: consecutive
/// windows share nearly their whole frustum, which is exactly the `overlap_mean` the frustum audit
/// reads, and the net rotation the audit divides by sweep is zero because there is no rotation.
///
/// It also clears three displacement gates at once. Each half is ONE press of 0.9-1.4 s, over the
/// `SHORT_RUN_FRAMES` (0.5 s) chatter floor — and a half that walks into a tree is cut by
/// `hold_visual`'s stall guard only after 1.2 s, so even blocked halves are long presses. The body
/// covers `2 * leg` blocks per round trip, over the frozen gate's 0.5 blocks/s. And net
/// displacement never leaves one leg, so the dwell cannot inflate the window displacement median.
async fn dolly_dwell(ctx: &mut Ctx, leg: f64, deadline_ms: i64) -> u32 {
    // Walking is ~4.3 blocks/s and sprint is forward-effective only, so time the press off the
    // walking figure. The 900 ms floor is what guarantees the press clears the chatter threshold
    // even when the caller draws a short leg.
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

/// Shuttle a FIXED two-post corridor for the whole episode: walk to the far post, turn a confirmed
/// 180 deg back onto the near post, dolly on that view, walk back, turn onto the far post again.
///
/// The revisit problem, the trajectory side AND the gaze side. The trajectory is closed by
/// construction — the bot never leaves a 10-block line, so `path_length` grows without bound while
/// displacement stays inside one window's worth of travel. The gaze is closed by the two posts:
/// there are only ever TWO views in the episode, each of them re-entered several times, and each
/// return is confirmed with `look_until_facing` rather than assumed, because a plain `look` returns
/// on its deadline and would record a 180 deg turn that timed out as a return that happened.
///
/// The net-rotation ratio falls out of the alternation: every turnaround is +180 deg followed by
/// -180 deg on the next visit, so over any window containing a pair the net cancels while the
/// sweep accumulates. Between turnarounds the camera does not rotate at all — the traverse is
/// travel-facing along the corridor axis, which IS the post bearing, and the dwells are pure
/// dollies.
///
/// Distinct from `sparse_checkpoint_return`, which draws a fresh outbound bearing every round,
/// stands at its checkpoint and sweeps the camera freely: here the corridor and both endpoint
/// views are fixed for the episode, nothing is swept, and the endpoint dwell is a dolly along the
/// sight line instead of a stand.
pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let anchor = ctx.anchor();
    // The corridor is built around wherever the bot spawned. Keeping it there — rather than
    // pulling every bot onto a derived station — is what leaves N=2 with two bodies a peer gap
    // apart (the manifest hands out `spread_range: 8`) while both corridors point at the anchor.
    let start = ctx.position().unwrap_or((anchor.0, 64.0, anchor.1));

    // Brief rule 5: open facing the SHARED subject. Every bot in the episode turns onto the anchor
    // before it does anything else, so the opening frame — the only context frame a 93-frame
    // training window gets — already contains the anchor's neighbourhood and whoever else is
    // standing in it. Solaris gets the same effect from a forced `lookAt` with `randomized:false`.
    // The turn is stationary, so it is invisible to the camera/travel gate.
    let opened = ctx
        .look_until_facing((anchor.0, start.1 + 1.4, anchor.1), 10.0, 4_000)
        .await;

    // Orient the corridor so ONE of its two views contains the anchor: at N=2 the other bot
    // spawned on the far side of that same anchor, so pointing a corridor view at it is what puts a
    // teammate in the OPENING frame instead of asking the model to invent one mid-window — the
    // 22.4% of multi-player windows the v5 audit found with a peer intruding and no first-frame
    // evidence for it.
    let toward_anchor = (anchor.0 - start.0, anchor.1 - start.2);
    let anchor_bearing = if toward_anchor.0.hypot(toward_anchor.1) > 1.0 {
        look_angles(head_of(start), (anchor.0, start.1, anchor.1)).0
    } else {
        vary(ctx, -180.0, 180.0)
    };
    // A per-bot skew off the anchor bearing: two bots whose corridors are exactly parallel trace
    // near-identical action streams, which is what the `near-clones` QC check reads.
    let axis_deg = anchor_bearing + vary(ctx, -20.0, 20.0);
    let half = vary(ctx, CORRIDOR_HALF - 0.5, CORRIDOR_HALF + 1.0);
    let dwell_ms = vary(ctx, 5_000.0, 7_000.0) as i64;
    let leg = vary(ctx, DOLLY_LEG - 0.5, DOLLY_LEG + 0.5);

    // Both ends of the corridor, as ground cells: `point_at_yaw` turns the axis bearing into a
    // world point, and the far end is the same bearing negated.
    let far_xz = point_at_yaw(head_of(start), axis_deg, half);
    let near_xz = point_at_yaw(head_of(start), axis_deg + 180.0, half);
    let ends = [(far_xz.0, far_xz.2), (near_xz.0, near_xz.2)];

    // A post at each end, raised from that end: the placement probe has to run from where the bot
    // is standing (`marker_base_near` excludes cells inside the placer's own hitbox and needs the
    // support cell loaded), and the arrival Y is the only honest guess at the local ground level.
    let mut focus = [None; 2];
    for (slot, &(x, z)) in ends.iter().enumerate() {
        travel_to(ctx, (x, z), TRAVEL_HOP, 3.0).await;
        let feet = ctx.position().map(|p| p.1).unwrap_or(start.1);
        let desired = (x.round() as i64, feet.round() as i64, z.round() as i64);
        focus[slot] = raise_landmark(ctx, desired, POST_HEIGHT, MATERIAL)
            .await
            .map(|(_, point)| point);
    }
    ctx.record(
        "observe",
        &format!("vis-shuttle-gate-setup-{index}"),
        "Raise the two gate posts this episode shuttles between.",
        json!({
            "global_index": global,
            "role": "shuttle",
            "opened_on_anchor": opened,
            "axis_deg": r2(axis_deg),
            "corridor_blocks": r2(half * 2.0),
            "hub": {"x": r2(start.0), "z": r2(start.2)},
            "far_post": focus[0].map(|p| json!({"x": r2(p.0), "y": r2(p.1), "z": r2(p.2)})),
            "near_post": focus[1].map(|p| json!({"x": r2(p.0), "y": r2(p.1), "z": r2(p.2)})),
            "dwell_ms": dwell_ms,
        }),
    );

    let mut traverse = 0u32;
    // `end` is the end the bot is HEADING FOR; the post it looks at on arrival is the other one,
    // so the arrival turn is always the ~180 deg reversal this scenario exists to record.
    let mut end = 0usize;
    while !ctx.target_met() {
        let target = ends[end];
        let remaining = travel_to(ctx, target, TRAVEL_HOP, 1.6).await;
        if ctx.target_met() {
            break;
        }
        let back = 1 - end;
        // The confirmed return. `arrived` is recorded rather than assumed: a wide turn that ran out
        // of budget must be distinguishable in the corpus from one that completed.
        let (aim, arrived) = match focus[back] {
            Some(point) => (point, ctx.look_until_facing(point, 6.0, 5_000).await),
            None => {
                // No post got placed at that end (terrain refused every candidate cell): fall back
                // to the corridor bearing, which is the same view minus the subject.
                let bearing = if back == 0 {
                    axis_deg
                } else {
                    axis_deg + 180.0
                };
                face_bearing(ctx, bearing).await;
                (
                    ctx.position()
                        .map(|p| point_at_yaw(head_of(p), bearing, 24.0))
                        .unwrap_or((target.0, start.1, target.1)),
                    false,
                )
            }
        };
        let halves = dolly_dwell(ctx, leg, ctx.elapsed_ms() + dwell_ms).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-shuttle-gate-turn-{index}-{traverse}"),
            &format!(
                "Traverse to the {} end and dolly on the opposite post.",
                if end == 0 { "far" } else { "near" }
            ),
            json!({
                "traverse": traverse,
                "end": end,
                "watching_post": back,
                "is_return": traverse > 0,
                "facing_confirmed": arrived,
                "arrival_gap": r2(remaining),
                "aim": {"x": r2(aim.0), "y": r2(aim.1), "z": r2(aim.2)},
                "dolly_halves_moved": halves,
                "elapsed_ms": ctx.elapsed_ms(),
            }),
        );
        end = back;
        traverse += 1;
    }
}
