//! CATALOG: family=locomotion min_players=1 single_process=false world_edits=false scenery=none far_players=false
//!
//! Walk a shoreline with the water on one flank and the land closing the other, keeping ONE point
//! out on the water as the persistent subject: it is re-framed from every station along the walk,
//! abandoned once for a look inland, and re-framed from the starting station at the end.
//!
//! The revisit problem, the parallax half. The other terrain scripts in this batch revisit a
//! subject after a change of heading or of elevation; this one revisits the SAME point from three
//! vantages spread along a line, so what the model has to carry is a region seen from genuinely
//! different angles rather than the same angle twice. **The sightline loops** on that point four
//! times per cycle.
//! **The trajectory loops**: the walk comes back along the shore to the station it started from.
//!
//! The occlusion is the asymmetry of a shore: out over the water the view runs to render distance
//! with nothing in it, inland it is closed within a few blocks by bank, trees and rising ground.
//! The inland look is a 180 deg turn off the water subject and therefore shares no pixels with it —
//! the turn-away that step 4's turn-back makes scorable.
//!
//! Terrain is generated, so the water is *hunted*, and no target is ever placed in it: this script
//! stays dry, and the land planner will not route through a fluid anyway (the lesson
//! `wade_column_detour` paid for). Degradation, none of it standing still (which fails the frozen
//! gate outright):
//!   1. water found -> its surface is the subject and the shore is its perpendicular;
//!   2. no water -> the LOWEST resolved ground stands in for it. A ridge edge has the same
//!      asymmetric flank as a beach, so the walk is unchanged (`water_found: false`);
//!   3. nothing resolves -> a fixed-length leg on the shared sector bearing and back until the next
//!      cycle re-probes (`probe_failed: true`).
use super::*;

/// Bearings and radii swept when hunting for water. Deterministic rather than random: the nearest
/// shore is what matters, and the sweep finds it in ~36 column scans.
const PROBE_BEARINGS: usize = 12;
const PROBE_RADII: [f64; 3] = [10.0, 18.0, 26.0];

/// Along-shore offsets of the stations, in blocks. Each hop is well over [`TRAVEL_HOP`], so a hop
/// is several planner legs of real travel rather than a corrective shuffle, and the walk stays
/// inside 14 blocks of the start so a window's displacement stays modest.
const STATION_OFFSETS: [f64; 3] = [0.0, 7.0, 14.0];

/// Vertical span a column scan searches around the bot's own altitude.
const SCAN_SPAN: i32 = 16;

/// How far inland the away-look is aimed. Far enough to be a horizon rather than a nearby block.
const INLAND_REACH: f64 = 20.0;

/// Dwell budget. Shorter than the other terrain scripts because this cycle has six dwells rather
/// than three; staggered per bot so two POVs never trace the same action stream.
const DWELL_BASE_MS: i64 = 4_500;
const DWELL_STEP_MS: i64 = 1_200;

/// One bout inside a dwell: over 1 200 ms so `hold_visual`'s stall guard can cut a blocked one, and
/// far over the 500 ms (`SHORT_RUN_FRAMES`) key-chatter floor.
const BOUT_MIN_MS: i64 = 1_300;

/// Arrival tolerance for a travel leg — well under [`TRAVEL_HOP`].
const ARRIVE: f64 = 2.0;

/// Feet altitude of the topmost open-sky LAND surface in column `(x, z)`, or `None` for water,
/// lava, canopy or an unloaded column. Scanning downward makes the first support the topmost one,
/// so a station is always the daylight surface. Each new terrain script carries its own probe —
/// `common.rs` is frozen for this batch, and this file needs the water-seeking twin below as well.
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

/// Y of the water surface in column `(x, z)`, or `None` when the first thing under the sky there is
/// not water. Depth is irrelevant here — unlike `terrain_shallow_wade_return` this script never
/// steps in, it only uses the surface as a subject to keep framing.
fn water_top_y(ctx: &Ctx, x: i32, z: i32, y_hint: i32) -> Option<i32> {
    for y in ((y_hint - SCAN_SPAN)..=(y_hint + SCAN_SPAN)).rev() {
        let cell = BlockPos::new(x, y, z);
        let id = ctx.block_id_at(cell)?;
        if block_is_air(ctx, cell) {
            continue;
        }
        return id.contains("water").then_some(y);
    }
    None
}

/// Dwell with the camera GENUINELY STILL and the body genuinely moving.
///
/// `hold_visual` pins `CameraIntent::LookAt` to the angles already held, once per bout, so no yaw
/// step is issued: those frames carry no `d_yaw` and are therefore not evidence under the
/// camera/travel-independence gate (`qc.rs:420` counts only frames where the camera turned while
/// the bot moved). They are also where the frustum overlap comes from — a window inside a dwell
/// shares essentially its whole frame with its own first frame. `hold_facing` is deliberately not
/// used: its legs run under `move_to`, whose `CameraIntent::Hold` faces travel, so the view flips
/// 180 deg per leg, which is the sweep v6 exists to remove.
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
            // Facing the water, "forward" walks into it and "backward" into the bank — the one
            // place in this script where both directions can block. Hand the body to the planner
            // instead of shoving terrain: the frozen gate reads displacement, not keys. One camera
            // re-issue after the escape, never per loop.
            if let Some((cx, _, cz)) = centre {
                travel_to(ctx, (cx, cz), TRAVEL_HOP, ARRIVE).await;
            }
            ctx.look_default(focus).await;
            stalls = 0;
        }
    }
}

/// The open flank this cycle walks: what to keep framing, and which way the shore runs.
struct Flank {
    /// The persistent subject — a point on the water surface (or, degraded, on low ground).
    landmark: (f64, f64, f64),
    /// Bearing from the bot toward the landmark: the open direction.
    open: f64,
    water: bool,
}

/// Sweep the ring for the nearest water surface; fall back to the lowest resolved land column.
fn find_flank(ctx: &mut Ctx, sector: f64) -> Option<Flank> {
    let own = ctx.position()?;
    let y_hint = own.1.floor() as i32;
    let jitter = ctx.rng_next() * std::f64::consts::TAU / PROBE_BEARINGS as f64;
    let mut lowest: Option<(f64, f64, i32, f64)> = None;
    for radius in PROBE_RADII {
        for slot in 0..PROBE_BEARINGS {
            // Start the sweep on the shared sector bearing so a two-bot episode converges on the
            // same shore; the per-bot jitter keeps the two off the identical column.
            let bearing =
                sector + jitter + slot as f64 * std::f64::consts::TAU / PROBE_BEARINGS as f64;
            let (x, z) = (
                own.0 + bearing.cos() * radius,
                own.2 + bearing.sin() * radius,
            );
            let (bx, bz) = (x.floor() as i32, z.floor() as i32);
            if let Some(top) = water_top_y(ctx, bx, bz, y_hint) {
                return Some(Flank {
                    landmark: (x, top as f64 + 0.5, z),
                    open: bearing,
                    water: true,
                });
            }
            if let Some(feet_y) = land_feet_y(ctx, bx, bz, y_hint)
                .filter(|&feet| lowest.is_none_or(|(_, _, best, _)| feet < best))
            {
                lowest = Some((x, z, feet_y, bearing));
            }
        }
    }
    // Degradation level 2: no water in range. The lowest ground nearby gives the same asymmetric
    // flank — open over the drop, closed behind.
    let (x, z, feet_y, bearing) = lowest?;
    Some(Flank {
        landmark: (x, feet_y as f64 + 1.0, z),
        open: bearing,
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
    // Which way along the shore this bot walks. Opposite hands for adjacent global indices, so two
    // bots share the subject and the shoreline but mirror each other's action streams — the
    // near-clone check reads exactly that agreement.
    let hand = if global.is_multiple_of(2) { 1.0 } else { -1.0 };

    // Open facing the anchor so peers are in frame from the first recorded frame rather than
    // intruding mid-window later.
    if let Some(own) = ctx.position() {
        ctx.look_until_facing((anchor.0, own.1 + 1.4, anchor.1), 8.0, 4_000)
            .await;
    }
    ctx.record(
        TYPE_LOOK,
        &format!("vis-terrain-shore-init-{index}"),
        "Open facing the anchor before hunting for a shoreline to walk.",
        json!({
            "global_index": global, "sector_rad": r2(sector), "dwell_ms": dwell_ms,
            "hand": hand, "stations": STATION_OFFSETS,
        }),
    );

    let mut cycle = 0u32;
    while !ctx.target_met() {
        let Some(start) = ctx.position() else {
            sleep_ms(200).await;
            continue;
        };
        let Some(flank) = find_flank(ctx, sector) else {
            // Degradation level 3: nothing resolved. Keep the body moving out and back, re-probe.
            let blind = (
                start.0 + sector.cos() * PROBE_RADII[0],
                start.2 + sector.sin() * PROBE_RADII[0],
            );
            travel_to(ctx, blind, TRAVEL_HOP, ARRIVE).await;
            let back = (start.0, start.1 + 1.4, start.2);
            ctx.look_until_facing(back, 8.0, 5_000).await;
            dwell_still(ctx, back, ctx.elapsed_ms() + dwell_ms).await;
            travel_to(ctx, (start.0, start.2), TRAVEL_HOP, ARRIVE).await;
            ctx.record(
                TYPE_MOVE,
                &format!("vis-terrain-shore-blind-{index}-{cycle}"),
                "No shore and no relief in range: walk a fixed leg out and back instead.",
                json!({"cycle": cycle, "probe_failed": true}),
            );
            cycle += 1;
            continue;
        };
        // The shore runs perpendicular to the open direction; `hand` picks which way along it.
        let along = flank.open + hand * std::f64::consts::FRAC_PI_2;

        for (station, offset) in STATION_OFFSETS.iter().copied().enumerate() {
            if ctx.target_met() {
                break;
            }
            // Station 0 is the start itself, so this leg is a no-op there and the loop needs no
            // special case for it.
            let wanted = (
                start.0 + along.cos() * offset,
                start.2 + along.sin() * offset,
            );
            // Snap to land: a station computed from one probe bearing can still land in the water
            // or on a boulder, and water is never a navigation target.
            let target = walkable_xz_near(ctx, wanted, start.1.floor() as i32).unwrap_or(wanted);
            let remaining = travel_to(ctx, target, TRAVEL_HOP, ARRIVE).await;

            // Re-frame the SAME point on the water from this station: the parallax revisit.
            let framed = ctx.look_until_facing(flank.landmark, 8.0, 5_000).await;
            ctx.record(
                TYPE_MOVE,
                &format!("vis-terrain-shore-station-{index}-{cycle}-{station}"),
                "Re-frame the water landmark from the next station along the shore.",
                json!({
                    "cycle": cycle, "station": station, "offset": offset,
                    "framed": framed, "remaining": r2(remaining),
                    "water_found": flank.water, "is_revisit": station > 0,
                    "landmark": {"x": r2(flank.landmark.0), "y": r2(flank.landmark.1),
                                 "z": r2(flank.landmark.2)},
                }),
            );
            dwell_still(ctx, flank.landmark, ctx.elapsed_ms() + dwell_ms).await;

            // At the middle station only, turn inland — a 180 deg swing off the landmark, past the
            // horizontal field of view — and then turn back to it. One turn-away per cycle keeps
            // the zero-overlap windows a minority while still making the return scorable.
            if station == 1 && !ctx.target_met() {
                let Some(here) = ctx.position() else { continue };
                let inland = (
                    here.0 - flank.open.cos() * INLAND_REACH,
                    here.1 + 1.4,
                    here.2 - flank.open.sin() * INLAND_REACH,
                );
                let turned = ctx.look_until_facing(inland, 10.0, 5_000).await;
                ctx.record(
                    TYPE_LOOK,
                    &format!("vis-terrain-shore-inland-{index}-{cycle}"),
                    "Turn inland, away from the water: the closed flank.",
                    json!({
                        "cycle": cycle, "framed": turned, "exceeds_horizontal_fov": true,
                    }),
                );
                dwell_still(ctx, inland, ctx.elapsed_ms() + dwell_ms).await;
                let returned = ctx.look_until_facing(flank.landmark, 8.0, 5_000).await;
                ctx.record(
                    TYPE_LOOK,
                    &format!("vis-terrain-shore-back-{index}-{cycle}"),
                    "Turn back to the water landmark after the inland look.",
                    json!({"cycle": cycle, "framed": returned, "is_return": true}),
                );
                dwell_still(ctx, flank.landmark, ctx.elapsed_ms() + dwell_ms).await;
            }
        }
        if ctx.target_met() {
            break;
        }

        // Walk back to the first station and re-frame the landmark from it: the same eye, the same
        // subject, a whole shoreline walk apart.
        let home = travel_to(ctx, (start.0, start.2), TRAVEL_HOP, ARRIVE).await;
        let reframed = ctx.look_until_facing(flank.landmark, 8.0, 5_000).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-terrain-shore-return-{index}-{cycle}"),
            "Walk back along the shore and re-frame the landmark from the opening station.",
            json!({
                "cycle": cycle, "reframed": reframed, "is_revisit": true,
                "distance_from_start": r2(home), "elapsed_ms": ctx.elapsed_ms(),
            }),
        );
        dwell_still(ctx, flank.landmark, ctx.elapsed_ms() + dwell_ms).await;
        cycle += 1;
    }
}
