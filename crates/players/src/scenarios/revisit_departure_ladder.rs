//! CATALOG: family=many_player_spatial min_players=1 single_process=true world_edits=true scenery=none far_players=false
//!
//! Three departures of different length from ONE home mark, each followed by a verified return, all
//! inside a single episode.
//!
//! The revisit problem is answered on the trajectory side by making every rung an out-and-back: the
//! bot never ends a leg at the far point, so net displacement over a rung is ~0 while path length
//! is twice the excursion. On the view side the home mark is re-acquired with `look_until_facing`
//! after each rung, so each return is a window whose ground truth is an already-observed region.
//!
//! The independent variable is TIME AWAY, and it varies WITHIN the episode rather than across
//! episodes. `eval_memory_dwell` spreads its dwell lengths over bots, which needs several POVs to
//! read one curve; here one POV carries a short, a medium and a long departure back to back, so a
//! single recorded stream contains three points of the same memory-decay curve and they share
//! terrain, lighting and marker. The rung order is permuted per bot, so two bots run the same ladder
//! in different sequences and the pairwise near-clones check still passes.
//!
//! Distinct from `depart_and_revisit`, which makes exactly one departure of one length per episode
//! and therefore yields one sample: the contrast between rungs is the whole point here.
use super::*;

/// The ladder. Distance and dwell scale together, so "a longer departure" means both farther away
/// and longer away — the two confounds are deliberately tied rather than crossed, because crossing
/// them would need four rungs and no episode is long enough for four returns.
///
/// 5 blocks is inside the same visual neighbourhood; 10 is past the point where the mark's pixels
/// are a small patch; 15 is the ceiling that still lets a return complete inside the episode budget
/// (a 15-block walk is ~8 s each way at walking pace) and keeps the mark's chunk loaded.
const RUNGS: [(f64, i64); 3] = [(5.0, 3_000), (10.0, 9_000), (15.0, 20_000)];

const MATERIAL: &str = "minecraft:stone_bricks";

/// Three tall so the mark is still a distinguishable silhouette from the far rung's 15 blocks,
/// where a single block would be a couple of pixels.
const MARK_HEIGHT: i64 = 3;

/// Per-bot excursion fan. 73 deg is coprime enough with 360 that up to eight bots leave on distinct
/// bearings, so peers are not walking the same corridor while their marks stay clustered near the
/// anchor and in each other's opening frame.
const FAN_DEG: f64 = 73.0;

/// One shuttle leg for a dwell, in blocks; see `hold_facing` for why 4 is the only safe value.
const SHUTTLE_LEG: f64 = 4.0;

/// Dwell at home after a verified return, before the next rung departs. Long enough for a training
/// window to sit entirely inside the return.
const HOME_HOLD_MS: i64 = 4_000;

const FACE_MAX_WAIT_MS: u64 = 5_000;
const ARRIVE_RADIUS: f64 = 2.0;

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let anchor = ctx.anchor();
    let start = ctx.position().unwrap_or((anchor.0, 64.0, anchor.1));

    // Mark home where the bot already stands: the compact-staging rule wants everyone's subject
    // near the anchor at frame 1, and walking somewhere else first would spend the episode's
    // opening on a ray.
    let desired = (
        start.0.round() as i64,
        start.1.round() as i64,
        start.2.round() as i64,
    );
    let raised = raise_landmark(ctx, desired, MARK_HEIGHT, MATERIAL).await;
    let home_cell = raised.map(|(base, _)| base);
    let home_xz = home_cell
        .map(|c| (c.x as f64 + 0.5, c.z as f64 + 0.5))
        .unwrap_or((start.0, start.2));
    let focus = raised.map(|(_, point)| point).unwrap_or((
        home_xz.0,
        start.1 + MARK_HEIGHT as f64,
        home_xz.1,
    ));

    // Rotate the rung order per bot. Same three rungs, three different orders, so the ladder is
    // comparable across POVs while the action streams are not near-clones of each other.
    let rung_offset = global % RUNGS.len();
    ctx.record(
        "observe",
        &format!("vis-revisit-ladder-setup-{index}"),
        "Mark home, then depart for a short, a medium and a long interval, verifying each return.",
        json!({
            "global_index": global,
            "role": "ladder_revisitor",
            "mark_placed": raised.is_some(),
            "home": {"x": r2(home_xz.0), "z": r2(home_xz.1)},
            "rung_offset": rung_offset,
            "rungs": RUNGS.iter().map(|(d, ms)| json!({"distance": d, "away_ms": ms}))
                .collect::<Vec<_>>(),
            "eval_only": false,
        }),
    );

    let mut step = 0usize;
    while !ctx.target_met() {
        let rung = (step + rung_offset) % RUNGS.len();
        let (distance, away_ms) = RUNGS[rung];
        // Each rung leaves on its own bearing so the three excursions are three different views of
        // the same home mark, not one corridor walked three times.
        let bearing = (global as f64 * FAN_DEG + step as f64 * 47.0).to_radians();
        let far = (
            home_xz.0 - bearing.sin() * distance,
            home_xz.1 + bearing.cos() * distance,
        );
        let out = travel_to(ctx, far, TRAVEL_HOP, ARRIVE_RADIUS).await;
        if ctx.target_met() {
            break;
        }
        // Face AWAY from home for the whole dwell, on a fixed bearing: the mark has to actually
        // leave the frame or the "return" is not a re-acquisition of anything. The bearing is the
        // outbound heading, which is 180 deg off home and therefore well past the 102.5 deg
        // horizontal FOV.
        let away_yaw = bearing.to_degrees();
        ctx.record(
            TYPE_MOVE,
            &format!("vis-revisit-ladder-away-{index}-{step}"),
            "Depart from the home mark for this rung's interval, facing away the whole time.",
            json!({
                "rung": rung,
                "requested_distance": distance,
                "away_ms": away_ms,
                "away_yaw_deg": r2(away_yaw),
                "arrival_distance": r2(out),
                "exceeds_horizontal_fov": true,
                "elapsed_ms": ctx.elapsed_ms(),
            }),
        );
        hold_facing(ctx, away_yaw, ctx.elapsed_ms() + away_ms, SHUTTLE_LEG).await;
        if ctx.target_met() {
            break;
        }

        let back = travel_to(ctx, home_xz, TRAVEL_HOP, ARRIVE_RADIUS).await;
        let faced = ctx.look_until_facing(focus, 5.0, FACE_MAX_WAIT_MS).await;
        // A window over the column, not the exact cell: the placer can be shoved and the mark can
        // shift, and a miss must stay distinguishable from a world that genuinely lost the mark.
        let observed = home_cell.and_then(|c| {
            area_find_block(
                ctx,
                (c.x, c.z),
                (c.y - 2, c.y + MARK_HEIGHT as i32 + 1),
                MATERIAL,
            )
        });
        ctx.record(
            "observe",
            &format!("vis-revisit-ladder-return-{index}-{step}"),
            "Return to the home mark and verify it after this rung's interval away.",
            json!({
                "rung": rung,
                "away_ms": away_ms,
                "requested_distance": distance,
                "expected_material": MATERIAL,
                "observed_cell": observed.map(|c| json!({"x": c.x, "y": c.y, "z": c.z})),
                "persisted": observed.is_some(),
                "faced_mark": faced,
                "return_distance": r2(back),
                "elapsed_ms": ctx.elapsed_ms(),
            }),
        );
        let home_yaw = ctx
            .position()
            .map(|p| look_angles(head_of(p), focus).0)
            .unwrap_or(0.0);
        hold_facing(ctx, home_yaw, ctx.elapsed_ms() + HOME_HOLD_MS, SHUTTLE_LEG).await;
        witness_peer_edits(ctx).await;
        step += 1;
    }
}
