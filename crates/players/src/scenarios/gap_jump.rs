use super::*;

/// Gap-jump (parkour): drive the bot across the RCON-built `gap_jump` SCENERY (issues #75/#79 — the
/// server builds the course, because the bot's own chat-commands are silently dropped by vanilla).
/// Pair with `scenery: gap_jump`: the coordinator builds a line of platforms with jumpable
/// gaps anchored at the swarm's spawn and writes that spawn to `<signal>/anchor` (this scenario reads
/// it via [`Ctx::anchor`]). We just point the pathfinder at the far end of the course with
/// `allow_parkour` on; it climbs onto the platforms and parkours across, executed by the decoupled
/// controller (quantized camera, `parkour_step` take-off timing). Without the scenery the bot simply
/// walks +X over open terrain (no course = no jumps), so the scenario is safe to run either way.
pub(super) async fn run(ctx: &mut Ctx) {
    let cfg = ctx.cfg().clone();
    let spawn = loop {
        if let Some(p) = ctx.position() {
            break p;
        }
        sleep_ms(100).await;
    };
    // The scenery course marches +X from the swarm spawn (anchor). Cross it in bounded legs so
    // the event stream and capture both expose take-off, mid-course progress, and completion.
    let (ax, az) = ctx.anchor();
    let index = ctx.index();
    let bot_count = ctx.player_count().max(1);
    let finish_lane_z = if bot_count == 1 {
        az
    } else {
        az - 2.75 + 5.5 * index as f64 / (bot_count - 1) as f64
    };
    sleep_ms(1200 + (index as u64) * 250).await; // let scenery build + chunks reach azalea

    // Keep navigation targets on actual platform centres. This mirrors the capture crate's `scenery.rs` exactly:
    // count=5+seed%3, start=anchor+3, span=3, pitch=5. Players may be distributed across
    // platforms by the coordinator, so derive each bot's actual starting platform first.
    let platform_count = 5 + (ctx.engine().seed % 3) as usize;
    let platform_center = |i: usize| ax + 4.0 + i as f64 * 5.0;
    let last_platform = platform_count - 1;
    let start_platform = (((spawn.0 - platform_center(0)) / 5.0).round() as isize)
        .clamp(0, last_platform as isize) as usize;
    let platform_surface_y = spawn.1 + 0.1;
    let first_landing = (start_platform + 1).min(last_platform);
    let course_start = (platform_center(first_landing), platform_surface_y, az);
    let course_far = (platform_center(last_platform), platform_surface_y, az);
    ctx.look(course_start, cfg.look_settle_ms.max(700)).await;
    ctx.record(
        TYPE_LOOK,
        &format!("vis-gap-start-{index}"),
        "Frame the first gap and landing platform before take-off.",
        json!({"phase": "start", "allow_parkour": cfg.allow_parkour,
            "anchor": {"x": r2(ax), "z": r2(az)}, "starting_platform": start_platform,
            "first_landing": {"x": r2(course_start.0), "z": r2(course_start.2)},
            "course_finish": {"x": r2(course_far.0), "z": r2(course_far.2)}}),
    );
    sleep_ms(cfg.action_hold_ms.max(600)).await;

    let progress_platforms: Vec<usize> = ((start_platform + 1)..last_platform).collect();
    for (leg, platform) in progress_platforms.iter().copied().enumerate() {
        let target_x = platform_center(platform);
        let stage = if leg == 0 {
            "early"
        } else if platform + 1 == last_platform {
            "late"
        } else {
            "middle"
        };
        let mut arrived = false;
        let mut attempts = 0u32;
        while attempts < 2 && !ctx.target_met() {
            attempts += 1;
            arrived = ctx.move_to(target_x, az, 4_000, 1.5).await;
            if arrived {
                break;
            }
            sleep_ms(500).await;
        }
        let p = ctx.position().unwrap_or(spawn);
        let next_landing = (platform + 1).min(last_platform);
        ctx.look(
            (platform_center(next_landing), platform_surface_y, az),
            cfg.look_settle_ms.max(1000),
        )
        .await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-gap-progress-{index}-{leg}"),
            &format!("Parkour {stage} leg: frame the next gap after advancing."),
            json!({"phase": "progress", "stage": stage, "leg": leg,
                "platform": platform, "next_landing": next_landing,
                "platform_count": platform_count, "arrived": arrived, "attempts": attempts,
                "advanced": r2(p.0 - ax), "target": {"x": r2(target_x), "z": r2(az)},
                "position": {"x": r2(p.0), "y": r2(p.1), "z": r2(p.2)}}),
        );
        sleep_ms(cfg.action_hold_ms.max(450)).await;
    }

    let mut arrived = false;
    let mut finish_attempts = 0u32;
    while finish_attempts < 2 && !ctx.target_met() {
        finish_attempts += 1;
        arrived = ctx.move_to(course_far.0, az, 4_000, 1.5).await;
        if arrived {
            break;
        }
        sleep_ms(500).await;
    }
    let fp = ctx.position().unwrap_or(spawn);
    let previous_platform = last_platform.saturating_sub(1);
    let finish_preview = (platform_center(previous_platform), platform_surface_y, az);
    ctx.look(finish_preview, cfg.look_settle_ms.max(3200)).await;
    ctx.record(
        TYPE_MOVE,
        &format!("vis-gap-finish-{index}"),
        &format!(
            "Parkour finish: advanced {} blocks (+X) across the gap_jump scenery, arrived={arrived}.",
            r2(fp.0 - ax)
        ),
        json!({"phase": "finish", "arrived": arrived, "advanced": r2(fp.0 - ax),
            "allow_parkour": cfg.allow_parkour, "platform_count": platform_count,
            "finish_platform": last_platform, "previous_platform": previous_platform,
            "anchor": {"x": r2(ax), "z": r2(az)},
            "final": {"x": r2(fp.0), "y": r2(fp.1), "z": r2(fp.2)}}),
    );

    // The scenery's widened final platform lets each bot step more than the navigation radius away
    // from the shared centre before looking back, preventing a teammate from filling the camera.
    let overlook = (course_far.0 + 4.5, finish_lane_z);
    let overlook_arrived = ctx
        .move_to(overlook.0, overlook.1, 4_000, cfg.goal_radius)
        .await;
    let mut peers_ready = bot_count <= 1;
    for _ in 0..100 {
        peers_ready = (0..bot_count).filter(|&j| j != index).all(|j| {
            ctx.peer_position(j)
                .is_some_and(|p| p.0 >= course_far.0 + 3.0)
        });
        if peers_ready {
            break;
        }
        sleep_ms(100).await;
    }
    let review = (
        platform_center(start_platform),
        platform_surface_y - 2.0,
        az,
    );
    ctx.look(review, cfg.look_settle_ms.max(3200)).await;
    ctx.record(
        TYPE_MOVE,
        &format!("vis-gap-overlook-{index}"),
        "Settle on the final platform with the completed parkour course in view.",
        json!({"phase": "observe", "arrived": overlook_arrived,
            "peers_ready": peers_ready, "finish_platform": last_platform,
            "target": {"x": r2(overlook.0), "z": r2(overlook.1)}}),
    );
    ctx.record(
        TYPE_LOOK,
        &format!("vis-gap-review-{index}"),
        "Look back over the gap-jump course from the finish overlook.",
        json!({"phase": "review", "peers_ready": peers_ready,
            "target": {"x": r2(review.0), "z": r2(review.2)}}),
    );
    let mut returning = true;
    let mut pass = 1u32;
    while !ctx.target_met() {
        let target_platform = if returning {
            start_platform
        } else {
            last_platform
        };
        let target_x = if target_platform == last_platform {
            overlook.0
        } else {
            platform_center(target_platform)
        };
        let target_z = if target_platform == last_platform {
            finish_lane_z
        } else {
            az
        };
        let arrived = ctx.move_to(target_x, target_z, 18_000, 1.5).await;
        let view_platform = if returning {
            (target_platform + 1).min(last_platform)
        } else {
            target_platform.saturating_sub(1)
        };
        ctx.look(
            (platform_center(view_platform), platform_surface_y, az),
            cfg.look_settle_ms.max(3200),
        )
        .await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-gap-shuttle-{index}-{pass}"),
            if returning {
                "Return across the gap-jump course toward the starting platform."
            } else {
                "Repeat the outward gap-jump crossing toward the finish platform."
            },
            json!({"phase": "shuttle", "pass": pass,
                "direction": if returning { "return" } else { "outward" },
                "target_platform": target_platform, "arrived": arrived}),
        );
        returning = !returning;
        pass += 1;
        sleep_ms(cfg.action_hold_ms.max(450)).await;
    }
}
