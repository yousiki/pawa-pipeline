use super::*;

const TAU: f64 = std::f64::consts::TAU;

/// The shape vocabulary a beat may write — the NON-COLUMN end of the `pattern_blocks` set
/// (6–7 blocks each; pillar/tower are excluded on purpose, so the metronome never reads as
/// pillar_rise_and_collapse). Consecutive beats never repeat a kind, so the site visibly MORPHS
/// across the erase.
const KINDS: [&str; 3] = ["wall", "arch", "cross"];

/// Materials a beat may write in — a PRIMARY and an ACCENT are drawn per beat and mixed through
/// the shape, so every written structure is visibly two-toned. Registry ids WITHOUT the
/// `minecraft:` prefix, exactly as `block_id_at` reports them, because the same list is the
/// erase's "did we lay this" test.
const MATERIALS: [&str; 6] = [
    "stone_bricks",
    "oak_planks",
    "oak_log",
    "cobblestone",
    "bricks",
    "stone",
];

/// How far a beat's origin may slide off the site centre, absorbed by the ring geometry.
const SITE_WANDER: f64 = 1.5;

/// Station ring cap keeping every site — plus its wander and orbit — inside the 41x41
/// `build_arena` slab (radius 20 around the anchor).
const MAX_RING: f64 = 15.5;

/// Draw in `[lo, hi)` from the per-bot stream, so a number this scenario could have hard-coded
/// varies per bot and per pass and still replays from the seed.
fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

/// Uniform draw in `0..n` from the per-bot stream.
fn pick(ctx: &mut Ctx, n: usize) -> usize {
    ((ctx.rng_next() * n as f64) as usize).min(n.saturating_sub(1))
}

/// Site ring radius plus the wander it can absorb, recomputed from the capped radius' actual
/// chord so the 4.5-block peer gap survives whatever `bot_count` does to the ring.
fn ring_geometry(bot_count: usize) -> (f64, f64) {
    let n = bot_count.max(2) as f64;
    let radius = station_ring(bot_count, SITE_WANDER).min(MAX_RING);
    let chord = 2.0 * radius * (std::f64::consts::PI / n).sin();
    let wander = ((chord - MIN_PEER_GAP) / 2.0).clamp(0.0, SITE_WANDER);
    (radius, wander)
}

/// Whether the cell holds a block a write beat could have laid. The erase digs nothing else, so
/// the arena floor is never opened and nothing ever darkens.
fn is_beat_block(ctx: &Ctx, cell: BlockPos) -> bool {
    matches!(ctx.block_id_at(cell).as_deref(), Some(id) if MATERIALS.contains(&id))
}

/// Ground level of the site column: the highest air-over-solid cell in a band around `ref_y`.
fn site_ground_y(ctx: &Ctx, sx: i64, sz: i64, ref_y: i64) -> i64 {
    for y in ((ref_y - 8)..=(ref_y + 12)).rev() {
        let pos = BlockPos::new(sx as i32, y as i32, sz as i32);
        if block_is_air(ctx, pos) && block_is_safe_support(ctx, pos.down(1)) {
            return y;
        }
    }
    ref_y
}

/// Half-orbit of the site between the write and the erase: two or three arc legs at a drawn
/// radius and direction, camera pinned on the structure, with the world-synchronization beats at
/// the leg boundaries. This is what keeps the metronome's "rest" musical instead of a statue.
async fn orbit(ctx: &mut Ctx, site: (f64, f64), focus_y: f64, beat: usize) {
    let index = ctx.index();
    let timeout = ctx.cfg().move_timeout_ms.min(6_000);
    let Some(p) = ctx.position() else { return };
    let mut angle = (p.2 - site.1).atan2(p.0 - site.0);
    let dir = if ctx.rng_next() < 0.5 { 1.0 } else { -1.0 };
    let legs = 2 + pick(ctx, 2);
    let radius = vary(ctx, 4.5, 6.5);
    let focus = (site.0, focus_y, site.1);
    ctx.record(
        TYPE_MOVE,
        &format!("vis-metronome-orbit-{index}-b{beat}"),
        "Orbit the site between beats, keeping the structure in frame.",
        json!({"beat": beat, "legs": legs, "radius": r2(radius), "direction": dir}),
    );
    for _ in 0..legs {
        if ctx.target_met() {
            return;
        }
        angle += dir * vary(ctx, 0.7, 1.2);
        ctx.move_to_watching_point(
            site.0 + angle.cos() * radius,
            site.1 + angle.sin() * radius,
            focus,
            timeout,
            1.8,
        )
        .await;
        sleep_ms(vary(ctx, 500.0, 1_300.0) as u64).await;
        witness_peer_edits(ctx).await;
    }
    glance_at_nearest_peer(ctx, None).await;
}

/// Every bot runs a write–erase metronome on one fixed footprint: raise a shape, half-orbit it,
/// tear the SAME shape entirely back down, half-orbit the bare ground, then write a DIFFERENT
/// shape in a different material on the same spot — tick, tock, for the whole episode. The site
/// itself blinks between filled and empty while its silhouette morphs across the vocabulary,
/// giving the corpus a dense, station-stable structure-level write-erase rhythm that peers
/// witness from around the ring. Zero mobs; only kit blocks are placed and only kit blocks are
/// ever dug, so the arena floor stays intact and lit. No barriers: beat lengths are per-bot
/// draws, so the swarm's metronomes drift freely out of phase.
pub(super) async fn run(ctx: &mut Ctx) {
    let index = ctx.index();
    let n = ctx.player_count().max(1);
    let anchor = ctx.anchor();
    let (radius, wander) = ring_geometry(n);
    let ang = index as f64 / n as f64 * TAU;
    let station = (anchor.0 + ang.cos() * radius, anchor.1 + ang.sin() * radius);
    let timeout = ctx.cfg().move_timeout_ms.min(8_000);
    ctx.record(
        TYPE_LOOK,
        &format!("vis-metronome-init-{index}"),
        "Structure metronome: write a shape, tear it fully down, write a different one on the same footprint.",
        json!({"station": {"x": r2(station.0), "z": r2(station.1)}, "ring_radius": r2(radius),
            "kinds": KINDS, "materials": MATERIALS}),
    );
    ctx.move_to(station.0, station.1, ctx.cfg().move_timeout_ms, 2.0)
        .await;
    let mut beat = 0usize;
    let mut last_kind = usize::MAX;
    while !ctx.target_met() {
        // --- WRITE: a fresh kind (never the previous one) in a two-tone material mix ------------
        let mut kind_i = pick(ctx, KINDS.len());
        if kind_i == last_kind {
            kind_i = (kind_i + 1) % KINDS.len();
        }
        last_kind = kind_i;
        let kind = KINDS[kind_i];
        // Primary and accent are distinct draws; the accent share is itself drawn per beat, so
        // no two written structures carry the same mix.
        let primary_i = pick(ctx, MATERIALS.len());
        let accent_i = (primary_i + 1 + pick(ctx, MATERIALS.len() - 1)) % MATERIALS.len();
        let primary = format!("minecraft:{}", MATERIALS[primary_i]);
        let accent = format!("minecraft:{}", MATERIALS[accent_i]);
        let accent_share = vary(ctx, 0.25, 0.5);
        let sx = (station.0 + vary(ctx, -wander, wander)).floor() as i64;
        let sz = (station.1 + vary(ctx, -wander, wander)).floor() as i64;
        let floor_y = ctx.position().map(|p| p.1).unwrap_or(64.0).floor() as i64;
        let base_y = site_ground_y(ctx, sx, sz, floor_y);
        // Stand off the origin at a drawn polar angle so no cell overlaps the placer's hitbox.
        let stand = ctx.rng_next() * TAU;
        ctx.move_to(
            sx as f64 + 0.5 + stand.cos() * 2.4,
            sz as f64 + 0.5 + stand.sin() * 2.4,
            timeout,
            1.5,
        )
        .await;
        let expected = pattern_blocks(kind, sx, base_y, sz);
        let total = expected.len();
        let blocks: Vec<((i64, i64, i64), String)> = expected
            .iter()
            .map(|&b| {
                let mat = if ctx.rng_next() < accent_share {
                    accent.clone()
                } else {
                    primary.clone()
                };
                (b, mat)
            })
            .collect();
        let (placed, attempted) = ctx
            .build_blocks(&blocks, |b, _t, _m| {
                (
                    format!("vis-metronome-write-{index}-b{beat}-{b}"),
                    format!("Tick: write {kind} block {}/{total}.", b + 1),
                    json!({"beat": beat, "kind": kind, "block_index": b}),
                )
            })
            .await;
        ctx.record(
            TYPE_PLACE,
            &format!("vis-metronome-tick-{index}-b{beat}"),
            &format!("Beat {beat} tick: {kind} written ({placed}/{attempted}/{total})."),
            json!({"beat": beat, "kind": kind, "primary": primary, "accent": accent,
                "accent_share": r2(accent_share), "placed": placed,
                "attempted": attempted, "expected": total, "site": {"x": sx, "z": sz}}),
        );
        if ctx.target_met() {
            break;
        }

        // --- REST: half-orbit the standing shape -----------------------------------------------
        let focus_y = base_y as f64 + 1.4;
        orbit(ctx, (sx as f64 + 0.5, sz as f64 + 0.5), focus_y, beat).await;
        if ctx.target_met() {
            break;
        }

        // --- ERASE: the same shape comes entirely back down, top-down ---------------------------
        let mut standing: Vec<BlockPos> = expected
            .iter()
            .map(|&(x, y, z)| BlockPos::new(x as i32, y as i32, z as i32))
            .filter(|&c| is_beat_block(ctx, c))
            .collect();
        standing.sort_by_key(|c| std::cmp::Reverse(c.y));
        ctx.equip("diamond_pickaxe").await;
        let mut erased = 0usize;
        for cell in standing {
            if ctx.target_met() {
                break;
            }
            if !is_beat_block(ctx, cell) {
                continue;
            }
            if !ctx.target_in_reach(cell) {
                ctx.move_to(cell.x as f64 + 0.5, cell.z as f64 + 0.5, timeout, 2.0)
                    .await;
            }
            let dug = ctx.dig(cell).await;
            if dug {
                erased += 1;
            }
            ctx.record(
                TYPE_MINE,
                &format!("vis-metronome-erase-{index}-b{beat}-{erased}"),
                &format!("Tock: erase one {kind} block back off the footprint."),
                json!({"beat": beat, "kind": kind, "dug": dug,
                    "block_position": {"x": cell.x, "y": cell.y, "z": cell.z}}),
            );
        }
        ctx.record(
            TYPE_MINE,
            &format!("vis-metronome-tock-{index}-b{beat}"),
            &format!("Beat {beat} tock: {kind} fully erased ({erased} blocks down)."),
            json!({"beat": beat, "kind": kind, "erased": erased}),
        );

        // --- REST: half-orbit the bare footprint before the next tick ---------------------------
        orbit(
            ctx,
            (sx as f64 + 0.5, sz as f64 + 0.5),
            base_y as f64 + 0.5,
            beat,
        )
        .await;
        beat += 1;
    }
}
