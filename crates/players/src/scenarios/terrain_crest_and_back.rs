//! CATALOG: family=locomotion min_players=1 single_process=false world_edits=false scenery=none far_players=false
//!
//! Climb to a nearby crest, look back down at the ground just left, then walk back down to it and
//! re-frame the crest from the original vantage.
//!
//! The revisit problem, both halves. **The trajectory loops**: every cycle ends where it started,
//! so a window's `path_length` is far larger than its `displacement` and the low ground is
//! genuinely revisited rather than merely left behind. **The sightline loops**: the crest is framed
//! at the start of a cycle, abandoned during the climb, and framed AGAIN from the same low vantage
//! at the end — two intervals of the same view with an excursion between them, which is what makes
//! the second one scorable against pixels the model has already seen.
//!
//! The occlusion comes from the terrain itself, which is why this cannot run on Superflat: while
//! the bot is on the low ground the crest body hides everything behind it, and the climb reveals
//! it. That reveal-and-reocclude is the content; a flat world would make every altitude look the
//! same and delete the whole point.
//!
//! Terrain is generated, so nothing here assumes a hill exists. The crest is *sampled*: candidate
//! columns are resolved to their topmost open-sky surface and scored by relief. Degradation is
//! layered, and no layer stands still — standing still fails the frozen gate outright:
//!   1. a candidate inside the wanted rise band wins;
//!   2. no candidate in band -> the highest resolved candidate wins whatever its rise (flat ground
//!      still yields a there-and-back loop, just without the vertical story), recorded as
//!      `in_band: false`;
//!   3. nothing resolves at all (water, canopy, unloaded chunks) -> walk a fixed-length leg on the
//!      shared sector bearing and come back, recorded as `probe_failed: true`.
use super::*;

/// Candidate columns drawn per cycle. Eight is enough for one to clear the rise band on ordinary
/// overworld relief while keeping the probe cost (a vertical scan per candidate) trivial.
const CANDIDATES: usize = 8;

/// Distance band the crest is drawn from. The near edge is the important one: a rise of up to
/// [`RISE_MAX`] seen from 12 blocks away sits at 27 deg of pitch, comfortably inside the vertical
/// field of view, so "facing the crest" actually frames it. Drawing crests closer would put them
/// above the frame while `look_until_facing` — which only verifies YAW — still reported success.
const RADIUS_MIN: f64 = 12.0;
const RADIUS_MAX: f64 = 18.0;

/// Rise band that reads as a climb on video. Below 3 blocks the altitude change is invisible;
/// above 6 the pitch needed to frame the crest from [`RADIUS_MIN`] starts leaving the frame.
const RISE_MIN: i32 = 3;
const RISE_MAX: i32 = 6;

/// Vertical span a column scan searches around the bot's own altitude.
const SCAN_SPAN: i32 = 20;

/// Half-width of the sector every bot draws its crests from, radians. All bots share the sector
/// centre (derived from the episode seed) so a two-bot episode climbs the SAME hillside and stays
/// in each other's frames; the per-bot RNG inside the sector is what keeps their paths and timings
/// apart for the near-clone check.
const SECTOR_HALF: f64 = 0.9;

/// Shortest dwell, and the per-bot step added on top of it. Dwells are where the high-overlap
/// windows come from (the camera is pinned, see [`dwell_still`]), so they carry most of the
/// episode's wall clock; staggering them by `global_index` keeps two bots in the same episode from
/// tracing the same action stream.
const DWELL_BASE_MS: i64 = 6_000;
const DWELL_STEP_MS: i64 = 1_500;

/// One side-step bout inside a dwell. Over 1 200 ms so `hold_visual`'s own stall guard can cut a
/// bout that walked into a wall, and far over the 500 ms (`SHORT_RUN_FRAMES` = 10 ticks) floor
/// under the key-chatter gate.
const BOUT_MIN_MS: i64 = 1_300;

/// Arrival tolerance for a travel leg. Well under [`TRAVEL_HOP`], and generous enough that the
/// final leg of a journey is a real press rather than a 30 cm correction.
const ARRIVE: f64 = 2.5;

/// A sampled surface waypoint: where to stand, and how high standing there is.
#[derive(Clone, Copy)]
struct Waypoint {
    x: f64,
    z: f64,
    feet_y: i32,
}

/// Feet altitude of the topmost OPEN-SKY surface in column `(x, z)`, or `None` when the column is
/// not a route. Scanning downward from above the bot's own altitude makes the first support found
/// the topmost one by construction, so a waypoint can never resolve to a cave floor. Water and
/// lava are refused (this script stays dry), canopy is refused (leaves and logs are not ground),
/// and a few blocks of grass, flowers or snow are tolerated above the real surface. Adapted from
/// `ridge_valley_circuit::skylit_feet_y` — `common.rs` is frozen for this batch, so each new
/// terrain script carries its own probe tuned to what it needs.
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

/// Dwell for a while with the camera GENUINELY STILL and the body genuinely moving.
///
/// `hold_visual` pins `CameraIntent::LookAt` to the angles the camera already holds, once per
/// bout, so no yaw step is ever issued: those frames carry `d_yaw == 0` and are therefore not
/// evidence at all under the camera/travel-independence gate (`qc.rs:420` only counts frames where
/// the camera actually turned while the bot was moving). They are also where this family's frustum
/// overlap comes from — a 4.65 s window that lands inside a dwell shares essentially its whole
/// frame with its own first frame.
///
/// `hold_facing` is the other validated hold shape and is deliberately NOT used here: its legs run
/// under `move_to`, which sets `CameraIntent::Hold`, and Hold faces travel — so the view flips 180
/// deg on every out-and-back leg. That flip is exactly the sweep v6 exists to remove.
///
/// The body alternates forward and backward along the pinned view, Solaris's 3-4 block
/// out-and-back primitive: net displacement stays near zero while path length keeps growing, which
/// is what the frozen gate (net displacement over a trailing second) wants to see.
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
            // Both directions are walls or ledges: hand the body to the planner rather than let it
            // shove terrain for the rest of the dwell. The frozen gate reads displacement, not
            // keys, so a blocked hold is indistinguishable from standing.
            if let Some((cx, _, cz)) = centre {
                travel_to(ctx, (cx, cz), TRAVEL_HOP, ARRIVE).await;
            }
            // One re-issue, only after an escape moved the camera (travel faces travel). This is
            // NOT the per-loop re-issue that pinned the camera/travel gate at 96-100%.
            ctx.look_default(focus).await;
            stalls = 0;
        }
    }
}

/// Eye-height point at a ground position — what a bot standing there would show a viewer.
fn eye_at(p: (f64, f64, f64)) -> (f64, f64, f64) {
    (p.0, p.1 + 1.4, p.2)
}

/// Draw crest candidates inside the shared sector and keep the ones that resolved to an open-sky
/// surface a real leg away. Returns the in-band pick first, else the highest resolved candidate.
fn draw_crest(ctx: &mut Ctx, sector: f64) -> Option<(Waypoint, bool)> {
    let own = ctx.position()?;
    let y_hint = own.1.floor() as i32;
    let mut best: Option<(Waypoint, bool, i32)> = None;
    for _ in 0..CANDIDATES {
        let bearing = sector + (ctx.rng_next() * 2.0 - 1.0) * SECTOR_HALF;
        let radius = RADIUS_MIN + ctx.rng_next() * (RADIUS_MAX - RADIUS_MIN);
        let (x, z) = (
            own.0 + bearing.cos() * radius,
            own.2 + bearing.sin() * radius,
        );
        let Some(feet_y) = surface_feet_y(ctx, x.floor() as i32, z.floor() as i32, y_hint) else {
            continue;
        };
        let rise = feet_y - y_hint;
        let in_band = (RISE_MIN..=RISE_MAX).contains(&rise);
        let candidate = (Waypoint { x, z, feet_y }, in_band, rise);
        // In-band beats out-of-band; among equals, more relief wins. This is the whole preference
        // order, so degradation level 2 (highest available rise) needs no separate code path.
        let better = match &best {
            None => true,
            Some((_, best_in_band, best_rise)) => (in_band, rise) > (*best_in_band, *best_rise),
        };
        if better {
            best = Some(candidate);
        }
    }
    best.map(|(waypoint, in_band, _)| (waypoint, in_band))
}

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let anchor = ctx.anchor();
    // The sector every bot draws crests from. Shared and seed-derived: no coordination needed, and
    // a two-bot episode works the same hillside instead of walking apart on private bearings.
    let sector = ((ctx.engine().seed % 360) as f64).to_radians();
    let dwell_ms = DWELL_BASE_MS + (global % 3) as i64 * DWELL_STEP_MS;

    // Open facing the anchor, so the swarm's other bots are in frame from the FIRST recorded frame
    // rather than intruding mid-window later (the 22.4% defect in the v5 audit).
    if let Some(own) = ctx.position() {
        let toward_anchor = (anchor.0, own.1 + 1.4, anchor.1);
        ctx.look_until_facing(toward_anchor, 8.0, 4_000).await;
    }
    ctx.record(
        TYPE_LOOK,
        &format!("vis-terrain-crest-init-{index}"),
        "Open facing the anchor before sampling a crest to climb.",
        json!({
            "global_index": global,
            "sector_rad": r2(sector),
            "dwell_ms": dwell_ms,
            "rise_band": [RISE_MIN, RISE_MAX],
            "radius_band": [RADIUS_MIN, RADIUS_MAX],
        }),
    );

    let mut cycle = 0u32;
    while !ctx.target_met() {
        let Some(home) = ctx.position() else {
            sleep_ms(200).await;
            continue;
        };
        let home_focus = eye_at(home);
        let picked = draw_crest(ctx, sector);

        let (crest_xz, crest_focus, in_band, rise) = match picked {
            Some((waypoint, in_band)) => (
                (waypoint.x, waypoint.z),
                (waypoint.x, waypoint.feet_y as f64 + 1.4, waypoint.z),
                in_band,
                waypoint.feet_y - home.1.floor() as i32,
            ),
            None => {
                // Degradation level 3: nothing resolved. Walk a fixed leg on the sector bearing
                // and back anyway — the loop shape survives, only the vertical story is lost.
                let blind = (
                    home.0 + sector.cos() * RADIUS_MIN,
                    home.2 + sector.sin() * RADIUS_MIN,
                );
                (blind, (blind.0, home.1 + 1.4, blind.1), false, 0)
            }
        };
        let probe_failed = picked.is_none();

        // 1. Frame the crest from the low ground. This view is re-established at the end of the
        //    cycle, from this same spot — that pair is the revisit being scored.
        let framed = ctx.look_until_facing(crest_focus, 8.0, 5_000).await;
        ctx.record(
            TYPE_LOOK,
            &format!("vis-terrain-crest-frame-{index}-{cycle}"),
            "Frame the sampled crest from the low ground before climbing it.",
            json!({
                "cycle": cycle, "framed": framed, "in_band": in_band, "rise": rise,
                "probe_failed": probe_failed,
                "crest": {"x": r2(crest_xz.0), "z": r2(crest_xz.1)},
            }),
        );
        dwell_still(ctx, crest_focus, ctx.elapsed_ms() + dwell_ms).await;
        if ctx.target_met() {
            break;
        }

        // 2. Climb.
        let climbed = travel_to(ctx, crest_xz, TRAVEL_HOP, ARRIVE).await;
        let arrived_y = ctx.position().map(|p| p.1).unwrap_or(home.1);
        // 3. Turn back and look DOWN at the ground just left: the far side of the same sightline.
        let looked_back = ctx.look_until_facing(home_focus, 8.0, 5_000).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-terrain-crest-climb-{index}-{cycle}"),
            "Climb the crest, then look back down at the ground the climb started from.",
            json!({
                "cycle": cycle, "looked_back": looked_back,
                "remaining_to_crest": r2(climbed),
                "climbed_blocks": r2(arrived_y - home.1),
                "home": {"x": r2(home.0), "z": r2(home.2)},
            }),
        );
        dwell_still(ctx, home_focus, ctx.elapsed_ms() + dwell_ms).await;
        if ctx.target_met() {
            break;
        }

        // 4. Walk back down to where the cycle started: the trajectory closes here.
        let returned = travel_to(ctx, (home.0, home.2), TRAVEL_HOP, ARRIVE).await;
        // 5. Re-frame the crest from the ORIGINAL vantage. Same eye, same subject, one excursion
        //    apart — the window whose ground truth is entirely already-observed.
        let reframed = ctx.look_until_facing(crest_focus, 8.0, 5_000).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-terrain-crest-return-{index}-{cycle}"),
            "Return to the low vantage and re-frame the crest from it.",
            json!({
                "cycle": cycle, "reframed": reframed, "is_revisit": true,
                "distance_from_home": r2(returned),
                "elapsed_ms": ctx.elapsed_ms(),
            }),
        );
        dwell_still(ctx, crest_focus, ctx.elapsed_ms() + dwell_ms).await;
        cycle += 1;
    }
}
