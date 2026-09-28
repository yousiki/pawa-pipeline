use super::*;

/// Fill material for causeway gaps, ramps and gate steps — a kit stone, one everywhere, so any
/// bot can recognise (and reuse) infrastructure another bot laid down.
const FILL: &str = "cobblestone";

/// Half-width of the cleared gap_jump trench in Z; the first natural rim column is one beyond.
const TRENCH_HALF_Z: i32 = 10;

/// Draw in `[lo, hi)` from the per-bot stream.
fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

/// The gap_jump course this episode spawned onto, derived exactly the way the coordinator builds
/// it: platforms at `ox+3 + g*5` (3 wide, block Y `py`), 2-cell gaps between them, the cleared
/// trench running to `x1` in X and `oz ± 10` in Z.
#[derive(Clone, Copy)]
struct Course {
    oz: i32,
    /// Platform block Y (bots stand at `py + 1`).
    py: i32,
    count: i32,
    /// Westmost / eastmost cleared (air) columns.
    x0: i32,
    x1: i32,
}

impl Course {
    fn platform_x0(&self, g: i32) -> i32 {
        self.x0 + g * 5
    }
    fn platform_center(&self, g: i32) -> f64 {
        self.platform_x0(g) as f64 + 1.5
    }
    /// The two air columns of the gap east of platform `g`.
    fn gap_cells(&self, g: i32) -> (i32, i32) {
        (self.platform_x0(g) + 3, self.platform_x0(g) + 4)
    }
}

/// Topmost solid full block WITH AIR ABOVE in a column near `y_hint` — rim surface resolution.
/// The air-above requirement rejects lake bottoms, so ramps never dive into water.
fn surface_block_y(ctx: &Ctx, x: i32, z: i32, y_hint: i32) -> Option<i32> {
    ((y_hint - 10)..=(y_hint + 6)).rev().find(|&y| {
        let cell = BlockPos::new(x, y, z);
        block_is_safe_support(ctx, cell) && block_is_air(ctx, cell.up(1))
    })
}

/// Place one infrastructure cell if it is still air, recording the attempt. Idempotent across
/// bots: whoever needs a cell first lays it, everyone after walks it.
async fn lay_cell(ctx: &mut Ctx, cell: BlockPos, what: &str) -> bool {
    if !block_is_air(ctx, cell) {
        return true;
    }
    // Reference gate: an unplaceable cell must fail in microseconds, not grind out
    // `place_block`'s full facing wait (the 22 s frozen stretch of the bridge scenario's round 1).
    if !ctx.target_in_reach(cell) || !ctx.equip(FILL).await {
        return false;
    }
    let ok = ctx.place_block(cell, FILL).await;
    let index = ctx.index();
    ctx.record(
        TYPE_PLACE,
        &format!("vis-ring-{what}-{index}-{}-{}", cell.x, cell.z),
        &format!("Lay a {what} cell so the causeway crossing is walkable."),
        json!({"placed": ok, "block": FILL,
            "block_position": {"x": cell.x, "y": cell.y, "z": cell.z}}),
    );
    ok
}

/// Build an ordered run of cells whose Y never changes by more than one per step. A rising step
/// plants the support under the riser first (clicked off the previous cell's side); a falling
/// step extends the run FLAT one cell and hangs the lower cell under that extension — both
/// targets sit ahead-diagonal of the builder, where the facing check settles (clicking the bottom
/// face of the block underfoot does not; that grind froze a bridge bot for 22 s in round 1).
async fn lay_ramp(ctx: &mut Ctx, cells: &[BlockPos], what: &str) {
    let mut prev: Option<BlockPos> = None;
    for cell in cells {
        if ctx.target_met() {
            return;
        }
        // Step out onto the laid run when the next cell has drifted out of placing reach.
        if block_is_air(ctx, *cell) && !ctx.target_in_reach(*cell) {
            let stand = prev
                .map(|p| (p.x as f64 + 0.5, p.z as f64 + 0.5))
                .unwrap_or((cell.x as f64 + 0.5, cell.z as f64 + 0.5));
            ctx.move_to(stand.0, stand.1, 4_000, 1.5).await;
        }
        if let Some(prev) = prev
            && cell.y != prev.y
        {
            let helper = if cell.y > prev.y {
                cell.down(1)
            } else {
                BlockPos::new(cell.x, prev.y, cell.z)
            };
            if block_is_air(ctx, helper) {
                lay_cell(ctx, helper, what).await;
            }
        }
        lay_cell(ctx, *cell, what).await;
        prev = Some(*cell);
    }
}

/// East tail: the last platform stops four cells short of the east rim, so the crossing needs a
/// short ramp between platform level and the rim surface. `rim_first` orders the cells from the
/// side the builder stands on (each cell's reference is the terrain or the previous cell); the
/// walking order works in both directions once built.
fn east_tail_cells(course: &Course, ry: i32, wz: i32, rim_first: bool) -> Vec<BlockPos> {
    let mut cells: Vec<BlockPos> = (0..4)
        .map(|k| {
            // k = 0 sits beside the rim at rim level; the Y walks toward deck level one step per
            // cell and holds there, so adjacent cells never differ by more than one block.
            let y = ry + (course.py - ry).clamp(-k, k);
            BlockPos::new(course.x1 - k, y, wz)
        })
        .collect();
    if !rim_first {
        cells.reverse();
    }
    cells
}

/// Whether any peer is currently on or over the causeway — the crossing is single-file wide, so
/// a bot only turns onto it when it is free.
fn causeway_busy(ctx: &Ctx, course: &Course) -> bool {
    (0..ctx.player_count())
        .filter(|&j| j != ctx.index())
        .filter_map(|j| ctx.peer_position(j))
        .any(|p| {
            p.0 >= (course.x0 - 2) as f64
                && p.0 <= (course.x1 + 2) as f64
                && (p.2 - course.oz as f64).abs() <= 3.5
        })
}

/// Escape the trench after a fall: walk the open floor to the nearest rim wall, then pillar up to
/// rim height and step off. The trench is 4+ blocks deep with sheer walls — without this a fallen
/// bot paces the floor for the rest of the episode.
async fn trench_escape(ctx: &mut Ctx, course: &Course) {
    let Some(p) = ctx.position() else { return };
    let side = if p.2 >= course.oz as f64 { 1 } else { -1 };
    let wall_z = course.oz + side * TRENCH_HALF_Z;
    ctx.move_to(p.0, (wall_z) as f64 + 0.5, 6_000, 1.5).await;
    let Some(p) = ctx.position() else { return };
    let rim_top = surface_block_y(
        ctx,
        p.0.floor() as i32,
        course.oz + side * (TRENCH_HALF_Z + 1),
        course.py,
    )
    .unwrap_or(course.py - 2);
    let need = (rim_top + 1) as i64 - p.1.floor() as i64;
    if need > 0 && ctx.equip(FILL).await {
        let rebuilt = ctx.build_tower_underneath(need, FILL).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-ring-escape-{}-{}", ctx.index(), ctx.elapsed_ms()),
            "Fell into the trench: pillar up beside the rim wall and step out.",
            json!({"needed": need, "rebuilt": rebuilt}),
        );
    }
    // The pillar may have risen a column or two short of the wall (`move_to` floors its arrival
    // radius at 1.5): lay connector cells at rim level until the run touches the rim, so the
    // walk-out never depends on a jump the planner will not make.
    if let Some(p) = ctx.position() {
        let (cx, cz) = (p.0.floor() as i32, p.2.floor() as i32);
        let mut prev_z = cz;
        for _ in 0..3 {
            let next_z = prev_z + side;
            if (next_z - course.oz).abs() > TRENCH_HALF_Z {
                break; // reached the rim column
            }
            let cell = BlockPos::new(cx, rim_top, next_z);
            if block_is_air(ctx, cell) {
                lay_cell(ctx, cell, "escape").await;
            }
            prev_z = next_z;
        }
    }
    ctx.move_to(
        p.0,
        (course.oz + side * (TRENCH_HALF_Z + 3)) as f64,
        6_000,
        1.5,
    )
    .await;
}

/// Walk the causeway west-to-east, filling any still-open gap on this bot's Z lane as it comes
/// to it. Entry/exit ramps are the caller's job. Crossings are ONE-WAY (see `cross_causeway`).
async fn walk_causeway(ctx: &mut Ctx, course: &Course, wz: i32, leg_tag: &str) {
    let index = ctx.index();
    let mut anchor = (ctx.position().unwrap_or((0.0, 0.0, 0.0)), ctx.elapsed_ms());
    let platforms: Vec<i32> = (0..course.count).collect();
    for pair in platforms.windows(2) {
        if ctx.target_met() {
            return;
        }
        let (here, next) = (pair[0], pair[1]);
        let g = here; // the gap east of `here`
        let (c1, c2) = course.gap_cells(g);
        let skip_fill = ctx.cfg().allow_parkour && ctx.rng_next() < 0.5;
        if !skip_fill {
            // Nearest cell first so each has a solid reference.
            for x in [c1, c2] {
                let cell = BlockPos::new(x, course.py, wz);
                if block_is_air(ctx, cell) && !ctx.target_in_reach(cell) {
                    ctx.move_to(course.platform_center(here), wz as f64 + 0.5, 5_000, 1.5)
                        .await;
                }
                lay_cell(ctx, cell, "gapfill").await;
            }
        }
        let arrived = ctx
            .move_to(course.platform_center(next), wz as f64 + 0.5, 6_000, 1.5)
            .await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-ring-{leg_tag}-{index}-{next}-{}", ctx.elapsed_ms()),
            &format!("Advance along the causeway to platform {next}."),
            json!({"platform": next, "arrived": arrived, "parkoured": skip_fill}),
        );
        if let Some(p) = ctx.position()
            && (p.1.floor() as i32) < course.py - 1
        {
            trench_escape(ctx, course).await;
            return; // back on a rim; abandon the crossing
        }
        if ctx.rng_next() < 0.3 {
            glance_at_nearest_peer(ctx, None).await;
        }
        // A pile-up mid-causeway (two bots shoving on the single-width lane) is a statue the
        // patrol loop's guard cannot see from out here — run it per platform hop.
        unstick(ctx, &mut anchor).await;
    }
}

/// Cross the trench over the causeway — strictly WEST TO EAST for every bot. One-way traffic is
/// what makes the single-width lane safe: the busy-check below races (peers decide from the same
/// snapshot), and when it loses, same-direction traffic just queues, while the old two-gate
/// version wedged two bots head-on mid-causeway for 28-34 s (8-bot review, p00+p06).
async fn cross_causeway(ctx: &mut Ctx, course: &Course, wz: i32) -> bool {
    let index = ctx.index();
    ctx.record(
        TYPE_MOVE,
        &format!("vis-ring-spur-{index}-{}", ctx.elapsed_ms()),
        "Take the causeway shortcut across the trench instead of the long way around.",
        json!({"lane_z": wz}),
    );
    // Gate step: rim (feet py-1) -> step top (py) -> platform top (py+1).
    let step = BlockPos::new(course.x0 - 1, course.py - 1, wz);
    ctx.move_to((course.x0 - 2) as f64 + 0.5, wz as f64 + 0.5, 8_000, 1.5)
        .await;
    lay_cell(ctx, step, "gatestep").await;
    ctx.move_to(course.platform_center(0), wz as f64 + 0.5, 6_000, 1.5)
        .await;
    // Second look at the mutex from platform 0: a peer that won the race is now visible on the
    // lane — step back off west (a two-block drop) and resume the ring instead of queueing.
    if causeway_busy(ctx, course) {
        ctx.move_to((course.x0 - 3) as f64, wz as f64 + 0.5, 6_000, 1.5)
            .await;
        return false;
    }
    walk_causeway(ctx, course, wz, "spur").await;
    // Exit east down the tail (build it deck-first if this is the first east exit).
    let ry = surface_block_y(ctx, course.x1 + 1, wz, course.py).unwrap_or(course.py - 2);
    lay_ramp(ctx, &east_tail_cells(course, ry, wz, false), "tail").await;
    ctx.move_to((course.x1 + 3) as f64, wz as f64 + 0.5, 8_000, 1.5)
        .await;
    if let Some(p) = ctx.position()
        && (p.1.floor() as i32) < course.py - 1
    {
        trench_escape(ctx, course).await;
    }
    true
}

/// Anti-statue guard: when the body has not displaced 1.5 blocks inside 12 s of scenario loop,
/// shake loose (random-heading hold) before the 20 s frozen gate can trip. The bridge scenario's
/// rounds measured 54-70 s statues from a still-unidentified placement-grind mode; the guard is
/// the ceiling on any repeat of it here.
async fn unstick(ctx: &mut Ctx, anchor: &mut ((f64, f64, f64), i64)) {
    let Some(p) = ctx.position() else { return };
    let now = ctx.elapsed_ms();
    let moved = (p.0 - anchor.0.0).hypot(p.2 - anchor.0.2) + (p.1 - anchor.0.1).abs();
    if moved >= 1.5 {
        *anchor = (p, now);
    } else if now - anchor.1 >= 12_000 {
        ctx.record(
            TYPE_MOVE,
            &format!("vis-ring-stall-{}-{now}", ctx.index()),
            "Stalled in place for 12 s: shake loose before the frozen gate trips.",
            json!({"position": {"x": r2(p.0), "y": r2(p.1), "z": r2(p.2)}}),
        );
        shake_loose(ctx).await;
        *anchor = (ctx.position().unwrap_or(p), ctx.elapsed_ms());
    }
}

/// Counterflow ring traffic around the gap_jump trench: bots leave their spawn platforms over
/// self-laid gap fills, then patrol a rectangular circuit on the natural rims — half clockwise,
/// half counter — glancing at oncoming traffic and down into the trench. At the two gate corners
/// a bot may take the causeway shortcut straight across instead of the long way around, laying
/// the gate step and east tail ramp the first time anyone needs them.
pub(crate) async fn run(ctx: &mut Ctx) {
    let index = ctx.index();
    let (ax, az) = ctx.anchor();
    let (ox, oz) = (ax.round() as i32, az.round() as i32);
    let spawn = loop {
        if let Some(p) = ctx.position() {
            break p;
        }
        sleep_ms(100).await;
    };
    // Long per-index stagger: same-platform pairs (index, index+count) leave several seconds
    // apart, so the second never queues against the first on the single-width gap fills.
    sleep_ms(600 + index as u64 * 600).await;

    let count = (5 + ctx.engine().seed % 3) as i32;
    let course = Course {
        oz,
        py: spawn.1.floor() as i32 - 1,
        count,
        x0: ox + 3,
        x1: ox + 3 + (count - 1) * 5 + 10,
    };
    let platform = (((spawn.0 - (ox as f64 + 4.0)) / 5.0).round() as i32).clamp(0, count - 1);
    let wz = oz + (index % 3) as i32 - 1;
    let dir: i32 = if index.is_multiple_of(2) { 1 } else { -1 };
    // Lane margin modulus 4, not 3: with `dir` keyed to index parity, bots sharing a margin are
    // then always 4 apart — same parity, co-rotating — so two bots never meet head-on in the same
    // lane (8-bot review 1 failed key chatter on exactly those shoving matches). Lanes sit 2.5
    // blocks apart: at 1.2 apart, opposite-direction pass-bys still brushed hitboxes and sprayed
    // sub-half-second corrective presses (8-bot review 3).
    let margin = 2.5 + (index % 4) as f64 * 2.5;

    ctx.record(
        TYPE_LOOK,
        &format!("vis-ring-init-{index}"),
        "Ring traffic: leave the causeway, patrol the trench perimeter in counterflow, sometimes cut back across.",
        json!({"platform": platform, "dir": dir, "lane_z": wz, "margin": margin,
            "platforms": count, "anchor": {"x": r2(ax), "z": r2(az)}}),
    );

    // --- egress: walk off the causeway toward the nearer end, filling gaps on the way ---
    let go_east = platform >= count / 2;
    let egress_start = ctx.elapsed_ms();
    let mut stall_anchor = (spawn, ctx.elapsed_ms());
    if go_east {
        // Walk east from own platform, filling gaps on the way, then down the tail.
        let plats: Vec<i32> = (platform..count).collect();
        for pair in plats.windows(2) {
            if ctx.target_met() || ctx.elapsed_ms() - egress_start > 40_000 {
                break;
            }
            let g = pair[0];
            let (c1, c2) = course.gap_cells(g);
            for x in [c1, c2] {
                let cell = BlockPos::new(x, course.py, wz);
                if block_is_air(ctx, cell) && !ctx.target_in_reach(cell) {
                    ctx.move_to(course.platform_center(g), wz as f64 + 0.5, 5_000, 1.5)
                        .await;
                }
                lay_cell(ctx, cell, "gapfill").await;
            }
            ctx.move_to(course.platform_center(pair[1]), wz as f64 + 0.5, 6_000, 1.5)
                .await;
            unstick(ctx, &mut stall_anchor).await;
        }
        let ry = surface_block_y(ctx, course.x1 + 1, wz, course.py).unwrap_or(course.py - 2);
        lay_ramp(ctx, &east_tail_cells(&course, ry, wz, false), "tail").await;
        ctx.move_to((course.x1 + 3) as f64, wz as f64 + 0.5, 8_000, 1.5)
            .await;
    } else {
        let plats: Vec<i32> = (0..=platform).rev().collect();
        for pair in plats.windows(2) {
            if ctx.target_met() || ctx.elapsed_ms() - egress_start > 40_000 {
                break;
            }
            let g = pair[1];
            let (c1, c2) = course.gap_cells(g);
            for x in [c2, c1] {
                let cell = BlockPos::new(x, course.py, wz);
                if block_is_air(ctx, cell) && !ctx.target_in_reach(cell) {
                    ctx.move_to(course.platform_center(pair[0]), wz as f64 + 0.5, 5_000, 1.5)
                        .await;
                }
                lay_cell(ctx, cell, "gapfill").await;
            }
            ctx.move_to(course.platform_center(pair[1]), wz as f64 + 0.5, 6_000, 1.5)
                .await;
            unstick(ctx, &mut stall_anchor).await;
        }
        ctx.move_to((course.x0 - 3) as f64, wz as f64 + 0.5, 8_000, 1.5)
            .await;
    }
    // Egress bail: still on or over the causeway after the walk-off? Step down into the trench
    // deliberately and take the escape pillar out — a deterministic exit that cannot depend on
    // the gap fills having worked.
    if let Some(p) = ctx.position()
        && p.0 > (course.x0 - 2) as f64
        && p.0 < (course.x1 + 2) as f64
        && (p.2 - oz as f64).abs() < 4.0
    {
        if !ctx.move_to(p.0, (oz + 6) as f64, 5_000, 1.5).await {
            // The planner refuses a six-block drop: walk off the edge open-loop instead.
            ctx.look(
                (p.0, p.1, (oz + 8) as f64),
                ctx.cfg().look_settle_ms.max(500),
            )
            .await;
            ctx.hold_visual(WalkDirection::Forward, false, 1_000, 0)
                .await;
        }
        trench_escape(ctx, &course).await;
    }
    if let Some(p) = ctx.position()
        && (p.1.floor() as i32) < course.py - 1
    {
        trench_escape(ctx, &course).await;
    }
    ctx.record(
        TYPE_MOVE,
        &format!("vis-ring-egress-{index}"),
        "Off the causeway; join the perimeter ring.",
        json!({"went_east": go_east}),
    );

    // --- the ring: 8 waypoints around the trench, per-bot lane margin, counterflow ---
    let w = (course.x0 - 1) as f64 - margin;
    let e = (course.x1 + 1) as f64 + margin;
    let s = (oz + TRENCH_HALF_Z + 1) as f64 + margin;
    let n = (oz - TRENCH_HALF_Z - 1) as f64 - margin;
    let midx = (course.x0 + course.x1) as f64 / 2.0;
    let ring: [(f64, f64); 8] = [
        (w, n),
        (midx, n),
        (e, n),
        (e, oz as f64), // east gate
        (e, s),
        (midx, s),
        (w, s),
        (w, oz as f64), // west gate
    ];
    // Start from the waypoint nearest the egress point.
    let mut wp = {
        let p = ctx.position().unwrap_or(spawn);
        (0..8)
            .min_by(|&a, &b| {
                let da = (ring[a].0 - p.0).hypot(ring[a].1 - p.2);
                let db = (ring[b].0 - p.0).hypot(ring[b].1 - p.2);
                da.total_cmp(&db)
            })
            .unwrap_or(0)
    };
    let mut leg = 0u32;
    while !ctx.target_met() {
        let (tx, tz) = ring[wp];
        let watching = ctx.rng_next() < 0.4;
        if watching {
            // Keep the trench in frame while the body travels — decouples camera from travel.
            let focus = (
                (tx + midx) / 2.0,
                (course.py + 1) as f64,
                oz as f64 + vary(ctx, -4.0, 4.0),
            );
            ctx.move_to_watching_point(tx, tz, focus, 14_000, 3.0).await;
        } else {
            travel_to(ctx, (tx, tz), TRAVEL_HOP, 3.0).await;
        }
        ctx.record(
            TYPE_MOVE,
            &format!("vis-ring-leg-{index}-{leg}"),
            &format!("Patrol leg to ring waypoint {wp}."),
            json!({"leg": leg, "waypoint": wp, "watching": watching, "dir": dir}),
        );
        // Garnish: trench gaze, oncoming-peer glance, free sweep, or straight on.
        match (ctx.rng_next() * 4.0) as u32 {
            0 => {
                let gx = ctx
                    .position()
                    .map(|p| p.0.clamp(course.x0 as f64, course.x1 as f64))
                    .unwrap_or(midx);
                ctx.look(
                    (gx, (course.py + 1) as f64, oz as f64),
                    ctx.cfg().look_settle_ms.max(700),
                )
                .await;
                sleep_ms(vary(ctx, 700.0, 1_800.0) as u64).await;
            }
            1 => glance_at_nearest_peer(ctx, None).await,
            2 => {
                let span = vary(ctx, 60.0, 150.0);
                let signed = span * if ctx.rng_next() < 0.5 { 1.0 } else { -1.0 };
                let pitch = vary(ctx, -15.0, 10.0);
                let rate = ctx.cfg().yaw_rate_deg_s.max(1.0);
                let settle = (span / rate * 1000.0) as u64 + 600;
                ctx.drag_view(signed, pitch, settle).await;
            }
            _ => {}
        }
        witness_peer_edits(ctx).await;
        unstick(ctx, &mut stall_anchor).await;
        if let Some(p) = ctx.position()
            && (p.1.floor() as i32) < course.py - 2
            && p.0 > (course.x0 - 1) as f64
            && p.0 < (course.x1 + 1) as f64
            && (p.2 - oz as f64).abs() < TRENCH_HALF_Z as f64 + 0.5
        {
            trench_escape(ctx, &course).await;
        }
        // At the WEST gate corner, maybe cut across instead of continuing around. West gate only:
        // crossings are one-way (see `cross_causeway`), so racing entrants can never meet head-on.
        if wp == 7
            && ctx.rng_next() < 0.35
            && !causeway_busy(ctx, &course)
            && cross_causeway(ctx, &course, wz).await
        {
            // Landed on the east side; resume the ring from the east gate. An aborted crossing
            // (mutex lost) keeps the bot at the west gate and the waypoint untouched — jumping
            // the waypoint across an uncrossed trench would hand the planner an impossible goal.
            wp = 3;
        }
        wp = (wp as i32 + dir).rem_euclid(8) as usize;
        leg += 1;
    }
}
