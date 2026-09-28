//! CATALOG: family=locomotion min_players=1 single_process=false world_edits=false scenery=none far_players=false
//!
//! Observe one naturally elevated landmark from two lateral stations, chosen to differ in both azimuth
//! and ground height, then return to the first station. This addresses the revisit problem's viewpoint closure:
//! the landmark is not merely revisited after a heading change, but compared from two sides of the same
//! object before the initial azimuth and height are restored. The route is a compact A -> B -> A loop.
//!
//! A candidate crest and two observer stations are resolved from sampled solid columns; no biome or
//! hill is presumed. The probe prefers a one-block station-height difference and records a degraded
//! pair when only lateral separation is available. If all probes fail, two four-block offsets still
//! move and return. The 9-block landmark radius, 6-block station offset, 90-degree lateral baseline,
//! 4-block hops, 0.8-block arrival, and 1.4-second dwell keep the subject visible, motion QC-safe,
//! and pauses live. Gaze changes are made only after travel has stopped.

use super::*;

const LANDMARK_RADII: [f64; 3] = [8.0, 11.0, 14.0];
const STATION_OFFSET: f64 = 6.0;
const SLOTS: usize = 12;
const SCAN_SPAN: i32 = 20;
const LEG: f64 = 4.0;
const ARRIVE: f64 = 0.8;
const DWELL_MS: u64 = 1_400;
const PREFERRED_HEIGHT_DELTA: i32 = 1;

fn surface_y(ctx: &Ctx, x: i32, z: i32, hint: i32) -> Option<i32> {
    for y in ((hint - SCAN_SPAN)..=(hint + SCAN_SPAN)).rev() {
        let cell = BlockPos::new(x, y, z);
        let id = ctx.block_id_at(cell)?;
        if id.contains("water")
            || id.contains("lava")
            || id.contains("leaves")
            || id.ends_with("log")
        {
            // Keep scanning below foliage/liquid for a genuine walkable surface.
            continue;
        }
        if block_is_safe_support(ctx, cell) {
            return Some(y + 1);
        }
    }
    None
}

struct ViewPair {
    first: (f64, f64),
    second: (f64, f64),
    focus: (f64, f64, f64),
    first_y: i32,
    second_y: i32,
    landmark_y: i32,
}

fn probe(ctx: &Ctx, sector: f64) -> Option<ViewPair> {
    let own = ctx.position()?;
    let hint = own.1.floor() as i32;
    let mut best: Option<ViewPair> = None;
    for slot in 0..SLOTS {
        let bearing = sector + slot as f64 * std::f64::consts::TAU / SLOTS as f64;
        for radius in LANDMARK_RADII {
            let lx = own.0 + bearing.cos() * radius;
            let lz = own.2 + bearing.sin() * radius;
            let Some(landmark_y) = surface_y(ctx, lx.floor() as i32, lz.floor() as i32, hint)
            else {
                continue;
            };
            let a_wanted = (
                lx - bearing.cos() * STATION_OFFSET,
                lz - bearing.sin() * STATION_OFFSET,
            );
            let Some(a_y) = surface_y(
                ctx,
                a_wanted.0.floor() as i32,
                a_wanted.1.floor() as i32,
                hint,
            ) else {
                continue;
            };
            let b_wanted = (
                lx + bearing.sin() * STATION_OFFSET,
                lz - bearing.cos() * STATION_OFFSET,
            );
            let Some(b_y) = surface_y(
                ctx,
                b_wanted.0.floor() as i32,
                b_wanted.1.floor() as i32,
                hint,
            ) else {
                continue;
            };
            let Some(first) = walkable_xz_near(ctx, a_wanted, a_y) else {
                continue;
            };
            let Some(second) = walkable_xz_near(ctx, b_wanted, b_y) else {
                continue;
            };
            let candidate = ViewPair {
                first,
                second,
                focus: (lx, landmark_y as f64 + 0.5, lz),
                first_y: a_y,
                second_y: b_y,
                landmark_y,
            };
            if (a_y - b_y).abs() >= PREFERRED_HEIGHT_DELTA {
                return Some(candidate);
            }
            if best.is_none() {
                best = Some(candidate);
            }
        }
    }
    best
}

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let anchor = ctx.anchor();
    let sector = ((ctx.engine().seed % 360) as f64 + global as f64 * 29.0).to_radians();
    if let Some(p) = ctx.position() {
        ctx.look_until_facing((anchor.0, p.1 + 1.4, anchor.1), 8.0, 4_000)
            .await;
    }
    let mut cycle = 0usize;

    while !ctx.target_met() {
        let Some(home) = ctx.position() else {
            sleep_ms(200).await;
            continue;
        };
        let found = probe(ctx, sector);
        let probe_failed = found.is_none();
        let pair = found.unwrap_or_else(|| {
            let first = (home.0 + sector.cos() * LEG, home.2 + sector.sin() * LEG);
            let second = (home.0 - sector.sin() * LEG, home.2 + sector.cos() * LEG);
            ViewPair {
                first,
                second,
                focus: (anchor.0, home.1 + 1.4, anchor.1),
                first_y: home.1.floor() as i32,
                second_y: home.1.floor() as i32,
                landmark_y: home.1.floor() as i32,
            }
        });
        ctx.record(
            TYPE_MOVE,
            &format!("vis-landmark-azimuth-setup-{index}-{cycle}"),
            "Select two lateral stations with measured natural height difference.",
            json!({"global_index": global, "cycle": cycle, "probe_failed": probe_failed, "landmark_y": pair.landmark_y, "first_y": pair.first_y, "second_y": pair.second_y, "height_delta": (pair.first_y - pair.second_y).abs(), "preferred_height_delta": PREFERRED_HEIGHT_DELTA}),
        );

        let first_distance = travel_to(ctx, pair.first, TRAVEL_HOP, ARRIVE).await;
        let first_faced = ctx.look_until_facing(pair.focus, 7.0, 5_000).await;
        ctx.record(
            TYPE_LOOK,
            &format!("vis-landmark-azimuth-first-{index}-{cycle}"),
            "Frame the natural landmark from the first azimuth and elevation.",
            json!({"cycle": cycle, "station": "first", "faced": first_faced, "arrival_distance": r2(first_distance)}),
        );
        ctx.still(DWELL_MS).await;
        if ctx.target_met() {
            break;
        }

        let second_distance = travel_to(ctx, pair.second, TRAVEL_HOP, ARRIVE).await;
        let second_faced = ctx.look_until_facing(pair.focus, 7.0, 5_000).await;
        ctx.record(
            TYPE_LOOK,
            &format!("vis-landmark-azimuth-second-{index}-{cycle}"),
            "Re-frame the same landmark from the lateral station.",
            json!({"cycle": cycle, "station": "second", "faced": second_faced, "arrival_distance": r2(second_distance), "height_delta": (pair.first_y - pair.second_y).abs()}),
        );
        ctx.still(DWELL_MS).await;
        if ctx.target_met() {
            break;
        }

        let return_distance = travel_to(ctx, pair.first, TRAVEL_HOP, ARRIVE).await;
        let returned = ctx.look_until_facing(pair.focus, 7.0, 5_000).await;
        ctx.record(
            TYPE_LOOK,
            &format!("vis-landmark-azimuth-return-{index}-{cycle}"),
            "Return to the first station and restore the original landmark view.",
            json!({"cycle": cycle, "is_revisit": true, "faced": returned, "return_distance": r2(return_distance)}),
        );
        ctx.still(DWELL_MS).await;
        cycle += 1;
    }
}
