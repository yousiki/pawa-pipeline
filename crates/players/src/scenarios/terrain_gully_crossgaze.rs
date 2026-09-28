//! CATALOG: family=locomotion min_players=1 single_process=false world_edits=false scenery=none far_players=false
//!
//! Find a dip in the ground, gaze across it at the far rim, descend into it, look back UP at both
//! rims from the floor, cross to the far rim, and come back.
//!
//! The revisit problem. The two rims are **mutual anchors**: everything framed from rim A is where
//! the bot will be standing two legs later, and from rim B the return gaze lands on the ground it
//! started from. So every long look in this script points at a region the recording has already
//! shown, and the cycle ends with rim A's opening view re-established from rim A itself.
//!
//! The occlusion is the dip's own shoulder. From the floor, the horizon closes on both sides and
//! the far rim is above the eye; from either rim, the floor is hidden behind the near shoulder.
//! Superflat has neither, which is exactly why it cannot host this test: there the view from the
//! "floor" and the view from the "rim" are the same picture.
//!
//! Terrain is generated, so the cross-section is *measured*, never assumed: a handful of bearings
//! are sampled at increasing range, each sample resolved to its topmost open-sky surface, and the
//! bearing with the deepest dip followed by the highest recovery wins. Degradation, all of it still
//! moving (standing still fails the frozen gate outright):
//!   1. a real dip -> descend, cross, return;
//!   2. no dip, but ground resolves -> the profile's own extremes stand in for floor and far rim,
//!      so the loop degenerates to a there-and-back over gentle ground (`dip_depth` records how
//!      little relief there was);
//!   3. the far rim is unreachable (a cliff the planner refuses) -> `travel_to` gives up on its own
//!      progress budget and the script keeps the sightline loop from where it stands, recorded as
//!      `crossed: false`. It does not keep shoving the wall.
use super::*;

/// Bearings probed per cycle, and the ranges sampled along each. Six by seven is 42 column scans —
/// enough to see a gully's shape at overworld scale without making the probe the episode's cost.
const BEARINGS: usize = 6;
const SAMPLE_STEP: f64 = 2.5;
const SAMPLES: usize = 7;

/// Vertical span a column scan searches around the bot's own altitude.
const SCAN_SPAN: i32 = 64;

/// Half-width of the sector the bearings are drawn from, radians. Shared across bots (the centre
/// comes from the episode seed) so a two-bot episode works the SAME dip and stays in frame; the
/// per-bot jitter inside it is what keeps their paths and timings apart.
const SECTOR_HALF: f64 = 1.0;

/// Dwell budget on a rim, and the shorter one used on the floor. The floor dwell is deliberately
/// half-length: it is the darkest, most enclosed part of the cycle and the darkness gate
/// (`max_dark_frame_frac` 0.25) has no patience for a bot that settles into a hole.
const DWELL_BASE_MS: i64 = 6_500;
const DWELL_STEP_MS: i64 = 1_500;

/// One bout inside a dwell: over 1 200 ms so `hold_visual`'s stall guard can cut a blocked one, and
/// far over the 500 ms (`SHORT_RUN_FRAMES`) key-chatter floor.
const BOUT_MIN_MS: i64 = 1_300;

/// Arrival tolerance for a travel leg — well under [`TRAVEL_HOP`].
const ARRIVE: f64 = 2.5;

/// Distance left over after a `travel_to` above which the target counts as unreached.
const CROSS_FAIL: f64 = 4.0;

/// Feet altitude of the topmost open-sky surface in column `(x, z)`, or `None` when the column is
/// not a route: water, lava, canopy, or an unloaded chunk. Scanning downward from above the bot's
/// own altitude makes the first support found the topmost by construction, so a sample can never
/// resolve to a cave floor and the measured profile is the daylight surface. Each new terrain
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
/// the bot moved). They are also where this script's frustum overlap comes from — a 4.65 s window
/// inside a dwell shares essentially its whole frame with its own first frame. `hold_facing` is
/// deliberately not used: its legs run under `move_to`, whose `CameraIntent::Hold` faces travel, so
/// the view flips 180 deg per leg — the very sweep v6 exists to remove.
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
            // Both directions are walls — likely, on a rim. Hand the body to the planner instead of
            // shoving terrain for the rest of the dwell; the frozen gate reads displacement, not
            // keys. One camera re-issue after the escape, never per loop.
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

/// A measured cross-section: the lowest point along one bearing and the highest ground beyond it.
struct Section {
    floor: (f64, f64),
    floor_y: i32,
    rim: (f64, f64),
    rim_y: i32,
    /// Blocks the floor sits below where the bot is standing. Zero on flat ground.
    depth: i32,
}

/// Sample bearings out of the shared sector and return the best cross-section: deepest dip, then
/// highest recovery beyond it. `None` only when no sample anywhere resolved to walkable ground.
fn measure_section(ctx: &mut Ctx, sector: f64) -> Option<Section> {
    let own = ctx.position()?;
    let y_hint = own.1.floor() as i32;
    let jitter = (ctx.rng_next() * 2.0 - 1.0) * SECTOR_HALF;
    let mut best: Option<(i32, Section)> = None;
    for slot in 0..BEARINGS {
        // Fan the bearings across the sector rather than drawing them independently: a gully is a
        // linear feature, and an even fan is far likelier to cut across one than six random draws.
        let bearing =
            sector + jitter + (slot as f64 / (BEARINGS - 1) as f64 - 0.5) * 2.0 * SECTOR_HALF;
        let (ux, uz) = (bearing.cos(), bearing.sin());
        let mut floor: Option<((f64, f64), i32)> = None;
        let mut rim: Option<((f64, f64), i32)> = None;
        for step in 1..=SAMPLES {
            let range = step as f64 * SAMPLE_STEP;
            let (x, z) = (own.0 + ux * range, own.2 + uz * range);
            let Some(feet_y) = surface_feet_y(ctx, x.floor() as i32, z.floor() as i32, y_hint)
            else {
                continue;
            };
            match floor {
                Some((_, low)) if feet_y >= low => {
                    // Past the low point: the highest ground beyond it is the far rim.
                    if rim.is_none_or(|(_, high)| feet_y > high) {
                        rim = Some(((x, z), feet_y));
                    }
                }
                _ => {
                    // A new low resets the far side: everything sampled so far was the near slope.
                    floor = Some(((x, z), feet_y));
                    rim = None;
                }
            }
        }
        // One barren bearing (all water, all canopy, unloaded chunks) must not abandon the probe —
        // the other bearings may still cut a perfectly good section.
        let Some((floor_xz, floor_y)) = floor else {
            continue;
        };
        // No recovery beyond the low point (a bot on a slope looking downhill): the low point is
        // then also the far anchor, which still gives a there-and-back loop.
        let (rim_xz, rim_y) = rim.unwrap_or((floor_xz, floor_y));
        let depth = y_hint - floor_y;
        let score = depth + (rim_y - floor_y);
        if best.as_ref().is_none_or(|(previous, _)| score > *previous) {
            best = Some((
                score,
                Section {
                    floor: floor_xz,
                    floor_y,
                    rim: rim_xz,
                    rim_y,
                    depth,
                },
            ));
        }
    }
    best.map(|(_, section)| section)
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
        &format!("vis-terrain-gully-init-{index}"),
        "Open facing the anchor before measuring a cross-section to cross.",
        json!({
            "global_index": global, "sector_rad": r2(sector), "dwell_ms": dwell_ms,
            "bearings": BEARINGS, "sample_reach": r2(SAMPLES as f64 * SAMPLE_STEP),
        }),
    );

    let mut cycle = 0u32;
    while !ctx.target_met() {
        let Some(rim_a) = ctx.position() else {
            sleep_ms(200).await;
            continue;
        };
        let rim_a_focus = eye_at(rim_a);
        let Some(section) = measure_section(ctx, sector) else {
            // Degradation level 3: not one sample resolved (water, canopy, unloaded chunks). Walk a
            // fixed leg on the sector bearing and back, so the body keeps moving and the loop shape
            // survives, then re-measure next cycle.
            let blind = (
                rim_a.0 + sector.cos() * SAMPLE_STEP * 3.0,
                rim_a.2 + sector.sin() * SAMPLE_STEP * 3.0,
            );
            travel_to(ctx, blind, TRAVEL_HOP, ARRIVE).await;
            ctx.look_until_facing(rim_a_focus, 8.0, 5_000).await;
            dwell_still(ctx, rim_a_focus, ctx.elapsed_ms() + dwell_ms).await;
            travel_to(ctx, (rim_a.0, rim_a.2), TRAVEL_HOP, ARRIVE).await;
            ctx.record(
                TYPE_MOVE,
                &format!("vis-terrain-gully-blind-{index}-{cycle}"),
                "No ground resolved along any bearing: walk a fixed leg out and back instead.",
                json!({"cycle": cycle, "probe_failed": true}),
            );
            cycle += 1;
            continue;
        };
        let rim_b_focus = (section.rim.0, section.rim_y as f64 + 1.4, section.rim.1);
        let floor_focus = (
            section.floor.0,
            section.floor_y as f64 + 1.0,
            section.floor.1,
        );

        // 1. Gaze across at the far rim. Re-established from this spot at the end of the cycle.
        let framed = ctx.look_until_facing(rim_b_focus, 8.0, 5_000).await;
        ctx.record(
            TYPE_LOOK,
            &format!("vis-terrain-gully-gaze-{index}-{cycle}"),
            "Gaze across the measured dip at the far rim before descending.",
            json!({
                "cycle": cycle, "framed": framed, "dip_depth": section.depth,
                "rim_rise": section.rim_y - section.floor_y,
                "rim": {"x": r2(section.rim.0), "z": r2(section.rim.1)},
                "floor": {"x": r2(section.floor.0), "z": r2(section.floor.1)},
            }),
        );
        dwell_still(ctx, rim_b_focus, ctx.elapsed_ms() + dwell_ms).await;
        if ctx.target_met() {
            break;
        }

        // 2. Descend to the floor and look UP at both rims in turn — the enclosed view, where the
        //    horizon is the dip's own shoulders. Half dwells: a hole is the one place in this
        //    script where the darkness gate has an opinion.
        travel_to(ctx, section.floor, TRAVEL_HOP, ARRIVE).await;
        let up_at_a = ctx.look_until_facing(rim_a_focus, 8.0, 5_000).await;
        dwell_still(ctx, rim_a_focus, ctx.elapsed_ms() + dwell_ms / 2).await;
        let up_at_b = ctx.look_until_facing(rim_b_focus, 8.0, 5_000).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-terrain-gully-floor-{index}-{cycle}"),
            "From the floor of the dip, look up at the rim behind and then the rim ahead.",
            json!({
                "cycle": cycle, "up_at_near_rim": up_at_a, "up_at_far_rim": up_at_b,
                "floor_y": ctx.position().map(|p| r2(p.1)),
            }),
        );
        dwell_still(ctx, rim_b_focus, ctx.elapsed_ms() + dwell_ms / 2).await;
        if ctx.target_met() {
            break;
        }

        // 3. Climb to the far rim. `travel_to` gives up on its own progress budget if the far side
        //    is a cliff, so an unreachable rim costs a bounded number of legs, never the episode.
        let remaining = travel_to(ctx, section.rim, TRAVEL_HOP, ARRIVE).await;
        let crossed = remaining <= CROSS_FAIL;
        // 4. Look back at rim A: from the far side, the opening vantage is the subject.
        let looked_back = ctx.look_until_facing(rim_a_focus, 8.0, 5_000).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-terrain-gully-cross-{index}-{cycle}"),
            "Cross to the far rim and look back at the rim the cycle started on.",
            json!({
                "cycle": cycle, "crossed": crossed, "remaining": r2(remaining),
                "looked_back": looked_back,
            }),
        );
        dwell_still(ctx, rim_a_focus, ctx.elapsed_ms() + dwell_ms).await;
        if ctx.target_met() {
            break;
        }

        // 5. Back across to rim A, and re-establish the opening gaze from it: same eye, same
        //    subject, a full crossing apart.
        let home = travel_to(ctx, (rim_a.0, rim_a.2), TRAVEL_HOP, ARRIVE).await;
        let reframed = ctx.look_until_facing(rim_b_focus, 8.0, 5_000).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-terrain-gully-return-{index}-{cycle}"),
            "Return to the near rim and re-establish the opening cross-gully gaze.",
            json!({
                "cycle": cycle, "reframed": reframed, "is_revisit": true,
                "distance_from_start": r2(home),
                "floor_focus": {"x": r2(floor_focus.0), "z": r2(floor_focus.2)},
                "elapsed_ms": ctx.elapsed_ms(),
            }),
        );
        dwell_still(ctx, rim_b_focus, ctx.elapsed_ms() + dwell_ms).await;
        cycle += 1;
    }
}
