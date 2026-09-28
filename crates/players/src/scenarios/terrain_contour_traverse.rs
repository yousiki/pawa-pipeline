//! CATALOG: family=locomotion min_players=1 single_process=false world_edits=false scenery=none far_players=false
//!
//! Traverse a hillside along its contour — altitude roughly constant, the slope body permanently
//! closing one flank — then turn uphill into the closed view, turn back downhill into the open one,
//! and walk the contour back to where the traverse began.
//!
//! The revisit problem. **The sightline loops** twice: at the far end of the traverse the camera
//! turns from the open downhill view to the closed uphill view and back, a 180 deg swing that
//! necessarily exceeds the 102.5 deg horizontal field of view — so the return is a genuinely
//! unobserved-then- reobserved pair rather than a partial overlap. **The trajectory loops**: the
//! traverse comes back along the same contour, and the opening downhill view is re-established from
//! the opening spot.
//!
//! The occlusion is the slope itself, and it is asymmetric: uphill the horizon is a wall a few
//! blocks away, downhill it runs to the render distance. A flat world has no such asymmetry — every
//! bearing looks the same there, which is precisely the property that makes Superflat useless for
//! this test.
//!
//! Terrain is generated, so the slope is *measured*: a ring of bearings is sampled at one range and
//! resolved to its topmost open-sky surface, the steepest pair gives the gradient, and the contour
//! is the perpendicular. Degradation, none of it standing still (which fails the frozen gate):
//!   1. a real gradient -> traverse its contour;
//!   2. flat ground -> the measured extremes are noise, the perpendicular is arbitrary, and the
//!      script still walks a there-and-back leg with a turn away and back at the far end. `grade`
//!      records how little relief there was;
//!   3. the contour target does not resolve to walkable ground, or nothing resolves at all -> a
//!      fixed-length leg on the shared sector bearing and back (`probe_failed`).
use super::*;

/// Bearings sampled around the bot to find the gradient, and the range each is sampled at. One
/// range is enough: the gradient only needs a direction, and 7 blocks is far enough to clear the
/// bot's own hitbox-scale noise while staying inside terrain that is certainly loaded.
const BEARINGS: usize = 12;
const GRADIENT_RANGE: f64 = 7.0;

/// Length of one contour leg. Well over [`TRAVEL_HOP`] so the traverse is several planner legs of
/// genuine travel, and short enough that a 4.65 s window's displacement stays modest.
const CONTOUR_LEG: f64 = 11.0;

/// Vertical span a column scan searches around the bot's own altitude.
const SCAN_SPAN: i32 = 64;

/// How far along a look direction the framed point is placed. Beyond this the point is past the
/// render distance and "facing it" stops meaning anything visible.
const LOOK_REACH: f64 = 24.0;

/// Snap an X/Z goal to the measured topmost surface, never the current cave-floor altitude.
fn surface_target(ctx: &Ctx, desired: (f64, f64), hint: i32) -> Option<(f64, f64)> {
    let x = desired.0.floor() as i32;
    let z = desired.1.floor() as i32;
    let y = surface_feet_y(ctx, x, z, hint)?;
    walkable_xz_near(ctx, desired, y)
}

/// Dwell budget, staggered per bot so two POVs in one episode never trace the same action stream.
const DWELL_BASE_MS: i64 = 6_500;
const DWELL_STEP_MS: i64 = 1_500;

/// One bout inside a dwell: over 1 200 ms so `hold_visual`'s stall guard can cut a blocked one, and
/// far over the 500 ms (`SHORT_RUN_FRAMES`) key-chatter floor.
const BOUT_MIN_MS: i64 = 1_300;

/// Arrival tolerance for a travel leg — well under [`TRAVEL_HOP`].
const ARRIVE: f64 = 2.5;

/// Feet altitude of the topmost open-sky surface in column `(x, z)`, or `None` when the column is
/// not a route: water, lava, canopy or an unloaded chunk. Scanning downward makes the first support
/// found the topmost by construction, so a sample is always the daylight surface rather than a cave
/// floor. Each new terrain script carries its own copy — `common.rs` is frozen for this batch.
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
            // Facing uphill, "forward" is a wall by construction — this is the escape that keeps a
            // blocked dwell from reading as frozen (that gate reads displacement, not keys). One
            // camera re-issue after the escape, never per loop.
            if let Some((cx, _, cz)) = centre {
                travel_to(ctx, (cx, cz), TRAVEL_HOP, ARRIVE).await;
            }
            ctx.look_default(focus).await;
            stalls = 0;
        }
    }
}

/// The measured slope: which way is up, which way is down, and how much relief there is.
struct Slope {
    uphill: f64,
    downhill: f64,
    /// Blocks of altitude between the highest and lowest sample. Zero on flat ground.
    grade: i32,
}

/// Sample a ring of bearings and report the gradient. `None` only when fewer than two bearings
/// resolved, which means there is nothing here to call a slope.
fn measure_slope(ctx: &Ctx, sector: f64) -> Option<Slope> {
    let own = ctx.position()?;
    let y_hint = own.1.floor() as i32;
    // Phase the ring off the shared sector bearing so every bot samples the same set of directions
    // and a two-bot episode agrees about which way is uphill.
    let mut high: Option<(f64, i32)> = None;
    let mut low: Option<(f64, i32)> = None;
    for slot in 0..BEARINGS {
        let bearing = sector + slot as f64 * std::f64::consts::TAU / BEARINGS as f64;
        let (x, z) = (
            own.0 + bearing.cos() * GRADIENT_RANGE,
            own.2 + bearing.sin() * GRADIENT_RANGE,
        );
        let Some(feet_y) = surface_feet_y(ctx, x.floor() as i32, z.floor() as i32, y_hint) else {
            continue;
        };
        if high.is_none_or(|(_, best)| feet_y > best) {
            high = Some((bearing, feet_y));
        }
        if low.is_none_or(|(_, best)| feet_y < best) {
            low = Some((bearing, feet_y));
        }
    }
    let (uphill, high_y) = high?;
    let (downhill, low_y) = low?;
    Some(Slope {
        uphill,
        downhill,
        grade: high_y - low_y,
    })
}

/// A point `LOOK_REACH` blocks away on `bearing`, level with the eye — what `look_until_facing`
/// needs to be handed to turn by an ANGLE rather than toward a thing. Level on purpose: this script
/// turns in yaw, and letting pitch wander would confuse the open/closed contrast it measures.
fn point_on_bearing(eye: (f64, f64, f64), bearing: f64) -> (f64, f64, f64) {
    (
        eye.0 + bearing.cos() * LOOK_REACH,
        eye.1,
        eye.2 + bearing.sin() * LOOK_REACH,
    )
}

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let anchor = ctx.anchor();
    let sector = ((ctx.engine().seed % 360) as f64).to_radians();
    let dwell_ms = DWELL_BASE_MS + (global % 3) as i64 * DWELL_STEP_MS;
    // Which way round the contour this bot walks. Derived from the global index, so two bots on the
    // same hillside traverse it in opposite directions: same subject, mirrored action streams, and
    // they pass each other in frame rather than marching in lockstep (the near-clone check reads
    // exactly that agreement).
    let hand = if global.is_multiple_of(2) { 1.0 } else { -1.0 };

    // Open facing the anchor so peers are in frame from the first recorded frame rather than
    // intruding mid-window later.
    if let Some(own) = ctx.position() {
        ctx.look_until_facing((anchor.0, own.1 + 1.4, anchor.1), 8.0, 4_000)
            .await;
    }
    ctx.record(
        TYPE_LOOK,
        &format!("vis-terrain-contour-init-{index}"),
        "Open facing the anchor before measuring the local gradient.",
        json!({
            "global_index": global, "sector_rad": r2(sector), "dwell_ms": dwell_ms,
            "hand": hand, "contour_leg": CONTOUR_LEG,
        }),
    );

    let mut cycle = 0u32;
    while !ctx.target_met() {
        let Some(start) = ctx.position() else {
            sleep_ms(200).await;
            continue;
        };
        let eye = (start.0, start.1 + 1.4, start.2);
        let slope = measure_slope(ctx, sector);
        let probe_failed = slope.is_none();
        // Degradation level 3: nothing resolved. Treat the shared sector bearing as the contour so
        // the loop shape and the body's motion both survive until the next cycle re-measures.
        let slope = slope.unwrap_or(Slope {
            uphill: sector,
            downhill: sector + std::f64::consts::PI,
            grade: 0,
        });
        // The contour runs perpendicular to the gradient; `hand` picks which of the two ways along
        // it this bot takes.
        let contour = slope.uphill + hand * std::f64::consts::FRAC_PI_2;
        let downhill_focus = point_on_bearing(eye, slope.downhill);
        let uphill_focus = point_on_bearing(eye, slope.uphill);

        // 1. The open view: downhill, where the horizon runs to render distance. It is
        //    re-established from this same spot at the end of the cycle.
        let framed = ctx.look_until_facing(downhill_focus, 8.0, 5_000).await;
        ctx.record(
            TYPE_LOOK,
            &format!("vis-terrain-contour-open-{index}-{cycle}"),
            "Face downhill from the start of the traverse: the open flank.",
            json!({
                "cycle": cycle, "framed": framed, "grade": slope.grade,
                "probe_failed": probe_failed,
                "uphill_rad": r2(slope.uphill), "downhill_rad": r2(slope.downhill),
                "contour_rad": r2(contour),
            }),
        );
        dwell_still(ctx, downhill_focus, ctx.elapsed_ms() + dwell_ms).await;
        if ctx.target_met() {
            break;
        }

        // 2. Traverse the contour. The target is snapped to a genuinely standable cell: a contour
        //    computed from a 7-block gradient sample can still land in a hollow or on a boulder.
        let wanted = (
            start.0 + contour.cos() * CONTOUR_LEG,
            start.2 + contour.sin() * CONTOUR_LEG,
        );
        let target = surface_target(ctx, wanted, start.1.floor() as i32).unwrap_or(wanted);
        let remaining = travel_to(ctx, target, TRAVEL_HOP, ARRIVE).await;
        let drift = ctx.position().map(|p| p.1 - start.1).unwrap_or(0.0);

        // 3. Turn uphill: a 180 deg swing off the open view, so the closed view shares no pixels
        //    with it at all. This is the "turn away" the family owes the revisit problem — and step 4 is the
        //    turn back that makes it scorable.
        let closed = ctx.look_until_facing(uphill_focus, 8.0, 5_000).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-terrain-contour-closed-{index}-{cycle}"),
            "At the far end of the traverse, turn uphill into the flank the slope closes.",
            json!({
                "cycle": cycle, "framed": closed, "remaining": r2(remaining),
                "altitude_drift": r2(drift), "exceeds_horizontal_fov": true,
                "target": {"x": r2(target.0), "z": r2(target.1)},
            }),
        );
        dwell_still(ctx, uphill_focus, ctx.elapsed_ms() + dwell_ms).await;
        if ctx.target_met() {
            break;
        }

        // 4. Turn back downhill: the same open view as step 1, from a new place along the contour.
        let reopened = ctx.look_until_facing(downhill_focus, 8.0, 5_000).await;
        ctx.record(
            TYPE_LOOK,
            &format!("vis-terrain-contour-reopen-{index}-{cycle}"),
            "Turn back downhill: the open flank again, from further along the contour.",
            json!({"cycle": cycle, "framed": reopened, "is_return": true}),
        );
        dwell_still(ctx, downhill_focus, ctx.elapsed_ms() + dwell_ms / 2).await;
        if ctx.target_met() {
            break;
        }

        // 5. Walk the contour back and re-establish the opening view from the opening spot: same
        //    eye, same subject, a whole traverse apart.
        let home = travel_to(ctx, (start.0, start.2), TRAVEL_HOP, ARRIVE).await;
        let reframed = ctx.look_until_facing(downhill_focus, 8.0, 5_000).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-terrain-contour-return-{index}-{cycle}"),
            "Traverse back along the contour and re-establish the opening downhill view.",
            json!({
                "cycle": cycle, "reframed": reframed, "is_revisit": true,
                "distance_from_start": r2(home), "elapsed_ms": ctx.elapsed_ms(),
            }),
        );
        dwell_still(ctx, downhill_focus, ctx.elapsed_ms() + dwell_ms).await;
        cycle += 1;
    }
}
