//! CATALOG: family=construction min_players=1 single_process=false world_edits=true scenery=none far_players=false
//!
//! Build a short screen, then walk around its end to recover the marker that the screen hid.
//!
//! Which face of the revisit problem this answers: OCCLUSION RECOVERY. The screen is built on a real support
//! cell found with `marker_base_near`; a contrasting marker sits just beyond it. From the front the
//! growing screen blocks the marker and nearby terrain, while the rear station reveals the same
//! marker. Front -> rear -> front is a bounded sight-line loop, and every stop uses
//! `look_until_facing` to prove the turn actually arrived. The trajectory repeats the two stations
//! and `hold_facing` shuttles during each dwell so construction never degenerates into a statue.
use super::*;

const SCREEN_MATERIAL: &str = "minecraft:deepslate_bricks";
/// A 2x2 screen is enough to hide a low marker from the front without scaffolding.
const SCREEN_WIDTH: i64 = 2;
const SCREEN_HEIGHT: i64 = 2;
const MARKER_HEIGHT: i64 = 2;
const FRONT_STANDOFF: f64 = 2.5;
const REAR_DISTANCE: f64 = 7.0;
const FRONT_HOLD_MS: [i64; 3] = [4_500, 5_500, 6_000];
const REAR_HOLD_MS: [i64; 3] = [6_000, 4_500, 5_500];
/// Four-block shuttle legs satisfy key-chatter while remaining local to each station.
const SHUTTLE_LEG: f64 = 4.0;

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let slot = global % FRONT_HOLD_MS.len();
    let anchor = ctx.anchor();
    let front = (anchor.0, anchor.1 - FRONT_STANDOFF);
    let rear = (anchor.0, anchor.1 + REAR_DISTANCE);
    travel_to(ctx, front, TRAVEL_HOP, 1.2).await;
    let feet_y = ctx.position().map(|p| p.1.round() as i64).unwrap_or(64);
    let screen_base = marker_base_near(
        ctx,
        (anchor.0.round() as i64, feet_y, anchor.1.round() as i64),
        SCREEN_HEIGHT,
    );
    let screen_focus = screen_base
        .map(|b| (b.x as f64 + 0.5, b.y as f64 + 0.8, b.z as f64 + 0.5))
        .unwrap_or((anchor.0, feet_y as f64 + 1.0, anchor.1));
    let marker_base = marker_base_near(
        ctx,
        (
            anchor.0.round() as i64,
            feet_y,
            (anchor.1 + 3.0).round() as i64,
        ),
        MARKER_HEIGHT,
    );
    let marker_focus = marker_base
        .map(|b| (b.x as f64 + 0.5, b.y as f64 + 1.0, b.z as f64 + 0.5))
        .unwrap_or((anchor.0, feet_y as f64 + 1.0, anchor.1 + 3.0));
    ctx.record(
        "observe",
        &format!("vis-loopbuild-occlusion-setup-{index}"),
        "Claim the screen and the rear marker whose sight line will be recovered.",
        json!({
            "global_index": global,
            "screen_base": screen_base.map(|b| json!({"x": b.x, "y": b.y, "z": b.z})),
            "marker_base": marker_base.map(|b| json!({"x": b.x, "y": b.y, "z": b.z})),
            "front": {"x": r2(front.0), "z": r2(front.1)},
            "rear": {"x": r2(rear.0), "z": r2(rear.1)},
        }),
    );

    let mut cycle = 0usize;
    while !ctx.target_met() {
        travel_to(ctx, front, TRAVEL_HOP, 1.2).await;
        let cell = cycle.min((SCREEN_WIDTH * SCREEN_HEIGHT - 1) as usize);
        let target = screen_base.map(|b| {
            let dx = (cell as i64) % SCREEN_WIDTH;
            let dy = (cell as i64) / SCREEN_WIDTH;
            BlockPos::new(b.x + dx as i32, b.y + dy as i32, b.z)
        });
        let faced_front = ctx.look_until_facing(screen_focus, 6.0, 4_000).await;
        let (placed, attempted) = if let Some(target) = target {
            let cells = [(
                (target.x as i64, target.y as i64, target.z as i64),
                SCREEN_MATERIAL.to_string(),
            )];
            ctx.build_blocks(&cells, |b, _target, _material| {
                (
                    format!("vis-loopbuild-occlusion-screen-{index}-c{cycle}-{b}"),
                    format!("Place screen cell {}/{SCREEN_WIDTH}.", b + 1),
                    json!({"cycle": cycle, "cell": cell}),
                )
            })
            .await
        } else {
            (0, 0)
        };
        ctx.record(
            TYPE_PLACE,
            &format!("vis-loopbuild-occlusion-front-{index}-c{cycle}"),
            "Work at the screen face; the rear marker is increasingly hidden here.",
            json!({"cycle": cycle, "faced_front": faced_front, "placed": placed, "attempted": attempted, "screen_resolved": screen_base.is_some()}),
        );
        let front_bearing = ctx
            .position()
            .map(|p| look_angles(head_of(p), screen_focus).0)
            .unwrap_or(0.0);
        hold_facing(
            ctx,
            front_bearing,
            ctx.elapsed_ms() + FRONT_HOLD_MS[slot],
            SHUTTLE_LEG,
        )
        .await;
        if ctx.target_met() {
            break;
        }

        travel_to(ctx, rear, TRAVEL_HOP, 1.2).await;
        let faced_rear = ctx.look_until_facing(marker_focus, 6.0, 5_000).await;
        let marker_block = marker_base.and_then(|b| ctx.block_id_at(b));
        ctx.record(
            "observe",
            &format!("vis-loopbuild-occlusion-reveal-{index}-c{cycle}"),
            "Walk around the screen and recover the rear marker from the opposite view.",
            json!({"cycle": cycle, "is_return": cycle > 0, "faced_rear": faced_rear, "marker_block": marker_block, "occlusion_recovered": faced_rear}),
        );
        let rear_bearing = ctx
            .position()
            .map(|p| look_angles(head_of(p), marker_focus).0)
            .unwrap_or(0.0);
        hold_facing(
            ctx,
            rear_bearing,
            ctx.elapsed_ms() + REAR_HOLD_MS[slot],
            SHUTTLE_LEG,
        )
        .await;
        cycle += 1;
    }
}
