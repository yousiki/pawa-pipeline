use super::*;

/// The three vertical motifs a pass may raise. Every one moves the builder's own altitude while it
/// works — climbing a ramp, riding a pillar, stepping onto a perch — so the displacement QC reads
/// genuine vertical motion, not a builder circling a flat footprint.
const MOTIFS: [&str; 3] = ["ramp", "pillar", "perch"];

/// Materials drawn per pass so neighbouring yards read as different hands.
const MATERIALS: [&str; 4] = [
    "minecraft:stone_bricks",
    "minecraft:oak_planks",
    "minecraft:cobblestone",
    "minecraft:bricks",
];

/// Sideways wander per pass absorbed by the widened station ring.
const SITE_JITTER: f64 = 2.0;

/// Cap on the station radius so every yard — plus a tangential retreat — stays inside the 41x41
/// `build_arena` (radius 20 around the anchor).
const MAX_SITE_RADIUS: f64 = 15.0;

/// Draw in `[lo, hi)` from the per-bot stream, so a number this scenario could have hard-coded
/// varies per bot and per pass and still replays from the seed.
fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

/// Triangle camera sweep of `deg` total: out to one edge, across to the other, back to centre.
async fn sweep(ctx: &Ctx, deg: f64) {
    let settle = ((deg.abs() / ctx.cfg().yaw_rate_deg_s.max(1.0) * 1000.0) as u64 + 600).min(4_000);
    for leg in [deg * 0.5, -deg, deg * 0.5] {
        ctx.drag_view(leg, 0.0, settle).await;
    }
}

/// Walk off a finished top under real controls and wait out the fall. A pathfinder cannot route
/// down a one-block-wide column, so a builder that never steps off is stranded for the episode.
async fn step_off(ctx: &Ctx, yaw_deg: f64) -> bool {
    let Some(start) = ctx.position() else {
        return false;
    };
    let yaw = yaw_deg.to_radians();
    let (fx, fz) = (-yaw.sin(), yaw.cos());
    ctx.look(
        (start.0 + fx * 12.0, start.1 + 1.4, start.2 + fz * 12.0),
        ctx.cfg().look_settle_ms.max(600),
    )
    .await;
    ctx.hold_visual(WalkDirection::Forward, false, 900, 0).await;
    let mut previous_y = ctx.position().map(|p| p.1).unwrap_or(start.1);
    let mut stable = 0;
    for _ in 0..80 {
        ctx.bot.wait_ticks(1).await;
        let Some(current) = ctx.position() else {
            continue;
        };
        if ctx.on_ground() && (current.1 - previous_y).abs() < 0.01 {
            stable += 1;
            if stable >= 4 {
                return current.1 < start.1 - 1.5;
            }
        } else {
            stable = 0;
        }
        previous_y = current.1;
    }
    false
}

/// A ziggurat ramp: column `i` of the run is `i + 1` blocks tall, so every block sits on a solid
/// support and the finished run is a staircase the builder then climbs under real controls.
async fn build_ramp(
    ctx: &mut Ctx,
    sx: i64,
    sz: i64,
    toward: (f64, f64),
    material: &str,
    pass: usize,
) {
    let index = ctx.index();
    // Snap the ramp onto the cardinal axis pointing back toward the anchor, so ramps radiate
    // inward and never grow past the arena edge or into the neighbouring yard.
    let (dx, dz) = if toward.0.abs() >= toward.1.abs() {
        (toward.0.signum() as i64, 0i64)
    } else {
        (0i64, toward.1.signum() as i64)
    };
    let len = vary(ctx, 3.0, 5.0).floor().max(3.0) as i64;
    let feet_y = ctx.position().map(|p| p.1).unwrap_or(64.0).floor() as i64;
    let Some(base) = marker_base_near(ctx, (sx, feet_y, sz), 1) else {
        ctx.record(
            TYPE_LOOK,
            &format!("vis-yard-ramp-nosite-{index}-p{pass}"),
            "No placeable ramp base near the station; pass skipped.",
            json!({"pass": pass, "site": {"x": sx, "z": sz}}),
        );
        return;
    };
    let mut blocks: Vec<((i64, i64, i64), String)> = Vec::new();
    for i in 0..len {
        for h in 0..=i {
            blocks.push((
                (
                    base.x as i64 + dx * i,
                    base.y as i64 + h,
                    base.z as i64 + dz * i,
                ),
                material.to_string(),
            ));
        }
    }
    let total = blocks.len();
    let (placed, attempted) = ctx
        .build_blocks(&blocks, |b, _t, _m| {
            (
                format!("vis-yard-ramp-{index}-p{pass}-{b}"),
                format!("Place ramp block {}/{total}.", b + 1),
                json!({"motif": "ramp", "block_index": b, "pass": pass}),
            )
        })
        .await;

    // Climb what was just built: stand one cell behind the lowest step, face up the run, and walk
    // it with jump pulses. Open-loop on purpose — a planner cannot be trusted up a fresh staircase,
    // and the stall guard in hold_visual bounds a failed climb to ~1.2 s.
    let approach = (
        base.x as f64 + 0.5 - dx as f64 * 1.5,
        base.z as f64 + 0.5 - dz as f64 * 1.5,
    );
    let timeout = ctx.cfg().move_timeout_ms.min(8_000);
    ctx.move_to(approach.0, approach.1, timeout, 1.6).await;
    let top = (
        base.x as f64 + 0.5 + dx as f64 * (len - 1) as f64,
        base.y as f64 + len as f64 + 1.0,
        base.z as f64 + 0.5 + dz as f64 * (len - 1) as f64,
    );
    ctx.look(top, ctx.cfg().look_settle_ms.max(700)).await;
    ctx.hold_visual(WalkDirection::Forward, false, len as u64 * 1_300 + 700, 550)
        .await;
    let top_y = ctx.position().map(|p| p.1).unwrap_or(base.y as f64);
    let climbed = top_y - base.y as f64 >= (len - 2) as f64;
    let span = vary(ctx, 90.0, 150.0);
    sweep(ctx, span).await;
    witness_peer_edits(ctx).await;

    // Come back down the way the ramp faces, riding the one-block drops.
    ctx.look(
        (
            base.x as f64 + 0.5 - dx as f64 * 6.0,
            base.y as f64,
            base.z as f64 + 0.5 - dz as f64 * 6.0,
        ),
        ctx.cfg().look_settle_ms.max(700),
    )
    .await;
    ctx.hold_visual(WalkDirection::Forward, false, len as u64 * 900 + 500, 0)
        .await;
    ctx.record(
        TYPE_MOVE,
        &format!("vis-yard-ramp-done-{index}-p{pass}"),
        &format!(
            "Ramp of {len} steps raised ({placed}/{attempted}/{total}), climbed and descended."
        ),
        json!({"pass": pass, "motif": "ramp", "len": len, "placed": placed,
            "attempted": attempted, "expected": total, "climbed": climbed,
            "dir": {"x": dx, "z": dz}, "base": {"x": base.x, "y": base.y, "z": base.z}}),
    );
}

/// The classic pillar ride: stack the material underfoot, read the yard from the top, step off.
async fn build_pillar(ctx: &mut Ctx, material: &str, pass: usize) {
    let index = ctx.index();
    let equipped = ctx.equip(material).await;
    let height = vary(ctx, 3.0, 6.0).floor().max(3.0) as i64;
    let placed = if equipped {
        ctx.build_tower_underneath(height, material).await
    } else {
        0
    };
    let span = vary(ctx, 100.0, 160.0);
    sweep(ctx, span).await;
    witness_peer_edits(ctx).await;
    glance_at_nearest_peer(ctx, None).await;
    let outward = vary(ctx, 0.0, 360.0);
    let landed = step_off(ctx, outward).await;
    ctx.record(
        TYPE_PLACE,
        &format!("vis-yard-pillar-{index}-p{pass}"),
        "Ride a pillar up, read the yard from the top, and step off it.",
        json!({"pass": pass, "motif": "pillar", "equipped": equipped, "expected": height,
            "placed": placed, "landed": landed, "heading_deg": r2(outward)}),
    );
}

/// A raised perch: short pillar ride, then a small platform placed out from the top so the builder
/// has somewhere to walk at altitude before stepping off the edge.
async fn build_perch(ctx: &mut Ctx, material: &str, pass: usize) {
    let index = ctx.index();
    let equipped = ctx.equip(material).await;
    let height = vary(ctx, 2.0, 4.0).floor().max(2.0) as i64;
    let rose = if equipped {
        ctx.build_tower_underneath(height, material).await
    } else {
        0
    };
    // Platform cells share the pillar's top level, so each has the pillar (or an earlier platform
    // cell) as a solid side neighbour to click against.
    let feet = ctx
        .position()
        .map(|p| BlockPos::new(p.0.floor() as i32, p.1.floor() as i32, p.2.floor() as i32));
    let mut placed = 0u64;
    let mut attempted = 0u64;
    if let Some(feet) = feet {
        let top_y = feet.y as i64 - 1;
        const SIDES: [(i64, i64); 5] = [(1, 0), (0, 1), (-1, 0), (0, -1), (1, 1)];
        let cells = 2 + (ctx.rng_next() * 3.0) as usize;
        let blocks: Vec<((i64, i64, i64), String)> = SIDES
            .iter()
            .take(cells.min(SIDES.len()))
            .map(|(dx, dz)| {
                (
                    (feet.x as i64 + dx, top_y, feet.z as i64 + dz),
                    material.to_string(),
                )
            })
            .collect();
        let total = blocks.len();
        let (ok, tried) = ctx
            .build_blocks(&blocks, |b, _t, _m| {
                (
                    format!("vis-yard-perch-{index}-p{pass}-{b}"),
                    format!("Extend the perch platform, cell {}/{total}.", b + 1),
                    json!({"motif": "perch", "block_index": b, "pass": pass}),
                )
            })
            .await;
        placed = ok;
        attempted = tried;
        // Walk out onto the fresh platform so the altitude is lived on, not just built.
        if ok > 0 {
            let (dx, dz) = SIDES[0];
            ctx.look(
                (
                    feet.x as f64 + 0.5 + dx as f64 * 8.0,
                    feet.y as f64 + 1.0,
                    feet.z as f64 + 0.5 + dz as f64 * 8.0,
                ),
                ctx.cfg().look_settle_ms.max(600),
            )
            .await;
            ctx.hold_visual(WalkDirection::Forward, false, 700, 0).await;
        }
    }
    let span = vary(ctx, 90.0, 150.0);
    sweep(ctx, span).await;
    glance_at_nearest_peer(ctx, None).await;
    let outward = vary(ctx, 0.0, 360.0);
    let landed = step_off(ctx, outward).await;
    ctx.record(
        TYPE_PLACE,
        &format!("vis-yard-perch-{index}-p{pass}"),
        "Raise a short pillar, extend a perch platform from its top, and step off the edge.",
        json!({"pass": pass, "motif": "perch", "equipped": equipped, "rose": rose,
            "platform_placed": placed, "platform_attempted": attempted, "landed": landed}),
    );
}

/// Every bot runs its own vertical construction yard on an N-extensible ring: each pass it raises
/// one of three height-changing motifs — a climbable ziggurat ramp, a pillar ride, or a perch
/// platform — then retreats to read its work and randomly turns to witness the neighbouring yards
/// growing. All placement is genuine survival placement; nothing digs below grade.
pub(crate) async fn run(ctx: &mut Ctx) {
    let index = ctx.index();
    let bot_count = ctx.player_count().max(1);
    let anchor = ctx.anchor();
    let ang = (index as f64 / bot_count as f64) * std::f64::consts::TAU;
    let ring = station_ring(bot_count, SITE_JITTER).min(MAX_SITE_RADIUS);
    let mut pass = 0usize;
    loop {
        let radius = ring + vary(ctx, 0.0, 1.5);
        let tangent = vary(ctx, -SITE_JITTER, SITE_JITTER);
        let sx = anchor.0 + ang.cos() * radius - ang.sin() * tangent;
        let sz = anchor.1 + ang.sin() * radius + ang.cos() * tangent;
        let timeout = ctx.cfg().move_timeout_ms.min(12_000);
        let arrived = ctx.move_to(sx, sz, timeout, 2.5).await;
        let motif = MOTIFS[(ctx.rng_next() * MOTIFS.len() as f64) as usize % MOTIFS.len()];
        let material =
            MATERIALS[(ctx.rng_next() * MATERIALS.len() as f64) as usize % MATERIALS.len()];
        ctx.record(
            TYPE_LOOK,
            &format!("vis-yard-pass-{index}-p{pass}"),
            &format!("Yard pass {pass}: bot {index} raises a {motif} at its station."),
            json!({"pass": pass, "motif": motif, "material": material, "arrived": arrived,
                "station": {"x": r2(sx), "z": r2(sz)}, "bot_count": bot_count}),
        );
        match motif {
            "ramp" => {
                let toward = (anchor.0 - sx, anchor.1 - sz);
                build_ramp(
                    ctx,
                    sx.floor() as i64,
                    sz.floor() as i64,
                    toward,
                    material,
                    pass,
                )
                .await;
            }
            "pillar" => build_pillar(ctx, material, pass).await,
            _ => build_perch(ctx, material, pass).await,
        }

        // Read the yard from a tangential seat, walked OPEN-LOOP: one long continuous key press
        // per pass. The pathfinder's short corrective taps around tight goals are what tripped the
        // key-chatter gate at eight bots (p04 hit 41.7% sub-0.5s presses); a held bearing walk
        // both dilutes them and stays inside the arena at every station radius.
        let side = if ctx.rng_next() < 0.5 { 1.0 } else { -1.0 };
        let (tx, tz) = (-ang.sin() * side, ang.cos() * side);
        let seat = vary(ctx, 4.0, 7.0);
        if let Some(p) = ctx.position() {
            ctx.look(
                (p.0 + tx * 24.0, p.1 + 1.62, p.2 + tz * 24.0),
                ctx.cfg().look_settle_ms.max(500),
            )
            .await;
        }
        let sprint = ctx.rng_next() < ctx.cfg().sprint_bout_prob;
        let hold = ((seat / if sprint { 5.6 } else { 4.3 }) * 1000.0) as u64 + 400;
        ctx.hold_visual(WalkDirection::Forward, sprint, hold, 0)
            .await;
        let feet_y = ctx.position().map(|p| p.1).unwrap_or(64.0);
        ctx.look_default((sx, feet_y + 2.5, sz)).await;
        witness_peer_edits(ctx).await;
        glance_at_nearest_peer(ctx, None).await;
        sleep_ms(vary(ctx, 600.0, 1_800.0) as u64).await;

        if ctx.target_met() {
            break;
        }
        pass += 1;
    }
}
