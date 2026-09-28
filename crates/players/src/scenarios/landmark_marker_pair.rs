//! CATALOG: family=locomotion min_players=1 single_process=false world_edits=true scenery=none far_players=false
//!
//! Pair a naturally resolved terrain feature with a small scripted marker, alternate between the two,
//! and return to the natural feature. This is the revisit problem's paired-identity case: the model must retain
//! which visible object is the natural anchor and which is the deliberate reference, rather than using
//! an arbitrary block as a proxy for the whole view. The route closes natural-post -> marker-post ->
//! natural-post, and every re-observation is confirmed while stationary.
//!
//! The natural feature is probed with `surface_y`; marker placement uses `raise_landmark` and may move
//! within the documented placement ring. Missing terrain falls back to a four-block site and records
//! `probe_failed`; failed placement is separately recorded and never treated as success. The 8-block
//! feature radius, 4-block marker offset, 2-block marker height, 4-block travel hop, 0.8-block arrival,
//! and 1.4-second dwell are chosen for visible pairing, reliable placement reach, compact staging, QC-safe
//! key presses, and a live (not frozen) pause. No camera is directed during movement.

use super::*;

const FEATURE_RADII: [f64; 3] = [7.0, 10.0, 13.0];
const SLOTS: usize = 12;
const SCAN_SPAN: i32 = 18;
const MARKER_OFFSET: f64 = 4.0;
const MARKER_HEIGHT: i64 = 2;
const LEG: f64 = 4.0;
const ARRIVE: f64 = 0.8;
const DWELL_MS: u64 = 1_400;
const MARKER_MATERIAL: &str = "minecraft:stone_bricks";

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
    natural_post: (f64, f64),
    marker_site: (f64, f64),
    marker_post: (f64, f64),
    natural_focus: (f64, f64, f64),
    natural_y: i32,
}

fn probe(ctx: &Ctx, sector: f64) -> Option<Pair> {
    let own = ctx.position()?;
    let hint = own.1.floor() as i32;
    for slot in 0..SLOTS {
        let bearing = sector + slot as f64 * std::f64::consts::TAU / SLOTS as f64;
        for radius in FEATURE_RADII {
            let x = own.0 + bearing.cos() * radius;
            let z = own.2 + bearing.sin() * radius;
            let Some(y) = surface_y(ctx, x.floor() as i32, z.floor() as i32, hint) else {
                continue;
            };
            let Some(natural_post) = walkable_xz_near(ctx, (x, z), y) else {
                continue;
            };
            let side = (bearing.sin(), -bearing.cos());
            let marker_site = (x + side.0 * MARKER_OFFSET, z + side.1 * MARKER_OFFSET);
            // Keep the paired station on the same real terrain band; never send navigation to an
            // unverified marker coordinate when the lateral site is a hole or wall.
            let Some(marker_post) = walkable_xz_near(ctx, marker_site, y) else {
                continue;
            };
            return Some(Pair {
                natural_post,
                marker_site,
                marker_post,
                natural_focus: (x, y as f64, z),
                natural_y: y,
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
    let sector = ((ctx.engine().seed % 360) as f64 + global as f64 * 23.0).to_radians();
    if let Some(p) = ctx.position() {
        ctx.look_until_facing((anchor.0, p.1 + 1.4, anchor.1), 8.0, 4_000)
            .await;
    }
    let mut known_marker = None;
    let mut cycle = 0usize;

    while !ctx.target_met() {
        let Some(home) = ctx.position() else {
            sleep_ms(200).await;
            continue;
        };
        let found = probe(ctx, sector);
        let probe_failed = found.is_none();
        let pair = found.unwrap_or_else(|| {
            let bearing = sector;
            let natural = (home.0 + bearing.cos() * LEG, home.2 + bearing.sin() * LEG);
            let side = (bearing.sin(), -bearing.cos());
            let marker = (
                natural.0 + side.0 * MARKER_OFFSET,
                natural.1 + side.1 * MARKER_OFFSET,
            );
            Pair {
                natural_post: natural,
                marker_site: marker,
                marker_post: marker,
                natural_focus: (anchor.0, home.1 + 1.4, anchor.1),
                natural_y: home.1.floor() as i32,
            }
        });

        let natural_distance = travel_to(ctx, pair.natural_post, TRAVEL_HOP, ARRIVE).await;
        let natural_faced = ctx.look_until_facing(pair.natural_focus, 7.0, 5_000).await;
        ctx.record(
            TYPE_LOOK,
            &format!("vis-landmark-marker-natural-{index}-{cycle}"),
            "Frame the natural feature before introducing its paired marker.",
            json!({"global_index": global, "cycle": cycle, "probe_failed": probe_failed, "natural_y": pair.natural_y, "faced": natural_faced, "arrival_distance": r2(natural_distance)}),
        );
        ctx.still(DWELL_MS).await;
        if ctx.target_met() {
            break;
        }

        let marker_distance = travel_to(ctx, pair.marker_post, TRAVEL_HOP, ARRIVE).await;
        let marker_y = ctx
            .position()
            .map(|p| p.1.round() as i64)
            .unwrap_or(pair.natural_y as i64);
        let mut marker_new = false;
        let marker = if let Some(existing) = known_marker {
            Some(existing)
        } else {
            let desired = (
                pair.marker_site.0.round() as i64,
                marker_y,
                pair.marker_site.1.round() as i64,
            );
            let placed = raise_landmark(ctx, desired, MARKER_HEIGHT, MARKER_MATERIAL).await;
            marker_new = placed.is_some();
            known_marker = placed;
            placed
        };
        let marker_focus = marker.map(|(_, focus)| focus).unwrap_or((
            pair.marker_site.0,
            marker_y as f64 + MARKER_HEIGHT as f64,
            pair.marker_site.1,
        ));
        let marker_faced = ctx.look_until_facing(marker_focus, 7.0, 5_000).await;
        let marker_observed = marker.and_then(|(base, _)| {
            area_find_block(
                ctx,
                (base.x, base.z),
                (base.y - 2, base.y + MARKER_HEIGHT as i32 + 1),
                MARKER_MATERIAL,
            )
        });
        ctx.record(
            TYPE_LOOK,
            &format!("vis-landmark-marker-script-{index}-{cycle}"),
            "Place and frame the scripted partner marker beside the natural feature.",
            json!({"cycle": cycle, "marker_placed": marker_new, "marker_known": marker.is_some(), "marker_visible": marker_observed.is_some(), "faced": marker_faced, "arrival_distance": r2(marker_distance), "material": MARKER_MATERIAL}),
        );
        ctx.still(DWELL_MS).await;
        if ctx.target_met() {
            break;
        }

        let returned = travel_to(ctx, pair.natural_post, TRAVEL_HOP, ARRIVE).await;
        let return_natural = ctx.look_until_facing(pair.natural_focus, 7.0, 5_000).await;
        ctx.record(
            TYPE_LOOK,
            &format!("vis-landmark-marker-return-{index}-{cycle}"),
            "Return to the natural post and re-identify both members of the pair.",
            json!({"cycle": cycle, "is_revisit": true, "natural_refaced": return_natural, "marker_present": marker.and_then(|(base, _)| area_find_block(ctx, (base.x, base.z), (base.y - 2, base.y + MARKER_HEIGHT as i32 + 1), MARKER_MATERIAL)).is_some(), "return_distance": r2(returned)}),
        );
        ctx.still(DWELL_MS).await;
        cycle += 1;
    }
}
