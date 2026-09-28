//! CATALOG: family=locomotion min_players=1 single_process=false world_edits=false scenery=none far_players=false
//!
//! Use a naturally wooded gap as a short visual tunnel: frame the trunk-and-leaf edge, pass to its
//! other side, then return to the opening station and frame the same edge again. The revisit
//! problem is the recoverable terrain occlusion: foliage removes the distant background from one
//! side while the same landmark is visible through the gap before and after the loop. The path is a
//! closed out, around, and back route; no camera is locked while moving.
//!
//! Trees are probed, never assumed. A 10-block ring checks for log or leaves above a walkable daylight
//! surface. A found tree gets `tree_found: true`; a solid terrain rise is retained as a degraded
//! blocker, and an empty probe falls back to a four-block out-and-back (`probe_failed: true`). The
//! 10-block ring, 3-block stand-off, 4-block leg, 18-block vertical scan, and 0.8-block tolerance
//! keep foliage close enough to occlude while every movement remains a real press.
use super::*;

const SEARCH_RADIUS: f64 = 10.0;
type TreeGapProbe = ((f64, f64), (f64, f64, f64), f64, bool);

const SLOTS: usize = 16;
const SCAN_SPAN: i32 = 18;
const STANDOFF: f64 = 3.0;
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

fn tree_here(ctx: &Ctx, x: i32, y: i32, z: i32) -> bool {
    (-2..=2).any(|dx| {
        (-2..=2).any(|dz| {
            (0..=5).any(|dy| {
                ctx.block_id_at(BlockPos::new(x + dx, y + dy, z + dz))
                    .is_some_and(|id| id.contains("leaves") || id.ends_with("log"))
            })
        })
    })
}

fn probe(ctx: &Ctx, sector: f64) -> Option<TreeGapProbe> {
    let own = ctx.position()?;
    let hint = own.1.floor() as i32;
    let mut fallback: Option<(f64, (f64, f64), i32)> = None;
    for slot in 0..SLOTS {
        let bearing = sector + slot as f64 * std::f64::consts::TAU / SLOTS as f64;
        let x = own.0 + bearing.cos() * SEARCH_RADIUS;
        let z = own.2 + bearing.sin() * SEARCH_RADIUS;
        let Some(feet) = surface(ctx, x.floor() as i32, z.floor() as i32, hint) else {
            continue;
        };
        let is_tree = tree_here(ctx, x.floor() as i32, feet, z.floor() as i32);
        let rise = (feet - hint).max(0);
        if is_tree {
            let focus = (x, feet as f64 + 2.0, z);
            return Some((
                (x - bearing.cos() * STANDOFF, z - bearing.sin() * STANDOFF),
                focus,
                0.0,
                true,
            ));
        }
        if fallback.as_ref().is_none_or(|(_, _, best)| rise > *best) {
            fallback = Some((bearing, (x, z), rise));
        }
    }
    let (bearing, centre, rise) = fallback?;
    Some((
        (
            centre.0 - bearing.cos() * STANDOFF,
            centre.1 - bearing.sin() * STANDOFF,
        ),
        (centre.0, hint as f64 + 1.4 + rise as f64, centre.1),
        rise as f64,
        false,
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
        &format!("vis-terrain-tree-init-{index}"),
        "Face the anchor before probing a natural tree gap.",
        json!({"global_index": global, "search_radius": SEARCH_RADIUS}),
    );
    let mut cycle = 0u32;
    while !ctx.target_met() {
        let Some(home) = ctx.position() else {
            sleep_ms(200).await;
            continue;
        };
        let found = probe(ctx, sector);
        let failed = found.is_none();
        let (near, focus, relief, tree) = found.unwrap_or_else(|| {
            let p = (home.0 + sector.cos() * LEG, home.2 + sector.sin() * LEG);
            (p, (p.0, home.1 + 1.4, p.1), 0.0, false)
        });
        let bearing = (focus.2 - home.2).atan2(focus.0 - home.0);
        let far_wanted = (
            focus.0 + bearing.cos() * STANDOFF,
            focus.2 + bearing.sin() * STANDOFF,
        );
        let far = walkable_xz_near(ctx, far_wanted, home.1.floor() as i32).unwrap_or(far_wanted);
        travel_to(ctx, near, TRAVEL_HOP, ARRIVE).await;
        let first = ctx.look_until_facing(focus, 8.0, 5_000).await;
        ctx.record(
            TYPE_LOOK,
            &format!("vis-terrain-tree-open-{index}-{cycle}"),
            "Frame the tree edge before passing around its occluding canopy.",
            json!({"cycle": cycle, "framed": first, "tree_found": tree, "relief": relief,
                   "probe_failed": failed}),
        );
        ctx.still(DWELL_MS).await;
        if ctx.target_met() {
            break;
        }
        travel_to(ctx, far, TRAVEL_HOP, ARRIVE).await;
        let back = ctx.look_until_facing(focus, 8.0, 5_000).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-terrain-tree-cross-{index}-{cycle}"),
            "Pass the canopy edge and re-frame the same trunk from the far side.",
            json!({"cycle": cycle, "framed": back, "occluder": if tree {"tree"} else {"terrain"},
                   "is_return": true}),
        );
        ctx.still(DWELL_MS).await;
        if ctx.target_met() {
            break;
        }
        let returned = travel_to(ctx, (home.0, home.2), TRAVEL_HOP, ARRIVE).await;
        ctx.look_until_facing(focus, 8.0, 5_000).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-terrain-tree-return-{index}-{cycle}"),
            "Return to the opening station and close the foliage sightline loop.",
            json!({"cycle": cycle, "is_revisit": true, "distance_from_home": r2(returned)}),
        );
        cycle += 1;
    }
}
