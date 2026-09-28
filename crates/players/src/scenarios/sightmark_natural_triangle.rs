//! CATALOG: family=locomotion min_players=1 single_process=false world_edits=false scenery=none far_players=false
//!
//! Build a triangle from three naturally distinct terrain points, visit A -> B -> C -> A, and use
//! each point as the next point's sightmark. This targets the revisit problem's landmark triangulation face:
//! the model must retain three spatially related natural anchors and restore the first relationship,
//! not merely remember a single marker or a straight departure. The body route and the sightline
//! sequence close together, while every gaze is confirmed after arrival.
//!
//! The triangle is sampled from solid surface columns at three 120-degree bearings; water, lava,
//! foliage, and missing chunks invalidate a candidate rather than being presumed away. If no complete
//! triangle resolves, three four-block offsets remain active and `probe_failed` records the degraded
//! case. The 10-block radius, 120-degree spacing, 4-block hops, 0.8-block arrival, and 1.4-second
//! dwell keep vertices separated but local, clear key-chatter, and prevent frozen pauses. The camera
//! is never commanded during a movement leg.

use super::*;

const TRIANGLE_RADIUS: f64 = 10.0;
const SLOTS: usize = 10;
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

struct NaturalTriangle {
    stations: [(f64, f64); 3],
    focuses: [(f64, f64, f64); 3],
    heights: [i32; 3],
}

fn probe(ctx: &Ctx, sector: f64) -> Option<NaturalTriangle> {
    let own = ctx.position()?;
    let hint = own.1.floor() as i32;
    for slot in 0..SLOTS {
        let base = sector + slot as f64 * std::f64::consts::TAU / SLOTS as f64;
        let mut stations = [(0.0, 0.0); 3];
        let mut focuses = [(0.0, 0.0, 0.0); 3];
        let mut heights = [0; 3];
        let mut valid = true;
        for vertex in 0..3 {
            let bearing = base + vertex as f64 * std::f64::consts::TAU / 3.0;
            let x = own.0 + bearing.cos() * TRIANGLE_RADIUS;
            let z = own.2 + bearing.sin() * TRIANGLE_RADIUS;
            let Some(y) = surface_y(ctx, x.floor() as i32, z.floor() as i32, hint) else {
                valid = false;
                break;
            };
            let Some(station) = walkable_xz_near(ctx, (x, z), y) else {
                valid = false;
                break;
            };
            stations[vertex] = station;
            focuses[vertex] = (x, y as f64 + 0.5, z);
            heights[vertex] = y;
        }
        if valid {
            return Some(NaturalTriangle {
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
    let sector = ((ctx.engine().seed % 360) as f64 + global as f64 * 31.0).to_radians();
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
        let triangle = found.unwrap_or_else(|| {
            let mut stations = [(0.0, 0.0); 3];
            let mut focuses = [(0.0, 0.0, 0.0); 3];
            for vertex in 0..3 {
                let bearing = sector + vertex as f64 * std::f64::consts::TAU / 3.0;
                stations[vertex] = (home.0 + bearing.cos() * LEG, home.2 + bearing.sin() * LEG);
                focuses[vertex] = (anchor.0, home.1 + 1.4, anchor.1);
            }
            NaturalTriangle {
                stations,
                focuses,
                heights: [home.1.floor() as i32; 3],
            }
        });
        ctx.record(
            TYPE_MOVE,
            &format!("vis-sightmark-natural-triangle-setup-{index}-{cycle}"),
            "Resolve three natural vertices before the closed triangulation route.",
            json!({"global_index": global, "cycle": cycle, "probe_failed": probe_failed, "radius": TRIANGLE_RADIUS, "heights": triangle.heights}),
        );

        for vertex in 0..3 {
            let distance = travel_to(ctx, triangle.stations[vertex], TRAVEL_HOP, ARRIVE).await;
            let target = (vertex + 1) % 3;
            let faced = ctx
                .look_until_facing(triangle.focuses[target], 7.0, 5_000)
                .await;
            ctx.record(
                TYPE_LOOK,
                &format!("vis-sightmark-natural-triangle-{index}-{cycle}-{vertex}"),
                "At one natural vertex, frame the next vertex in the triangle.",
                json!({"cycle": cycle, "vertex": vertex, "target_vertex": target, "faced": faced, "arrival_distance": r2(distance), "target_height": triangle.heights[target]}),
            );
            ctx.still(DWELL_MS).await;
            if ctx.target_met() {
                break;
            }
        }
        if ctx.target_met() {
            break;
        }

        let return_distance = travel_to(ctx, triangle.stations[0], TRAVEL_HOP, ARRIVE).await;
        let returned = ctx.look_until_facing(triangle.focuses[1], 7.0, 5_000).await;
        ctx.record(
            TYPE_LOOK,
            &format!("vis-sightmark-natural-triangle-return-{index}-{cycle}"),
            "Re-enter the first vertex and restore the triangle's opening relationship.",
            json!({"cycle": cycle, "is_revisit": true, "faced": returned, "return_distance": r2(return_distance)}),
        );
        ctx.still(DWELL_MS).await;
        cycle += 1;
    }
}
