//! CATALOG: family=locomotion min_players=1 single_process=false world_edits=false scenery=none far_players=false
//!
//! Compare one natural landform from its lower apron and upper shoulder, then close the route at the
//! original apron. This targets the revisit problem's multi-layer landmark face: the same crest is first seen
//! above a lower foreground, then from a higher station with a different depth ordering, and finally
//! re-acquired from the opening station. The body route is a three-leg loop rather than a one-way climb.
//!
//! No hill is assumed. `surface_y` samples solid daylight columns on paired radii; a pair is in-band
//! only when the outer point is at least two blocks higher. A missing pair degrades to two four-block
//! waypoints and records `probe_failed`, but the bot still walks. The 6/12-block radii, two-block rise,
//! four-block hops, 0.8-block arrival, and 1.4-second dwells are respectively the visual tier scale,
//! useful terrain contrast, Solaris movement primitive, sub-hop correction margin, and non-frozen pause.
//! Looks happen only after travel, so the Hold camera never competes with a scripted turn.

use super::*;

const RADII: [f64; 2] = [6.0, 12.0];
const SLOTS: usize = 12;
const SCAN_SPAN: i32 = 20;
const MIN_RISE: i32 = 2;
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

struct Tiers {
    low: (f64, f64),
    high: (f64, f64),
    low_y: i32,
    high_y: i32,
    focus: (f64, f64, f64),
}

fn probe(ctx: &Ctx, sector: f64) -> Option<Tiers> {
    let own = ctx.position()?;
    let hint = own.1.floor() as i32;
    for bearing_slot in 0..SLOTS {
        let bearing = sector + bearing_slot as f64 * std::f64::consts::TAU / SLOTS as f64;
        let low_x = own.0 + bearing.cos() * RADII[0];
        let low_z = own.2 + bearing.sin() * RADII[0];
        let high_x = own.0 + bearing.cos() * RADII[1];
        let high_z = own.2 + bearing.sin() * RADII[1];
        let Some(low_y) = surface_y(ctx, low_x.floor() as i32, low_z.floor() as i32, hint) else {
            continue;
        };
        let Some(high_y) = surface_y(ctx, high_x.floor() as i32, high_z.floor() as i32, hint)
        else {
            continue;
        };
        if high_y - low_y < MIN_RISE {
            continue;
        }
        let Some(low) = walkable_xz_near(ctx, (low_x, low_z), low_y) else {
            continue;
        };
        let Some(high) = walkable_xz_near(ctx, (high_x, high_z), high_y) else {
            continue;
        };
        return Some(Tiers {
            low,
            high,
            low_y,
            high_y,
            focus: (high_x, high_y as f64 + 0.5, high_z),
        });
    }
    None
}

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let anchor = ctx.anchor();
    let sector = ((ctx.engine().seed % 360) as f64 + global as f64 * 17.0).to_radians();
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
        let tiers = found.unwrap_or_else(|| {
            let low = (home.0 + sector.cos() * LEG, home.2 + sector.sin() * LEG);
            Tiers {
                low,
                high: (home.0, home.2),
                low_y: home.1.floor() as i32,
                high_y: home.1.floor() as i32,
                focus: (anchor.0, home.1 + 1.4, anchor.1),
            }
        });
        ctx.record(
            TYPE_MOVE,
            &format!("vis-landmark-tier-setup-{index}-{cycle}"),
            "Probe two natural elevation tiers before the closed observation loop.",
            json!({
                "global_index": global,
                "cycle": cycle,
                "probe_failed": probe_failed,
                "low_y": tiers.low_y,
                "high_y": tiers.high_y,
                "rise": tiers.high_y - tiers.low_y,
                "min_rise": MIN_RISE,
            }),
        );

        let low_distance = travel_to(ctx, tiers.low, TRAVEL_HOP, ARRIVE).await;
        let low_faced = ctx.look_until_facing(tiers.focus, 7.0, 5_000).await;
        ctx.record(
            TYPE_LOOK,
            &format!("vis-landmark-tier-low-{index}-{cycle}"),
            "Frame the upper natural landform from its lower apron.",
            json!({"cycle": cycle, "station": "low", "faced": low_faced, "arrival_distance": r2(low_distance)}),
        );
        ctx.still(DWELL_MS).await;
        if ctx.target_met() {
            break;
        }

        let high_distance = travel_to(ctx, tiers.high, TRAVEL_HOP, ARRIVE).await;
        let high_faced = ctx.look_until_facing(tiers.focus, 7.0, 5_000).await;
        ctx.record(
            TYPE_LOOK,
            &format!("vis-landmark-tier-high-{index}-{cycle}"),
            "Re-frame the same crest from the higher natural shoulder.",
            json!({"cycle": cycle, "station": "high", "faced": high_faced, "arrival_distance": r2(high_distance), "height_change": tiers.high_y - tiers.low_y}),
        );
        ctx.still(DWELL_MS).await;
        if ctx.target_met() {
            break;
        }

        let return_distance = travel_to(ctx, (home.0, home.2), TRAVEL_HOP, ARRIVE).await;
        let returned = ctx.look_until_facing(tiers.focus, 7.0, 5_000).await;
        ctx.record(
            TYPE_LOOK,
            &format!("vis-landmark-tier-return-{index}-{cycle}"),
            "Return to the opening apron and close the elevation sightline loop.",
            json!({"cycle": cycle, "is_revisit": true, "faced": returned, "return_distance": r2(return_distance)}),
        );
        ctx.still(DWELL_MS).await;
        cycle += 1;
    }
}
