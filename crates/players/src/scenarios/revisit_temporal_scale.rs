//! CATALOG: family=many_player_spatial min_players=1 single_process=false world_edits=true scenery=none far_players=false
//!
//! Revisit one fixed landmark after short, medium, and long gaps while keeping the departure distance
//! constant. Existing distance ladders tie farther travel to longer delay; this scene separates the
//! temporal variable from geometry inside one POV.
//!
//! This is the revisit problem's temporal-hierarchy face. Every rung observes the same column, walks the same
//! 12-block radius out and back, and confirms the return with `look_until_facing`. The event stream
//! records requested and measured absence time beside expected/observed material, so downstream can
//! score a memory curve without guessing how long the subject was out of view.
//!
//! Natural terrain can make an excursion unreachable or a marker unplaceable; both are recorded and
//! the next rung still runs. The 2/7/16-second gaps straddle one, two, and several 4.65-second training
//! windows, 12 blocks makes the column visually small, 4-block travel/hold legs clear key chatter,
//! and the rung order rotates per bot to avoid near-clone action streams at N=2.
use super::*;

const MATERIAL: &str = "minecraft:stone_bricks";
const MARK_HEIGHT: i64 = 3;
const DWELLS_MS: [i64; 3] = [2_000, 7_000, 16_000];
const EXCURSION: f64 = 12.0;
const FAN_DEG: f64 = 83.0;
const RUNG_TURN_DEG: f64 = 47.0;
const RETURN_HOLD_MS: i64 = 3_500;
const SHUTTLE_LEG: f64 = 4.0;
const ARRIVE_RADIUS: f64 = 2.0;
const FACE_MAX_WAIT_MS: u64 = 5_000;

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let anchor = ctx.anchor();
    let start = ctx.position().unwrap_or((anchor.0, 64.0, anchor.1));
    let desired = (
        start.0.round() as i64,
        start.1.round() as i64,
        start.2.round() as i64,
    );
    let mark = raise_landmark(ctx, desired, MARK_HEIGHT, MATERIAL).await;
    let base = mark.map(|(base, _)| base);
    let home = base
        .map(|cell| (cell.x as f64 + 0.5, cell.z as f64 + 0.5))
        .unwrap_or((start.0, start.2));
    let focus =
        mark.map(|(_, point)| point)
            .unwrap_or((home.0, start.1 + MARK_HEIGHT as f64, home.1));
    let opened = ctx.look_until_facing(focus, 5.0, FACE_MAX_WAIT_MS).await;
    ctx.record(
        "observe",
        &format!("vis-revisit-temporal-setup-{index}"),
        "Observe the fixed landmark before running three absence-time rungs.",
        json!({
            "global_index": global,
            "expected": MATERIAL,
            "observed": base.and_then(|cell| ctx.block_id_at(cell)),
            "changed": mark.is_none(),
            "opened": opened,
            "dwell_ms": DWELLS_MS,
            "excursion": EXCURSION,
        }),
    );

    let rung_offset = global % DWELLS_MS.len();
    let mut cycle = 0usize;
    while !ctx.target_met() {
        let rung = (cycle + rung_offset) % DWELLS_MS.len();
        let requested_ms = DWELLS_MS[rung];
        let bearing = global as f64 * FAN_DEG + cycle as f64 * RUNG_TURN_DEG;
        let far = point_at_yaw((home.0, 0.0, home.1), bearing, EXCURSION);
        let last_seen_ms = ctx.elapsed_ms();
        let out = travel_to(ctx, (far.0, far.2), TRAVEL_HOP, ARRIVE_RADIUS).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-revisit-temporal-leave-{index}-{cycle}"),
            "Leave the landmark for this rung while holding distance constant.",
            json!({
                "cycle": cycle,
                "rung": rung,
                "requested_away_ms": requested_ms,
                "expected_excursion": EXCURSION,
                "observed_arrival_distance": r2(out),
                "last_seen_ms": last_seen_ms,
            }),
        );
        hold_facing(ctx, bearing, ctx.elapsed_ms() + requested_ms, SHUTTLE_LEG).await;
        if ctx.target_met() {
            break;
        }

        let returned = travel_to(ctx, home, TRAVEL_HOP, ARRIVE_RADIUS).await;
        let faced = ctx.look_until_facing(focus, 5.0, FACE_MAX_WAIT_MS).await;
        let observed = base.and_then(|cell| {
            area_find_block(
                ctx,
                (cell.x, cell.z),
                (cell.y - 2, cell.y + MARK_HEIGHT as i32 + 1),
                MATERIAL,
            )
        });
        let measured_absence_ms = ctx.elapsed_ms() - last_seen_ms;
        ctx.record(
            "observe",
            &format!("vis-revisit-temporal-return-{index}-{cycle}"),
            "Return after the requested time gap and verify the same landmark.",
            json!({
                "cycle": cycle,
                "rung": rung,
                "expected": MATERIAL,
                "observed": observed.map(|c| json!({"x": c.x, "y": c.y, "z": c.z})),
                "changed": observed.is_none(),
                "requested_away_ms": requested_ms,
                "measured_absence_ms": measured_absence_ms,
                "faced_landmark": faced,
                "return_distance": r2(returned),
                "elapsed_ms": ctx.elapsed_ms(),
            }),
        );
        let home_yaw = ctx
            .position()
            .map(|p| look_angles(head_of(p), focus).0)
            .unwrap_or(0.0);
        hold_facing(
            ctx,
            home_yaw,
            ctx.elapsed_ms() + RETURN_HOLD_MS,
            SHUTTLE_LEG,
        )
        .await;
        cycle += 1;
    }
}
