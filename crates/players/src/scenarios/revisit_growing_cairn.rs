//! CATALOG: family=many_player_spatial min_players=1 single_process=true world_edits=true scenery=none far_players=false
//!
//! The same site, revisited over and over, one block TALLER every time.
//!
//! Every other revisit script in the corpus verifies a subject that is supposed to be identical, so
//! the only two outcomes are "persisted" and "gone". That teaches change detection nothing. Here the
//! subject changes by a known, minimal increment between consecutive returns: the bot adds one layer,
//! leaves, comes back, and the cairn is exactly one layer higher than the view it is remembering.
//! The increment is one block on purpose — the smallest change that is still visible — because a
//! large change is trivially detectable and tells us nothing about the resolution of the memory.
//!
//! The revisit problem: the trajectory is a lobe walked out and back for every layer, so path
//! length is twice the excursion while net displacement is ~0; the sight line comes back onto the
//! cairn under
//! `look_until_facing`, and the expected height is written into the event stream next to the observed
//! one, so a downstream scorer can tell a stale reconstruction from a correct one.
//!
//! Once a cairn reaches its reach ceiling the bot starts a NEW cairn a few blocks over in a different
//! material and leaves the finished one standing. Returns later in the episode therefore look at a
//! small skyline of cairns of different heights and materials, and "which one am I building" becomes
//! a second, harder discrimination on top of "how tall is it".
use super::*;

/// Materials cycle per cairn, so two cairns standing side by side are distinguishable in the pixels
/// and in the probe.
const MATERIALS: [&str; 3] = [
    "minecraft:stone_bricks",
    "minecraft:cobblestone",
    "minecraft:oak_log",
];

/// Layers a cairn is grown to before the bot starts a new one. 4 is a reach limit, not a taste one:
/// placing onto the top of a 4-tall cairn from the 2-block arrival standoff is a 3.1-block reach
/// against the 4.5-block maximum, and a fifth layer would silently fail and corrupt the expected
/// height the whole script is built on.
const MAX_LAYERS: i32 = 4;

/// Cairn sites sit on a ring around the anchor, stepping one notch per cairn. 7 blocks keeps every
/// cairn inside one loaded neighbourhood and inside a peer's opening frame; 47 deg between
/// consecutive sites is 5.6 blocks of separation, enough that two cairns never merge into one column.
const SITE_RADIUS: f64 = 7.0;
const SITE_STEP_DEG: f64 = 47.0;

/// Per-bot rotation of the whole ring of sites, so peers build in different places and their action
/// streams are not near-clones.
const FAN_DEG: f64 = 83.0;

/// Excursion per layer, and time spent away from the cairn. 11 blocks puts the cairn far enough away
/// that a one-block height change is a subtle difference rather than an obvious one, and 8 s away is
/// long enough that the departure and the return land in different training windows.
const EXCURSION: f64 = 11.0;
const AWAY_MS: i64 = 8_000;

/// Facing tolerance when placing a layer. Looser than the 5 deg the cairn check uses: the target is
/// one block within arm's reach, where a small standoff drift is a large angular error. Still needed
/// at all because the server judges `use_item_on` on the packet's hit position, so without it the
/// block lands correctly while the footage shows the bot placing it over its shoulder.
const PLACE_FACE_TOL_DEG: f64 = 8.0;

const SHUTTLE_LEG: f64 = 4.0;
const CAIRN_HOLD_MS: i64 = 4_000;
const FACE_MAX_WAIT_MS: u64 = 5_000;
const ARRIVE_RADIUS: f64 = 2.0;

/// Consecutive cells of `material` upward from `base`. Reads one past the ceiling so an unexpected
/// extra layer shows up as a discrepancy instead of being clamped away.
fn stack_height(ctx: &Ctx, base: BlockPos, material: &str) -> i32 {
    let wanted = material.strip_prefix("minecraft:").unwrap_or(material);
    (0..=MAX_LAYERS + 1)
        .take_while(|&dy| ctx.block_id_at(base.up(dy)).as_deref() == Some(wanted))
        .count() as i32
}

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let anchor = ctx.anchor();
    let start = ctx.position().unwrap_or((anchor.0, 64.0, anchor.1));
    let feet_y = start.1.round() as i64;

    ctx.record(
        "observe",
        &format!("vis-revisit-cairn-setup-{index}"),
        "Grow one cairn a layer at a time, leaving and returning between every layer.",
        json!({
            "global_index": global,
            "role": "growing_revisitor",
            "max_layers": MAX_LAYERS,
            "site_radius": SITE_RADIUS,
            "fan_deg": global as f64 * FAN_DEG,
        }),
    );

    let mut cairn = 0usize;
    let mut layers = 0i32;
    let mut base: Option<BlockPos> = None;
    let mut cycle = 0usize;
    while !ctx.target_met() {
        let material = MATERIALS[cairn % MATERIALS.len()];
        let bearing = (global as f64 * FAN_DEG + cairn as f64 * SITE_STEP_DEG).to_radians();
        let planned_site = (
            anchor.0 - bearing.sin() * SITE_RADIUS,
            anchor.1 + bearing.cos() * SITE_RADIUS,
        );
        let site = marker_base_near(
            ctx,
            (
                planned_site.0.round() as i64,
                feet_y,
                planned_site.1.round() as i64,
            ),
            1,
        )
        .map(|b| (b.x as f64 + 0.5, b.z as f64 + 0.5))
        .unwrap_or(planned_site);
        travel_to(ctx, site, TRAVEL_HOP, ARRIVE_RADIUS).await;
        if ctx.target_met() {
            break;
        }

        // First layer of a cairn goes through `marker_base_near`, which is what finds a cell real
        // terrain will actually accept; later layers stack onto the base it chose.
        let existing = base;
        let placed = match existing {
            None => {
                let raised = raise_landmark(
                    ctx,
                    (site.0.round() as i64, feet_y, site.1.round() as i64),
                    1,
                    material,
                )
                .await;
                base = raised.map(|(cell, _)| cell);
                if base.is_some() {
                    layers = 1;
                }
                base.is_some()
            }
            Some(cell) => {
                let target = cell.up(layers);
                let aim = (
                    target.x as f64 + 0.5,
                    target.y as f64 + 0.5,
                    target.z as f64 + 0.5,
                );
                // Face the cell before placing: `use_item_on` is judged on the packet's hit
                // position, so without this the block lands correctly while the footage shows the
                // bot placing it over its shoulder.
                ctx.look_until_facing(aim, PLACE_FACE_TOL_DEG, FACE_MAX_WAIT_MS)
                    .await;
                let ok = ctx.equip(material).await && ctx.place_block(target, material).await;
                if ok {
                    layers += 1;
                }
                ok
            }
        };
        let focus = base
            .map(|cell| {
                (
                    cell.x as f64 + 0.5,
                    cell.y as f64 + layers as f64 - 0.5,
                    cell.z as f64 + 0.5,
                )
            })
            .unwrap_or((site.0, start.1 + 1.0, site.1));
        ctx.record(
            TYPE_PLACE,
            &format!("vis-revisit-cairn-grow-{index}-{cycle}"),
            "Add one layer to the cairn, so the next return sees it exactly one block taller.",
            json!({
                "cairn": cairn,
                "cycle": cycle,
                "material": material,
                "layer_placed": placed,
                "height_after": layers,
                "elapsed_ms": ctx.elapsed_ms(),
            }),
        );
        if ctx.target_met() {
            break;
        }

        // Leave on a bearing that turns every cycle, so the cairn is re-acquired from a different
        // side each time and the return is not a replay of the same pixels.
        let away_yaw = bearing.to_degrees() + 180.0 + cycle as f64 * 31.0;
        let away = point_at_yaw((site.0, 0.0, site.1), away_yaw, EXCURSION);
        travel_to(ctx, (away.0, away.2), TRAVEL_HOP, ARRIVE_RADIUS).await;
        if ctx.target_met() {
            break;
        }
        hold_facing(ctx, away_yaw, ctx.elapsed_ms() + AWAY_MS, SHUTTLE_LEG).await;

        travel_to(ctx, site, TRAVEL_HOP, ARRIVE_RADIUS).await;
        let faced = ctx.look_until_facing(focus, 5.0, FACE_MAX_WAIT_MS).await;
        let observed = base.map(|cell| stack_height(ctx, cell, material));
        ctx.record(
            "observe",
            &format!("vis-revisit-cairn-return-{index}-{cycle}"),
            "Return to the cairn and check its real height against the height just built.",
            json!({
                "cairn": cairn,
                "cycle": cycle,
                "material": material,
                "expected_height": layers,
                "observed_height": observed,
                "matches": observed == Some(layers),
                "faced_cairn": faced,
                "away_yaw_deg": r2(away_yaw),
                "elapsed_ms": ctx.elapsed_ms(),
            }),
        );
        let home_yaw = ctx
            .position()
            .map(|p| look_angles(head_of(p), focus).0)
            .unwrap_or(0.0);
        hold_facing(ctx, home_yaw, ctx.elapsed_ms() + CAIRN_HOLD_MS, SHUTTLE_LEG).await;
        witness_peer_edits(ctx).await;

        // Finished cairns stay standing: the accumulating skyline is what makes later returns a
        // discrimination task and not just a height read.
        if layers >= MAX_LAYERS || base.is_none() {
            cairn += 1;
            layers = 0;
            base = None;
        }
        cycle += 1;
    }
}
