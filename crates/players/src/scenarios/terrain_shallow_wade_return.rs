//! CATALOG: family=locomotion min_players=1 single_process=false world_edits=false scenery=none far_players=false
//!
//! Step off a shore into shallow water, turn round and look back at the bank just left, then wade
//! back out and re-frame the water from the bank.
//!
//! The revisit problem. **The sightline loops** twice per cycle: the water is framed from the bank,
//! left behind while the bot is standing IN it (where the eye sits a block lower and the water
//! surface cuts across the lower frame), and framed again from the bank afterwards. **The
//! trajectory loops**: the wade is four to six blocks out and the same back, so displacement stays
//! inside one leg while path length keeps accumulating.
//!
//! The occlusion here is the one a flat world cannot produce at all: entering water drops the eye
//! and the surface plane itself hides the near ground, and the shoreline is an asymmetric horizon —
//! open over the water, closed over the bank. That the same bank view is re-established after the
//! eye has been somewhere else is what makes the return scorable.
//!
//! Nothing assumes water exists. `shallow_water` probes columns for a water top with a solid floor
//! at most [`WADE_DEPTH_MAX`] blocks under it, so a found target is by construction waist-deep
//! rather than a lake to drown in, and every degradation still moves:
//!   1. shallow water found -> wade it;
//!   2. no water resolved -> the LOWEST resolved land column stands in for it, and the same
//!      descend / look-back / climb-out loop runs against a hollow instead of a pond
//!      (`water_found: false`);
//!   3. nothing resolves at all -> a fixed leg on the shared sector bearing and back
//!      (`probe_failed: true`).
//!
//! Water is never a navigation target: the land planner will not route through a fluid, and
//! `wade_column_detour` already paid for that lesson. Entering is an open-loop `hold_visual` bout
//! aimed at the water and verified with an `in_water` probe; leaving is a `travel_to` back to the
//! dry cell the script remembered, which the controller executes straight-line while submerged.
use super::*;

/// Bearings and radii probed when hunting for water. A ring sweep rather than random draws: the
/// nearest shore matters here, and a coarse deterministic sweep finds it in ~30 column scans.
const PROBE_BEARINGS: usize = 12;
const PROBE_RADII: [f64; 3] = [8.0, 14.0, 20.0];

/// Deepest water this script will step into. Two blocks is wading depth — the eye stays above the
/// surface and there is no drowning clock to race.
const WADE_DEPTH_MAX: i32 = 2;

/// How far short of the water's edge the dry staging cell sits, and how far the wade bout carries
/// the body past it. Both small on purpose: the point is the transition, not a swim.
const SHORE_STANDOFF: f64 = 3.0;
const WADE_BOUT_MS: u64 = 1_500;

/// Vertical span a column scan searches around the bot's own altitude.
const SCAN_SPAN: i32 = 64;

/// Dwell budget, staggered per bot so two POVs in one episode never trace the same action stream.
const DWELL_BASE_MS: i64 = 6_000;
const DWELL_STEP_MS: i64 = 1_500;

/// One side-step bout inside a dwell: over 1 200 ms so `hold_visual`'s stall guard can cut a
/// blocked one, and far over the 500 ms key-chatter floor.
const BOUT_MIN_MS: i64 = 1_300;

/// Arrival tolerance for a travel leg — well under [`TRAVEL_HOP`].
const ARRIVE: f64 = 2.0;

/// Feet altitude of the topmost open-sky LAND surface in column `(x, z)`, or `None` for water,
/// lava, canopy or an unloaded column. Scanning downward makes the first support the topmost one,
/// so a target can never resolve to a cave floor. Each new terrain script carries its own probe:
/// `common.rs` is frozen for this batch, and this file needs a water-SEEKING twin of it below.
fn land_feet_y(ctx: &Ctx, x: i32, z: i32, y_hint: i32) -> Option<i32> {
    let mut fluff = 0u32;
    for y in ((y_hint - SCAN_SPAN)..=(y_hint + SCAN_SPAN)).rev() {
        let cell = BlockPos::new(x, y, z);
        let id = ctx.block_id_at(cell)?;
        if id.contains("water") || id.contains("lava") {
            return None;
        }
        if block_is_safe_support(ctx, cell) {
            if id.contains("leaves") || id.ends_with("log") {
                return None;
            }
            return Some(y + 1);
        }
        if !block_is_air(ctx, cell) {
            fluff += 1;
            if fluff > 3 {
                return None;
            }
        }
    }
    None
}

/// Snap an X/Z goal to the measured topmost surface, never the current cave-floor altitude.
fn surface_target(ctx: &Ctx, desired: (f64, f64), hint: i32) -> Option<(f64, f64)> {
    let x = desired.0.floor() as i32;
    let z = desired.1.floor() as i32;
    let y = land_feet_y(ctx, x, z, hint)?;
    walkable_xz_near(ctx, desired, y)
}

/// Surface Y of column `(x, z)` when it is WATER no deeper than [`WADE_DEPTH_MAX`], else `None`.
///
/// The depth check is the safety property that lets this script step in at all: it walks down from
/// the water surface and insists on hitting a solid floor within the wading budget. A lake, an
/// ocean, or a column whose first non-air block is anything but water is refused.
fn shallow_water(ctx: &Ctx, x: i32, z: i32, y_hint: i32) -> Option<i32> {
    for y in ((y_hint - SCAN_SPAN)..=(y_hint + SCAN_SPAN)).rev() {
        let cell = BlockPos::new(x, y, z);
        let id = ctx.block_id_at(cell)?;
        if block_is_air(ctx, cell) {
            continue;
        }
        if !id.contains("water") {
            // The first thing under the sky is not water: either land (handled by `land_feet_y`)
            // or foliage sitting over it, neither of which is a wade.
            return None;
        }
        // Walk on down through the water and insist on a floor inside the wading budget.
        for depth in 1..=(WADE_DEPTH_MAX + 1) {
            let below = cell.down(depth);
            if block_is_safe_support(ctx, below) {
                return (depth <= WADE_DEPTH_MAX).then_some(y);
            }
            if !ctx
                .block_id_at(below)
                .is_some_and(|below_id| below_id.contains("water"))
            {
                return None;
            }
        }
        return None;
    }
    None
}

/// Whether the bot's feet cell is water right now — the only honest confirmation that an open-loop
/// wade bout actually got in. Same probe `wade_column_detour` uses.
fn in_water(ctx: &Ctx) -> bool {
    ctx.position()
        .and_then(|p| {
            ctx.block_id_at(BlockPos::new(
                p.0.floor() as i32,
                p.1.floor() as i32,
                p.2.floor() as i32,
            ))
        })
        .map(|id| id.contains("water"))
        .unwrap_or(false)
}

/// Dwell with the camera GENUINELY STILL and the body genuinely moving.
///
/// `hold_visual` pins `CameraIntent::LookAt` to the angles already held, once per bout, so no yaw
/// step is issued and those frames are not evidence under the camera/travel-independence gate
/// (`qc.rs:420` counts only frames where the camera turned while the bot moved). They are also
/// where the frustum overlap comes from: a window inside a dwell shares its whole frame with its
/// own first frame. `hold_facing` is deliberately not used — its legs run under `move_to`, whose
/// `CameraIntent::Hold` faces travel, so the view flips 180 deg per leg.
async fn dwell_still(ctx: &mut Ctx, focus: (f64, f64, f64), deadline_ms: i64) {
    let centre = ctx.position();
    let sprint_prob = ctx.cfg().sprint_bout_prob;
    let mut forward = ctx.rng_next() < 0.5;
    let mut stalls = 0u32;
    while !ctx.target_met() && ctx.elapsed_ms() + BOUT_MIN_MS < deadline_ms {
        let bout = BOUT_MIN_MS as u64 + (ctx.rng_next() * 600.0) as u64;
        let dir = if forward {
            WalkDirection::Forward
        } else {
            WalkDirection::Backward
        };
        // Draw sprint per forward bout. Not for speed: `qc.rs` fails a POV whose sustained (>= 1 s)
        // forward presses ALL agree on the sprint bit, and a dwell bout is long enough to be
        // sustained — a dwell that never sprinted would leave that bit inferable from "this bot is
        // dwelling". Backward bouts never sprint, because Minecraft only sprints forward.
        let sprint = forward && ctx.rng_next() < sprint_prob;
        if ctx.hold_visual(dir, sprint, bout, 0).await {
            stalls = 0;
        } else {
            stalls += 1;
        }
        forward = !forward;
        if stalls >= 2 {
            // Both directions blocked: hand the body to the planner instead of shoving terrain for
            // the rest of the dwell. One camera re-issue after the escape, never per loop.
            if let Some((cx, _, cz)) = centre {
                travel_to(ctx, (cx, cz), TRAVEL_HOP, ARRIVE).await;
            }
            ctx.look_default(focus).await;
            stalls = 0;
        }
    }
}

/// Eye-height point at a ground position.
fn eye_at(p: (f64, f64, f64)) -> (f64, f64, f64) {
    (p.0, p.1 + 1.4, p.2)
}

/// The target this cycle wades into (or, degraded, descends into) and whether it is really water.
struct Basin {
    /// Point to aim the wade bout at, and to frame from the bank.
    focus: (f64, f64, f64),
    /// Dry cell to stage on and to retreat to.
    shore: (f64, f64),
    water: bool,
}

/// Sweep the ring for the nearest shallow water; fall back to the lowest resolved land column.
fn find_basin(ctx: &mut Ctx, sector: f64) -> Option<Basin> {
    let own = ctx.position()?;
    let y_hint = own.1.floor() as i32;
    let jitter = ctx.rng_next() * std::f64::consts::TAU / PROBE_BEARINGS as f64;
    let mut lowest: Option<(f64, f64, i32)> = None;
    for radius in PROBE_RADII {
        for slot in 0..PROBE_BEARINGS {
            // Start the sweep on the shared sector bearing so a two-bot episode converges on the
            // same shore; the per-bot jitter keeps the two from picking the identical cell.
            let bearing =
                sector + jitter + slot as f64 * std::f64::consts::TAU / PROBE_BEARINGS as f64;
            let (x, z) = (
                own.0 + bearing.cos() * radius,
                own.2 + bearing.sin() * radius,
            );
            let (bx, bz) = (x.floor() as i32, z.floor() as i32);
            if let Some(top) = shallow_water(ctx, bx, bz, y_hint) {
                // Stage on dry ground short of the edge: water is never a navigation target.
                let shore = (
                    x - bearing.cos() * SHORE_STANDOFF,
                    z - bearing.sin() * SHORE_STANDOFF,
                );
                let shore = surface_target(ctx, shore, y_hint).unwrap_or(shore);
                return Some(Basin {
                    // Aim at the surface, not at eye height: the wade bout and the framing both
                    // want the water itself in the lower half of the frame.
                    focus: (x, top as f64 + 0.5, z),
                    shore,
                    water: true,
                });
            }
            if let Some(feet_y) = land_feet_y(ctx, bx, bz, y_hint)
                .filter(|&feet| lowest.is_none_or(|(_, _, best)| feet < best))
            {
                lowest = Some((x, z, feet_y));
            }
        }
    }
    // Degradation level 2: no water anywhere in range. The lowest ground nearby gives the same
    // descend / look-back / climb-out loop, minus the surface plane.
    let (x, z, feet_y) = lowest?;
    let shore = surface_target(ctx, (own.0, own.2), y_hint).unwrap_or((own.0, own.2));
    Some(Basin {
        focus: (x, feet_y as f64 + 0.5, z),
        shore,
        water: false,
    })
}

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let anchor = ctx.anchor();
    let sector = ((ctx.engine().seed % 360) as f64).to_radians();
    let dwell_ms = DWELL_BASE_MS + (global % 3) as i64 * DWELL_STEP_MS;

    // Open facing the anchor so peers are in frame from the first recorded frame instead of
    // intruding mid-window later.
    if let Some(own) = ctx.position() {
        ctx.look_until_facing((anchor.0, own.1 + 1.4, anchor.1), 8.0, 4_000)
            .await;
    }
    ctx.record(
        TYPE_LOOK,
        &format!("vis-terrain-wade-init-{index}"),
        "Open facing the anchor before hunting for a shallow shore to wade.",
        json!({
            "global_index": global, "sector_rad": r2(sector), "dwell_ms": dwell_ms,
            "wade_depth_max": WADE_DEPTH_MAX, "probe_radii": PROBE_RADII,
        }),
    );

    let mut cycle = 0u32;
    while !ctx.target_met() {
        let Some(home) = ctx.position() else {
            sleep_ms(200).await;
            continue;
        };
        let home_focus = eye_at(home);
        let basin = find_basin(ctx, sector);
        let probe_failed = basin.is_none();
        // Degradation level 3: nothing resolved at all. A fixed leg on the sector bearing and back
        // keeps the loop shape (and keeps the body moving, which the frozen gate insists on).
        let basin = basin.unwrap_or_else(|| {
            let blind = (
                home.0 + sector.cos() * PROBE_RADII[0],
                home.2 + sector.sin() * PROBE_RADII[0],
            );
            Basin {
                focus: (blind.0, home.1 + 1.0, blind.1),
                shore: blind,
                water: false,
            }
        });

        // 1. Stage on the bank and frame the water. Re-established at step 5 from the same spot.
        travel_to(ctx, basin.shore, TRAVEL_HOP, ARRIVE).await;
        let framed = ctx.look_until_facing(basin.focus, 8.0, 5_000).await;
        ctx.record(
            TYPE_LOOK,
            &format!("vis-terrain-wade-shore-{index}-{cycle}"),
            "Stage on the bank and frame the water before stepping in.",
            json!({
                "cycle": cycle, "framed": framed, "water_found": basin.water,
                "probe_failed": probe_failed,
                "shore": {"x": r2(basin.shore.0), "z": r2(basin.shore.1)},
                "focus": {"x": r2(basin.focus.0), "y": r2(basin.focus.1), "z": r2(basin.focus.2)},
            }),
        );
        dwell_still(ctx, basin.focus, ctx.elapsed_ms() + dwell_ms).await;
        if ctx.target_met() {
            break;
        }

        // 2. Step in. Open-loop and bounded: the planner refuses to route into a fluid, so the
        //    entry has to be a held key aimed at the water rather than a `move_to` target.
        let entered = ctx
            .hold_visual(WalkDirection::Forward, false, WADE_BOUT_MS, 0)
            .await;
        let wet = in_water(ctx);
        // 3. Turn round in the water and look back at the bank — the eye is a block lower and the
        //    surface plane cuts the frame, so this is the same subject from a genuinely new view.
        let looked_back = ctx.look_until_facing(home_focus, 8.0, 5_000).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-terrain-wade-enter-{index}-{cycle}"),
            "Step off the bank into the shallows, then look back at the bank.",
            json!({
                "cycle": cycle, "bout_completed": entered, "in_water": wet,
                "looked_back": looked_back,
                "position": ctx.position()
                    .map(|p| json!({"x": r2(p.0), "y": r2(p.1), "z": r2(p.2)})),
            }),
        );
        // Kept short whether or not the entry actually got wet: a long stay is a drowning clock,
        // and swimming is slow enough that the frozen gate has less margin than it does on land.
        dwell_still(ctx, home_focus, ctx.elapsed_ms() + dwell_ms / 2).await;
        if ctx.target_met() {
            break;
        }

        // 4. Wade back out to the remembered dry cell. `travel_to` works from inside water: the
        //    controller heads straight for the goal while submerged rather than following a
        //    corridor the land planner never built.
        let out = travel_to(ctx, basin.shore, TRAVEL_HOP, ARRIVE).await;
        // 5. Re-frame the water from the bank: same eye, same subject, one immersion apart.
        let reframed = ctx.look_until_facing(basin.focus, 8.0, 5_000).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-terrain-wade-out-{index}-{cycle}"),
            "Climb back out onto the bank and re-frame the water from it.",
            json!({
                "cycle": cycle, "reframed": reframed, "is_revisit": true,
                "still_in_water": in_water(ctx), "distance_from_shore": r2(out),
                "elapsed_ms": ctx.elapsed_ms(),
            }),
        );
        dwell_still(ctx, basin.focus, ctx.elapsed_ms() + dwell_ms).await;

        // 6. Close the trajectory on the cycle's own start, so the loop is a loop and not a drift
        //    along the shoreline.
        travel_to(ctx, (home.0, home.2), TRAVEL_HOP, ARRIVE).await;
        cycle += 1;
    }
}
