use super::*;

const LANE_NAMES: [&str; 2] = ["east_lane", "west_lane"];

/// Blocks between two bots sharing a lane. Wide enough that neither ever stands in the other's
/// head box while both walk the same bearing.
const LANE_PITCH: f64 = 5.0;

fn rand_range(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

fn nearest_peer(ctx: &Ctx) -> Option<usize> {
    let own = ctx.position()?;
    (0..ctx.player_count())
        .filter(|&j| j != ctx.index())
        .filter_map(|j| {
            ctx.peer_position(j)
                .map(|p| (j, (p.0 - own.0).powi(2) + (p.2 - own.2).powi(2)))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(j, _)| j)
}

async fn sweep(ctx: &Ctx, span_deg: f64, pitch_deg: f64) {
    let rate = ctx.cfg().yaw_rate_deg_s.max(1.0);
    for (yaw, pitch) in [
        (span_deg * 0.5, pitch_deg),
        (-span_deg, -pitch_deg),
        (span_deg * 0.5, 0.0),
    ] {
        if ctx.target_met() {
            return;
        }
        let settle = (yaw.abs() / rate * 1000.0) as u64 + 700;
        ctx.drag_view(yaw, pitch, settle).await;
    }
}

/// Stand still for `hold_ms`, but only while the accumulated dead air is still under a tenth of the
/// episode so far. Idle frames are the largest measured corpus defect, and a scenario built around
/// standing at a checkpoint is exactly the one that would drift over the line.
async fn budgeted_pause(ctx: &Ctx, spent_ms: &mut u64, hold_ms: u64) -> bool {
    let hold = hold_ms.clamp(200, 3_000);
    let elapsed = ctx.elapsed_ms().max(1) as u64;
    if *spent_ms + hold > elapsed / 10 {
        return false;
    }
    *spent_ms += hold;
    ctx.still(hold).await;
    true
}

/// Two mirrored lanes each walk out to a single distant checkpoint, stand briefly, sweep the camera
/// once, then walk the whole way back — one isolated movement decision per bot with nothing else
/// happening around it.
///
/// The action vocabulary stays deliberately thin, because sparseness is the point of the episode.
/// What varies is every parameter: the outbound bearing wanders inside the lane's sector, the
/// checkpoint distance, the sweep span and the return point are all per-round draws, and the lane's
/// bots are spread along a perpendicular pitch so a lane is a lane and not a queue.
pub(crate) async fn run(ctx: &mut Ctx) {
    let index = ctx.index();
    let bot_count = ctx.player_count().max(1);
    let anchor = ctx.anchor();
    let lane = index % 2;
    // Mirrored sector centres: the east lane leaves toward +X, the west lane toward -X.
    let lane_bearing = if lane == 0 { 90.0 } else { 270.0 };
    // Rank inside the lane, centred, so the lane's bots start abreast rather than stacked.
    let in_lane = (0..bot_count).filter(|j| j % 2 == lane).count().max(1);
    let rank = (index / 2) as f64 - (in_lane as f64 - 1.0) / 2.0;
    let lateral = rank * LANE_PITCH;
    let move_timeout = ctx.cfg().move_timeout_ms.min(24_000);

    ctx.record(
        TYPE_LOOK,
        &format!("vis-checkpoint-{index}"),
        "Sparse checkpoint return: walk out to one distant point, stand, look once, walk back.",
        json!({
            "role": LANE_NAMES[lane],
            "lane_bearing_deg": lane_bearing,
            "lane_offset": r2(lateral),
        }),
    );

    let mut pause_ms = 0u64;
    let mut trip = 0u32;
    while !ctx.target_met() {
        // One checkpoint, drawn fresh inside this lane's sector.
        let bearing = (lane_bearing + rand_range(ctx, -28.0, 28.0)).to_radians();
        let distance = rand_range(ctx, 13.0, 22.0);
        let (fx, fz) = (-bearing.sin(), bearing.cos());
        let checkpoint = (
            anchor.0 + fx * distance - fz * lateral,
            anchor.1 + fz * distance + fx * lateral,
        );
        let arrived = ctx
            .move_to(checkpoint.0, checkpoint.1, move_timeout, 2.5)
            .await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-checkpoint-out-{index}-{trip}"),
            &format!("Walk the {} out to checkpoint {trip}.", LANE_NAMES[lane]),
            json!({
                "role": LANE_NAMES[lane], "trip": trip, "phase": "outbound",
                "bearing_deg": r2(bearing.to_degrees()), "requested_distance": r2(distance),
                "checkpoint": {"x": r2(checkpoint.0), "z": r2(checkpoint.1)},
                "arrived": arrived,
            }),
        );
        if ctx.target_met() {
            break;
        }

        let want_pause = rand_range(ctx, 1_200.0, 2_600.0) as u64;
        let paused = budgeted_pause(ctx, &mut pause_ms, want_pause).await;
        let span = rand_range(ctx, 60.0, 170.0);
        let pitch = rand_range(ctx, -15.0, 15.0);
        sweep(ctx, span, pitch).await;
        ctx.record(
            TYPE_LOOK,
            &format!("vis-checkpoint-look-{index}-{trip}"),
            "Stand at the checkpoint and sweep the camera once before turning back.",
            json!({"trip": trip, "paused_ms": if paused { want_pause } else { 0 },
                "pause_budget_ms": pause_ms, "sweep_deg": r2(span), "pitch_deg": r2(pitch)}),
        );
        if ctx.target_met() {
            break;
        }

        // Walk the whole way back to a point near where the group actually is, not to a literal.
        let home = ctx.group_center().map(|c| (c.0, c.2)).unwrap_or(anchor);
        let home_dir = ctx.rng_next() * std::f64::consts::TAU;
        let home_spread = rand_range(ctx, 1.5, 4.5);
        let goal = (
            home.0 + home_dir.cos() * home_spread,
            home.1 + home_dir.sin() * home_spread,
        );
        let back = ctx.move_to(goal.0, goal.1, move_timeout, 3.0).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-checkpoint-back-{index}-{trip}"),
            &format!("Walk the {} the whole way back.", LANE_NAMES[lane]),
            json!({
                "role": LANE_NAMES[lane], "trip": trip, "phase": "return",
                "goal": {"x": r2(goal.0), "z": r2(goal.1)}, "arrived": back,
            }),
        );

        if let Some(peer) = nearest_peer(ctx) {
            let settle = rand_range(ctx, 900.0, 2_400.0) as u64;
            look_at_peer(ctx, peer, settle).await;
            ctx.record(
                TYPE_LOOK,
                &format!("vis-checkpoint-peer-{index}-{trip}"),
                &format!("Glance at bot {peer} on the way in."),
                json!({"peer": peer, "settle_ms": settle}),
            );
        }
        trip += 1;
    }
}
