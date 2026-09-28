use super::*;

/// Shapes a later pass may pick. The seeded kind opens the episode; after that the bot works
/// through the vocabulary instead of re-attempting one shape it has already left standing.
const KINDS: [&str; 5] = ["pillar", "wall", "arch", "cross", "tower"];

/// Draw in `[lo, hi)` from the per-bot stream, so a number this scenario could have hard-coded
/// varies per bot and per pass and still replays from the seed.
fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

/// How far sideways off its own slot a pass may place its structure. `station_ring` widens the ring
/// to absorb two neighbours wandering toward each other by this much.
const SITE_JITTER: f64 = 2.0;

/// Each bot independently builds a compact structure near the swarm anchor with genuine survival
/// placement. Stations lie on an N-extensible ring, so no target falls back to the world origin.
/// After building, the bot retreats 4–7 blocks, holds a readable view of its own structure, and
/// briefly acknowledges the nearest peer — then walks further out along its own spoke and builds the
/// next shape, so the episode never runs out of structure to build.
pub(super) async fn run(ctx: &mut Ctx) {
    let cfg = ctx.cfg().clone();
    let index = ctx.index();
    let bot_count = ctx.player_count().max(1);
    let (kind, material, spacing) = {
        let s = &ctx.engine().shared;
        (
            s.native_structure_kind.clone(),
            s.native_block_pattern.clone(),
            s.native_spacing.max(6.0),
        )
    };
    let seeded_kind = if kind.is_empty() {
        "pillar".to_string()
    } else {
        kind
    };
    let material = if material.is_empty() {
        "minecraft:stone_bricks".to_string()
    } else {
        material
    };
    let anchor = ctx.anchor();
    let station_gap = spacing.max(7.0);
    let ring = station_gap
        .max((bot_count as f64 * station_gap) / std::f64::consts::TAU)
        .max(station_ring(bot_count, SITE_JITTER));
    let ang = (index as f64 / bot_count as f64) * std::f64::consts::TAU;

    let mut pass = 0usize;
    loop {
        // A finished structure stays in the world, so the next one needs its own footprint. The site
        // steps out along this bot's own spoke and slides along it, and both draws only ever push
        // the ring outward, so the sites spread without closing on the neighbouring bot's.
        let kind = if pass == 0 {
            seeded_kind.clone()
        } else {
            KINDS[(ctx.rng_next() * KINDS.len() as f64) as usize % KINDS.len()].to_string()
        };
        let radius = ring + vary(ctx, 0.0, 4.0);
        let tangent = vary(ctx, -SITE_JITTER, SITE_JITTER);
        let sx =
            anchor.0.round() as i64 + (ang.cos() * radius - ang.sin() * tangent).round() as i64;
        let sz =
            anchor.1.round() as i64 + (ang.sin() * radius + ang.cos() * tangent).round() as i64;
        let approach_x = sx as f64 - ang.cos() * 2.0;
        let approach_z = sz as f64 - ang.sin() * 2.0;
        ctx.move_to(approach_x, approach_z, cfg.move_timeout_ms.min(8000), 1.5)
            .await;
        // The bot's own feet are the wrong ground whenever the approach stalled short: a ledge
        // drop leaves the bot below the plateau, and a structure sited at feet height goes inside
        // rock — where place_block bails before sending any packet, so the whole pass grinds out
        // a 25 s statue (p00 on the 08-19 audit). Probe the site column itself.
        let feet_y = ctx.position().map(|p| p.1).unwrap_or(64.0).floor() as i64;
        let base_y = site_ground_y(ctx, sx, sz, feet_y);
        let blocks_raw = pattern_blocks(&kind, sx, base_y, sz);
        let kind_for_desc = kind.clone();
        ctx.record(
            TYPE_LOOK,
            &format!("vis-struct-start-{index}-p{pass}"),
            &format!("Bot {index} independently builds a compact {kind} at ({sx}, {sz})."),
            json!({"pass": pass, "kind": kind, "origin": {"x": sx, "z": sz},
                "anchor": {"x": r2(anchor.0), "z": r2(anchor.1)}, "material": material,
                "method": "real_place"}),
        );
        let total = blocks_raw.len();
        // Attach the (single) material to each block for build_blocks.
        let blocks: Vec<((i64, i64, i64), String)> = blocks_raw
            .into_iter()
            .map(|b| (b, material.clone()))
            .collect();
        let (ok, attempted) = ctx
            .build_blocks(&blocks, |b, _t, _m| {
                (
                    format!("vis-struct-{index}-p{pass}-{b}"),
                    format!("Place {kind_for_desc} block {}/{total}.", b + 1),
                    json!({"kind": kind_for_desc, "block_index": b}),
                )
            })
            .await;
        let complete = ok as usize == total;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-struct-done-{index}-p{pass}"),
            &format!(
                "Structure {}: placed {ok}/{attempted}/{total} {kind} blocks \
                 (placed/attempted/expected).",
                if complete { "complete" } else { "incomplete" }
            ),
            json!({"pass": pass, "kind": kind, "complete": complete, "expected": total,
                "placed": ok, "attempted": attempted}),
        );

        // Retreat outward: the distance is its own draw inside the requested 4–7-block observation
        // range, so the viewpoint is not the same framing on every pass.
        let retreat = vary(ctx, 4.5, 6.5);
        let retreat_x = sx as f64 + 0.5 + ang.cos() * retreat;
        let retreat_z = sz as f64 + 0.5 + ang.sin() * retreat;
        ctx.move_to(retreat_x, retreat_z, cfg.move_timeout_ms.min(8000), 1.5)
            .await;
        let structure_focus = (sx as f64 + 0.5, base_y as f64 + 1.4, sz as f64 + 0.5);
        ctx.look_default(structure_focus).await;
        witness_peer_edits(ctx).await;
        glance_at_nearest_peer(ctx, None).await;
        ctx.look_default(structure_focus).await;
        sleep_ms(vary(ctx, 800.0, 2_400.0) as u64).await;

        if ctx.target_met() {
            break;
        }
        pass += 1;
    }
}

/// Ground level of the site column: the highest air-over-solid cell in a band around the bot's own
/// feet (the band reaches further up than down because the failure mode is a bot stranded *below*
/// its plateau site). Falls back to the feet when the column is sheer or unloaded.
fn site_ground_y(ctx: &Ctx, sx: i64, sz: i64, feet_y: i64) -> i64 {
    for y in ((feet_y - 8)..=(feet_y + 16)).rev() {
        let here = ctx.block_id_at(BlockPos::new(sx as i32, y as i32, sz as i32));
        let below = ctx.block_id_at(BlockPos::new(sx as i32, (y - 1) as i32, sz as i32));
        if super::audience_build::is_air_id(here.as_deref())
            && !super::audience_build::is_air_id(below.as_deref())
        {
            return y;
        }
    }
    feet_y
}
