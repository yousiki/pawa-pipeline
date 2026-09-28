use super::*;

/// How far a subgroup's region swings between rounds. Every bot on a side has to agree on where
/// that side is going and `rng_next` is a per-bot stream, so the per-round variation is derived
/// from the seeded waypoint and the round index rather than drawn locally. The golden angle means
/// no two rounds of an episode reuse a bearing.
const ROUND_TURN_RAD: f64 = 2.399_963;

fn nearest_peer_distance(ctx: &Ctx) -> Option<f64> {
    let own = ctx.position()?;
    (0..ctx.player_count())
        .filter(|&j| j != ctx.index())
        .filter_map(|j| ctx.peer_position(j))
        .map(|p| (p.0 - own.0).hypot(p.2 - own.2))
        .min_by(f64::total_cmp)
}

fn same_side_focus(ctx: &Ctx, side: usize, fallback: (f64, f64)) -> (f64, f64, f64) {
    let own = ctx.position().unwrap_or((fallback.0, 64.0, fallback.1));
    let own_global = global_index(ctx);
    let mut nearest: Option<((f64, f64, f64), f64)> = None;
    for j in 0..ctx.player_count() {
        let peer_global = ctx.cfg().index_offset + j;
        if peer_global == own_global || peer_global % 2 != side {
            continue;
        }
        let Some(position) = ctx.peer_position(j) else {
            continue;
        };
        let distance = (position.0 - own.0).hypot(position.2 - own.2);
        if nearest
            .as_ref()
            .map(|entry| distance < entry.1)
            .unwrap_or(true)
        {
            nearest = Some((position, distance));
        }
    }
    nearest
        .map(|entry| head_of(entry.0))
        .unwrap_or((fallback.0, own.1, fallback.1))
}

fn cross_side_visible(ctx: &Ctx, side: usize) -> bool {
    (0..ctx.player_count()).any(|j| {
        j != ctx.index()
            && (ctx.cfg().index_offset + j) % 2 != side
            && ctx.peer_position(j).is_some()
    })
}

/// One beat of waiting on a phase deadline: read the scene, and most beats also shift footing near
/// the station rather than standing on it. The three waiting phases are over half of every cycle,
/// and spent frozen they put this scenario's idle share on the QC limit (0.80 measured against a
/// 0.75 ceiling) — a bot with nothing to do should still be a body, not a tripod. The step stays
/// inside `DRIFT` of the station so the subgroup keeps its shape and its peer gaps.
async fn wait_beat(ctx: &mut Ctx, station: (f64, f64), focus: (f64, f64, f64)) {
    /// Kept under half the smallest chord between adjacent stations: two neighbours drifting toward
    /// each other still clear the peer gap.
    const DRIFT: f64 = 3.5;
    ctx.look_default(focus).await;
    if ctx.rng_next() < 0.6 {
        let bearing = ctx.rng_next() * std::f64::consts::TAU;
        // `move_to` counts anything inside 1.5 blocks as arrived, so a shorter step is no step.
        let reach = 2.0 + ctx.rng_next() * (DRIFT - 2.0);
        ctx.move_to(
            station.0 + bearing.cos() * reach,
            station.1 + bearing.sin() * reach,
            4_000,
            1.5,
        )
        .await;
    }
    ctx.pause().await;
}

pub(super) async fn run(ctx: &mut Ctx) {
    let cfg = ctx.cfg().clone();
    let index = ctx.index();
    let global = global_index(ctx);
    let side = global % 2;
    let anchor = ctx.anchor();
    let (tag_distance, native_round_ms, offset) = {
        let shared = &ctx.engine().shared;
        (
            shared.tag_distance,
            shared.native_round_ms,
            shared
                .native_waypoints
                .get(side)
                .copied()
                .unwrap_or((0.0, 0.0)),
        )
    };
    let start_distance_to_anchor = ctx
        .position()
        .map(|p| (anchor.0 - p.0).hypot(anchor.1 - p.2))
        .unwrap_or(f64::INFINITY);
    ctx.record(
        "observe",
        &format!("vis-split_and_reunite-start-{index}"),
        "Observe the initial layout before the swarm splits into two subgroups.",
        json!({
            "global_index": global,
            "role": format!("subgroup_{side}"),
            "shard_index": cfg.shard_index,
            "shard_count": cfg.shard_count,
            "far_players": cfg.far_players,
            "start_distance_to_anchor": start_distance_to_anchor,
            "single_process_violated": cfg.shard_count > 1,
        }),
    );

    // Reunion needs the largest phase, not the smallest: 16 bots converging on one anchor spend most
    // of their time in the final congested approach. Depart by 35% of a cycle, dwell to 50%, leaving
    // half of it to regroup. Measured stragglers previously overran the episode entirely.
    //
    // The cycle is a third of the episode rather than the whole of it. Splitting once and reuniting
    // once left the swarm standing at the anchor for the remaining minute; a cycle this size fits
    // two or three complete separations into the same episode.
    let cycle_ms = if cfg.episode_duration_ms > 0 {
        (cfg.episode_duration_ms / 3).max(24_000)
    } else {
        (native_round_ms.round() as i64 * 2).max(24_000)
    };

    let bot_count = ctx.player_count().max(1);
    let base_bearing = offset.1.atan2(offset.0);
    let base_reach = offset.0.hypot(offset.1).max(tag_distance * 3.0);
    // Give each bot its own slot on a crowd-sized ring — the same radial-station convention the
    // build scenarios use — rather than letting 8-16 bots target one point and fail the final
    // approach for whoever loses. `ring_radius` sizes the ring so adjacent slots clear
    // `MIN_PEER_GAP`; sqrt-scaling alone left them contending.
    let ring = tag_distance.max(ring_radius(bot_count));
    let slot_angle = (global as f64 / bot_count as f64) * std::f64::consts::TAU;
    let slot = (
        anchor.0 + slot_angle.cos() * ring,
        anchor.1 + slot_angle.sin() * ring,
    );
    let reunion_radius = ring + tag_distance;

    let mut round = 0usize;
    while !ctx.target_met() {
        let cycle_start = ctx.elapsed_ms();
        let depart_deadline = cycle_start + cycle_ms * 35 / 100;
        let dwell_deadline = cycle_start + cycle_ms / 2;
        // Each round swings the side's region to a new bearing and a different depth, so the split
        // is not the same two spots repeated. Both terms depend only on the seed and the round, so
        // every bot on the side computes the same region without exchanging anything.
        let bearing = base_bearing + round as f64 * ROUND_TURN_RAD;
        let reach = base_reach * (0.7 + 0.3 * (round % 3) as f64);
        let region = (
            anchor.0 + bearing.cos() * reach,
            anchor.1 + bearing.sin() * reach,
        );
        // The region used to be a single point every bot on the side walked onto. The reunion ring
        // serves out here too: only half the swarm is there, on alternating slot angles, so a
        // full-swarm ring is already more room than the subgroup strictly needs.
        let post = (
            region.0 + slot_angle.cos() * ring,
            region.1 + slot_angle.sin() * ring,
        );

        let depart_distance = travel_to(ctx, post, TRAVEL_HOP, cfg.goal_radius.max(2.0)).await;
        ctx.record(
            "observe",
            &format!("vis-split-depart-{index}-r{round}"),
            "Reach the assigned subgroup region.",
            json!({
                "side": side,
                "round": round,
                "region": {"x": r2(region.0), "z": r2(region.1)},
                "post": {"x": r2(post.0), "z": r2(post.1)},
                "final_distance": depart_distance,
            }),
        );

        while !ctx.target_met() && ctx.elapsed_ms() < depart_deadline {
            let focus = same_side_focus(ctx, side, region);
            wait_beat(ctx, post, focus).await;
        }

        let mut dwell = 0usize;
        while !ctx.target_met() && ctx.elapsed_ms() < dwell_deadline {
            let focus = same_side_focus(ctx, side, region);
            wait_beat(ctx, post, focus).await;
            glance_at_nearest_peer(ctx, None).await;
            ctx.record(
                "observe",
                &format!("vis-split-dwell-{index}-r{round}-{dwell}"),
                "Dwell with the local subgroup before reunion.",
                json!({
                    "side": side,
                    "round": round,
                    "nearest_peer_distance": nearest_peer_distance(ctx),
                    "cross_side_visible": cross_side_visible(ctx, side),
                }),
            );
            dwell += 1;
        }

        let slot_distance = travel_to(ctx, slot, TRAVEL_HOP, cfg.goal_radius.max(2.0)).await;
        let final_distance = ctx
            .position()
            .map(|p| (anchor.0 - p.0).hypot(anchor.1 - p.2))
            .unwrap_or(f64::INFINITY);
        ctx.record(
            "observe",
            &format!("vis-reunite-{index}-r{round}"),
            "Return from the separated subgroup region to the shared anchor.",
            json!({
                "round": round,
                "final_distance": final_distance,
                "reunited": final_distance <= reunion_radius,
                "reunion_radius": reunion_radius,
                "slot": {"x": r2(slot.0), "z": r2(slot.1)},
                "slot_distance": slot_distance,
                "ring": ring,
                "elapsed_ms": ctx.elapsed_ms(),
            }),
        );

        // Hold the regrouped formation, then split again. The reunion ends a cycle, not the episode.
        let regroup_until = ctx.elapsed_ms() + (cycle_ms / 8).max(2_000);
        while !ctx.target_met() && ctx.elapsed_ms() < regroup_until {
            let focus = ctx.group_center().unwrap_or_else(|| {
                let own_y = ctx.position().map(|p| p.1).unwrap_or(64.0);
                (anchor.0, own_y, anchor.1)
            });
            wait_beat(ctx, slot, focus).await;
        }
        round += 1;
    }
}
