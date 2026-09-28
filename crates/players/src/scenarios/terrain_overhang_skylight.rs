//! CATALOG: family=locomotion min_players=1 single_process=false world_edits=false scenery=none far_players=false
//!
//! Step beneath a natural overhang or canopy, look back through its bright opening, pitch up to the
//! roof edge, then return outside and re-frame that edge. The revisit problem is vertical
//! recoverability: the sky is present in the opening view, removed by overhead terrain, and
//! restored from the same outside station after a closed in-and-out trajectory. This differs from a
//! cave-depth walk by staying at the mouth and making the roof/sky boundary, not darkness, the
//! subject.
//!
//! Shelter is searched, never assumed. A 19-by-19 grid at three-block stride looks for two blocks of
//! headroom plus a solid roof three to six blocks above the feet. A real roof records `in_band: true`;
//! if none exists, a four-block out-and-back remains active and records `probe_failed: true`. The
//! 9-block reach keeps the mouth shallow, the 3..6 roof band both fits a player and fills the upper
//! frame, the 1.2-second dwell stays far below the frozen threshold, and 0.8 arrival cannot swallow
//! a 4-block leg. `look_until_facing` proves yaw; requested pitch is recorded separately because the
//! helper does not verify pitch. No camera intent is reissued during movement.
use super::*;

const SEARCH_REACH: i32 = 9;
const STRIDE: usize = 3;
const LIFT: i32 = 2;
const ROOF_MIN: i32 = 3;
const ROOF_MAX: i32 = 6;
const LEG: f64 = 4.0;
const ARRIVE: f64 = 0.8;
const DWELL_MS: u64 = 1_200;
const SURFACE_SCAN: i32 = 64;
const MOUTH_DEPTH_MAX: i32 = 2;

/// Find the natural open-sky surface above a candidate mouth. This prevents a cave floor from
/// masquerading as a shallow overhang when the bot itself has dropped below the local surface.
fn surface_y(ctx: &Ctx, x: i32, z: i32, hint: i32) -> Option<i32> {
    for y in ((hint - SURFACE_SCAN)..=(hint + SURFACE_SCAN)).rev() {
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

struct Shelter {
    xz: (f64, f64),
    roof: (f64, f64, f64),
    height: i32,
}

fn probe(ctx: &Ctx) -> Option<Shelter> {
    let own = ctx.position()?;
    let hint = own.1.floor() as i32;
    let (bx, bz) = (own.0.floor() as i32, own.2.floor() as i32);
    let open_surface = surface_y(ctx, bx, bz, hint).unwrap_or(hint);
    let mut best: Option<(f64, Shelter)> = None;
    for dx in (-SEARCH_REACH..=SEARCH_REACH).step_by(STRIDE) {
        for dz in (-SEARCH_REACH..=SEARCH_REACH).step_by(STRIDE) {
            for lift in -LIFT..=LIFT {
                let feet = BlockPos::new(bx + dx, hint + lift, bz + dz);
                if feet.y < open_surface - MOUTH_DEPTH_MAX
                    || !block_is_air(ctx, feet)
                    || !block_is_air(ctx, feet.up(1))
                    || !block_is_safe_support(ctx, feet.down(1))
                {
                    continue;
                }
                let Some(height) =
                    (ROOF_MIN..=ROOF_MAX).find(|&dy| block_is_safe_support(ctx, feet.up(dy)))
                else {
                    continue;
                };
                let distance = ((dx * dx + dz * dz) as f64).sqrt();
                let shelter = Shelter {
                    xz: (feet.x as f64 + 0.5, feet.z as f64 + 0.5),
                    roof: (
                        feet.x as f64 + 0.5,
                        (feet.y + height) as f64 + 0.5,
                        feet.z as f64 + 0.5,
                    ),
                    height,
                };
                if best.as_ref().is_none_or(|(old, _)| distance < *old) {
                    best = Some((distance, shelter));
                }
            }
        }
    }
    best.map(|(_, shelter)| shelter)
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
    ctx.record(TYPE_LOOK, &format!("vis-terrain-overhang-init-{index}"),
        "Face the anchor before searching for a shallow roofed station.",
        json!({"global_index": global, "search_reach": SEARCH_REACH, "roof_band": [ROOF_MIN, ROOF_MAX]}));
    let mut cycle = 0u32;
    while !ctx.target_met() {
        let Some(home) = ctx.position() else {
            sleep_ms(200).await;
            continue;
        };
        let home_focus = head_of(home);
        let found = probe(ctx);
        let failed = found.is_none();
        let shelter = found.unwrap_or_else(|| {
            let x = home.0 + sector.cos() * LEG;
            let z = home.2 + sector.sin() * LEG;
            Shelter {
                xz: (x, z),
                roof: (x, home.1 + 3.0, z),
                height: 0,
            }
        });
        let first = ctx.look_until_facing(shelter.roof, 8.0, 5_000).await;
        let first_pitch = ctx
            .position()
            .map(|p| look_angles(head_of(p), shelter.roof).1)
            .unwrap_or(0.0);
        ctx.record(TYPE_LOOK, &format!("vis-terrain-overhang-open-{index}-{cycle}"),
            "From outside, frame the roof edge against the visible sky before entering.",
            json!({"cycle": cycle, "framed": first, "pitch_deg": r2(first_pitch),
                   "roof_height": shelter.height, "in_band": shelter.height > 0, "probe_failed": failed}));
        ctx.still(DWELL_MS).await;
        if ctx.target_met() {
            break;
        }
        travel_to(ctx, shelter.xz, TRAVEL_HOP, ARRIVE).await;
        let out = ctx.look_until_facing(home_focus, 8.0, 5_000).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-terrain-overhang-mouth-{index}-{cycle}"),
            "Under the roof, look back out through the opening at the ground just left.",
            json!({"cycle": cycle, "framed": out, "overhead_occlusion": shelter.height > 0}),
        );
        ctx.still(DWELL_MS).await;
        if ctx.target_met() {
            break;
        }
        let up = ctx.look_until_facing(shelter.roof, 8.0, 5_000).await;
        let up_pitch = ctx
            .position()
            .map(|p| look_angles(head_of(p), shelter.roof).1)
            .unwrap_or(0.0);
        ctx.record(
            TYPE_LOOK,
            &format!("vis-terrain-overhang-roof-{index}-{cycle}"),
            "Pitch up beneath the overhang so the roof replaces the sky in the upper frame.",
            json!({"cycle": cycle, "framed": up, "pitch_deg": r2(up_pitch), "is_return": true}),
        );
        ctx.still(DWELL_MS).await;
        if ctx.target_met() {
            break;
        }
        let returned = travel_to(ctx, (home.0, home.2), TRAVEL_HOP, ARRIVE).await;
        let reframed = ctx.look_until_facing(shelter.roof, 8.0, 5_000).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-terrain-overhang-return-{index}-{cycle}"),
            "Return outside and re-frame the roof edge against the restored sky.",
            json!({"cycle": cycle, "framed": reframed, "is_revisit": true,
                   "distance_from_home": r2(returned)}),
        );
        cycle += 1;
    }
}
