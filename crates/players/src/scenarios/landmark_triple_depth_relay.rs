//! CATALOG: family=locomotion min_players=1 single_process=false world_edits=false scenery=none far_players=false
//!
//! Relay among three naturally sampled depth layers on one bearing, then return to the opening view.
//! This is the revisit problem's ordered-depth face: near, middle, and far terrain are seen in one known order,
//! the order is queried from inside the stack, and the far layer is recovered after the body leaves the
//! stack. The route home -> near -> middle -> far -> home closes instead of exporting a one-way ray.
//!
//! Three solid columns are required only as observations; `surface_y` rejects water, lava, leaves, and
//! trunks, and `walkable_xz_near` resolves safe feet on real terrain. A missing stack degrades to three
//! four-block offsets and records `probe_failed`, so terrain absence cannot strand the episode. The
//! 6/12/18-block radii, 4-block hops, 0.8-block arrival, and 1.4-second dwell are chosen for visible
//! depth ordering, QC-safe key duration, a small correction margin, and live pauses. Camera turns are
//! discrete station actions, never a target lock during travel.

use super::*;

const RADII: [f64; 3] = [6.0, 12.0, 18.0];
const SLOTS: usize = 12;
const SCAN_SPAN: i32 = 20;
const MIN_SPACING: f64 = 5.0;
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
            // Keep scanning below foliage/liquid for a genuine walkable surface.
            continue;
        }
        if block_is_safe_support(ctx, cell) {
            return Some(y + 1);
        }
    }
    None
}

struct Stack {
    stations: [(f64, f64); 3],
    focuses: [(f64, f64, f64); 3],
    heights: [i32; 3],
}

fn probe(ctx: &Ctx, sector: f64) -> Option<Stack> {
    let own = ctx.position()?;
    let hint = own.1.floor() as i32;
    for slot in 0..SLOTS {
        let bearing = sector + slot as f64 * std::f64::consts::TAU / SLOTS as f64;
        let mut stations = [(0.0, 0.0); 3];
        let mut focuses = [(0.0, 0.0, 0.0); 3];
        let mut heights = [0; 3];
        let mut valid = true;
        for (i, radius) in RADII.into_iter().enumerate() {
            let x = own.0 + bearing.cos() * radius;
            let z = own.2 + bearing.sin() * radius;
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
            return Some(Stack {
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
    let sector = ((ctx.engine().seed % 360) as f64 + global as f64 * 37.0).to_radians();
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
        let stack = found.unwrap_or_else(|| {
            let mut stations = [(0.0, 0.0); 3];
            let mut focuses = [(0.0, 0.0, 0.0); 3];
            for i in 0..3 {
                let bearing = sector + i as f64 * 0.4;
                stations[i] = (home.0 + bearing.cos() * LEG, home.2 + bearing.sin() * LEG);
                focuses[i] = (anchor.0, home.1 + 1.4, anchor.1);
            }
            Stack {
                stations,
                focuses,
                heights: [home.1.floor() as i32; 3],
            }
        });
        ctx.record(TYPE_MOVE, &format!("vis-landmark-triple-depth-setup-{index}-{cycle}"), "Resolve three natural depth layers for the relay.", json!({"global_index": global, "cycle": cycle, "probe_failed": probe_failed, "radii": RADII, "min_spacing": MIN_SPACING, "heights": stack.heights}));

        let near_distance = travel_to(ctx, stack.stations[0], TRAVEL_HOP, ARRIVE).await;
        let far_from_near = ctx.look_until_facing(stack.focuses[2], 7.0, 5_000).await;
        ctx.record(TYPE_LOOK, &format!("vis-landmark-triple-depth-near-{index}-{cycle}"), "From the near layer, frame the far natural layer.", json!({"cycle": cycle, "station": "near", "target": "far", "faced": far_from_near, "arrival_distance": r2(near_distance)}));
        ctx.still(DWELL_MS).await;
        if ctx.target_met() {
            break;
        }

        let middle_distance = travel_to(ctx, stack.stations[1], TRAVEL_HOP, ARRIVE).await;
        let near_from_middle = ctx.look_until_facing(stack.focuses[0], 7.0, 5_000).await;
        ctx.record(TYPE_LOOK, &format!("vis-landmark-triple-depth-middle-near-{index}-{cycle}"), "From the middle layer, recover the near natural layer.", json!({"cycle": cycle, "station": "middle", "target": "near", "faced": near_from_middle, "arrival_distance": r2(middle_distance)}));
        ctx.still(DWELL_MS).await;
        if ctx.target_met() {
            break;
        }
        let far_from_middle = ctx.look_until_facing(stack.focuses[2], 7.0, 5_000).await;
        ctx.record(
            TYPE_LOOK,
            &format!("vis-landmark-triple-depth-middle-far-{index}-{cycle}"),
            "At the same middle station, reverse the gaze to the far layer.",
            json!({"cycle": cycle, "station": "middle", "target": "far", "faced": far_from_middle}),
        );
        ctx.still(DWELL_MS).await;
        if ctx.target_met() {
            break;
        }

        let far_distance = travel_to(ctx, stack.stations[2], TRAVEL_HOP, ARRIVE).await;
        let middle_from_far = ctx.look_until_facing(stack.focuses[1], 7.0, 5_000).await;
        ctx.record(TYPE_LOOK, &format!("vis-landmark-triple-depth-far-{index}-{cycle}"), "From the far layer, frame the middle natural layer before returning.", json!({"cycle": cycle, "station": "far", "target": "middle", "faced": middle_from_far, "arrival_distance": r2(far_distance)}));
        ctx.still(DWELL_MS).await;
        if ctx.target_met() {
            break;
        }

        let return_distance = travel_to(ctx, (home.0, home.2), TRAVEL_HOP, ARRIVE).await;
        let far_revisited = ctx.look_until_facing(stack.focuses[2], 7.0, 5_000).await;
        ctx.record(TYPE_LOOK, &format!("vis-landmark-triple-depth-return-{index}-{cycle}"), "Return to the opening hub and recover the far-layer sightline.", json!({"cycle": cycle, "is_revisit": true, "faced": far_revisited, "return_distance": r2(return_distance)}));
        ctx.still(DWELL_MS).await;
        cycle += 1;
    }
}
