//! CATALOG: family=locomotion min_players=2 single_process=false world_edits=false scenery=none far_players=false
//!
//! Measure two close terrain walls, walk along the corridor between them, turn across the corridor at
//! the far station, and retrace the same axis. The revisit problem is recoverable lateral
//! occlusion: each wall removes one half of the horizon, the cross-canyon turn exceeds a normal
//! partial glance, and the opening wall is framed again after the route returns. Adjacent bots take
//! opposite hands along the same measured axis, so N=2 produces crossings without lockstep action
//! streams.
//!
//! A canyon is not assumed. Twelve opposite-bearing pairs are sampled four blocks from the bot and
//! scored by their common rise. Two sides at least two blocks high are `in_band`; one low or missing
//! side degrades to the best resolved pair, and no pair falls back to a four-block out-and-back with
//! `probe_failed: true`. Four blocks is close enough for a wall to dominate the frame, the 9-block
//! corridor leg is long enough to be real travel, the 18-block scan covers normal ravines, and the
//! 0.8-block arrival tolerance cannot swallow a 4-block hop. Camera turns happen only while stopped.
use super::*;

const PAIRS: usize = 12;
const WALL_RANGE: f64 = 4.0;
const AXIS_LEG: f64 = 9.0;
const SCAN_SPAN: i32 = 64;
const RISE_MIN: i32 = 2;
const LEG: f64 = 4.0;
const ARRIVE: f64 = 0.8;
const DWELL_MS: u64 = 1_300;

struct Corridor {
    axis: f64,
    left: (f64, f64, f64),
    right: (f64, f64, f64),
    common_rise: i32,
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

fn probe(ctx: &Ctx, sector: f64) -> Option<Corridor> {
    let own = ctx.position()?;
    let hint = own.1.floor() as i32;
    let mut best: Option<(i32, Corridor)> = None;
    for slot in 0..PAIRS {
        let cross = sector + slot as f64 * std::f64::consts::PI / PAIRS as f64;
        let left_x = own.0 + cross.cos() * WALL_RANGE;
        let left_z = own.2 + cross.sin() * WALL_RANGE;
        let right_x = own.0 - cross.cos() * WALL_RANGE;
        let right_z = own.2 - cross.sin() * WALL_RANGE;
        let Some(left_y) = surface(ctx, left_x.floor() as i32, left_z.floor() as i32, hint) else {
            continue;
        };
        let Some(right_y) = surface(ctx, right_x.floor() as i32, right_z.floor() as i32, hint)
        else {
            continue;
        };
        let common = (left_y - hint).min(right_y - hint);
        let corridor = Corridor {
            axis: cross + std::f64::consts::FRAC_PI_2,
            left: (left_x, left_y as f64 + 0.8, left_z),
            right: (right_x, right_y as f64 + 0.8, right_z),
            common_rise: common,
            in_band: common >= RISE_MIN,
        };
        if best.as_ref().is_none_or(|(old, _)| common > *old) {
            best = Some((common, corridor));
        }
    }
    best.map(|(_, corridor)| corridor)
}

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let anchor = ctx.anchor();
    let sector = ((ctx.engine().seed % 360) as f64).to_radians();
    let hand = if global.is_multiple_of(2) { 1.0 } else { -1.0 };
    if let Some(p) = ctx.position() {
        ctx.look_until_facing((anchor.0, p.1 + 1.4, anchor.1), 8.0, 4_000)
            .await;
    }
    ctx.record(TYPE_LOOK, &format!("vis-terrain-canyon-init-{index}"),
        "Face the anchor before measuring a two-wall corridor.",
        json!({"global_index": global, "wall_range": WALL_RANGE, "axis_leg": AXIS_LEG, "hand": hand}));
    let mut cycle = 0u32;
    while !ctx.target_met() {
        let Some(home) = ctx.position() else {
            sleep_ms(200).await;
            continue;
        };
        let found = probe(ctx, sector);
        let failed = found.is_none();
        let corridor = found.unwrap_or(Corridor {
            axis: sector,
            left: (
                home.0 + sector.cos() * LEG,
                home.1 + 1.4,
                home.2 + sector.sin() * LEG,
            ),
            right: (
                home.0 - sector.cos() * LEG,
                home.1 + 1.4,
                home.2 - sector.sin() * LEG,
            ),
            common_rise: 0,
            in_band: false,
        });
        let distance = if failed { LEG } else { AXIS_LEG };
        let far_wanted = (
            home.0 + corridor.axis.cos() * distance * hand,
            home.2 + corridor.axis.sin() * distance * hand,
        );
        let far = surface_target(ctx, far_wanted, home.1.floor() as i32).unwrap_or(far_wanted);
        let first = ctx.look_until_facing(corridor.left, 8.0, 5_000).await;
        ctx.record(
            TYPE_LOOK,
            &format!("vis-terrain-canyon-left-{index}-{cycle}"),
            "Frame one canyon wall before walking the measured corridor axis.",
            json!({"cycle": cycle, "framed": first, "common_rise": corridor.common_rise,
                   "in_band": corridor.in_band, "probe_failed": failed}),
        );
        ctx.still(DWELL_MS).await;
        if ctx.target_met() {
            break;
        }
        travel_to(ctx, far, TRAVEL_HOP, ARRIVE).await;
        let crossed = ctx.look_until_facing(corridor.right, 8.0, 5_000).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-terrain-canyon-cross-{index}-{cycle}"),
            "At the far corridor station, turn across to the opposite closing wall.",
            json!({"cycle": cycle, "framed": crossed, "exceeds_horizontal_fov": true}),
        );
        ctx.still(DWELL_MS).await;
        if ctx.target_met() {
            break;
        }
        let returned = travel_to(ctx, (home.0, home.2), TRAVEL_HOP, ARRIVE).await;
        let reframed = ctx.look_until_facing(corridor.left, 8.0, 5_000).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-terrain-canyon-return-{index}-{cycle}"),
            "Retrace the corridor and re-frame the opening wall from the opening station.",
            json!({"cycle": cycle, "framed": reframed, "is_revisit": true,
                   "distance_from_home": r2(returned)}),
        );
        cycle += 1;
    }
}
