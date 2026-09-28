//! CATALOG: family=locomotion min_players=1 single_process=false world_edits=false scenery=none far_players=false
//!
//! Shuttle between a high shelf and a low one, looking DOWN at the low shelf from above and UP at
//! the high shelf from below, and close the loop back on the vantage the cycle started from.
//!
//! The revisit problem, with pitch as the variable the rest of this batch leaves alone. Every long
//! look in the cycle points at one of two places the bot itself stands in, so the subject of a look
//! is always ground the recording has shown — and each subject is seen from above and from below,
//! so the model is asked to keep a region across a change of ELEVATION, not merely of heading.
//! **The trajectory loops**: high, low, back to the start, every cycle. **The sightline loops**:
//! the high shelf is framed at step 1 and re-framed from the same spot at step 4.
//!
//! The occlusion comes from the step itself: standing on the low shelf, the shelf's own lip hides
//! everything behind the high one; standing on the high shelf, the low one is the only ground in
//! view and the horizon is far. Superflat has one elevation and therefore neither view.
//!
//! `look_until_facing` verifies YAW only, so the geometry has to keep pitch inside the frame rather
//! than trusting the check: candidates are drawn from [`RADIUS_MIN`] blocks out or further, and a
//! step of the size this script looks for is then at most ~30 deg off level. The requested pitch is
//! recorded on every look so the audit can confirm it.
//!
//! Terrain is generated, so the two shelves are *sampled*, never assumed. Degradation, all of it
//! still moving (standing still fails the frozen gate outright):
//!   1. two resolved candidates with real relief -> shuttle the step;
//!   2. relief but no separation, or no relief at all -> the two extremes of whatever resolved are
//!      used anyway and `step_height` records how flat it was: the loop and the look pair survive;
//!   3. fewer than two candidates resolve -> a fixed-length leg on the shared sector bearing and
//!      back until the next cycle re-probes.
use super::*;

/// Candidate columns drawn per cycle.
const CANDIDATES: usize = 10;

/// Distance band candidates are drawn from. The near edge is what keeps a step inside the frame:
/// [`STEP_MAX`] blocks of rise seen from 9 blocks away is 30 deg of pitch, comfortably inside the
/// vertical field of view.
const RADIUS_MIN: f64 = 9.0;
const RADIUS_MAX: f64 = 15.0;

/// Rise above which a step stops being framable from [`RADIUS_MIN`]. Not a filter — candidates are
/// never rejected for being too tall, since the shuttle is worth having either way — but the band
/// this geometry was chosen for, and what `step_height` is read against.
const STEP_MAX: i32 = 5;

/// Least separation between the two shelves for the shuttle to read as travel rather than a step
/// aside. Below it the second shelf is re-picked as the candidate furthest from the first.
const MIN_SEPARATION: f64 = 6.0;

/// Vertical span a column scan searches around the bot's own altitude.
const SCAN_SPAN: i32 = 20;

/// Half-width of the sector candidates are drawn from, radians. The centre is shared (seed-derived)
/// so a two-bot episode shuttles the SAME pair of shelves and stays in frame; the per-bot RNG
/// inside the sector is what keeps their paths and timings apart.
const SECTOR_HALF: f64 = 1.0;

/// Dwell budget, staggered per bot so two POVs never trace the same action stream.
const DWELL_BASE_MS: i64 = 6_000;
const DWELL_STEP_MS: i64 = 1_500;

/// One bout inside a dwell: over 1 200 ms so `hold_visual`'s stall guard can cut a blocked one, and
/// far over the 500 ms (`SHORT_RUN_FRAMES`) key-chatter floor.
const BOUT_MIN_MS: i64 = 1_300;

/// Arrival tolerance for a travel leg — well under [`TRAVEL_HOP`].
const ARRIVE: f64 = 2.5;

/// A sampled shelf: where to stand and how high standing there is.
#[derive(Clone, Copy)]
struct Shelf {
    x: f64,
    z: f64,
    feet_y: i32,
}

impl Shelf {
    /// Eye-height point on this shelf — what a bot standing on it would show a viewer.
    fn focus(&self) -> (f64, f64, f64) {
        (self.x, self.feet_y as f64 + 1.4, self.z)
    }
}

/// Feet altitude of the topmost open-sky surface in column `(x, z)`, or `None` when the column is
/// water, lava, canopy or unloaded. Scanning downward makes the first support found the topmost by
/// construction, so a shelf is always a daylight surface and never a cave floor. Each new terrain
/// script carries its own copy — `common.rs` is frozen for this batch.
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
/// shares essentially its whole frame with its own first frame. Pitch survives a dwell too, which
/// matters more here than anywhere else in the batch: the pinned angle is the full look direction,
/// so a down-pitched stare stays down-pitched while the body works. `hold_facing` is deliberately
/// not used: its legs run under `move_to`, whose `CameraIntent::Hold` faces travel, which flattens
/// pitch and flips yaw 180 deg per leg.
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
            // On a shelf, one of the two directions is a drop and the other a wall. Hand the body
            // to the planner rather than shove terrain for the rest of the dwell: the frozen gate
            // reads displacement, not keys. One camera re-issue after the escape, never per loop.
            if let Some((cx, _, cz)) = centre {
                travel_to(ctx, (cx, cz), TRAVEL_HOP, ARRIVE).await;
            }
            ctx.look_default(focus).await;
            stalls = 0;
        }
    }
}

/// Draw candidates in the shared sector and return the highest and the lowest, re-picking the
/// second if the two landed on top of each other. `None` when fewer than two resolved.
fn sample_shelves(ctx: &mut Ctx, sector: f64) -> Option<(Shelf, Shelf)> {
    let own = ctx.position()?;
    let y_hint = own.1.floor() as i32;
    let mut found: Vec<Shelf> = Vec::with_capacity(CANDIDATES);
    for _ in 0..CANDIDATES {
        let bearing = sector + (ctx.rng_next() * 2.0 - 1.0) * SECTOR_HALF;
        let radius = RADIUS_MIN + ctx.rng_next() * (RADIUS_MAX - RADIUS_MIN);
        let (x, z) = (
            own.0 + bearing.cos() * radius,
            own.2 + bearing.sin() * radius,
        );
        if let Some(feet_y) = surface_feet_y(ctx, x.floor() as i32, z.floor() as i32, y_hint) {
            found.push(Shelf { x, z, feet_y });
        }
    }
    if found.len() < 2 {
        return None;
    }
    let high = *found.iter().max_by_key(|shelf| shelf.feet_y)?;
    let low = *found.iter().min_by_key(|shelf| shelf.feet_y)?;
    if (high.x - low.x).hypot(high.z - low.z) >= MIN_SEPARATION {
        return Some((high, low));
    }
    // Flat ground: every candidate resolves to the same altitude, so "highest" and "lowest" can be
    // neighbours. Keep the shuttle a real journey by taking the furthest candidate instead — the
    // pitch story is gone but the loop and the look pair are not.
    let far = found
        .iter()
        .max_by(|a, b| {
            (a.x - high.x)
                .hypot(a.z - high.z)
                .total_cmp(&(b.x - high.x).hypot(b.z - high.z))
        })
        .copied()?;
    Some((high, far))
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
        &format!("vis-terrain-step-init-{index}"),
        "Open facing the anchor before sampling a pair of shelves to shuttle.",
        json!({
            "global_index": global, "sector_rad": r2(sector), "dwell_ms": dwell_ms,
            "radius_band": [RADIUS_MIN, RADIUS_MAX], "step_max": STEP_MAX,
        }),
    );

    let mut cycle = 0u32;
    while !ctx.target_met() {
        let Some(start) = ctx.position() else {
            sleep_ms(200).await;
            continue;
        };
        let Some((high, low)) = sample_shelves(ctx, sector) else {
            // Degradation level 3: fewer than two candidates resolved. Keep the body moving on a
            // fixed leg out and back, then re-probe.
            let blind = (
                start.0 + sector.cos() * RADIUS_MIN,
                start.2 + sector.sin() * RADIUS_MIN,
            );
            travel_to(ctx, blind, TRAVEL_HOP, ARRIVE).await;
            let back = (start.0, start.1 + 1.4, start.2);
            ctx.look_until_facing(back, 8.0, 5_000).await;
            dwell_still(ctx, back, ctx.elapsed_ms() + dwell_ms).await;
            travel_to(ctx, (start.0, start.2), TRAVEL_HOP, ARRIVE).await;
            ctx.record(
                TYPE_MOVE,
                &format!("vis-terrain-step-blind-{index}-{cycle}"),
                "Fewer than two shelves resolved: walk a fixed leg out and back instead.",
                json!({"cycle": cycle, "probe_failed": true}),
            );
            cycle += 1;
            continue;
        };
        let step_height = high.feet_y - low.feet_y;
        let (high_focus, low_focus) = (high.focus(), low.focus());

        // 1. Frame the high shelf from the starting vantage. Re-established from here at step 4.
        let framed = ctx.look_until_facing(high_focus, 8.0, 5_000).await;
        ctx.record(
            TYPE_LOOK,
            &format!("vis-terrain-step-frame-{index}-{cycle}"),
            "Frame the high shelf from the vantage the cycle starts on.",
            json!({
                "cycle": cycle, "framed": framed, "step_height": step_height,
                "within_step_max": step_height <= STEP_MAX,
                "high": {"x": r2(high.x), "z": r2(high.z), "feet_y": high.feet_y},
                "low": {"x": r2(low.x), "z": r2(low.z), "feet_y": low.feet_y},
            }),
        );
        dwell_still(ctx, high_focus, ctx.elapsed_ms() + dwell_ms).await;
        if ctx.target_met() {
            break;
        }

        // 2. Climb the high shelf and look DOWN at the low one.
        travel_to(ctx, (high.x, high.z), TRAVEL_HOP, ARRIVE).await;
        let down_pitch = ctx
            .position()
            .map(|p| look_angles(head_of(p), low_focus).1)
            .unwrap_or(0.0);
        let looked_down = ctx.look_until_facing(low_focus, 8.0, 5_000).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-terrain-step-down-{index}-{cycle}"),
            "From the high shelf, look down at the low one.",
            json!({
                "cycle": cycle, "framed": looked_down, "pitch_deg": r2(down_pitch),
                "arrived_y": ctx.position().map(|p| r2(p.1)),
            }),
        );
        dwell_still(ctx, low_focus, ctx.elapsed_ms() + dwell_ms).await;
        if ctx.target_met() {
            break;
        }

        // 3. Drop to the low shelf and look UP at the high one: the same subject as step 1, from
        //    below instead of alongside.
        travel_to(ctx, (low.x, low.z), TRAVEL_HOP, ARRIVE).await;
        let up_pitch = ctx
            .position()
            .map(|p| look_angles(head_of(p), high_focus).1)
            .unwrap_or(0.0);
        let looked_up = ctx.look_until_facing(high_focus, 8.0, 5_000).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-terrain-step-up-{index}-{cycle}"),
            "From the low shelf, look up at the high one.",
            json!({
                "cycle": cycle, "framed": looked_up, "pitch_deg": r2(up_pitch),
                "is_return": true, "arrived_y": ctx.position().map(|p| r2(p.1)),
            }),
        );
        dwell_still(ctx, high_focus, ctx.elapsed_ms() + dwell_ms).await;
        if ctx.target_met() {
            break;
        }

        // 4. Back to the starting vantage and re-establish the opening view: same eye, same
        //    subject, a whole shuttle apart.
        let home = travel_to(ctx, (start.0, start.2), TRAVEL_HOP, ARRIVE).await;
        let reframed = ctx.look_until_facing(high_focus, 8.0, 5_000).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-terrain-step-return-{index}-{cycle}"),
            "Return to the starting vantage and re-establish the opening view of the high shelf.",
            json!({
                "cycle": cycle, "reframed": reframed, "is_revisit": true,
                "distance_from_start": r2(home), "elapsed_ms": ctx.elapsed_ms(),
            }),
        );
        dwell_still(ctx, high_focus, ctx.elapsed_ms() + dwell_ms).await;
        cycle += 1;
    }
}
