use super::*;

const TAU: f64 = std::f64::consts::TAU;
const SHAFT_NAMES: [&str; 2] = ["shaft_a", "shaft_b"];

/// Blocks kept between two bots' shaft heads. Two bots sinking one shaft is both an occlusion and a
/// pair of mutually blocked digs.
const MIN_SITE_GAP: f64 = 5.0;

/// The fill material is chosen to DIFFER from what the shaft was cut through, which is the whole
/// point of the step: the face put back reads as a different stone in the recording.
const FILL_MATERIALS: [&str; 4] = ["cobblestone", "stone", "stone_bricks", "bricks"];

/// The four horizontal faces of the cell a bot stands in, in world (dx, dz).
const FACES: [(i32, i32); 4] = [(1, 0), (-1, 0), (0, 1), (0, -1)];

fn rand_range(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

/// A fresh dig site on a ring around the anchor, at least `MIN_SITE_GAP` from every peer this
/// process can see. Best effort over a bounded number of draws — a crowded arena still gets a site.
fn fresh_site(ctx: &mut Ctx, anchor: (f64, f64)) -> (f64, f64) {
    let mut best = (anchor.0, anchor.1);
    let mut best_gap = -1.0;
    for _ in 0..6 {
        let angle = ctx.rng_next() * TAU;
        let radius = rand_range(ctx, 7.0, 17.0);
        let candidate = (
            anchor.0 + angle.cos() * radius,
            anchor.1 + angle.sin() * radius,
        );
        let gap = (0..ctx.player_count())
            .filter(|&j| j != ctx.index())
            .filter_map(|j| ctx.peer_position(j))
            .map(|p| (p.0 - candidate.0).hypot(p.2 - candidate.1))
            .fold(f64::INFINITY, f64::min);
        if gap >= MIN_SITE_GAP {
            return candidate;
        }
        if gap > best_gap {
            best_gap = gap;
            best = candidate;
        }
    }
    best
}

/// Wait for the bot to stop falling after a cell under its feet was broken. Bounded: a dig that did
/// not actually open the floor simply returns the pose it already had.
async fn settle(ctx: &Ctx) -> Option<(f64, f64, f64)> {
    let mut previous = ctx.position()?;
    let mut stable = 0;
    for _ in 0..40 {
        ctx.bot.wait_ticks(1).await;
        let Some(current) = ctx.position() else {
            continue;
        };
        if (current.1 - previous.1).abs() < 0.01 && ctx.on_ground() {
            stable += 1;
            if stable >= 3 {
                return Some(current);
            }
        } else {
            stable = 0;
        }
        previous = current;
    }
    ctx.position()
}

async fn sweep(ctx: &Ctx, span_deg: f64, pitch_deg: f64) {
    let rate = ctx.cfg().yaw_rate_deg_s.max(1.0);
    for (yaw, pitch) in [
        (span_deg * 0.5, pitch_deg),
        (-span_deg, -pitch_deg),
        (span_deg * 0.5, 0.0),
    ] {
        if ctx.target_met() {
            return;
        }
        let settle = (yaw.abs() / rate * 1000.0) as u64 + 700;
        ctx.drag_view(yaw, pitch, settle).await;
    }
}

fn feet_cell(position: (f64, f64, f64)) -> BlockPos {
    BlockPos::new(
        position.0.floor() as i32,
        position.1.floor() as i32,
        position.2.floor() as i32,
    )
}

/// Each bot sinks a short shaft, opens three faces at the bottom, then puts one of them back with a
/// different stone before climbing out and moving to fresh ground.
///
/// Shaft depth, which three of the four faces are opened, which one is filled, the fill material and
/// the site of the next shaft are all per-cycle draws. The climb out is the pillar jump
/// (`build_tower_underneath`) rather than a hope that the pathfinder finds a way up a hole the bot
/// just cut, so a cycle always ends standing on the surface and able to walk to the next site.
pub(crate) async fn run(ctx: &mut Ctx) {
    let index = ctx.index();
    let anchor = ctx.anchor();
    let role = SHAFT_NAMES[index % 2];
    let move_timeout = ctx.cfg().move_timeout_ms.min(16_000);

    ctx.record(
        TYPE_LOOK,
        &format!("vis-shaft-init-{index}"),
        "Shaft, three faces, one fill: sink a short shaft, open its bottom, put one face back.",
        json!({"role": role}),
    );

    let mut cycle = 0u32;
    while !ctx.target_met() {
        let site = fresh_site(ctx, anchor);
        let arrived = ctx.move_to(site.0, site.1, move_timeout, 2.0).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-shaft-site-{index}-{cycle}"),
            &format!("Walk to fresh ground for shaft {cycle}."),
            json!({"role": role, "cycle": cycle, "arrived": arrived,
                "site": {"x": r2(site.0), "z": r2(site.1)}}),
        );
        if ctx.target_met() {
            break;
        }

        let pick = ctx.equip("diamond_pickaxe").await;
        let depth = 2 + (ctx.rng_next() * 2.0) as i64; // 2 or 3
        let mut sunk = 0i64;
        for _ in 0..depth {
            if ctx.target_met() {
                break;
            }
            let Some(position) = ctx.position() else {
                break;
            };
            let floor = feet_cell(position).down(1);
            // Never open a floor that is not a solid full block: that is the signature of standing
            // over a cave, and the shaft would become a fall instead of a shaft.
            if !block_is_safe_support(ctx, floor) {
                break;
            }
            if !ctx.dig(floor).await {
                break;
            }
            settle(ctx).await;
            sunk += 1;
        }
        ctx.record(
            TYPE_MINE,
            &format!("vis-shaft-sink-{index}-{cycle}"),
            &format!("Sink the shaft {sunk} of {depth} blocks."),
            json!({"role": role, "cycle": cycle, "requested_depth": depth, "sunk": sunk,
                "pickaxe": pick}),
        );

        // Three of the four faces at the bottom, in a drawn order so no two cycles open the same
        // cross-section.
        let mut faces: Vec<(i32, i32)> = FACES.to_vec();
        for i in (1..faces.len()).rev() {
            let j = (ctx.rng_next() * (i + 1) as f64) as usize;
            faces.swap(i, j.min(i));
        }
        faces.truncate(3);
        let mut opened: Vec<(i32, i32, i32)> = Vec::new();
        for (dx, dz) in faces {
            if ctx.target_met() {
                break;
            }
            let Some(position) = ctx.position() else {
                break;
            };
            let feet = feet_cell(position);
            let cell = BlockPos::new(feet.x + dx, feet.y, feet.z + dz);
            if block_is_air(ctx, cell) {
                continue;
            }
            let centre = cell.center();
            let settle_ms = ctx.cfg().look_settle_ms.max(300);
            ctx.look((centre.x, centre.y, centre.z), settle_ms).await;
            if ctx.dig(cell).await {
                opened.push((cell.x, cell.y, cell.z));
            }
        }
        ctx.record(
            TYPE_MINE,
            &format!("vis-shaft-faces-{index}-{cycle}"),
            &format!("Open {} faces at the bottom of the shaft.", opened.len()),
            json!({"role": role, "cycle": cycle, "opened": opened.len(),
                "cells": opened.iter().map(|c| json!({"x": c.0, "y": c.1, "z": c.2}))
                    .collect::<Vec<_>>()}),
        );

        // Put exactly one of them back, with a stone that is not what came out.
        let fill = FILL_MATERIALS
            [(ctx.rng_next() * FILL_MATERIALS.len() as f64) as usize % FILL_MATERIALS.len()];
        let mut filled = false;
        if let Some(&cell) = opened.first() {
            let target = BlockPos::new(cell.0, cell.1, cell.2);
            if ctx.equip(fill).await {
                filled = ctx.place_block(target, fill).await;
            }
            ctx.record(
                TYPE_PLACE,
                &format!("vis-shaft-fill-{index}-{cycle}"),
                &format!("Put one opened face back as {fill}."),
                json!({"role": role, "cycle": cycle, "block": fill, "placed": filled,
                    "block_position": {"x": target.x, "y": target.y, "z": target.z}}),
            );
        }

        // Climb out on a pillar of the same stone, then read the site before leaving it.
        let mut climbed = 0i64;
        if sunk > 0 && ctx.equip(fill).await {
            climbed = ctx.build_tower_underneath(sunk, fill).await;
        }
        ctx.record(
            TYPE_PLACE,
            &format!("vis-shaft-climb-{index}-{cycle}"),
            &format!("Climb {climbed} of {sunk} blocks back out of the shaft."),
            json!({"role": role, "cycle": cycle, "climbed": climbed, "shaft_depth": sunk,
                "block": fill}),
        );

        let span = rand_range(ctx, 70.0, 175.0);
        let pitch = rand_range(ctx, -20.0, 10.0);
        sweep(ctx, span, pitch).await;
        witness_peer_edits(ctx).await;
        cycle += 1;
    }
}
