//! CATALOG: family=locomotion min_players=1 single_process=false world_edits=false scenery=none far_players=false
//!
//! Resolve a near and a far natural landmark on one bearing, inspect them in both depth orders, and
//! close at the opening point. The revisit problem here is depth-order memory: the near terrain is
//! a foreground occluder in one station's view, while the far crest is the persistent subject that
//! must be recovered after the body has moved past the foreground. The route is home -> near -> far
//! -> home, so path length is real while net displacement is small.
//!
//! The pair is sampled from 6- and 15-block radii with `surface_y`; water, lava, leaves, and trunks are
//! rejected rather than assumed absent. If no pair resolves, two four-block fallback points still get
//! traversed and `probe_failed` is recorded. The 9-block depth separation, 4-block hops, 0.8-block
//! arrival, and 1.4-second dwell are the minimum useful visual ordering, QC-safe travel primitive,
//! sub-hop tolerance, and live pause. All looks use `look_until_facing` after travel; no target is
//! tracked while walking.

use super::*;

const NEAR_RADIUS: f64 = 6.0;
const FAR_RADIUS: f64 = 15.0;
const SLOTS: usize = 14;
const SCAN_SPAN: i32 = 20;
const MIN_DEPTH: f64 = 7.0;
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

struct DepthPair {
    near: (f64, f64),
    far: (f64, f64),
    near_focus: (f64, f64, f64),
    far_focus: (f64, f64, f64),
    near_y: i32,
    far_y: i32,
}

fn probe(ctx: &Ctx, sector: f64) -> Option<DepthPair> {
    let own = ctx.position()?;
    let hint = own.1.floor() as i32;
    for slot in 0..SLOTS {
        let bearing = sector + slot as f64 * std::f64::consts::TAU / SLOTS as f64;
        let near_x = own.0 + bearing.cos() * NEAR_RADIUS;
        let near_z = own.2 + bearing.sin() * NEAR_RADIUS;
        let far_x = own.0 + bearing.cos() * FAR_RADIUS;
        let far_z = own.2 + bearing.sin() * FAR_RADIUS;
        let Some(near_y) = surface_y(ctx, near_x.floor() as i32, near_z.floor() as i32, hint)
        else {
            continue;
        };
        let Some(far_y) = surface_y(ctx, far_x.floor() as i32, far_z.floor() as i32, hint) else {
            continue;
        };
        let Some(near) = walkable_xz_near(ctx, (near_x, near_z), near_y) else {
            continue;
        };
        let Some(far) = walkable_xz_near(ctx, (far_x, far_z), far_y) else {
            continue;
        };
        return Some(DepthPair {
            near,
            far,
            near_focus: (near_x, near_y as f64 + 0.5, near_z),
            far_focus: (far_x, far_y as f64 + 0.5, far_z),
            near_y,
            far_y,
        });
    }
    None
}

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let anchor = ctx.anchor();
    let sector = ((ctx.engine().seed % 360) as f64 + global as f64 * 11.0).to_radians();
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
            let near = (home.0 + sector.cos() * LEG, home.2 + sector.sin() * LEG);
            let far = (
                home.0 + sector.cos() * LEG * 2.0,
                home.2 + sector.sin() * LEG * 2.0,
            );
            DepthPair {
                near,
                far,
                near_focus: (near.0, home.1 + 1.4, near.1),
                far_focus: (anchor.0, home.1 + 1.4, anchor.1),
                near_y: home.1.floor() as i32,
                far_y: home.1.floor() as i32,
            }
        });
        ctx.record(
            TYPE_MOVE,
            &format!("vis-landmark-depth-setup-{index}-{cycle}"),
            "Resolve a near/far natural pair before alternating their depth order.",
            json!({"global_index": global, "cycle": cycle, "probe_failed": probe_failed, "depth_blocks": FAR_RADIUS - NEAR_RADIUS, "min_depth": MIN_DEPTH, "near_y": pair.near_y, "far_y": pair.far_y}),
        );

        let near_distance = travel_to(ctx, pair.near, TRAVEL_HOP, ARRIVE).await;
        let far_seen_from_near = ctx.look_until_facing(pair.far_focus, 7.0, 5_000).await;
        ctx.record(
            TYPE_LOOK,
            &format!("vis-landmark-depth-near-{index}-{cycle}"),
            "From the near station, frame the far landmark over the natural foreground.",
            json!({"cycle": cycle, "station": "near", "target": "far", "faced": far_seen_from_near, "arrival_distance": r2(near_distance)}),
        );
        ctx.still(DWELL_MS).await;
        if ctx.target_met() {
            break;
        }

        let far_distance = travel_to(ctx, pair.far, TRAVEL_HOP, ARRIVE).await;
        let near_seen_from_far = ctx.look_until_facing(pair.near_focus, 7.0, 5_000).await;
        ctx.record(
            TYPE_LOOK,
            &format!("vis-landmark-depth-far-{index}-{cycle}"),
            "From the far station, reverse the depth order and frame the near landmark.",
            json!({"cycle": cycle, "station": "far", "target": "near", "faced": near_seen_from_far, "arrival_distance": r2(far_distance)}),
        );
        ctx.still(DWELL_MS).await;
        if ctx.target_met() {
            break;
        }

        let return_distance = travel_to(ctx, (home.0, home.2), TRAVEL_HOP, ARRIVE).await;
        let far_revisited = ctx.look_until_facing(pair.far_focus, 7.0, 5_000).await;
        ctx.record(
            TYPE_LOOK,
            &format!("vis-landmark-depth-return-{index}-{cycle}"),
            "Return to the opening station and recover the original far-depth view.",
            json!({"cycle": cycle, "is_revisit": true, "faced": far_revisited, "return_distance": r2(return_distance)}),
        );
        ctx.still(DWELL_MS).await;
        cycle += 1;
    }
}
