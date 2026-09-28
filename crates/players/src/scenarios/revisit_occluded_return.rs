//! CATALOG: family=many_player_spatial min_players=1 single_process=false world_edits=true scenery=none far_players=false
//!
//! Observe a low landmark from the side, leave it, then return first on the blocked centre line and
//! finally around the screen that hides it. The two-stage return makes occlusion an explicit state:
//! the landmark can still exist while its pixels are absent, before the same pixels are reacquired.
//!
//! This is the revisit problem's occlusion face. The trajectory closes from the observation post through an
//! excursion and back to the landmark, while every return gaze is confirmed by
//! `look_until_facing`. The scorer gets separate screen and landmark probes, so “not visible behind
//! the screen”, “world edit disappeared”, and “camera never returned” are not collapsed together.
//!
//! Real terrain remains enabled and can add trees, slopes, and water to the scripted screen. Failed
//! placement or travel is recorded and the bot continues around the loop; the screen is evidence,
//! not an assumption. The 4-block travel hops and 4-block living-wait legs are the QC-safe Solaris
//! primitive, 12 blocks removes the landmark from useful resolution, and 5 seconds covers a full
//! 180-degree turn at 72 deg/s with margin.
use super::*;

const MARKER: &str = "minecraft:bricks";
const SCREEN: &str = "minecraft:stone_bricks";
const MARKER_HEIGHT: i64 = 2;
const SCREEN_HEIGHT: i64 = 4;
const CORRIDOR: f64 = 8.0;
const SIDE_CLEARANCE: f64 = 6.0;
const EXCURSION: f64 = 12.0;
const AWAY_MS: i64 = 6_000;
const RETURN_HOLD_MS: i64 = 4_000;
const SHUTTLE_LEG: f64 = 4.0;
const ARRIVE_RADIUS: f64 = 2.0;
const FACE_MAX_WAIT_MS: u64 = 5_000;

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let anchor = ctx.anchor();
    let start = ctx.position().unwrap_or((anchor.0, 64.0, anchor.1));
    let angle = (global as f64 * 71.0).to_radians();
    let forward = (-angle.sin(), angle.cos());
    let side = (forward.1, -forward.0);
    let marker_site = (
        start.0 + forward.0 * CORRIDOR,
        start.2 + forward.1 * CORRIDOR,
    );
    let screen_site = (
        start.0 + forward.0 * CORRIDOR * 0.5,
        start.2 + forward.1 * CORRIDOR * 0.5,
    );

    travel_to(ctx, marker_site, TRAVEL_HOP, ARRIVE_RADIUS).await;
    let marker = raise_landmark(
        ctx,
        (
            marker_site.0.round() as i64,
            start.1.round() as i64,
            marker_site.1.round() as i64,
        ),
        MARKER_HEIGHT,
        MARKER,
    )
    .await;
    travel_to(ctx, screen_site, TRAVEL_HOP, ARRIVE_RADIUS).await;
    let screen = raise_landmark(
        ctx,
        (
            screen_site.0.round() as i64,
            start.1.round() as i64,
            screen_site.1.round() as i64,
        ),
        SCREEN_HEIGHT,
        SCREEN,
    )
    .await;
    let marker_focus = marker.map(|(_, focus)| focus).unwrap_or((
        marker_site.0,
        start.1 + MARKER_HEIGHT as f64,
        marker_site.1,
    ));
    let marker_base = marker.map(|(base, _)| base);
    let screen_base = screen.map(|(base, _)| base);
    let side_post = (
        screen_site.0 + side.0 * SIDE_CLEARANCE,
        screen_site.1 + side.1 * SIDE_CLEARANCE,
    );
    travel_to(ctx, side_post, TRAVEL_HOP, ARRIVE_RADIUS).await;
    let initially_faced = ctx
        .look_until_facing(marker_focus, 6.0, FACE_MAX_WAIT_MS)
        .await;
    ctx.record(
        "observe",
        &format!("vis-revisit-occluded-setup-{index}"),
        "Observe the landmark from beside the screen before leaving it.",
        json!({
            "global_index": global,
            "expected_marker": MARKER,
            "marker_placed": marker.is_some(),
            "screen_placed": screen.is_some(),
            "initially_faced": initially_faced,
            "marker_site": {"x": r2(marker_site.0), "z": r2(marker_site.1)},
            "screen_site": {"x": r2(screen_site.0), "z": r2(screen_site.1)},
        }),
    );

    let mut cycle = 0usize;
    while !ctx.target_met() {
        let away = (
            marker_site.0 + forward.0 * EXCURSION,
            marker_site.1 + forward.1 * EXCURSION,
        );
        let out = travel_to(ctx, away, TRAVEL_HOP, ARRIVE_RADIUS).await;
        let away_yaw = look_angles((marker_site.0, 0.0, marker_site.1), (away.0, 0.0, away.1)).0;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-revisit-occluded-leave-{index}-{cycle}"),
            "Leave the screened landmark beyond useful visual resolution.",
            json!({
                "cycle": cycle,
                "expected_excursion": EXCURSION,
                "observed_arrival_distance": r2(out),
                "away_ms": AWAY_MS,
            }),
        );
        hold_facing(ctx, away_yaw, ctx.elapsed_ms() + AWAY_MS, SHUTTLE_LEG).await;
        if ctx.target_met() {
            break;
        }

        let blocked_post = (start.0 + forward.0 * 1.5, start.2 + forward.1 * 1.5);
        let blocked_distance = travel_to(ctx, blocked_post, TRAVEL_HOP, ARRIVE_RADIUS).await;
        let blocked_faced = ctx
            .look_until_facing(marker_focus, 6.0, FACE_MAX_WAIT_MS)
            .await;
        let screen_observed = screen_base.and_then(|base| {
            area_find_block(
                ctx,
                (base.x, base.z),
                (base.y - 2, base.y + SCREEN_HEIGHT as i32 + 1),
                SCREEN,
            )
        });
        ctx.record(
            "observe",
            &format!("vis-revisit-occluded-blocked-{index}-{cycle}"),
            "Return to the centre line where the screen should hide the remembered landmark.",
            json!({
                "cycle": cycle,
                "expected": "landmark_occluded_by_screen",
                "observed_screen": screen_observed.map(|c| json!({"x": c.x, "y": c.y, "z": c.z})),
                "screen_present": screen_observed.is_some(),
                "changed": screen_observed.is_none(),
                "faced_landmark_bearing": blocked_faced,
                "arrival_distance": r2(blocked_distance),
            }),
        );

        let bypass = (
            screen_site.0 + side.0 * SIDE_CLEARANCE,
            screen_site.1 + side.1 * SIDE_CLEARANCE,
        );
        travel_to(ctx, bypass, TRAVEL_HOP, ARRIVE_RADIUS).await;
        let returned = travel_to(ctx, marker_site, TRAVEL_HOP, ARRIVE_RADIUS).await;
        let reacquired = ctx
            .look_until_facing(marker_focus, 6.0, FACE_MAX_WAIT_MS)
            .await;
        let observed = marker_base.and_then(|base| {
            area_find_block(
                ctx,
                (base.x, base.z),
                (base.y - 2, base.y + MARKER_HEIGHT as i32 + 1),
                MARKER,
            )
        });
        ctx.record(
            "observe",
            &format!("vis-revisit-occluded-reacquire-{index}-{cycle}"),
            "Bypass the screen, return to the landmark, and verify its material.",
            json!({
                "cycle": cycle,
                "expected": MARKER,
                "observed": observed.map(|c| json!({"x": c.x, "y": c.y, "z": c.z})),
                "changed": observed.is_none(),
                "reacquired": reacquired,
                "return_distance": r2(returned),
                "elapsed_ms": ctx.elapsed_ms(),
            }),
        );
        let bearing = ctx
            .position()
            .map(|p| look_angles(head_of(p), marker_focus).0)
            .unwrap_or(0.0);
        hold_facing(ctx, bearing, ctx.elapsed_ms() + RETURN_HOLD_MS, SHUTTLE_LEG).await;
        cycle += 1;
    }
}
