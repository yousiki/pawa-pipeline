//! CATALOG: family=locomotion min_players=1 single_process=false world_edits=false scenery=none far_players=false
//!
//! Compare a high bank with the lower dry lip beside a natural water surface, then return to the
//! opening bank and frame the same far-water point again. The revisit problem here is
//! height-dependent visibility: the low lip lets the bank and water plane cut the frame, while the
//! high station sees over it. The route high-to-low-to-high is a closed trajectory and every
//! deliberate gaze is made after travel has stopped.
//!
//! Water is probed rather than presumed. The ring first finds a water column, then searches dry
//! support at three and eight blocks back from that column. If water is absent, the lowest resolved
//! land point is used as the low lip (`water_found: false`); if neither resolves, a four-block leg
//! out and back is used (`probe_failed: true`). The 3/8-block offsets keep both stations on land,
//! the 16-block scan covers ordinary banks, the 3-block rise is the meaningful height band, and
//! 0.8-block arrival is smaller than the 4-block travel hop so movement is not a correction shuffle.
use super::*;

const RADII: [f64; 3] = [8.0, 16.0, 24.0];
const SLOTS: usize = 12;
const SCAN_SPAN: i32 = 64;
const LOW_OFFSET: f64 = 3.0;
const HIGH_OFFSET: f64 = 8.0;
const RISE_MIN: i32 = 3;
const LEG: f64 = 4.0;
const ARRIVE: f64 = 0.8;
const DWELL_MS: u64 = 1_400;

fn surface(ctx: &Ctx, x: i32, z: i32, hint: i32) -> Option<i32> {
    for y in ((hint - SCAN_SPAN)..=(hint + SCAN_SPAN)).rev() {
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
    }
    None
}

fn water(ctx: &Ctx, x: i32, z: i32, hint: i32) -> Option<i32> {
    for y in ((hint - SCAN_SPAN)..=(hint + SCAN_SPAN)).rev() {
        let cell = BlockPos::new(x, y, z);
        let id = ctx.block_id_at(cell)?;
        if block_is_air(ctx, cell) {
            continue;
        }
        return id.contains("water").then_some(y);
    }
    None
}

/// Snap an X/Z goal to the measured topmost surface, never the current cave-floor altitude.
fn surface_target(ctx: &Ctx, desired: (f64, f64), hint: i32) -> Option<(f64, f64)> {
    let x = desired.0.floor() as i32;
    let z = desired.1.floor() as i32;
    let y = surface(ctx, x, z, hint)?;
    walkable_xz_near(ctx, desired, y)
}

struct Banks {
    low: (f64, f64),
    high: (f64, f64),
    focus: (f64, f64, f64),
    rise: i32,
    water: bool,
}

fn probe(ctx: &Ctx, sector: f64) -> Option<Banks> {
    let own = ctx.position()?;
    let hint = own.1.floor() as i32;
    let mut lowest: Option<(f64, f64, i32, f64)> = None;
    for radius in RADII {
        for slot in 0..SLOTS {
            let bearing = sector + slot as f64 * std::f64::consts::TAU / SLOTS as f64;
            let x = own.0 + bearing.cos() * radius;
            let z = own.2 + bearing.sin() * radius;
            if let Some(top) = water(ctx, x.floor() as i32, z.floor() as i32, hint) {
                let low_wanted = (
                    x - bearing.cos() * LOW_OFFSET,
                    z - bearing.sin() * LOW_OFFSET,
                );
                let high_wanted = (
                    x - bearing.cos() * HIGH_OFFSET,
                    z - bearing.sin() * HIGH_OFFSET,
                );
                let low = surface_target(ctx, low_wanted, hint).unwrap_or(low_wanted);
                let high = surface_target(ctx, high_wanted, hint).unwrap_or(high_wanted);
                let low_y =
                    surface(ctx, low.0.floor() as i32, low.1.floor() as i32, hint).unwrap_or(hint);
                let high_y = surface(ctx, high.0.floor() as i32, high.1.floor() as i32, hint)
                    .unwrap_or(low_y);
                return Some(Banks {
                    low,
                    high,
                    focus: (x, top as f64 + 0.5, z),
                    rise: high_y - low_y,
                    water: true,
                });
            }
            if let Some(y) = surface(ctx, x.floor() as i32, z.floor() as i32, hint)
                .filter(|&y| lowest.is_none_or(|(_, _, best, _)| y < best))
            {
                lowest = Some((x, z, y, bearing));
            }
        }
    }
    let (x, z, low_y, bearing) = lowest?;
    let low = surface_target(ctx, (x, z), hint).unwrap_or((x, z));
    let high_wanted = (
        x - bearing.cos() * HIGH_OFFSET,
        z - bearing.sin() * HIGH_OFFSET,
    );
    let high = surface_target(ctx, high_wanted, hint).unwrap_or(high_wanted);
    let high_y = surface(ctx, high.0.floor() as i32, high.1.floor() as i32, hint).unwrap_or(low_y);
    Some(Banks {
        low,
        high,
        focus: (x, low_y as f64 + 1.0, z),
        rise: high_y - low_y,
        water: false,
    })
}

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let anchor = ctx.anchor();
    let sector = ((ctx.engine().seed % 360) as f64).to_radians();
    if let Some(p) = ctx.position() {
        ctx.look_until_facing((anchor.0, p.1 + 1.4, anchor.1), 8.0, 4_000)
            .await;
    }
    ctx.record(
        TYPE_LOOK,
        &format!("vis-terrain-bank-init-{index}"),
        "Face the anchor before probing a water edge and its height change.",
        json!({"global_index": global, "rise_min": RISE_MIN, "water_radii": RADII}),
    );
    let mut cycle = 0u32;
    while !ctx.target_met() {
        let Some(home) = ctx.position() else {
            sleep_ms(200).await;
            continue;
        };
        let found = probe(ctx, sector);
        let failed = found.is_none();
        let banks = found.unwrap_or_else(|| {
            let p = (home.0 + sector.cos() * LEG, home.2 + sector.sin() * LEG);
            Banks {
                low: p,
                high: (home.0, home.2),
                focus: (p.0, home.1 + 1.0, p.1),
                rise: 0,
                water: false,
            }
        });
        let high = surface_target(ctx, banks.high, home.1.floor() as i32).unwrap_or(banks.high);
        let low = surface_target(ctx, banks.low, home.1.floor() as i32).unwrap_or(banks.low);
        travel_to(ctx, high, TRAVEL_HOP, ARRIVE).await;
        let first = ctx.look_until_facing(banks.focus, 8.0, 5_000).await;
        ctx.record(
            TYPE_LOOK,
            &format!("vis-terrain-bank-high-{index}-{cycle}"),
            "Frame the water or low ground from the higher bank before descending.",
            json!({"cycle": cycle, "framed": first, "water_found": banks.water,
                   "rise": banks.rise, "in_band": banks.rise >= RISE_MIN, "probe_failed": failed}),
        );
        ctx.still(DWELL_MS).await;
        if ctx.target_met() {
            break;
        }
        travel_to(ctx, low, TRAVEL_HOP, ARRIVE).await;
        let second = ctx.look_until_facing(banks.focus, 8.0, 5_000).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-terrain-bank-low-{index}-{cycle}"),
            "Descend to the lower lip and re-frame the same water point through the bank edge.",
            json!({"cycle": cycle, "framed": second, "is_return": true, "rise": banks.rise}),
        );
        ctx.still(DWELL_MS).await;
        if ctx.target_met() {
            break;
        }
        travel_to(ctx, (home.0, home.2), TRAVEL_HOP, ARRIVE).await;
        ctx.look_until_facing(banks.focus, 8.0, 5_000).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-terrain-bank-return-{index}-{cycle}"),
            "Return to the opening bank and close the height-change sightline loop.",
            json!({"cycle": cycle, "is_revisit": true}),
        );
        cycle += 1;
    }
}
