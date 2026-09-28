//! CATALOG: family=locomotion min_players=1 single_process=false world_edits=false scenery=none far_players=false
//!
//! Walk to opposite sides of a naturally raised rock mass, using the same boulder as the visual
//! subject before and after the crossing. The revisit problem is the terrain-occlusion case: a
//! two-sided sightline loop makes the rock face and the background behind it recoverable after the
//! camera and body have gone around it. The trajectory closes by returning to the opening station.
//!
//! No boulder is assumed. The probe samples solid, daylight surface columns and prefers a nearby
//! rise of at least two blocks while rejecting water, lava, leaves, and trunks. If only a low rise
//! resolves, it is retained as a degraded rock (`in_band: false`); if no column resolves, a fixed
//! four-block out-and-back still moves the bot (`probe_failed: true`). The 8-block search radius,
//! 2-block minimum rise, 4-block stand-off, and 0.8-block arrival tolerance are deliberate: they
//! keep the obstruction in frame, leave the two stations navigable, and keep each travel press well
//! above the short-run chatter threshold. All looks happen while standing; `move_to` owns the
//! camera during travel and is never mistaken for a locked gaze.
use super::*;

const SEARCH_RADIUS: f64 = 8.0;
const SEARCH_SLOTS: usize = 12;
const SCAN_SPAN: i32 = 64;
const RISE_MIN: i32 = 2;
const STANDOFF: f64 = 4.0;
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

/// Snap an X/Z goal to the measured topmost surface, never the current cave-floor altitude.
fn surface_target(ctx: &Ctx, desired: (f64, f64), hint: i32) -> Option<(f64, f64)> {
    let x = desired.0.floor() as i32;
    let z = desired.1.floor() as i32;
    let y = surface(ctx, x, z, hint)?;
    walkable_xz_near(ctx, desired, y)
}

type BoulderProbe = ((f64, f64), (f64, f64, f64), i32, bool);

fn find_boulder(ctx: &Ctx, sector: f64) -> Option<BoulderProbe> {
    let own = ctx.position()?;
    let hint = own.1.floor() as i32;
    let mut best: Option<(i32, f64, (f64, f64), f64)> = None;
    for slot in 0..SEARCH_SLOTS {
        let bearing = sector + slot as f64 * std::f64::consts::TAU / SEARCH_SLOTS as f64;
        let x = own.0 + bearing.cos() * SEARCH_RADIUS;
        let z = own.2 + bearing.sin() * SEARCH_RADIUS;
        let Some(feet) = surface(ctx, x.floor() as i32, z.floor() as i32, hint) else {
            continue;
        };
        let rise = feet - hint;
        let score = (rise.max(0), -(x - own.0).hypot(z - own.2));
        if best.map(|b| score > (b.0, b.1)).unwrap_or(true) {
            best = Some((rise, score.1, (x, z), bearing));
        }
    }
    let (rise, _, centre, bearing) = best?;
    let in_band = rise >= RISE_MIN;
    let eye = (centre.0, hint as f64 + 1.0 + rise.max(1) as f64, centre.1);
    Some((
        (
            centre.0 - bearing.cos() * STANDOFF,
            centre.1 - bearing.sin() * STANDOFF,
        ),
        eye,
        rise,
        in_band,
    ))
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
        &format!("vis-terrain-boulder-init-{index}"),
        "Face the shared anchor before probing a natural rock obstruction.",
        json!({"global_index": global, "search_radius": SEARCH_RADIUS, "rise_min": RISE_MIN}),
    );

    let mut cycle = 0u32;
    while !ctx.target_met() {
        let Some(home) = ctx.position() else {
            sleep_ms(200).await;
            continue;
        };
        let found = find_boulder(ctx, sector);
        let probe_failed = found.is_none();
        let (near, focus, rise, in_band) = found.unwrap_or_else(|| {
            let near = (home.0 + sector.cos() * LEG, home.2 + sector.sin() * LEG);
            (near, (near.0, home.1 + 1.4, near.1), 0, false)
        });
        let bearing = (focus.2 - home.2).atan2(focus.0 - home.0);
        let far_wanted = (
            focus.0 + bearing.cos() * STANDOFF,
            focus.2 + bearing.sin() * STANDOFF,
        );
        let near = surface_target(ctx, near, home.1.floor() as i32).unwrap_or(near);
        let far = surface_target(ctx, far_wanted, home.1.floor() as i32).unwrap_or(far_wanted);
        let start_near = travel_to(ctx, near, TRAVEL_HOP, ARRIVE).await;
        let first = ctx.look_until_facing(focus, 8.0, 5_000).await;
        ctx.record(
            TYPE_LOOK,
            &format!("vis-terrain-boulder-near-{index}-{cycle}"),
            "Frame the raised rock from its approach side before walking around it.",
            json!({"cycle": cycle, "framed": first, "rise": rise, "in_band": in_band,
                   "probe_failed": probe_failed, "remaining": r2(start_near)}),
        );
        ctx.still(DWELL_MS).await;
        if ctx.target_met() {
            break;
        }

        let crossed = travel_to(ctx, far, TRAVEL_HOP, ARRIVE).await;
        let second = ctx.look_until_facing(focus, 8.0, 5_000).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-terrain-boulder-backsight-{index}-{cycle}"),
            "Come around the rock and re-frame its face from the opposite side.",
            json!({"cycle": cycle, "framed": second, "remaining": r2(crossed),
                   "occluder": "rock_or_terrain_rise", "is_return": true}),
        );
        ctx.still(DWELL_MS).await;
        if ctx.target_met() {
            break;
        }

        let returned = travel_to(ctx, (home.0, home.2), TRAVEL_HOP, ARRIVE).await;
        ctx.look_until_facing(focus, 8.0, 5_000).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-terrain-boulder-home-{index}-{cycle}"),
            "Return to the opening station and close the rock sightline loop.",
            json!({"cycle": cycle, "is_revisit": true, "distance_from_home": r2(returned)}),
        );
        cycle += 1;
    }
}
