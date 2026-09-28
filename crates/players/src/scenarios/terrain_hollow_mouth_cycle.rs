//! CATALOG: family=locomotion min_players=1 single_process=false world_edits=false scenery=none far_players=false
//!
//! Step from open ground into a roofed pocket of terrain — an overhang, an undercut, a cave mouth —
//! frame the bright opening from inside it, glance deeper, then step back out and re-frame the
//! mouth from the ground outside.
//!
//! The revisit problem. **The sightline loops**: the mouth is framed from outside, abandoned while
//! the bot is inside it, and framed again from the same outside vantage. The view from inside
//! pointing OUT is the strongest revisit in this batch — the opening is a hard-edged frame around
//! ground the recording has just shown, so a model that failed to keep it is obvious at a glance.
//! **The trajectory loops**: in and out is one short leg each way, so displacement stays tiny while
//! path length accumulates.
//!
//! The occlusion is total and it arrives in two steps: outside, the horizon runs to render
//! distance; inside, it is a rock roof three blocks up and a wall behind. Nothing on a Superflat
//! world can produce that transition at all.
//!
//! Two things bound this deliberately. Depth: the station is a pocket a few blocks from open sky,
//! never a cave interior — the darkness gate (`max_dark_frame_frac` 0.25, `max_dark_run_s` 20)
//! exists because early-round bots walked into unlit caves and filmed black, and scripts are
//! expected to stay out rather than lean on the gate. Time: the inward-facing dwell is half length
//! and it is the only part of the cycle pointing away from daylight.
//!
//! Terrain is generated, so a hollow is *searched for*, not assumed, and the fallback is a
//! first-class path rather than a limp:
//!   1. a roofed standing cell within reach -> enter it (`sheltered: true`);
//!   2. none found -> the tallest terrain wall nearby stands in for the roof. Standing at its foot
//!      closes the same flank the roof would have, and the in / look-out / out loop is unchanged
//!      (`sheltered: false`);
//!   3. neither resolves -> a fixed-length leg on the shared sector bearing and back, so the body
//!      keeps moving (standing still fails the frozen gate) until the next cycle re-probes.
use super::*;

/// Half-width, in blocks, of the neighbourhood searched for a hollow, and the stride the search
/// walks it with. A stride of 3 samples 49 columns — coarse enough to be cheap every cycle, fine
/// enough that an overhang wider than a couple of blocks cannot hide between samples.
const SEARCH_REACH: i32 = 9;
const SEARCH_STRIDE: i32 = 3;

/// Vertical band searched for a standing cell, relative to the bot's own feet. Wider than this and
/// the "hollow" is a different terrace, not a pocket in the ground the bot is on.
const SEARCH_LIFT: i32 = 2;

/// Roof heights that count as shelter, measured from the feet cell. Two is a crawl-height undercut,
/// four an airy overhang; above that the "roof" is a canopy that occludes nothing useful.
const ROOF_MIN: i32 = 2;
const ROOF_MAX: i32 = 4;

/// Rise that makes a terrain wall a usable stand-in for a roof, and how far short of it to stand.
/// Three blocks is above eye height, so the flank really is closed.
const WALL_RISE_MIN: i32 = 3;
const WALL_STANDOFF: f64 = 2.0;

/// Vertical span a column scan searches around the bot's own altitude.
const SCAN_SPAN: i32 = 16;

/// How far past the station the inward gaze is aimed.
const INWARD_REACH: f64 = 8.0;

/// Dwell budget, staggered per bot so two POVs never trace the same action stream.
const DWELL_BASE_MS: i64 = 6_000;
const DWELL_STEP_MS: i64 = 1_500;

/// One bout inside a dwell: over 1 200 ms so `hold_visual`'s stall guard can cut a blocked one, and
/// far over the 500 ms (`SHORT_RUN_FRAMES`) key-chatter floor.
const BOUT_MIN_MS: i64 = 1_300;

/// Arrival tolerance for a travel leg — well under [`TRAVEL_HOP`].
const ARRIVE: f64 = 2.0;

/// Feet altitude of the topmost open-sky surface in column `(x, z)`, or `None` when the column is
/// water, lava, canopy or unloaded. Used only for the WALL fallback, which wants daylight relief;
/// the hollow search below reads cells directly, because a roofed pocket is by definition not the
/// topmost thing in its column. Each new terrain script carries its own probe — `common.rs` is
/// frozen for this batch.
fn surface_feet_y(ctx: &Ctx, x: i32, z: i32, y_hint: i32) -> Option<i32> {
    let mut fluff = 0u32;
    for y in ((y_hint - SCAN_SPAN)..=(y_hint + SCAN_SPAN)).rev() {
        let cell = BlockPos::new(x, y, z);
        let id = ctx.block_id_at(cell)?;
        if block_is_safe_support(ctx, cell) {
            if id.contains("leaves") || id.ends_with("log") {
                return None;
            }
            return Some(y + 1);
        }
        if id.contains("water") || id.contains("lava") {
            return None;
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

/// Dwell with the camera GENUINELY STILL and the body genuinely moving.
///
/// `hold_visual` pins `CameraIntent::LookAt` to the angles already held, once per bout, so no yaw
/// step is issued: those frames carry no `d_yaw` and are therefore not evidence under the
/// camera/travel-independence gate (`qc.rs:420` counts only frames where the camera turned while
/// the bot moved). They are also where the frustum overlap comes from — a window inside a dwell
/// shares essentially its whole frame with its own first frame. `hold_facing` is deliberately not
/// used: its legs run under `move_to`, whose `CameraIntent::Hold` faces travel, so the view flips
/// 180 deg per leg — the sweep v6 exists to remove.
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
            // Inside a pocket both directions are wall within a couple of blocks, so this escape
            // matters more here than anywhere else in the batch: the frozen gate reads
            // displacement, not keys. One camera re-issue after the escape, never per loop.
            if let Some((cx, _, cz)) = centre {
                travel_to(ctx, (cx, cz), TRAVEL_HOP, ARRIVE).await;
            }
            ctx.look_default(focus).await;
            stalls = 0;
        }
    }
}

/// Where this cycle goes to have its view closed, and by what.
struct Station {
    /// Standing position.
    xz: (f64, f64),
    /// Eye-height point AT the station: what the open ground frames when it looks at the feature.
    focus: (f64, f64, f64),
    /// True when a real roof was found, false when a terrain wall is standing in for one.
    sheltered: bool,
    /// Roof height above the feet cell, or the wall's rise for the fallback. Reported, not used.
    cover: i32,
}

/// Search the neighbourhood for a standing cell with a rock roof [`ROOF_MIN`]..=[`ROOF_MAX`] blocks
/// overhead; failing that, for the foot of the tallest wall. `None` when neither resolves.
fn find_station(ctx: &Ctx, own: (f64, f64, f64)) -> Option<Station> {
    let y_hint = own.1.floor() as i32;
    let (bx, bz) = (own.0.floor() as i32, own.2.floor() as i32);
    let mut wall: Option<((f64, f64), i32)> = None;
    let mut nearest: Option<(f64, Station)> = None;
    let stride = SEARCH_STRIDE as usize;
    for dx in (-SEARCH_REACH..=SEARCH_REACH).step_by(stride) {
        for dz in (-SEARCH_REACH..=SEARCH_REACH).step_by(stride) {
            let (x, z) = (bx + dx, bz + dz);
            let distance = ((dx * dx + dz * dz) as f64).sqrt();
            // The wall fallback, gathered on the same sweep so a second scan is never needed.
            if let Some(feet_y) = surface_feet_y(ctx, x, z, y_hint).filter(|&feet| {
                feet - y_hint >= WALL_RISE_MIN && wall.is_none_or(|(_, best)| feet > best)
            }) {
                wall = Some(((x as f64 + 0.5, z as f64 + 0.5), feet_y));
            }
            for lift in -SEARCH_LIFT..=SEARCH_LIFT {
                let feet = BlockPos::new(x, y_hint + lift, z);
                if !block_is_air(ctx, feet)
                    || !block_is_air(ctx, feet.up(1))
                    || !block_is_safe_support(ctx, feet.down(1))
                {
                    continue;
                }
                let Some(roof) =
                    (ROOF_MIN..=ROOF_MAX).find(|&up| block_is_safe_support(ctx, feet.up(up)))
                else {
                    continue;
                };
                // Nearest hollow wins: the shortest leg into cover is the one least likely to walk
                // the bot somewhere the planner cannot get it back out of.
                if nearest.as_ref().is_none_or(|(best, _)| distance < *best) {
                    nearest = Some((
                        distance,
                        Station {
                            xz: (feet.x as f64 + 0.5, feet.z as f64 + 0.5),
                            focus: (
                                feet.x as f64 + 0.5,
                                feet.y as f64 + 1.4,
                                feet.z as f64 + 0.5,
                            ),
                            sheltered: true,
                            cover: roof,
                        },
                    ));
                }
                break;
            }
        }
    }
    if let Some((_, station)) = nearest {
        return Some(station);
    }
    // Degradation level 2: no roof anywhere. The foot of the tallest wall closes the same flank.
    let ((wx, wz), wall_y) = wall?;
    let (dx, dz) = (wx - own.0, wz - own.2);
    let span = dx.hypot(dz).max(0.001);
    let foot = (
        wx - dx / span * WALL_STANDOFF,
        wz - dz / span * WALL_STANDOFF,
    );
    let foot = walkable_xz_near(ctx, foot, y_hint).unwrap_or(foot);
    Some(Station {
        xz: foot,
        // Frame the wall face, not the empty air in front of it.
        focus: (wx, wall_y as f64 + 0.5, wz),
        sheltered: false,
        cover: wall_y - y_hint,
    })
}

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let anchor = ctx.anchor();
    let sector = ((ctx.engine().seed % 360) as f64).to_radians();
    let dwell_ms = DWELL_BASE_MS + (global % 3) as i64 * DWELL_STEP_MS;

    // Open facing the anchor so peers are in frame from the first recorded frame rather than
    // intruding mid-window later.
    if let Some(own) = ctx.position() {
        ctx.look_until_facing((anchor.0, own.1 + 1.4, anchor.1), 8.0, 4_000)
            .await;
    }
    ctx.record(
        TYPE_LOOK,
        &format!("vis-terrain-hollow-init-{index}"),
        "Open facing the anchor before searching for a roofed pocket of terrain.",
        json!({
            "global_index": global, "dwell_ms": dwell_ms, "search_reach": SEARCH_REACH,
            "roof_band": [ROOF_MIN, ROOF_MAX], "sector_rad": r2(sector),
        }),
    );

    let mut cycle = 0u32;
    while !ctx.target_met() {
        let Some(outside) = ctx.position() else {
            sleep_ms(200).await;
            continue;
        };
        let outside_focus = (outside.0, outside.1 + 1.4, outside.2);
        let Some(station) = find_station(ctx, outside) else {
            // Degradation level 3: neither a roof nor a wall. Keep the body moving on a fixed leg
            // out and back, and re-probe next cycle from wherever that ends.
            let blind = (
                outside.0 + sector.cos() * SEARCH_REACH as f64,
                outside.2 + sector.sin() * SEARCH_REACH as f64,
            );
            travel_to(ctx, blind, TRAVEL_HOP, ARRIVE).await;
            ctx.look_until_facing(outside_focus, 8.0, 5_000).await;
            dwell_still(ctx, outside_focus, ctx.elapsed_ms() + dwell_ms).await;
            travel_to(ctx, (outside.0, outside.2), TRAVEL_HOP, ARRIVE).await;
            ctx.record(
                TYPE_MOVE,
                &format!("vis-terrain-hollow-blind-{index}-{cycle}"),
                "Neither shelter nor wall in range: walk a fixed leg out and back instead.",
                json!({"cycle": cycle, "probe_failed": true}),
            );
            cycle += 1;
            continue;
        };

        // 1. Frame the mouth (or the wall) from open ground. Re-established from here at step 5.
        let framed = ctx.look_until_facing(station.focus, 8.0, 5_000).await;
        ctx.record(
            TYPE_LOOK,
            &format!("vis-terrain-hollow-outside-{index}-{cycle}"),
            "Frame the hollow's mouth from the open ground before entering it.",
            json!({
                "cycle": cycle, "framed": framed, "sheltered": station.sheltered,
                "cover": station.cover,
                "station": {"x": r2(station.xz.0), "z": r2(station.xz.1)},
            }),
        );
        dwell_still(ctx, station.focus, ctx.elapsed_ms() + dwell_ms).await;
        if ctx.target_met() {
            break;
        }

        // 2. Go in, turn round, and frame the opening: the bright rectangle of ground the recording
        //    was standing on a moment ago, seen through a hard terrain edge.
        let reached = travel_to(ctx, station.xz, TRAVEL_HOP, ARRIVE).await;
        let looked_out = ctx.look_until_facing(outside_focus, 8.0, 5_000).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-terrain-hollow-inside-{index}-{cycle}"),
            "From inside the hollow, look back out through the mouth at the open ground.",
            json!({
                "cycle": cycle, "looked_out": looked_out, "remaining": r2(reached),
                "sheltered": station.sheltered,
                "position": ctx.position()
                    .map(|p| json!({"x": r2(p.0), "y": r2(p.1), "z": r2(p.2)})),
            }),
        );
        dwell_still(ctx, outside_focus, ctx.elapsed_ms() + dwell_ms).await;
        if ctx.target_met() {
            break;
        }

        // 3. Glance deeper, away from daylight. Half length, and the only beat in the cycle facing
        //    away from the sky — this script does not film the inside of a cave.
        if let Some(here) = ctx.position() {
            let (dx, dz) = (here.0 - outside.0, here.2 - outside.2);
            let span = dx.hypot(dz).max(0.001);
            let inward = (
                here.0 + dx / span * INWARD_REACH,
                here.1 + 1.0,
                here.2 + dz / span * INWARD_REACH,
            );
            let looked_in = ctx.look_until_facing(inward, 10.0, 5_000).await;
            ctx.record(
                TYPE_LOOK,
                &format!("vis-terrain-hollow-deeper-{index}-{cycle}"),
                "Glance deeper into the hollow, away from the opening.",
                json!({"cycle": cycle, "framed": looked_in, "exceeds_horizontal_fov": true}),
            );
            dwell_still(ctx, inward, ctx.elapsed_ms() + dwell_ms / 2).await;
        }
        if ctx.target_met() {
            break;
        }

        // 4/5. Back out to the open vantage and re-frame the mouth from it: same eye, same subject,
        //      one immersion apart.
        let home = travel_to(ctx, (outside.0, outside.2), TRAVEL_HOP, ARRIVE).await;
        let reframed = ctx.look_until_facing(station.focus, 8.0, 5_000).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-terrain-hollow-return-{index}-{cycle}"),
            "Step back out and re-frame the mouth from the open ground.",
            json!({
                "cycle": cycle, "reframed": reframed, "is_revisit": true,
                "distance_from_start": r2(home), "elapsed_ms": ctx.elapsed_ms(),
            }),
        );
        dwell_still(ctx, station.focus, ctx.elapsed_ms() + dwell_ms).await;
        cycle += 1;
    }
}
