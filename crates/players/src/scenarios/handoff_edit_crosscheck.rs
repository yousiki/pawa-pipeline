//! CATALOG: family=construction min_players=2 single_process=true world_edits=true scenery=none far_players=false
//!
//! An editor changes one shared marker at the handoff endpoint; a verifier visits two different,
//! already observed stations to identify the new material, then the shared phase swaps their roles.
//! This is the revisit problem's causal multi-view face: the partner is in the opening view before the edit, and
//! the return observations bind one player's action to the same cell from two camera positions.
//!
//! The editor approaches and retreats once; the verifier makes two unequal cross-check spokes, so the
//! N=2 streams are not near-clones. `move_to` owns the camera only while travelling; each marker and
//! peer gaze is a stopped `look_until_facing`. A marker may fail on natural terrain or be shifted by
//! `marker_base_near`; those facts are recorded, and both roles continue their route rather than
//! assuming a placement or digging anything outside the small kit set.

use super::*;

const MATERIALS: [&str; 2] = ["minecraft:oak_planks", "minecraft:bricks"];
const KIT_IDS: [&str; 6] = [
    "stone_bricks",
    "oak_planks",
    "oak_log",
    "cobblestone",
    "bricks",
    "stone",
];
const HOME_HALF: f64 = 6.0;
const APPROACH: f64 = 2.2;
const VERIFY_OFFSETS: [f64; 2] = [3.5, 6.5];
const ROLE_MS: f64 = 14_000.0;
const FACE_TOL_DEG: f64 = 8.0;

fn shared_axis(ctx: &Ctx) -> (f64, f64) {
    let v = ctx
        .engine()
        .shared
        .native_waypoints
        .first()
        .copied()
        .unwrap_or((1.0, 0.0));
    let d = v.0.hypot(v.1).max(0.001);
    // Pair roles remain slot 0/1, but each pair gets its own rotated axis at N=4+ so pairs do
    // not converge on the same homes, screens or workstations. N=2 keeps the original axis.
    let angle = ctx.index().saturating_div(2) as f64 * 0.83;
    let (sin, cos) = angle.sin_cos();
    (v.0 / d * cos - v.1 / d * sin, v.0 / d * sin + v.1 / d * cos)
}

fn find_subject(ctx: &Ctx, anchor: (f64, f64), feet_y: i32) -> Option<(BlockPos, String)> {
    MATERIALS.iter().find_map(|material| {
        area_find_block(
            ctx,
            (anchor.0.round() as i32, anchor.1.round() as i32),
            (feet_y - 3, feet_y + 5),
            material,
        )
        .and_then(|cell| ctx.block_id_at(cell).map(|id| (cell, id)))
    })
}

async fn face_peer(ctx: &Ctx, peer: Option<usize>, fallback: (f64, f64, f64)) -> bool {
    let target = peer
        .and_then(|j| ctx.peer_position(j))
        .map(head_of)
        .unwrap_or(fallback);
    ctx.look_until_facing(target, FACE_TOL_DEG, 5_000).await
}

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let slot = global % 2;
    let peer = Some(index ^ 1).filter(|&j| j < ctx.player_count());
    let anchor = ctx.anchor();
    let axis = shared_axis(ctx);
    let perp = (-axis.1, axis.0);
    let side = if slot == 0 { 1.0 } else { -1.0 };
    let home = (
        anchor.0 + axis.0 * side * HOME_HALF,
        anchor.1 + axis.1 * side * HOME_HALF,
    );

    let opening = face_peer(ctx, peer, (anchor.0, 64.0, anchor.1)).await;
    travel_to(ctx, home, TRAVEL_HOP, 1.2).await;
    let start = ctx.position().unwrap_or((home.0, 64.0, home.1));
    let focus = (anchor.0, start.1 + 1.2, anchor.1);
    ctx.record(
        "observe",
        &format!("vis-handoff-edit-setup-{index}"),
        "Open on the partner before the editor and verifier begin their shared-cell handoff.",
        json!({
            "global_index": global,
            "slot": slot,
            "peer": peer,
            "opening_confirmed": opening,
            "home": {"x": r2(home.0), "z": r2(home.1)},
        }),
    );

    let mut beat = 0u64;
    while !ctx.target_met() {
        let phase = ctx.engine().shared.phase_at(ROLE_MS);
        let editor = (phase + global as i64).rem_euclid(2) == 0;
        if editor {
            let approach = (
                anchor.0 + axis.0 * side * APPROACH,
                anchor.1 + axis.1 * side * APPROACH,
            );
            let approached = ctx.move_to(approach.0, approach.1, 7_000, 1.2).await;
            let feet_y = ctx.position().map(|p| p.1).unwrap_or(start.1).round() as i32;
            let old = find_subject(ctx, anchor, feet_y);
            let mut dug = false;
            let mut placed = false;
            // The editor phase advances by two for the same bot (the other parity is the verifier),
            // so divide by two before indexing. Otherwise `phase + global` is always even here and
            // every replacement silently selected oak planks forever.
            let edit_index = phase.div_euclid(2).rem_euclid(MATERIALS.len() as i64) as usize;
            let mut material = MATERIALS[(global + edit_index) % MATERIALS.len()];
            if old.as_ref().is_some_and(|(_, id)| {
                id == material.strip_prefix("minecraft:").unwrap_or(material)
            }) {
                material = MATERIALS[1 - ((global + edit_index) % MATERIALS.len())];
            }
            if let Some((cell, id)) = old {
                if KIT_IDS.contains(&id.as_str()) {
                    ctx.equip("diamond_pickaxe").await;
                    dug = ctx.dig(cell).await;
                    if dug && ctx.equip(material).await {
                        placed = ctx.place_block(cell, material).await;
                    }
                }
                ctx.record(
                    TYPE_PLACE,
                    &format!("vis-handoff-edit-change-{index}-{beat}"),
                    "Replace the shared marker and leave a material identity for the verifier.",
                    json!({
                        "phase": phase,
                        "approached": approached,
                        "old_cell": {"x": cell.x, "y": cell.y, "z": cell.z},
                        "old_block": id,
                        "material": material,
                        "dug": dug,
                        "placed": placed,
                    }),
                );
            } else {
                let desired = (
                    anchor.0.round() as i64,
                    feet_y as i64,
                    anchor.1.round() as i64,
                );
                let cell = marker_base_near(ctx, desired, 1);
                let equipped = ctx.equip(material).await;
                placed = match cell {
                    Some(cell) if equipped => ctx.place_block(cell, material).await,
                    _ => false,
                };
                ctx.record(
                    TYPE_PLACE,
                    &format!("vis-handoff-edit-seed-{index}-{beat}"),
                    "Seed the shared marker when no earlier handoff material is available.",
                    json!({
                        "phase": phase,
                        "approached": approached,
                        "material": material,
                        "equipped": equipped,
                        "placed": placed,
                        "cell": cell.map(|c| json!({"x": c.x, "y": c.y, "z": c.z})),
                    }),
                );
            }
            let peer_confirmed = face_peer(ctx, peer, focus).await;
            let returned = ctx.move_to(home.0, home.1, 7_000, 1.0).await;
            ctx.record(
                TYPE_MOVE,
                &format!("vis-handoff-edit-return-{index}-{beat}"),
                "Return from the edit endpoint and hand the changed cell to the verifier.",
                json!({"phase": phase, "peer_confirmed": peer_confirmed, "home_arrived": returned}),
            );
        } else {
            let mut checks = Vec::new();
            for (check, offset) in VERIFY_OFFSETS.iter().copied().enumerate() {
                if ctx.target_met() || ctx.engine().shared.phase_at(ROLE_MS) != phase {
                    break;
                }
                let station = (
                    home.0 + perp.0 * side * offset,
                    home.1 + perp.1 * side * offset,
                );
                let arrived = ctx.move_to(station.0, station.1, 7_000, 1.0).await;
                let faced = ctx.look_until_facing(focus, FACE_TOL_DEG, 5_000).await;
                let feet_y = ctx.position().map(|p| p.1).unwrap_or(start.1).round() as i32;
                let observed = find_subject(ctx, anchor, feet_y);
                let peer_faced = face_peer(ctx, peer, focus).await;
                checks.push(json!({
                    "check": check,
                    "offset": offset,
                    "arrived": arrived,
                    "marker_faced": faced,
                    "observed": observed.map(|(cell, id)| json!({"x": cell.x, "y": cell.y, "z": cell.z, "block": id})),
                    "peer_faced": peer_faced,
                }));
                ctx.move_to(home.0, home.1, 7_000, 1.0).await;
            }
            ctx.record(
                TYPE_LOOK,
                &format!("vis-handoff-edit-verify-{index}-{beat}"),
                "Verify the editor's marker from two unequal, closed cross-check spokes.",
                json!({"phase": phase, "checks": checks}),
            );
        }
        beat += 1;
    }
}
