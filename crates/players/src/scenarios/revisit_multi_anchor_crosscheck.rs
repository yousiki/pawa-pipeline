//! CATALOG: family=many_player_spatial min_players=1 single_process=false world_edits=true scenery=none far_players=false
//!
//! Memorise two same-material anchors whose different heights encode their identities, leave both,
//! then return to each and cross-check the pair's spatial relation. A single persistent landmark can
//! be solved by recognition; this scene requires binding two observations to two places.
//!
//! This is the revisit problem's multi-anchor face. The route closes anchor A -> anchor B -> remote point -> A
//! -> B -> hub, and both return gazes use `look_until_facing`. Verification records material, height,
//! and the expected inter-anchor separation, so a scorer can distinguish an identity swap from a
//! missing edit or a camera turn that never completed.
//!
//! Natural terrain is intentionally left untouched. Each `raise_landmark` may shift to a placeable
//! neighbour or fail; those outcomes stay in the records and the route continues. A 7-block ring
//! keeps the two anchors compact, 133 degrees gives 12.8 blocks between them, and the 13-block remote
//! excursion removes both from useful resolution while remaining in the loaded neighbourhood.
use super::*;

const MATERIAL: &str = "minecraft:stone_bricks";
const HEIGHTS: [i64; 2] = [2, 3];
const SITE_RADIUS: f64 = 7.0;
const SITE_SEPARATION_DEG: f64 = 133.0;
const FAN_DEG: f64 = 79.0;
const EXCURSION: f64 = 13.0;
const AWAY_MS: i64 = 7_000;
const ANCHOR_HOLD_MS: i64 = 3_500;
const SHUTTLE_LEG: f64 = 4.0;
const ARRIVE_RADIUS: f64 = 2.0;
const FACE_MAX_WAIT_MS: u64 = 5_000;

fn stack_height(ctx: &Ctx, base: BlockPos) -> i32 {
    let wanted = MATERIAL.strip_prefix("minecraft:").unwrap_or(MATERIAL);
    (0..=4)
        .take_while(|&dy| ctx.block_id_at(base.up(dy)).as_deref() == Some(wanted))
        .count() as i32
}

type MarkedAnchor = (BlockPos, (f64, f64, f64));

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let anchor = ctx.anchor();
    let start = ctx.position().unwrap_or((anchor.0, 64.0, anchor.1));
    let fan = global as f64 * FAN_DEG;
    let sites: [(f64, f64); 2] = std::array::from_fn(|i| {
        let angle = (fan + i as f64 * SITE_SEPARATION_DEG).to_radians();
        (
            anchor.0 - angle.sin() * SITE_RADIUS,
            anchor.1 + angle.cos() * SITE_RADIUS,
        )
    });
    let expected_separation = (sites[1].0 - sites[0].0).hypot(sites[1].1 - sites[0].1);
    let mut marks: Vec<Option<MarkedAnchor>> = Vec::with_capacity(2);

    for (i, site) in sites.iter().copied().enumerate() {
        let arrival = travel_to(ctx, site, TRAVEL_HOP, ARRIVE_RADIUS).await;
        let mark = raise_landmark(
            ctx,
            (
                site.0.round() as i64,
                start.1.round() as i64,
                site.1.round() as i64,
            ),
            HEIGHTS[i],
            MATERIAL,
        )
        .await;
        let focus =
            mark.map(|(_, point)| point)
                .unwrap_or((site.0, start.1 + HEIGHTS[i] as f64, site.1));
        let faced = ctx.look_until_facing(focus, 6.0, FACE_MAX_WAIT_MS).await;
        ctx.record(
            "observe",
            &format!("vis-revisit-multi-anchor-open-{index}-{i}"),
            "Observe one member of the two-anchor memory before the joint departure.",
            json!({
                "anchor_id": i,
                "expected_material": MATERIAL,
                "expected_height": HEIGHTS[i],
                "placed": mark.is_some(),
                "faced": faced,
                "arrival_distance": r2(arrival),
            }),
        );
        marks.push(mark);
    }

    ctx.record(
        "observe",
        &format!("vis-revisit-multi-anchor-setup-{index}"),
        "Commit the two anchors and their spatial relation before leaving both.",
        json!({
            "global_index": global,
            "expected_anchor_count": 2,
            "observed_anchor_count": marks.iter().filter(|m| m.is_some()).count(),
            "expected_separation": r2(expected_separation),
            "changed": marks.iter().any(Option::is_none),
            "sites": sites.iter().map(|(x, z)| json!({"x": r2(*x), "z": r2(*z)})).collect::<Vec<_>>(),
        }),
    );

    let mut cycle = 0usize;
    while !ctx.target_met() {
        let away_angle = (fan + 247.0 + cycle as f64 * 41.0).to_radians();
        let remote = (
            anchor.0 - away_angle.sin() * EXCURSION,
            anchor.1 + away_angle.cos() * EXCURSION,
        );
        let out = travel_to(ctx, remote, TRAVEL_HOP, ARRIVE_RADIUS).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-revisit-multi-anchor-leave-{index}-{cycle}"),
            "Leave both anchors for one shared memory interval.",
            json!({
                "cycle": cycle,
                "expected_excursion": EXCURSION,
                "observed_arrival_distance": r2(out),
                "away_ms": AWAY_MS,
            }),
        );
        hold_facing(
            ctx,
            away_angle.to_degrees(),
            ctx.elapsed_ms() + AWAY_MS,
            SHUTTLE_LEG,
        )
        .await;
        if ctx.target_met() {
            break;
        }

        for i in 0..2 {
            let returned = travel_to(ctx, sites[i], TRAVEL_HOP, ARRIVE_RADIUS).await;
            let focus = marks[i].map(|(_, point)| point).unwrap_or((
                sites[i].0,
                start.1 + HEIGHTS[i] as f64,
                sites[i].1,
            ));
            let faced = ctx.look_until_facing(focus, 6.0, FACE_MAX_WAIT_MS).await;
            let observed_height = marks[i].map(|(base, _)| stack_height(ctx, base));
            ctx.record(
                "observe",
                &format!("vis-revisit-multi-anchor-return-{index}-{cycle}-{i}"),
                "Return to one remembered anchor and verify its identity inside the pair.",
                json!({
                    "cycle": cycle,
                    "anchor_id": i,
                    "expected": {"material": MATERIAL, "height": HEIGHTS[i]},
                    "observed": {"height": observed_height},
                    "changed": observed_height != Some(HEIGHTS[i] as i32),
                    "faced_anchor": faced,
                    "return_distance": r2(returned),
                    "expected_separation": r2(expected_separation),
                    "elapsed_ms": ctx.elapsed_ms(),
                }),
            );
            let bearing = ctx
                .position()
                .map(|p| look_angles(head_of(p), focus).0)
                .unwrap_or(0.0);
            hold_facing(ctx, bearing, ctx.elapsed_ms() + ANCHOR_HOLD_MS, SHUTTLE_LEG).await;
        }
        let closed = travel_to(ctx, anchor, TRAVEL_HOP, ARRIVE_RADIUS).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-revisit-multi-anchor-close-{index}-{cycle}"),
            "Close the two-anchor route at the common hub.",
            json!({"cycle": cycle, "hub_distance": r2(closed)}),
        );
        cycle += 1;
    }
}
