//! CATALOG: family=many_player_spatial min_players=1 single_process=true world_edits=true scenery=none far_players=false
//!
//! Walk out dropping breadcrumbs, then walk the trail back verifying every crumb IN REVERSE ORDER.
//!
//! This is the densest revisit shape in the family: one lap contains four departures and four
//! returns, because every crumb is observed once on the way out and once on the way back. A 4.65 s
//! training window anywhere inside the inbound half therefore has an already-observed subject in its
//! ground truth, which is the property 26.7% of generation-5 windows lacked.
//!
//! Order is what the return verifies. The four crumbs are four different materials laid in a fixed
//! sequence, so the inbound pass can assert not just "a crumb is here" but "crumb 2 is here, and it
//! is bricks, and crumb 3 was behind me" — a sequence memory rather than a presence check. That is
//! also why the trail CURVES: each leg turns by 27 deg, so the previous crumb slides out of the
//! 102.5 deg frame during the next leg and each verification is a genuine re-acquisition rather than
//! a subject that never left the picture.
//!
//! Distinct from `trail_write_and_erase`, which scatters trail blocks around an annulus and then
//! hunts for a peer's to erase — there is no ordering and no backtrack there, and the trail is the
//! task rather than the memory. Distinct from `landmark_tour`, which visits places without leaving
//! anything behind to check.
use super::*;

/// One material per crumb, in laying order. Different textures are what make the ORDER readable from
/// the footage, not only from the event stream.
const MATERIALS: [&str; 4] = [
    "minecraft:stone_bricks",
    "minecraft:bricks",
    "minecraft:oak_planks",
    "minecraft:cobblestone",
];

/// Blocks per leg. Must stay well above 4: each leg is one press-to-release, and `move_to` floors its
/// arrival radius at 1.5 blocks, so a 5.5-block leg still walks ~4 blocks — about 0.7 s sprinting,
/// which clears the key-chatter floor of 0.5 s. A 4-block leg would leave only 2.5 blocks of walking
/// and start tripping it.
const LEG: f64 = 5.5;

/// Turn between consecutive outbound legs. 27 deg over three legs bends the trail by 81 deg total,
/// which is enough that a crumb leaves the frame during the following leg while the whole lap still
/// stays inside a compact neighbourhood (three legs of 5.5 blocks reach ~15 blocks, not 16.5).
const LEG_TURN_DEG: f64 = 27.0;

/// Per-bot fan of the first outbound bearing, and per-lap rotation. 137 deg is close to the golden
/// angle, so successive bots and successive laps fan out without ever repeating a corridor.
const FAN_DEG: f64 = 137.0;
const LAP_TURN_DEG: f64 = 61.0;

const SHUTTLE_LEG: f64 = 4.0;
/// Dwell at a verified crumb. Short: a lap has eight of them, and the lap has to fit in an episode.
const CRUMB_HOLD_MS: i64 = 2_500;
/// Wider than a tall landmark's 5 deg — a crumb is one block on the floor a couple of blocks away,
/// where a small position drift is a large angular error.
const CRUMB_FACE_TOL_DEG: f64 = 10.0;
const FACE_MAX_WAIT_MS: u64 = 5_000;
const ARRIVE_RADIUS: f64 = 1.8;

/// Standoff the inbound pass aims for instead of the crumb's own cell. Must exceed
/// [`ARRIVE_RADIUS`], so that even an arrival on the near side of the standoff point leaves the bot
/// beside the crumb rather than on top of it: nav will happily route over a 1-high block, and a
/// `look_until_facing` target directly underfoot has a degenerate yaw that never converges. That
/// would record `faced: false` for a return which actually happened, and `faced` is downstream
/// evidence for telling a hallucination from a real change — a known false negative in it is worse
/// than three blocks of standoff.
const CRUMB_STANDOFF: f64 = 3.0;

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let anchor = ctx.anchor();
    let start = ctx.position().unwrap_or((anchor.0, 64.0, anchor.1));
    let feet_y = start.1.round() as i64;

    ctx.record(
        "observe",
        &format!("vis-revisit-crumbs-setup-{index}"),
        "Lay a four-crumb trail outbound, then backtrack it verifying every crumb in reverse.",
        json!({
            "global_index": global,
            "role": "backtracker",
            "leg": LEG,
            "leg_turn_deg": LEG_TURN_DEG,
            "materials": MATERIALS,
            "fan_deg": global as f64 * FAN_DEG,
        }),
    );

    let mut lap = 0usize;
    while !ctx.target_met() {
        // Crumb 0 is dropped where the lap starts, so the trail's near end is itself a verifiable
        // waypoint and the lap closes on a mark rather than on bare coordinates.
        let mut crumbs: Vec<(BlockPos, &str)> = Vec::with_capacity(MATERIALS.len());
        let mut bearing = global as f64 * FAN_DEG + lap as f64 * LAP_TURN_DEG;

        for (leg, material) in MATERIALS.iter().enumerate() {
            if ctx.target_met() {
                break;
            }
            if leg > 0 {
                // The turn happens BEFORE the leg, so the crumb just laid falls behind the shoulder
                // as the bot walks on.
                bearing += LEG_TURN_DEG;
                let here = ctx.position().unwrap_or(start);
                let planned_step = point_at_yaw((here.0, 0.0, here.2), bearing, LEG);
                let step = walkable_xz_near(ctx, (planned_step.0, planned_step.2), feet_y as i32)
                    .unwrap_or((planned_step.0, planned_step.2));
                travel_to(ctx, step, TRAVEL_HOP, ARRIVE_RADIUS).await;
            }
            let here = ctx.position().unwrap_or(start);
            let desired = (here.0.round() as i64, feet_y, here.2.round() as i64);
            let cell = marker_base_near(ctx, desired, 1);
            let mut placed = false;
            if let Some(cell) = cell {
                let aim = (
                    cell.x as f64 + 0.5,
                    cell.y as f64 + 0.5,
                    cell.z as f64 + 0.5,
                );
                // Face the cell before placing: the server judges `use_item_on` on the packet's hit
                // position, so skipping this yields footage of a bot placing over its shoulder.
                ctx.look_until_facing(aim, CRUMB_FACE_TOL_DEG, FACE_MAX_WAIT_MS)
                    .await;
                placed = ctx.equip(material).await && ctx.place_block(cell, material).await;
                if placed {
                    crumbs.push((cell, *material));
                }
            }
            ctx.record(
                TYPE_PLACE,
                &format!("vis-revisit-crumbs-lay-{index}-{lap}-{leg}"),
                "Lay the next breadcrumb on the outbound trail.",
                json!({
                    "lap": lap,
                    "leg": leg,
                    "material": material,
                    "bearing_deg": r2(bearing),
                    "cell": cell.map(|c| json!({"x": c.x, "y": c.y, "z": c.z})),
                    "placed": placed,
                }),
            );
        }

        // Inbound: the same crumbs in reverse. The bot is standing on the last one, so the first
        // verification costs no travel and the rest are one leg apart — the exact reverse of the
        // outbound press pattern, which is what makes the lap a loop and not a there-and-elsewhere.
        for (step, (cell, material)) in crumbs.iter().enumerate().rev() {
            if ctx.target_met() {
                break;
            }
            // Approach from wherever the bot currently is and stop [`CRUMB_STANDOFF`] short, so the
            // crumb stays in front of the camera instead of under the feet.
            let here = ctx.position().unwrap_or(start);
            let (dx, dz) = (here.0 - cell.x as f64 - 0.5, here.2 - cell.z as f64 - 0.5);
            let span = dx.hypot(dz).max(0.01);
            let target = (
                cell.x as f64 + 0.5 + dx / span * CRUMB_STANDOFF,
                cell.z as f64 + 0.5 + dz / span * CRUMB_STANDOFF,
            );
            let travelled = travel_to(ctx, target, TRAVEL_HOP, ARRIVE_RADIUS).await;
            let focus = (
                cell.x as f64 + 0.5,
                cell.y as f64 + 0.5,
                cell.z as f64 + 0.5,
            );
            let faced = ctx
                .look_until_facing(focus, CRUMB_FACE_TOL_DEG, FACE_MAX_WAIT_MS)
                .await;
            // Probe the neighbourhood for THIS crumb's material: finding the right texture at the
            // right place is what distinguishes "I am back at crumb 2" from "I am back at some crumb".
            let observed =
                area_find_block(ctx, (cell.x, cell.z), (cell.y - 2, cell.y + 2), material);
            ctx.record(
                "observe",
                &format!("vis-revisit-crumbs-check-{index}-{lap}-{step}"),
                "Backtrack onto a breadcrumb and verify its material and its place in the order.",
                json!({
                    "lap": lap,
                    "leg": step,
                    "inbound_rank": crumbs.len() - 1 - step,
                    "expected_material": material,
                    "expected_cell": {"x": cell.x, "y": cell.y, "z": cell.z},
                    "observed_cell": observed.map(|c| json!({"x": c.x, "y": c.y, "z": c.z})),
                    "verified": observed.is_some(),
                    "faced_crumb": faced,
                    "standoff_blocks": CRUMB_STANDOFF,
                    "arrival_distance": r2(travelled),
                    "elapsed_ms": ctx.elapsed_ms(),
                }),
            );
            // Hold the fixed bearing onto the crumb rather than tracking it, so a training window can
            // sit inside the verification without the camera turning as a consequence of the walk.
            let crumb_yaw = ctx
                .position()
                .map(|p| look_angles(head_of(p), focus).0)
                .unwrap_or(0.0);
            hold_facing(
                ctx,
                crumb_yaw,
                ctx.elapsed_ms() + CRUMB_HOLD_MS,
                SHUTTLE_LEG,
            )
            .await;
        }
        witness_peer_edits(ctx).await;
        lap += 1;
    }
}
