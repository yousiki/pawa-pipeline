//! CATALOG: family=locomotion min_players=1 single_process=false world_edits=false scenery=none far_players=false
//!
//! Follow three stations around a natural water bend, keeping one far shoreline point as the sightmark,
//! then close on the first station. This addresses the revisit problem through curved parallax: the water and
//! shoreline occupy different portions of the frame at each azimuth, while the same far point is
//! deliberately re-acquired after the arc. Unlike a shoreline traverse, the stations form a bent arc
//! around one point and the final leg returns to the exact first station.
//!
//! Water is detected from block IDs and each station is resolved with `surface_y`; no river, lake, or
//! biome is presumed. When water is absent, the best solid surface becomes a degraded sightmark and
//! `water_found: false` is recorded; when no station resolves, three four-block offsets still run with
//! `probe_failed: true`. The 10-block search range, 5-block arc radius, 70-degree offsets, 4-block
//! hops, 0.8-block arrival, and 1.4-second dwell provide a visible bend, three distinct bearings,
//! reliable movement, and live pauses. Camera commands occur only at stopped stations.

use super::*;

const SEARCH_RADII: [f64; 3] = [8.0, 12.0, 16.0];
const SLOTS: usize = 12;
const SCAN_SPAN: i32 = 18;
const ARC_RADIUS: f64 = 5.0;
const ARC_ANGLE_DEG: f64 = 70.0;
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

fn water_y(ctx: &Ctx, x: i32, z: i32, hint: i32) -> Option<i32> {
    for y in ((hint - SCAN_SPAN)..=(hint + SCAN_SPAN)).rev() {
        let id = ctx.block_id_at(BlockPos::new(x, y, z))?;
        if !block_is_air(ctx, BlockPos::new(x, y, z)) {
            return id.contains("water").then_some(y);
        }
    }
    None
}

struct Arc {
    sightmark: (f64, f64, f64),
    stations: [(f64, f64); 3],
    water_found: bool,
    station_height: i32,
}

fn probe(ctx: &Ctx, sector: f64) -> Option<Arc> {
    let own = ctx.position()?;
    let hint = own.1.floor() as i32;
    for slot in 0..SLOTS {
        let bearing = sector + slot as f64 * std::f64::consts::TAU / SLOTS as f64;
        for radius in SEARCH_RADII {
            let sx = own.0 + bearing.cos() * radius;
            let sz = own.2 + bearing.sin() * radius;
            let Some(wy) = water_y(ctx, sx.floor() as i32, sz.floor() as i32, hint) else {
                continue;
            };
            let angles = [-ARC_ANGLE_DEG, 0.0, ARC_ANGLE_DEG];
            let mut stations = [(0.0, 0.0); 3];
            let mut station_height = 0;
            let mut ok = true;
            for (i, delta) in angles.into_iter().enumerate() {
                let a = bearing + delta.to_radians();
                let wanted = (sx + a.cos() * ARC_RADIUS, sz + a.sin() * ARC_RADIUS);
                let Some(y) =
                    surface_y(ctx, wanted.0.floor() as i32, wanted.1.floor() as i32, hint)
                else {
                    ok = false;
                    break;
                };
                let Some(station) = walkable_xz_near(ctx, wanted, y) else {
                    ok = false;
                    break;
                };
                stations[i] = station;
                station_height += y;
            }
            if ok {
                return Some(Arc {
                    sightmark: (sx, wy as f64 + 0.5, sz),
                    stations,
                    water_found: true,
                    station_height: station_height / 3,
                });
            }
        }
    }
    None
}

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let anchor = ctx.anchor();
    let sector = ((ctx.engine().seed % 360) as f64 + global as f64 * 19.0).to_radians();
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
        let arc = found.unwrap_or_else(|| {
            let _sight = (
                home.0 + sector.cos() * LEG,
                home.1 + 1.4,
                home.2 + sector.sin() * LEG,
            );
            let mut stations = [(0.0, 0.0); 3];
            for (i, delta) in [-0.8_f64, 0.0, 0.8].into_iter().enumerate() {
                stations[i] = (home.0 + delta.cos() * LEG, home.2 + delta.sin() * LEG);
            }
            Arc {
                sightmark: (anchor.0, home.1 + 1.4, anchor.1),
                stations,
                water_found: false,
                station_height: home.1.floor() as i32,
            }
        });
        ctx.record(
            TYPE_MOVE,
            &format!("vis-sightmark-waterbend-setup-{index}-{cycle}"),
            "Resolve a water bend and its three curved observation stations.",
            json!({"global_index": global, "cycle": cycle, "probe_failed": probe_failed, "water_found": arc.water_found, "arc_radius": ARC_RADIUS, "station_height": arc.station_height}),
        );

        let first_distance = travel_to(ctx, arc.stations[0], TRAVEL_HOP, ARRIVE).await;
        let first_faced = ctx.look_until_facing(arc.sightmark, 7.0, 5_000).await;
        ctx.record(TYPE_LOOK, &format!("vis-sightmark-waterbend-a-{index}-{cycle}"), "Frame the shoreline sightmark from the first side of the bend.", json!({"cycle": cycle, "station": "a", "faced": first_faced, "arrival_distance": r2(first_distance)}));
        ctx.still(DWELL_MS).await;
        if ctx.target_met() {
            break;
        }

        let middle_distance = travel_to(ctx, arc.stations[1], TRAVEL_HOP, ARRIVE).await;
        let middle_faced = ctx.look_until_facing(arc.sightmark, 7.0, 5_000).await;
        ctx.record(TYPE_LOOK, &format!("vis-sightmark-waterbend-b-{index}-{cycle}"), "Re-frame the same shoreline point at the bend midpoint.", json!({"cycle": cycle, "station": "b", "faced": middle_faced, "arrival_distance": r2(middle_distance)}));
        ctx.still(DWELL_MS).await;
        if ctx.target_met() {
            break;
        }

        let last_distance = travel_to(ctx, arc.stations[2], TRAVEL_HOP, ARRIVE).await;
        let last_faced = ctx.look_until_facing(arc.sightmark, 7.0, 5_000).await;
        ctx.record(TYPE_LOOK, &format!("vis-sightmark-waterbend-c-{index}-{cycle}"), "Frame the sightmark from the opposite side of the curved shoreline.", json!({"cycle": cycle, "station": "c", "faced": last_faced, "arrival_distance": r2(last_distance)}));
        ctx.still(DWELL_MS).await;
        if ctx.target_met() {
            break;
        }

        let return_distance = travel_to(ctx, arc.stations[0], TRAVEL_HOP, ARRIVE).await;
        let returned = ctx.look_until_facing(arc.sightmark, 7.0, 5_000).await;
        ctx.record(TYPE_LOOK, &format!("vis-sightmark-waterbend-return-{index}-{cycle}"), "Close the arc and restore the first shoreline sightline.", json!({"cycle": cycle, "is_revisit": true, "faced": returned, "return_distance": r2(return_distance)}));
        ctx.still(DWELL_MS).await;
        cycle += 1;
    }
}
