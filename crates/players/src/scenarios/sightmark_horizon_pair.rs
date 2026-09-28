//! CATALOG: family=locomotion min_players=1 single_process=false world_edits=false scenery=none far_players=false
//!
//! Resolve two lateral natural horizon landmarks, alternate which one is framed from the other's
//! station, and return to the common opening hub. This covers the revisit problem's paired sightline loop:
//! A and B are both visible at the hub, each becomes the other's remembered reference after a move,
//! and the final hub view restores the original pair. It is deliberately a lateral pair, not a depth
//! stack or a constructed-marker circuit.
//!
//! Both landmarks and their stations come from `surface_y` and `walkable_xz_near`; generated terrain is
//! never assumed to contain a particular biome. If either side cannot be sampled, the script uses two
//! four-block fallback offsets and records `probe_failed`, while still traversing A -> B -> hub. The
//! 11-block radius, 65-degree separation, 4-block hops, 0.8-block arrival, and 1.4-second dwell keep
//! the pair in one loaded neighbourhood, make the views meaningfully different, and satisfy movement
//! QC. All camera turns are issued while stationary and use `look_until_facing`.

use super::*;

const LANDMARK_RADIUS: f64 = 11.0;
const SIDE_SEPARATION_DEG: f64 = 65.0;
const SLOTS: usize = 12;
const SCAN_SPAN: i32 = 20;
const LEG: f64 = 4.0;
const ARRIVE: f64 = 0.8;
const DWELL_MS: u64 = 1_400;

fn surface_y(ctx: &Ctx, x: i32, z: i32, hint: i32) -> Option<i32> {
    for y in ((hint - SCAN_SPAN)..=(hint + SCAN_SPAN)).rev() {
        let cell = BlockPos::new(x, y, z);
        let id = ctx.block_id_at(cell)?;
        if id.contains("water")
            || id.contains("lava")
            || id.contains("leaves")
            || id.ends_with("log")
        {
            // Skip vegetation/liquid in this column and keep searching for the real ground below.
            continue;
        }
        if block_is_safe_support(ctx, cell) {
            return Some(y + 1);
        }
    }
    None
}

struct Pair {
    stations: [(f64, f64); 2],
    focuses: [(f64, f64, f64); 2],
    heights: [i32; 2],
}

fn probe(ctx: &Ctx, sector: f64) -> Option<Pair> {
    let own = ctx.position()?;
    let hint = own.1.floor() as i32;
    for slot in 0..SLOTS {
        let base = sector + slot as f64 * std::f64::consts::TAU / SLOTS as f64;
        let bearings = [
            base - SIDE_SEPARATION_DEG.to_radians() / 2.0,
            base + SIDE_SEPARATION_DEG.to_radians() / 2.0,
        ];
        let mut stations = [(0.0, 0.0); 2];
        let mut focuses = [(0.0, 0.0, 0.0); 2];
        let mut heights = [0; 2];
        let mut valid = true;
        for (i, bearing) in bearings.into_iter().enumerate() {
            let x = own.0 + bearing.cos() * LANDMARK_RADIUS;
            let z = own.2 + bearing.sin() * LANDMARK_RADIUS;
            let Some(y) = surface_y(ctx, x.floor() as i32, z.floor() as i32, hint) else {
                valid = false;
                break;
            };
            let Some(station) = walkable_xz_near(ctx, (x, z), y) else {
                valid = false;
                break;
            };
            stations[i] = station;
            focuses[i] = (x, y as f64 + 0.5, z);
            heights[i] = y;
        }
        if valid {
            return Some(Pair {
                stations,
                focuses,
                heights,
            });
        }
    }
    None
}

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let anchor = ctx.anchor();
    let sector = ((ctx.engine().seed % 360) as f64 + global as f64 * 13.0).to_radians();
    let mut cycle = 0usize;

    while !ctx.target_met() {
        let Some(home) = ctx.position() else {
            sleep_ms(200).await;
            continue;
        };
        let found = probe(ctx, sector);
        let probe_failed = found.is_none();
        let pair = found.unwrap_or_else(|| {
            let a = (home.0 + sector.cos() * LEG, home.2 + sector.sin() * LEG);
            let b = (home.0 - sector.sin() * LEG, home.2 + sector.cos() * LEG);
            Pair {
                stations: [a, b],
                focuses: [(anchor.0, home.1 + 1.4, anchor.1); 2],
                heights: [home.1.floor() as i32; 2],
            }
        });
        ctx.record(
            TYPE_MOVE,
            &format!("vis-sightmark-horizon-pair-setup-{index}-{cycle}"),
            "Resolve two lateral natural horizon landmarks around the opening hub.",
            json!({"global_index": global, "cycle": cycle, "probe_failed": probe_failed, "radius": LANDMARK_RADIUS, "separation_deg": SIDE_SEPARATION_DEG, "heights": pair.heights}),
        );

        let hub_a = ctx.look_until_facing(pair.focuses[0], 7.0, 5_000).await;
        ctx.record(
            TYPE_LOOK,
            &format!("vis-sightmark-horizon-hub-a-{index}-{cycle}"),
            "Frame the first natural horizon point from the common hub.",
            json!({"cycle": cycle, "landmark": "a", "faced": hub_a}),
        );
        ctx.still(DWELL_MS).await;
        if ctx.target_met() {
            break;
        }
        let hub_b = ctx.look_until_facing(pair.focuses[1], 7.0, 5_000).await;
        ctx.record(
            TYPE_LOOK,
            &format!("vis-sightmark-horizon-hub-b-{index}-{cycle}"),
            "Frame the second natural horizon point from the same hub.",
            json!({"cycle": cycle, "landmark": "b", "faced": hub_b}),
        );
        ctx.still(DWELL_MS).await;
        if ctx.target_met() {
            break;
        }

        let a_distance = travel_to(ctx, pair.stations[0], TRAVEL_HOP, ARRIVE).await;
        let from_a = ctx.look_until_facing(pair.focuses[1], 7.0, 5_000).await;
        ctx.record(TYPE_LOOK, &format!("vis-sightmark-horizon-from-a-{index}-{cycle}"), "From A, re-frame the lateral B landmark as a cross-check.", json!({"cycle": cycle, "from": "a", "target": "b", "faced": from_a, "arrival_distance": r2(a_distance)}));
        ctx.still(DWELL_MS).await;
        if ctx.target_met() {
            break;
        }

        let b_distance = travel_to(ctx, pair.stations[1], TRAVEL_HOP, ARRIVE).await;
        let from_b = ctx.look_until_facing(pair.focuses[0], 7.0, 5_000).await;
        ctx.record(TYPE_LOOK, &format!("vis-sightmark-horizon-from-b-{index}-{cycle}"), "From B, reverse the cross-check toward A.", json!({"cycle": cycle, "from": "b", "target": "a", "faced": from_b, "arrival_distance": r2(b_distance)}));
        ctx.still(DWELL_MS).await;
        if ctx.target_met() {
            break;
        }

        let return_distance = travel_to(ctx, (home.0, home.2), TRAVEL_HOP, ARRIVE).await;
        let hub_return = ctx.look_until_facing(pair.focuses[0], 7.0, 5_000).await;
        ctx.record(TYPE_LOOK, &format!("vis-sightmark-horizon-return-{index}-{cycle}"), "Return to the hub and restore the first landmark relationship.", json!({"cycle": cycle, "is_revisit": true, "faced": hub_return, "return_distance": r2(return_distance)}));
        ctx.still(DWELL_MS).await;
        cycle += 1;
    }
}
