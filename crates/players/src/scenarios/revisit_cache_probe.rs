//! CATALOG: family=many_player_spatial min_players=1 single_process=true world_edits=true scenery=none far_players=false
//!
//! Hide one small thing, walk away, come back and FIND it — with the older hiding places still in
//! the world, so the return is a discrimination and not just a presence check.
//!
//! The other revisit scripts verify a tall landmark, which a model can re-acquire from its silhouette
//! against the sky. A single ground-level block cannot be re-acquired that way: it is a few pixels of
//! the wrong texture on a grass floor, only visible with the camera pitched down and only from close
//! range. That makes the memory under test a POSITIONAL one — where exactly did I leave it — rather
//! than a recognition one, which is the harder half of the revisit problem's revisit question.
//!
//! Every cycle caches a new block in a fresh spot and in the next material of the rotation, and the
//! previous caches are left in place. So the return has to distinguish this cycle's cache from the
//! decoys the bot itself planted, and the script records both probes: whether the current cache was
//! found, and whether the previous one is still standing where it was left. Two positive probes with
//! two different materials at two different spots is a much stronger revisit signal than one.
//!
//! The trajectory is an out-and-back per cache, so path length is twice the excursion while net
//! displacement is ~0.
use super::*;

/// Cache materials, rotated per cycle so consecutive caches never share a texture — that is what
/// makes "this cache" and "the previous cache" separable both in the pixels and in the probe.
const MATERIALS: [&str; 4] = [
    "minecraft:bricks",
    "minecraft:oak_planks",
    "minecraft:stone_bricks",
    "minecraft:cobblestone",
];

/// Cache sites sit on a ring around the anchor. Small radius: the point is that the cache is hard to
/// SEE, not hard to reach, so keeping every site in one compact neighbourhood also keeps peers in
/// frame. 59 deg between consecutive sites is 6.9 blocks apart at this radius — far enough that a
/// probe of one site cannot accidentally find its neighbour, since the probe window is +/-2 blocks.
const SITE_RADIUS: f64 = 7.0;
const SITE_STEP_DEG: f64 = 59.0;

/// Per-bot rotation of the ring, so two bots never cache in the same spots.
const FAN_DEG: f64 = 101.0;

/// How far the bot goes while the cache is unattended, and for how long. 14 blocks is past the range
/// at which a single ground block is resolvable at all, so the return genuinely re-acquires it.
const EXCURSION: f64 = 14.0;
const AWAY_MS: i64 = 10_000;

/// Pitch-down look tolerance. Wider than the 5 deg used for tall landmarks because the target is one
/// block a couple of metres away, where a small position drift is a large angular error.
const CACHE_FACE_TOL_DEG: f64 = 10.0;

const SHUTTLE_LEG: f64 = 4.0;
const CACHE_HOLD_MS: i64 = 3_500;
const FACE_MAX_WAIT_MS: u64 = 5_000;
const ARRIVE_RADIUS: f64 = 2.0;

/// Standoff the return aims for instead of the cache's own cell. Must exceed [`ARRIVE_RADIUS`] so
/// that even the nearest legal arrival leaves the bot beside the cache rather than standing on it:
/// nav routes over a 1-high block happily, and a `look_until_facing` target directly underfoot has a
/// degenerate yaw that never converges, which would log `faced: false` for a return that did happen.
/// `faced` is downstream evidence for separating hallucination from real change, so a known false
/// negative in it costs more than three blocks of standoff.
const CACHE_STANDOFF: f64 = 3.0;

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let anchor = ctx.anchor();
    let start = ctx.position().unwrap_or((anchor.0, 64.0, anchor.1));
    let feet_y = start.1.round() as i64;

    ctx.record(
        "observe",
        &format!("vis-revisit-cache-setup-{index}"),
        "Cache one ground block per cycle, leave, and come back to find it among earlier caches.",
        json!({
            "global_index": global,
            "role": "cache_revisitor",
            "site_radius": SITE_RADIUS,
            "fan_deg": global as f64 * FAN_DEG,
            "excursion": EXCURSION,
        }),
    );

    // The previous cycle's cache, kept so the return can prove it is telling two caches apart rather
    // than reporting the first block of the right colour it stumbles over.
    let mut previous: Option<(BlockPos, &str)> = None;
    let mut cycle = 0usize;
    while !ctx.target_met() {
        let material = MATERIALS[cycle % MATERIALS.len()];
        let bearing = (global as f64 * FAN_DEG + cycle as f64 * SITE_STEP_DEG).to_radians();
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

        // Height 1: a cache is one block, deliberately. `marker_base_near` is what makes that
        // survivable on real terrain — it walks a small ring for a cell with solid support and air
        // above, and excludes cells inside the placer's own hitbox.
        let cached = marker_base_near(
            ctx,
            (site.0.round() as i64, feet_y, site.1.round() as i64),
            1,
        );
        let mut placed = false;
        if let Some(cell) = cached {
            let aim = (
                cell.x as f64 + 0.5,
                cell.y as f64 + 0.5,
                cell.z as f64 + 0.5,
            );
            ctx.look_until_facing(aim, CACHE_FACE_TOL_DEG, FACE_MAX_WAIT_MS)
                .await;
            placed = ctx.equip(material).await && ctx.place_block(cell, material).await;
        }
        ctx.record(
            TYPE_PLACE,
            &format!("vis-revisit-cache-hide-{index}-{cycle}"),
            "Cache a single ground-level block and observe it once before leaving.",
            json!({
                "cycle": cycle,
                "material": material,
                "cell": cached.map(|c| json!({"x": c.x, "y": c.y, "z": c.z})),
                "placed": placed,
                "elapsed_ms": ctx.elapsed_ms(),
            }),
        );
        if ctx.target_met() {
            break;
        }

        // Walk out and hold a fixed bearing pointing away. A ground block at 14 blocks is invisible
        // regardless of the camera, but facing away makes the departure unambiguous in the footage
        // and keeps the hold from tracking anything (a tracked hold fails the camera/travel gate).
        let away_yaw = bearing.to_degrees() + 180.0 + cycle as f64 * 37.0;
        let away = point_at_yaw((site.0, 0.0, site.1), away_yaw, EXCURSION);
        let out = travel_to(ctx, (away.0, away.2), TRAVEL_HOP, ARRIVE_RADIUS).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-revisit-cache-leave-{index}-{cycle}"),
            "Leave the cache unattended for a fixed interval.",
            json!({
                "cycle": cycle,
                "away_yaw_deg": r2(away_yaw),
                "away_ms": AWAY_MS,
                "arrival_distance": r2(out),
            }),
        );
        hold_facing(ctx, away_yaw, ctx.elapsed_ms() + AWAY_MS, SHUTTLE_LEG).await;
        if ctx.target_met() {
            break;
        }

        // Come back down the corridor the excursion left by, stopping short of the cache itself.
        let approach = cached
            .map(|c| {
                let (dx, dz) = (away.0 - c.x as f64 - 0.5, away.2 - c.z as f64 - 0.5);
                let span = dx.hypot(dz).max(0.01);
                (
                    c.x as f64 + 0.5 + dx / span * CACHE_STANDOFF,
                    c.z as f64 + 0.5 + dz / span * CACHE_STANDOFF,
                )
            })
            .unwrap_or(site);
        travel_to(ctx, approach, TRAVEL_HOP, ARRIVE_RADIUS).await;
        // Search the neighbourhood rather than the exact cell, and record the OFFSET at which it was
        // found: that offset is the quantitative form of "it was where I left it", and it separates a
        // bot that was shoved off its mark from a world that lost the cache.
        let found =
            cached.and_then(|c| area_find_block(ctx, (c.x, c.z), (c.y - 2, c.y + 2), material));
        let offset = cached.zip(found).map(|(want, got)| {
            (got.x - want.x).abs() + (got.y - want.y).abs() + (got.z - want.z).abs()
        });
        let focus = found
            .or(cached)
            .map(|c| (c.x as f64 + 0.5, c.y as f64 + 0.5, c.z as f64 + 0.5))
            .unwrap_or((site.0, start.1, site.1));
        let faced = ctx
            .look_until_facing(focus, CACHE_FACE_TOL_DEG, FACE_MAX_WAIT_MS)
            .await;
        // The decoy probe: is the PREVIOUS cache, in its own material, still where it was left? A
        // return that reports the current cache found and the previous one intact has demonstrated
        // discrimination, not just detection.
        let decoy = previous.and_then(|(cell, mat)| {
            area_find_block(ctx, (cell.x, cell.z), (cell.y - 2, cell.y + 2), mat)
                .map(|got| json!({"material": mat, "cell": {"x": got.x, "y": got.y, "z": got.z}}))
        });
        ctx.record(
            "observe",
            &format!("vis-revisit-cache-find-{index}-{cycle}"),
            "Return to the cache site and confirm the cache, plus the previous cycle's decoy.",
            json!({
                "cycle": cycle,
                "expected_material": material,
                "expected_cell": cached.map(|c| json!({"x": c.x, "y": c.y, "z": c.z})),
                "found_cell": found.map(|c| json!({"x": c.x, "y": c.y, "z": c.z})),
                "found": found.is_some(),
                "search_offset_blocks": offset,
                "previous_cache_intact": decoy,
                "faced_cache": faced,
                "standoff_blocks": CACHE_STANDOFF,
                "elapsed_ms": ctx.elapsed_ms(),
            }),
        );
        let cache_yaw = ctx
            .position()
            .map(|p| look_angles(head_of(p), focus).0)
            .unwrap_or(0.0);
        hold_facing(
            ctx,
            cache_yaw,
            ctx.elapsed_ms() + CACHE_HOLD_MS,
            SHUTTLE_LEG,
        )
        .await;
        witness_peer_edits(ctx).await;
        if placed {
            previous = cached.map(|cell| (cell, material));
        }
        cycle += 1;
    }
}
