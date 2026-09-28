use crate::*;

// =============================================================================================
// Combat episodes (real azalea `Client::attack`). Ported from the solaris reference
// controller/episode-handlers/{pvp,pve}-episode.js + primitives/fighting.js. Both bounded +
// terminating; both equip the kit's `diamond_sword` and fight with genuine melee swings
// (NOT the mineflayer-pvp plugin — we drive approach + cooldown-gated swing directly).
// =============================================================================================

pub(super) const MELEE_RANGE: f64 = 3.0; // solaris MELEE_RANGE: swing when within this many blocks
pub(super) const ATTACK_COOLDOWN_MS: u64 = 500; // solaris ATTACK_COOLDOWN_MS: min gap between swings
pub(super) const COMBAT_TICK_MS: u64 = 100; // solaris COMBAT_LOOP_INTERVAL_MS

pub(super) fn head_of(pos: (f64, f64, f64)) -> (f64, f64, f64) {
    (pos.0, pos.1 + 1.4, pos.2)
}

pub(super) async fn look_at_peer(ctx: &Ctx, j: usize, settle_ms: u64) -> Option<(f64, f64, f64)> {
    let pos = ctx.peer_position(j);
    if let Some(p) = pos {
        ctx.look(head_of(p), settle_ms).await;
    }
    pos
}

pub(super) fn block_is_air(ctx: &Ctx, pos: BlockPos) -> bool {
    matches!(
        ctx.block_id_at(pos).as_deref(),
        Some(
            "air"
                | "cave_air"
                | "void_air"
                | "minecraft:air"
                | "minecraft:cave_air"
                | "minecraft:void_air"
        )
    )
}

pub(super) fn block_is_safe_support(ctx: &Ctx, pos: BlockPos) -> bool {
    let Ok(world) = ctx.bot.world() else {
        return false;
    };
    let world = world.read();
    world
        .get_block_state(pos)
        .map(|state| !state.is_air() && state.is_collision_shape_full())
        .unwrap_or(false)
}

/// The gap every script has to leave between two players. Anything tighter and one bot's camera is
/// inside the other's head box, which is what the corpus audits keep measuring as "a face fills the
/// frame".
pub(super) const MIN_PEER_GAP: f64 = 4.5;

/// Unit vector pointing from `peer` back toward `own`: the direction to stand off, back away or
/// flee in. Falls back to +X when the two share a column and the direction is undefined.
pub(super) fn away_unit(own: Option<(f64, f64, f64)>, peer: (f64, f64, f64)) -> (f64, f64) {
    let Some(own) = own else { return (1.0, 0.0) };
    let (dx, dz) = (own.0 - peer.0, own.2 - peer.2);
    let magnitude = dx.hypot(dz);
    if magnitude < 0.01 {
        (1.0, 0.0)
    } else {
        (dx / magnitude, dz / magnitude)
    }
}

/// Radius of a ring that leaves at least `MIN_PEER_GAP` between adjacent slots of an evenly spaced
/// swarm: the chord between neighbours is `2 r sin(pi/n)`. A fixed radius collapses to a three-block
/// gap once the swarm is large, so it has to grow with `bot_count`.
pub(super) fn ring_radius(bot_count: usize) -> f64 {
    let n = bot_count.max(2) as f64;
    (MIN_PEER_GAP / (2.0 * (std::f64::consts::PI / n).sin())).max(8.0)
}

/// Radius of a ring whose adjacent slots still clear [`MIN_PEER_GAP`] after each of two neighbours
/// has wandered up to `jitter` blocks sideways off its own slot.
///
/// Scenarios that redraw their station every round need that wander — a bot that rebuilds on one
/// exact spot has nowhere to put the second structure — but two neighbours drawing toward each
/// other eat twice it. [`ring_radius`] is linear in the gap it is asked for, so asking for a gap
/// widened by both wanders scales it directly.
pub(super) fn station_ring(bot_count: usize, jitter: f64) -> f64 {
    ring_radius(bot_count) * (MIN_PEER_GAP + 2.0 * jitter.max(0.0)) / MIN_PEER_GAP
}

/// Return a nearby two-block-high standing cell, preferring the requested X/Z.
pub(super) fn walkable_xz_near(ctx: &Ctx, desired: (f64, f64), feet_y: i32) -> Option<(f64, f64)> {
    const OFFSETS: [(i32, i32); 9] = [
        (0, 0),
        (1, 0),
        (-1, 0),
        (0, 1),
        (0, -1),
        (1, 1),
        (1, -1),
        (-1, 1),
        (-1, -1),
    ];
    let bx = desired.0.floor() as i32;
    let bz = desired.1.floor() as i32;
    for dy in [0, 1, -1, 2, -2] {
        for (dx, dz) in OFFSETS {
            let feet = BlockPos::new(bx + dx, feet_y + dy, bz + dz);
            if block_is_air(ctx, feet)
                && block_is_air(ctx, feet.up(1))
                && block_is_safe_support(ctx, feet.down(1))
            {
                return Some((feet.x as f64 + 0.5, feet.z as f64 + 0.5));
            }
        }
    }
    None
}

/// Shake a bot loose after the pathfinder has refused its goal twice running. The planner answers
/// from where the body is standing, so on terrain it cannot solve — a ledge it keeps trying to jump,
/// the floor of a hole — no fresh goal helps until the body has moved. A blind hop with jump pulses
/// does move it, and costs at most a second and a half: `hold_visual` gives up the moment it stops
/// translating. Measured on `group_orbit`, where one orbiter re-issued the same unreachable slot for
/// 28 seconds while jump-pulsing on the spot.
pub(super) async fn shake_loose(ctx: &mut Ctx) {
    if ctx.requires_exterior_surface() {
        return;
    }
    let heading = (ctx.rng_next() * 360.0).to_radians();
    if let Some(p) = ctx.position() {
        let settle = ctx.cfg().look_settle_ms.max(400);
        ctx.look(
            (
                p.0 - heading.sin() * 24.0,
                p.1 + 1.62,
                p.2 + heading.cos() * 24.0,
            ),
            settle,
        )
        .await;
    }
    ctx.hold_visual(WalkDirection::Forward, false, 1_600, 500)
        .await;
}

/// Quiet time between two witness gazes by one bot, so a builder is not constantly interrupted.
const WITNESS_MIN_GAP_MS: i64 = 8_000;

/// Beyond this a peer's edit is out of render range and turning toward it teaches nothing.
const WITNESS_MAX_DISTANCE: f64 = 64.0;

/// Turn and look at a peer's recent world edit — the world-synchronization beat the corpus was
/// missing: A's placement must show up in B's view, a few seconds later, or the model never
/// learns that edits persist across viewpoints.
///
/// The editors broadcast every verified edit from the Ctx primitives; this consumes them. The
/// reaction delay (2–8 s) and the dwell (1–3 s) are drawn per call so witnessing never locks
/// phase with the editor, and a burst of placements is witnessed once at its newest cell. Called
/// at scenario loop boundaries: it is a camera action plus a bounded dwell, so it cannot touch
/// the displacement-based QC gates. Returns whether a witness gaze happened.
pub(super) async fn witness_peer_edits(ctx: &mut Ctx) -> bool {
    if ctx.player_count() < 2 || ctx.target_met() {
        return false;
    }
    let now = ctx.elapsed_ms();
    if now - ctx.last_witness_ms < WITNESS_MIN_GAP_MS {
        return false;
    }
    let min_age = Duration::from_millis(2_000 + (ctx.rng_next() * 6_000.0) as u64);
    let Some(edit) = ctx.take_witnessable_edit(min_age, WITNESS_MAX_DISTANCE) else {
        return false;
    };
    let focus = (
        edit.x as f64 + 0.5,
        edit.y as f64 + 0.5,
        edit.z as f64 + 0.5,
    );
    ctx.look(focus, ctx.cfg().look_settle_ms.max(600)).await;
    let dwell = 1_000 + (ctx.rng_next() * 2_000.0) as u64;
    sleep_ms(dwell).await;
    ctx.last_witness_ms = ctx.elapsed_ms();
    ctx.record(
        TYPE_LOOK,
        &format!("vis-witness-{}-{}", ctx.index(), ctx.elapsed_ms()),
        &format!("Witness peer bot {}'s recent world edit.", edit.by),
        json!({
            "witness": true,
            "editor": edit.by,
            "block_position": {"x": edit.x, "y": edit.y, "z": edit.z},
            "edit_age_ms": edit.at.elapsed().as_millis() as u64,
            "dwell_ms": dwell,
        }),
    );
    true
}

/// Glance at the nearest visible peer that is not `exclude` — keeps other players in frame.
pub(super) async fn glance_at_nearest_peer(ctx: &mut Ctx, exclude: Option<usize>) {
    let Some(own) = ctx.position() else { return };
    let mut best: Option<(usize, (f64, f64, f64), f64)> = None;
    for j in 0..ctx.player_count() {
        if j == ctx.index() || Some(j) == exclude {
            continue;
        }
        let Some(p) = ctx.peer_position(j) else {
            continue;
        };
        let d = ((p.0 - own.0).powi(2) + (p.2 - own.2).powi(2)).sqrt();
        if best.as_ref().map(|b| d < b.2).unwrap_or(true) {
            best = Some((j, p, d));
        }
    }
    if let Some((j, p, d)) = best {
        ctx.look(head_of(p), 300).await;
        ctx.record(
            TYPE_LOOK,
            &format!("vis-peer-{}-{}", ctx.index(), ctx.elapsed_ms()),
            &format!("Glance at nearest peer bot {j}."),
            json!({"target_peer": j, "target_distance": r2(d)}),
        );
    }
}

/// Block layout for an audience_build pattern (parity with scenarios.js blocksFor).
pub(super) fn pattern_blocks(pattern: &str, sx: i64, sy: i64, sz: i64) -> Vec<(i64, i64, i64)> {
    match pattern {
        "pillar" => (0..3).map(|dy| (sx, sy + dy, sz)).collect(),
        "wall" => {
            let mut out = Vec::new();
            for dx in 0..3 {
                for dy in 0..2 {
                    out.push((sx + dx, sy + dy, sz));
                }
            }
            out
        }
        "arch" => vec![
            (sx, sy, sz),
            (sx, sy + 1, sz),
            (sx + 2, sy, sz),
            (sx + 2, sy + 1, sz),
            (sx, sy + 2, sz),
            (sx + 1, sy + 2, sz),
            (sx + 2, sy + 2, sz),
        ],
        "tower" => (0..4).map(|dy| (sx, sy + dy, sz)).collect(),
        _ => vec![
            // cross
            (sx, sy, sz),
            (sx + 1, sy, sz),
            (sx - 1, sy, sz),
            (sx, sy, sz + 1),
            (sx, sy, sz - 1),
            (sx, sy + 1, sz),
        ],
    }
}

/// Global swarm index (coordinator username ordinal) of this bot.
pub(super) fn global_index(ctx: &Ctx) -> usize {
    ctx.cfg().index_offset + ctx.index()
}

/// Whether this bot is one of the `far_players` travelers the coordinator placed in the far
/// region. The coordinator places the LAST K usernames, so the rule is index-only and needs no
/// extra signal file.
pub(super) fn is_far_traveler(ctx: &Ctx) -> bool {
    let cfg = ctx.cfg();
    let total = cfg.total_bot_count.max(1);
    let k = cfg.far_players.min(total.saturating_sub(1));
    k > 0 && global_index(ctx) + k >= total
}

/// Leg length (blocks) for every hop-wise traversal.
///
/// 8.0 was measured to abort traversals: a leg target that lands in water, on a cliff, or inside a
/// peer's body fails, and two such failures end the journey. Shorter legs keep each `move_to` inside
/// terrain the planner can actually route, and let a crowded final approach retry around bodies.
pub(super) const TRAVEL_HOP: f64 = 4.0;

/// Hop-wise travel toward absolute world `(x, z)`: issue `TRAVEL_HOP`-block `move_to` legs so a long
/// traversal never depends on one long path plan. Stops on arrival within `radius`, on `target_met`,
/// or once a bounded budget of non-converging iterations is exhausted. Returns the final 2D distance.
pub(super) async fn travel_to(ctx: &mut Ctx, target: (f64, f64), hop: f64, radius: f64) -> f64 {
    /// Iterations without real progress tolerated before declaring the target unreachable.
    const PROGRESS_BUDGET: u32 = 6;
    /// Distance (blocks) a leg must close before it counts as progress.
    const PROGRESS_EPSILON: f64 = 0.5;
    let mut failures: u32 = 0;
    let mut best = f64::INFINITY;
    while !ctx.target_met() {
        let Some((x, _, z)) = ctx.position() else {
            return f64::INFINITY;
        };
        let dx = target.0 - x;
        let dz = target.1 - z;
        let dist = dx.hypot(dz);
        if dist <= radius {
            return dist;
        }
        // Budget PROGRESS, not leg return codes. A sidestep that succeeds without converging is not
        // progress, and resetting on it let a traveler drift sideways for 138 s while closing only 40
        // of 101 blocks. Requiring a real improvement is what makes the budget bite.
        if dist + PROGRESS_EPSILON < best {
            best = dist;
            failures = 0;
        } else {
            failures += 1;
            if failures >= PROGRESS_BUDGET {
                break;
            }
            // Half the budget spent without closing any distance is not a busy corridor, it is
            // terrain the planner cannot cross from where the body stands. Measured: one
            // `split_and_reunite` bot spent a whole episode inside a six-block box, 27 blocks short
            // of its region, because every leg and every sidestep replanned from the same spot.
            if failures == PROGRESS_BUDGET / 2 {
                shake_loose(ctx).await;
            }
        }
        let leg = hop.min(dist);
        let (ux, uz) = (dx / dist, dz / dist);
        // Bound each leg by its own length rather than the whole-journey `move_timeout_ms`. A 4-block
        // leg walks in ~2 s, so a blocked one must not burn 30 s: measured reunions overran their
        // 300-second episode (`elapsed_ms` 307 s and 350 s) purely because a few blocked legs ate the
        // entire phase budget. Only hop-wise travel is shortened; other scenarios keep their timeout.
        let timeout = ctx.cfg().move_timeout_ms.min(8_000);
        let goal_radius = ctx.cfg().goal_radius;
        if ctx
            .move_to(x + ux * leg, z + uz * leg, timeout, goal_radius)
            .await
        {
            continue;
        }
        // A blocked leg is routine: peers stand in the corridor and terrain breaks the straight line.
        // Sidestep perpendicular to the bearing, alternating sides so consecutive blocks probe both
        // ways instead of shoving into the same obstacle.
        let side = if failures.is_multiple_of(2) {
            1.0
        } else {
            -1.0
        };
        ctx.move_to(
            x - uz * leg * side,
            z + ux * leg * side,
            timeout,
            goal_radius,
        )
        .await;
    }
    ctx.position()
        .map(|(x, _, z)| (target.0 - x).hypot(target.1 - z))
        .unwrap_or(f64::INFINITY)
}

/// Small horizontal search ring used when a desired marker column is unusable.
const MARKER_OFFSETS: [(i32, i32); 13] = [
    (0, 0),
    (1, 0),
    (-1, 0),
    (0, 1),
    (0, -1),
    (1, 1),
    (1, -1),
    (-1, 1),
    (-1, -1),
    (2, 0),
    (-2, 0),
    (0, 2),
    (0, -2),
];

/// Resolve the base cell of a placeable marker column near `desired`.
///
/// Natural terrain defeats a cell derived from the bot's own feet Y: the column can be inside a
/// slope, or occupied by grass, leaves, petals, or snow layers, and `place_block` then refuses
/// because the target is not loaded air. Cells overlapping the placer's own hitbox are excluded
/// too — they read as air and look supported, yet vanilla always rejects placing a block into the
/// space you occupy. Scan vertically first, then over a small horizontal ring, for a base whose
/// support below is a full solid block and whose next `height` cells are all air. Returning `None`
/// means the scenario must report an unplaceable marker instead of pretending.
pub(super) fn marker_base_near(
    ctx: &Ctx,
    desired: (i64, i64, i64),
    height: i64,
) -> Option<BlockPos> {
    let stack = height.max(1) as i32;
    let own = ctx
        .position()
        .map(|(x, y, z)| BlockPos::new(x.floor() as i32, y.floor() as i32, z.floor() as i32));
    let occupies_self = |cell: BlockPos| {
        own.is_some_and(|own| {
            cell.x == own.x && cell.z == own.z && (cell.y == own.y || cell.y == own.y + 1)
        })
    };
    for dy in [0, 1, -1, 2, -2, 3, -3] {
        for (dx, dz) in MARKER_OFFSETS {
            let base = BlockPos::new(
                desired.0 as i32 + dx,
                desired.1 as i32 + dy,
                desired.2 as i32 + dz,
            );
            if (0..stack).any(|layer| occupies_self(base.up(layer))) {
                continue;
            }
            if block_is_safe_support(ctx, base.down(1))
                && (0..stack).all(|layer| block_is_air(ctx, base.up(layer)))
            {
                return Some(base);
            }
        }
    }
    None
}

/// Search a single block column for `expected`, returning the Y where it was found.
///
/// A marker's exact Y is not shared between bots, and a placer can be shoved before it probes, so
/// persistence must be judged over a bounded vertical window rather than one guessed cell.
pub(super) fn column_find_block(
    ctx: &Ctx,
    x: i32,
    z: i32,
    y_range: (i32, i32),
    expected: &str,
) -> Option<i32> {
    let wanted = expected.strip_prefix("minecraft:").unwrap_or(expected);
    (y_range.0..=y_range.1)
        .find(|&y| ctx.block_id_at(BlockPos::new(x, y, z)).as_deref() == Some(wanted))
}

/// Search the same neighbourhood a placer may have used for `expected`, returning its cell.
///
/// `marker_base_near` may shift a marker off its desired column by up to two blocks, so an observer
/// that probes only the desired column reports a false negative. Both sides share `MARKER_OFFSETS`
/// so the search area can never drift from the placement area.
pub(super) fn area_find_block(
    ctx: &Ctx,
    center: (i32, i32),
    y_range: (i32, i32),
    expected: &str,
) -> Option<BlockPos> {
    MARKER_OFFSETS.iter().find_map(|(dx, dz)| {
        let (x, z) = (center.0 + dx, center.1 + dz);
        column_find_block(ctx, x, z, y_range, expected).map(|y| BlockPos::new(x, y, z))
    })
}

/// World point an eye at `eye` must look at to face `yaw_deg` (notchian: 0 = +Z, 90 = -X), level
/// with the eye. Inverse of [`look_angles`] restricted to pitch 0.
///
/// The eval scripts need to turn by a chosen ANGLE, but every look API here takes a target POINT,
/// so the angle has to be converted into one. Kept honest by a round-trip test against
/// `look_angles` — deriving the constants by hand is exactly how a 90 deg sign error hides.
pub(super) fn point_at_yaw(eye: (f64, f64, f64), yaw_deg: f64, distance: f64) -> (f64, f64, f64) {
    let yaw = yaw_deg.to_radians();
    (
        eye.0 - yaw.sin() * distance,
        eye.1,
        eye.2 + yaw.cos() * distance,
    )
}

/// Arrival tolerance for a shuttle leg. MUST stay well under `SHUTTLE_LEG`: the first version of
/// this used 1.0 against a 0.48-1.2 block step, so the bot was already inside tolerance when the
/// leg was issued and never took a single step — measured as `0 moving frames` and a 31.7 s frozen
/// stretch across all three eval scripts.
const SHUTTLE_ARRIVE: f64 = 0.8;
/// Wall-clock a leg can need at walking pace, plus margin. A hold stops issuing legs this long
/// before its deadline so the last one is never cut off mid-press: a truncated leg is a SHORT press,
/// and enough of them trip key chatter on their own (measured 40% against a 35% ceiling once the
/// leg length itself was fixed — the residue was purely truncation).
const LEG_BUDGET_MS: i64 = 2_200;

/// Keep a bot alive and covering ground for a while, with the camera QUIET, until `deadline_ms` or
/// the episode target. Walks out-and-back along straight `leg`-block legs around the entry point.
///
/// **What the camera actually does, which is not what the name suggests.** `move_to` overwrites
/// `camera_intent` with `CameraIntent::Hold` on entry (`ctx.rs`), and `Hold` faces the direction of
/// travel while moving and freezes the heading while still (`control.rs`). So the bearing set here
/// survives only until the first leg starts: during a leg the camera tracks travel, between legs it
/// holds wherever it ended up. A scenario CANNOT watch a subject while walking with this — aim the
/// camera while STANDING (`look_until_facing`), then walk.
///
/// **Why the single `look` still matters.** The camera/travel-independence gate scores how
/// predictable the camera's turn direction is against the bearing error, over frames where the bot
/// moves and the camera turns, and it is TWO-SIDED: `agreement()` is `max(p, 1-p)`, so turning
/// consistently AWAY from travel fails exactly like turning consistently toward it. Only a mixed
/// signal passes — or too few qualifying frames to judge (`MIN_CAMERA_FRAMES` = 100).
///
/// Re-issuing `LookAt` every pass makes the scenario fight `Hold` for the camera: the two pull in
/// different directions, every tick produces a quantized `d_yaw`, and the sign of that step follows
/// the body. Measured 96-100% agreement against a 95% ceiling on all three eval scripts. Setting it
/// ONCE lets the bearing error collapse to zero and stay there, so the qualifying frames fall under
/// the threshold and the only turns the gate ever sees are the scripted ones between legs.
///
/// Two more gates bound the leg geometry itself:
///
/// - **frozen** reads position: under 0.5 blocks of net displacement over the trailing second is
///   frozen, and 20 s of that fails the episode. So the hold must keep covering ground, ~15 blocks
///   of path per 30 s. Standing still fails; so does a shuffle too small to leave the arrival
///   tolerance.
/// - **key chatter** reads press duration: over 35% of movement-key presses under 10 ticks = 0.5 s
///   fails. Each leg is one press-to-release, so a SHORT leg fails even while covering ground. A
///   circular walk of 1.7-block chords cleared frozen and then failed this at 95-100% — sprinting
///   covers 1.7 blocks in ~0.3 s.
///
/// A 4-block leg sprints in ~0.7 s and walks in ~1.9 s, clearing the press floor either way, while
/// net displacement stays inside one leg. It is also Solaris's random-walk primitive: 3-4 blocks
/// out, then back to the origin, so trajectories are loops rather than rays.
pub(super) async fn hold_facing(ctx: &mut Ctx, bearing_deg: f64, deadline_ms: i64, leg: f64) {
    let centre = ctx.position();
    // Set the bearing ONCE, and accept that the first leg hands the camera to `Hold`. Re-issuing it
    // every pass would fight `Hold` for the camera and manufacture the exact evidence the
    // camera/travel gate scores — see the doc comment.
    if let Some(position) = ctx.position() {
        ctx.look_default(point_at_yaw(head_of(position), bearing_deg, 24.0))
            .await;
    }
    // Each bot starts on its own heading, so peers in the same episode do not trace identical
    // action streams — the pairwise `near-clones` QC check reads exactly that.
    let mut heading = ctx.rng_next() * std::f64::consts::TAU;
    let mut outbound = true;
    while !ctx.target_met() && ctx.elapsed_ms() + LEG_BUDGET_MS < deadline_ms {
        if let Some((cx, _, cz)) = centre {
            let (target_x, target_z) = if outbound {
                (cx + heading.cos() * leg, cz + heading.sin() * leg)
            } else {
                (cx, cz)
            };
            ctx.move_to(target_x, target_z, 6_000, SHUTTLE_ARRIVE).await;
        }
        if !outbound {
            // Back at the origin: pick a fresh heading, so the hold explores around the entry
            // position instead of pacing one line for the whole episode.
            heading += 1.1 + ctx.rng_next();
        }
        outbound = !outbound;
    }
}

/// Wait out the spawn fall before probing the world. At episode start the bot is still dropping
/// onto its spread position and the surrounding chunks may not be loaded, so an immediate
/// placement probe sees neither support nor air and the marker is lost for the whole episode.
pub(super) async fn settle_on_ground(ctx: &mut Ctx) {
    for _ in 0..40 {
        if ctx.on_ground() {
            break;
        }
        ctx.bot.wait_ticks(1).await;
    }
    ctx.pause().await;
}

/// Build a `height`-tall pillar of `material` near `desired`; report the base cell and the point
/// to aim at (the top block's centre). Superflat has no terrain features, so an eval script that
/// needs a fixed visual subject has to place one. The base cell comes back so the caller can probe
/// whether the subject survived an excursion.
pub(super) async fn raise_landmark(
    ctx: &mut Ctx,
    desired: (i64, i64, i64),
    height: i64,
    material: &str,
) -> Option<(BlockPos, (f64, f64, f64))> {
    let base = marker_base_near(ctx, desired, height)?;
    if !ctx.equip(material).await {
        return None;
    }
    let mut placed = 0i64;
    for step in 0..height {
        let cell = BlockPos::new(base.x, base.y + step as i32, base.z);
        if ctx.place_block(cell, material).await {
            placed += 1;
        }
    }
    if placed == 0 {
        return None;
    }
    Some((
        base,
        (
            base.x as f64 + 0.5,
            base.y as f64 + placed as f64 - 0.5,
            base.z as f64 + 0.5,
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::{MIN_PEER_GAP, point_at_yaw, ring_radius, station_ring};
    use crate::control::look_angles;

    /// `point_at_yaw` is a hand-derived inverse of `look_angles`, and a sign or 90 deg error in it
    /// would silently aim every eval turn at the wrong quarter of the world. Round-trip it.
    #[test]
    fn a_yaw_target_reads_back_as_the_same_yaw() {
        let eye = (12.5, 70.0, -33.5);
        for step in 0..24 {
            let want = -180.0 + step as f64 * 15.0;
            let (got, pitch) = look_angles(eye, point_at_yaw(eye, want, 24.0));
            let delta = (got - want + 540.0) % 360.0 - 180.0;
            assert!(delta.abs() < 1e-6, "want={want} got={got}");
            assert!(
                pitch.abs() < 1e-9,
                "level target must have zero pitch, got {pitch}"
            );
        }
        // Notchian convention, pinned explicitly: yaw 0 looks toward +Z, yaw 90 toward -X.
        let south = point_at_yaw(eye, 0.0, 10.0);
        assert!((south.2 - (eye.2 + 10.0)).abs() < 1e-9 && (south.0 - eye.0).abs() < 1e-9);
        let west = point_at_yaw(eye, 90.0, 10.0);
        assert!((west.0 - (eye.0 - 10.0)).abs() < 1e-9 && (west.2 - eye.2).abs() < 1e-9);
    }

    /// Every ring script spaces its bots evenly around the ring, and group_orbit puts one more in
    /// the middle. Adjacent slots are the tightest pair on the ring and the radius is the pivot's own
    /// distance; both have to clear the gap at every swarm size we record.
    #[test]
    fn ring_slots_clear_the_peer_gap_at_every_swarm_size() {
        for bots in 1..=32 {
            let radius = ring_radius(bots);
            let chord = 2.0 * radius * (std::f64::consts::PI / bots.max(2) as f64).sin();
            assert!(chord >= MIN_PEER_GAP - 1e-9, "bots={bots} chord={chord}");
            assert!(radius >= MIN_PEER_GAP, "bots={bots}: centre slot too close");
        }
        // Small swarms are floored rather than collapsed onto the anchor.
        assert!(ring_radius(2) >= 8.0);
    }

    /// The build and patrol scripts redraw their station every round, so two neighbours can both
    /// wander toward each other. The widened ring has to survive that worst case, not just the
    /// evenly spaced one.
    #[test]
    fn jittered_stations_clear_the_peer_gap_at_every_swarm_size() {
        let jitter = 2.0;
        for bots in 1..=32 {
            let radius = station_ring(bots, jitter);
            let slot_chord = 2.0 * radius * (std::f64::consts::PI / bots.max(2) as f64).sin();
            let worst = slot_chord - 2.0 * jitter;
            assert!(worst >= MIN_PEER_GAP - 1e-9, "bots={bots} worst={worst}");
        }
        // No jitter is the plain ring, not a wider one.
        assert_eq!(station_ring(8, 0.0), ring_radius(8));
    }
}
