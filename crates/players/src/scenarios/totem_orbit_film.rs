use super::*;

const TAU: f64 = std::f64::consts::TAU;

/// One totem material per bot (mod), so the ring of camera subjects reads as distinct hands.
const TOTEM_MATERIALS: [&str; 4] = [
    "minecraft:stone_bricks",
    "minecraft:oak_log",
    "minecraft:bricks",
    "minecraft:cobblestone",
];

/// Orbit radius band around the filmed totem: close enough that a 2-4 block column fills a
/// readable share of the frame, far enough that the column never fills it. The first test round
/// ran 3.5-5.0 and its closest sampled frames were a single block face wall-to-wall — the subject
/// needs its surroundings in frame to read as a subject at all.
const ORBIT_MIN: f64 = 4.5;
const ORBIT_MAX: f64 = 6.0;

/// Sideways wander allowed when siting or re-siting a totem; [`film_ring`] absorbs it.
const SITE_JITTER: f64 = 1.5;

/// Tallest a totem grows before its owner retires it and starts a fresh one nearby, so the build
/// window never runs out of placement work.
const MAX_HEIGHT: i64 = 4;

/// Wall-clock cycle every bot derives from its own episode clock (the carousel_chase recipe: a
/// pure clock function, no shared barrier): a build window at each cycle start, the rest spent
/// orbit-filming the next bot's totem. Synchronized windows are what keep a totem's owner away
/// while someone else is orbiting it.
const CYCLE_MS: i64 = 40_000;
const BUILD_WINDOW_MS: i64 = 14_000;

/// Draw in `[lo, hi)` from the per-bot stream, so a number this scenario could have hard-coded
/// varies per bot and per beat and still replays from the seed.
fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

/// Station ring radius, solved directly for the chord this scene needs: neighbouring totems must
/// clear [`MIN_PEER_GAP`] even when two filmers orbit adjacent totems at [`ORBIT_MAX`] and both
/// totems have wandered [`SITE_JITTER`] toward each other. `station_ring`'s floored base radius
/// would land the same guarantee much farther off the prepared arena.
fn film_ring(n: usize) -> f64 {
    let n = n.max(2) as f64;
    ((MIN_PEER_GAP + 2.0 * (ORBIT_MAX + SITE_JITTER)) / (2.0 * (std::f64::consts::PI / n).sin()))
        .max(8.0)
}

/// Index-only totem station formula, so a filmer aims at a peer's totem without any channel.
fn station_of(anchor: (f64, f64), j: usize, n: usize) -> (f64, f64) {
    let ang = (j as f64 / n.max(1) as f64) * TAU;
    let r = film_ring(n);
    (anchor.0 + ang.cos() * r, anchor.1 + ang.sin() * r)
}

/// Each bot raises a small totem on a shared ring, then spends most of every cycle as a camera
/// operator: orbiting the NEXT bot's totem with the view pinned on it while the body strafes the
/// arc — the gaze-decoupled-from-travel shot this scene exists for. Build windows are
/// clock-synchronized so every
/// subject is unattended while filmed, and each window adds a layer (or a fresh totem) so the
/// witness channel keeps carrying edits.
pub(crate) async fn run(ctx: &mut Ctx) {
    let index = ctx.index();
    let n = ctx.player_count().max(1);
    let anchor = ctx.anchor();
    let material = TOTEM_MATERIALS[index % TOTEM_MATERIALS.len()];
    let subject = (index + 1) % n;
    // Per-bot lag on the cycle boundary, so window changes ripple instead of snapping in sync.
    let clock_jitter = (ctx.rng_next() * 3_000.0) as i64;
    let mut totem: Option<(BlockPos, i64)> = None;
    let mut orbit_dir = if ctx.rng_next() < 0.5 { 1.0 } else { -1.0 };
    let mut orbit_radius = vary(ctx, ORBIT_MIN, ORBIT_MAX);
    let mut round = 0u32;
    ctx.record(
        TYPE_LOOK,
        &format!("vis-film-init-{index}"),
        &format!("Totem film ring: bot {index} builds at slot {index}, films bot {subject}."),
        json!({"slot": index, "subject": subject, "ring_radius": r2(film_ring(n)),
            "cycle_ms": CYCLE_MS, "build_window_ms": BUILD_WINDOW_MS, "material": material}),
    );
    while !ctx.target_met() {
        let building = (ctx.elapsed_ms() + clock_jitter).rem_euclid(CYCLE_MS) < BUILD_WINDOW_MS;
        if building {
            build_beat(ctx, anchor, n, material, &mut totem, round).await;
        } else {
            film_beat(
                ctx,
                anchor,
                n,
                subject,
                &mut orbit_dir,
                &mut orbit_radius,
                round,
            )
            .await;
        }
        round += 1;
    }
}

/// One build-window beat at the bot's own station: raise a fresh totem, or add a layer to the
/// standing one, then witness and take a keep-alive shuffle so the window's tail is never a statue.
async fn build_beat(
    ctx: &mut Ctx,
    anchor: (f64, f64),
    n: usize,
    material: &str,
    totem: &mut Option<(BlockPos, i64)>,
    round: u32,
) {
    let index = ctx.index();
    let station = station_of(anchor, index, n);
    // Retire a totem that topped out or lost its footing; the next one sites itself nearby.
    if let Some((base, height)) = *totem
        && (height >= MAX_HEIGHT || !block_is_safe_support(ctx, base.down(1)))
    {
        *totem = None;
    }
    match *totem {
        None => {
            let sx = station.0 + vary(ctx, -SITE_JITTER, SITE_JITTER);
            let sz = station.1 + vary(ctx, -SITE_JITTER, SITE_JITTER);
            let timeout = ctx.cfg().move_timeout_ms.min(10_000);
            ctx.move_to(sx, sz, timeout, 2.0).await;
            let feet_y = ctx.position().map(|p| p.1.floor() as i64).unwrap_or(64);
            let height = 2 + (ctx.rng_next() < 0.5) as i64;
            let Some(base) =
                marker_base_near(ctx, (sx.floor() as i64, feet_y, sz.floor() as i64), height)
            else {
                // No placeable column here: report it and move the body rather than pretending.
                ctx.record(
                    TYPE_MOVE,
                    &format!("vis-totem-nosite-{index}-r{round}"),
                    "No placeable totem column near the station this beat.",
                    json!({"round": round, "station": {"x": r2(station.0), "z": r2(station.1)}}),
                );
                shake_loose(ctx).await;
                return;
            };
            let cells: Vec<((i64, i64, i64), String)> = (0..height)
                .map(|dy| {
                    (
                        (base.x as i64, base.y as i64 + dy, base.z as i64),
                        material.to_string(),
                    )
                })
                .collect();
            let (placed, attempted) = ctx
                .build_blocks(&cells, |i, _t, _m| {
                    (
                        format!("vis-totem-raise-{index}-r{round}-{i}"),
                        format!("Raise totem layer {}/{height} for the film ring.", i + 1),
                        json!({"round": round, "layer": i}),
                    )
                })
                .await;
            *totem = Some((base, placed as i64));
            ctx.record(
                TYPE_PLACE,
                &format!("vis-totem-new-{index}-r{round}"),
                &format!("Totem stands {placed}/{height} layers tall (placed/planned)."),
                json!({"round": round, "placed": placed, "attempted": attempted,
                    "base": {"x": base.x, "y": base.y, "z": base.z}, "material": material}),
            );
        }
        Some((base, height)) => {
            let cell = (base.x as i64, base.y as i64 + height, base.z as i64);
            let cells = vec![(cell, material.to_string())];
            let (placed, _) = ctx
                .build_blocks(&cells, |_, _t, _m| {
                    (
                        format!("vis-totem-extend-{index}-r{round}"),
                        format!("Add layer {} to the standing totem.", height + 1),
                        json!({"round": round, "layer": height}),
                    )
                })
                .await;
            if placed > 0 {
                *totem = Some((base, height + 1));
            } else if !block_is_air(ctx, base.up(height as i32)) {
                // The next cell is occupied (someone or something intruded): retire this totem so
                // the following window sites a fresh one instead of grinding the same dead cell.
                *totem = None;
            }
        }
    }
    // The world-sync beat plus a shuffle: witness a peer's fresh edit, then take a new stand and
    // read the own totem from there.
    witness_peer_edits(ctx).await;
    if ctx.rng_next() < 0.6 {
        glance_at_nearest_peer(ctx, None).await;
    }
    let ang = ctx.rng_next() * TAU;
    let d = vary(ctx, 3.0, 4.5);
    ctx.move_to(
        station.0 + ang.cos() * d,
        station.1 + ang.sin() * d,
        5_000,
        1.5,
    )
    .await;
    if let Some((base, height)) = *totem {
        let focus = (
            base.x as f64 + 0.5,
            base.y as f64 + (height.max(1) as f64) * 0.5,
            base.z as f64 + 0.5,
        );
        ctx.look(focus, ctx.cfg().look_settle_ms.max(500)).await;
    }
    sleep_ms(vary(ctx, 400.0, 1_000.0) as u64).await;
}

/// One film-window beat: approach the subject's totem if still far (a plain travel leg whose
/// toward-travel camera frames offset the orbit's away-frames), otherwise strafe one orbit arc
/// with the view pinned on the totem. Direction and radius re-draw between arcs.
async fn film_beat(
    ctx: &mut Ctx,
    anchor: (f64, f64),
    n: usize,
    subject: usize,
    orbit_dir: &mut f64,
    orbit_radius: &mut f64,
    round: u32,
) {
    let index = ctx.index();
    let (sx, sz) = station_of(anchor, subject, n);
    let Some(own) = ctx.position() else {
        sleep_ms(250).await;
        return;
    };
    let focus = (sx, own.1 + 1.2, sz);
    let dist = (own.0 - sx).hypot(own.2 - sz);
    if dist > ORBIT_MAX + 2.5 {
        // Enter the orbit at the angle nearest the current stand: the shortest approach, and the
        // one that naturally diverges from the subject's owner leaving the other way.
        let ang = (own.2 - sz).atan2(own.0 - sx);
        let entry = (
            sx + ang.cos() * *orbit_radius,
            sz + ang.sin() * *orbit_radius,
        );
        let arrived = ctx
            .move_to(entry.0, entry.1, ctx.cfg().move_timeout_ms.min(8_000), 1.5)
            .await;
        ctx.look(focus, ctx.cfg().look_settle_ms.max(500)).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-film-approach-{index}-r{round}"),
            &format!("Walk to the orbit ring around bot {subject}'s totem."),
            json!({"round": round, "subject": subject, "arrived": arrived,
                "radius": r2(*orbit_radius)}),
        );
        return;
    }
    if ctx.rng_next() < 0.2 {
        *orbit_dir = -*orbit_dir;
    }
    if ctx.rng_next() < 0.25 {
        *orbit_radius = vary(ctx, ORBIT_MIN, ORBIT_MAX);
    }
    // One orbit arc, long enough that the press behind it can never read as key chatter.
    let span_deg = vary(ctx, 55.0, 100.0);
    let ang = (own.2 - sz).atan2(own.0 - sx) + span_deg.to_radians() * *orbit_dir;
    let to = (
        sx + ang.cos() * *orbit_radius,
        sz + ang.sin() * *orbit_radius,
    );
    let arrived = ctx
        .move_to_watching_point(to.0, to.1, focus, 6_000, 1.5)
        .await;
    ctx.record(
        TYPE_MOVE,
        &format!("vis-film-arc-{index}-r{round}"),
        &format!(
            "Orbit-film bot {subject}'s totem along a {} deg arc at {} blocks.",
            r2(span_deg),
            r2(*orbit_radius)
        ),
        json!({"round": round, "subject": subject, "span_deg": r2(span_deg),
            "radius": r2(*orbit_radius), "direction": *orbit_dir, "arrived": arrived}),
    );
    // Occasional cutaway: witness a fresh edit elsewhere, or check the filmer working this bot's
    // own totem, then re-acquire the subject.
    if ctx.rng_next() < 0.3 {
        if !witness_peer_edits(ctx).await {
            glance_at_nearest_peer(ctx, Some(subject)).await;
        }
        ctx.look(focus, 400).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two filmers orbiting adjacent totems at full radius, around totems that both wandered
    /// toward each other, are the tightest pair this scene can produce — the ring has to keep
    /// them over the peer gap at every swarm size.
    #[test]
    fn adjacent_orbits_clear_the_peer_gap_at_every_swarm_size() {
        for n in 2..=32 {
            let r = film_ring(n);
            let chord = 2.0 * r * (std::f64::consts::PI / n.max(2) as f64).sin();
            let worst = chord - 2.0 * (ORBIT_MAX + SITE_JITTER);
            assert!(worst >= MIN_PEER_GAP - 1e-9, "n={n} worst={worst}");
        }
    }

    /// The build window must be a strict minority of the cycle (this is a camera scene) while
    /// still fitting a 3-layer build plus travel, and a 120 s episode must see multiple cycles.
    #[test]
    fn the_cycle_is_film_dominant_and_repeats_within_an_episode() {
        const {
            assert!(BUILD_WINDOW_MS * 2 < CYCLE_MS);
            assert!(BUILD_WINDOW_MS >= 12_000);
            assert!(120_000 / CYCLE_MS >= 2);
        }
    }
}
