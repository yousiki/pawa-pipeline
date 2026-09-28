//! CATALOG: family=locomotion min_players=1 single_process=false world_edits=false scenery=none far_players=false
//!
//! Cross a natural saddle: frame the far shoulder, descend through the measured low pass, look back
//! at the shoulder from below, climb the opposite shoulder, and return to the opening station.
//! The revisit problem is a terrain-profile loop rather than a single hill climb. The two shoulders
//! occlude each other from the pass, while the same shoulder is framed again after the body closes
//! the loop. Looks are issued only at stations, never repeatedly during travel.
//!
//! The profile is sampled along six bearings at 8, 14, and 20 blocks. A real saddle needs both
//! shoulders at least two blocks above its middle. If no such profile exists, the most separated
//! resolved three-point profile is retained with `in_band: false`; if fewer than three points
//! resolve, a fixed four-block out-and-back is used with `probe_failed: true`. The 20-block reach
//! keeps the shoulders in the camera, the 2-block relief makes occlusion visible, and the 0.8-block
//! arrival tolerance is deliberately much smaller than the 4-block planner hop.
use super::*;

const BEARINGS: usize = 6;
const RANGES: [f64; 3] = [8.0, 14.0, 20.0];
const SCAN_SPAN: i32 = 64;
const RELIEF_MIN: i32 = 2;
const LEG: f64 = 4.0;
const ARRIVE: f64 = 0.8;
const DWELL_MS: u64 = 1_300;

#[derive(Clone, Copy)]
struct Spot {
    x: f64,
    z: f64,
    y: i32,
}
struct Saddle {
    near: Spot,
    pass: Spot,
    far: Spot,
    relief: i32,
    in_band: bool,
}

fn surface(ctx: &Ctx, x: i32, z: i32, hint: i32) -> Option<i32> {
    for y in ((hint - SCAN_SPAN)..=(hint + SCAN_SPAN)).rev() {
        let c = BlockPos::new(x, y, z);
        let id = ctx.block_id_at(c)?;
        if id.contains("water") || id.contains("lava") {
            return None;
        }
        if block_is_safe_support(ctx, c) {
            if id.contains("leaves") || id.ends_with("log") {
                return None;
            }
            return Some(y + 1);
        }
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

fn probe(ctx: &Ctx, sector: f64) -> Option<Saddle> {
    let own = ctx.position()?;
    let hint = own.1.floor() as i32;
    let mut best: Option<(i32, Saddle)> = None;
    for slot in 0..BEARINGS {
        let bearing = sector + slot as f64 * std::f64::consts::TAU / BEARINGS as f64;
        let mut spots = Vec::new();
        for range in RANGES {
            let x = own.0 + bearing.cos() * range;
            let z = own.2 + bearing.sin() * range;
            if let Some(y) = surface(ctx, x.floor() as i32, z.floor() as i32, hint) {
                spots.push(Spot { x, z, y });
            }
        }
        if spots.len() != 3 {
            continue;
        }
        let relief = (spots[0].y - spots[1].y).min(spots[2].y - spots[1].y);
        let in_band = relief >= RELIEF_MIN;
        let score = relief.max(0) * 10 + (spots[0].y - spots[2].y).abs().min(4);
        if best.as_ref().is_none_or(|(old, _)| score > *old) {
            best = Some((
                score,
                Saddle {
                    near: spots[0],
                    pass: spots[1],
                    far: spots[2],
                    relief,
                    in_band,
                },
            ));
        }
    }
    best.map(|(_, mut s)| {
        s.in_band = s.relief >= RELIEF_MIN;
        s
    })
}

fn focus(s: Spot) -> (f64, f64, f64) {
    (s.x, s.y as f64 + 1.2, s.z)
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
        &format!("vis-terrain-saddle-init-{index}"),
        "Face the anchor before measuring a natural saddle pass.",
        json!({"global_index": global, "ranges": RANGES, "relief_min": RELIEF_MIN}),
    );
    let mut cycle = 0u32;
    while !ctx.target_met() {
        let Some(home) = ctx.position() else {
            sleep_ms(200).await;
            continue;
        };
        let found = probe(ctx, sector);
        let failed = found.is_none();
        let saddle = found.unwrap_or_else(|| {
            let p = (home.0 + sector.cos() * LEG, home.2 + sector.sin() * LEG);
            let s = Spot {
                x: p.0,
                z: p.1,
                y: home.1.floor() as i32,
            };
            Saddle {
                near: s,
                pass: s,
                far: s,
                relief: 0,
                in_band: false,
            }
        });
        let near = surface_target(ctx, (saddle.near.x, saddle.near.z), home.1.floor() as i32)
            .unwrap_or((saddle.near.x, saddle.near.z));
        let pass = surface_target(ctx, (saddle.pass.x, saddle.pass.z), home.1.floor() as i32)
            .unwrap_or((saddle.pass.x, saddle.pass.z));
        let far = surface_target(ctx, (saddle.far.x, saddle.far.z), home.1.floor() as i32)
            .unwrap_or((saddle.far.x, saddle.far.z));
        travel_to(ctx, near, TRAVEL_HOP, ARRIVE).await;
        let first = ctx.look_until_facing(focus(saddle.far), 8.0, 5_000).await;
        ctx.record(
            TYPE_LOOK,
            &format!("vis-terrain-saddle-open-{index}-{cycle}"),
            "Frame the far shoulder before descending through the measured pass.",
            json!({"cycle": cycle, "framed": first, "relief": saddle.relief,
                   "in_band": saddle.in_band, "probe_failed": failed}),
        );
        ctx.still(DWELL_MS).await;
        if ctx.target_met() {
            break;
        }
        travel_to(ctx, pass, TRAVEL_HOP, ARRIVE).await;
        let below = ctx.look_until_facing(focus(saddle.near), 8.0, 5_000).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-terrain-saddle-floor-{index}-{cycle}"),
            "At the low pass, look back at the shoulder that was behind the opening view.",
            json!({"cycle": cycle, "framed": below, "pass_y": saddle.pass.y, "is_return": true}),
        );
        ctx.still(DWELL_MS).await;
        if ctx.target_met() {
            break;
        }
        travel_to(ctx, far, TRAVEL_HOP, ARRIVE).await;
        ctx.look_until_facing(focus(saddle.near), 8.0, 5_000).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-terrain-saddle-cross-{index}-{cycle}"),
            "Climb the opposite shoulder and retain the reverse sightline across the pass.",
            json!({"cycle": cycle, "relief": saddle.relief}),
        );
        ctx.still(DWELL_MS).await;
        if ctx.target_met() {
            break;
        }
        travel_to(ctx, (home.0, home.2), TRAVEL_HOP, ARRIVE).await;
        ctx.look_until_facing(focus(saddle.far), 8.0, 5_000).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-terrain-saddle-return-{index}-{cycle}"),
            "Return to the opening shoulder and close the saddle sightline loop.",
            json!({"cycle": cycle, "is_revisit": true}),
        );
        cycle += 1;
    }
}
