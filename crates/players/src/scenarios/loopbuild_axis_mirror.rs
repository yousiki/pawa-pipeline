//! CATALOG: family=construction min_players=1 single_process=true world_edits=true scenery=none far_players=false
//!
//! Build symmetrically about a centre post, standing ON the axis so both wings are within reach:
//! place the near wing, swing the camera a full half-circle onto the far wing, place that, then
//! swing back onto the post. Three view stations, revisited every cycle, with almost no walking.
//!
//! Which face of the revisit problem this answers: the SWEEP RATIO. v5's net-turn/sweep is 0.80 — nearly every
//! window is one continuous rotation, so the camera never re-enters a region it has left and a
//! window's ground truth is unobservable from its context frame. This script is the opposite
//! extreme by construction: the camera has exactly three destinations and it cycles through them,
//! so over any few seconds the net turn is small while the total sweep is large. Each of the three
//! is confirmed with `look_until_facing` — a plain `look` returns on its deadline, and a
//! half-circle turn that timed out would otherwise be logged as an arrival that never happened.
//!
//! Why standing on the axis matters: an alternating build normally means walking back and forth
//! across the structure, and a 4.65 s window that contains one crossing reads as a large
//! displacement. With the wings `WING_OFFSET` blocks either side of the stand, both are inside
//! `place_reach`, so the alternation is pure camera work and the audit's displacement stays tiny
//! while `hold_facing`'s shuttle keeps the frozen gate satisfied.
//!
//! Occlusion: the near wing grows to two blocks and, from a stand only three blocks away, hides
//! part of the ground behind it; the post occludes the far half of the site once a bot has walked
//! a few cycles down the axis. Real terrain supplies the rest.
use super::*;

/// The centre post material. Distinct from the wings so a probe can tell the shared anchor from the
/// bot's own work.
const POST_MATERIAL: &str = "minecraft:stone";

/// Wing material.
const WING_MATERIAL: &str = "minecraft:oak_planks";

/// Post height. Three blocks is placeable from the ground with room to spare under the 4.0-block
/// `place_reach` clamp, and tall enough to stay visible over a two-block wing.
const POST_HEIGHT: i64 = 3;

/// Wing column height, in blocks.
const WING_HEIGHT: i64 = 2;

/// Perpendicular distance from the axis to each wing, in blocks. Three keeps every wing cell inside
/// `place_reach` from a stand on the axis (eye at 1.62, so the far cell is 3.2 blocks away) — that
/// reach is the entire reason this scenario can alternate sides without walking.
const WING_OFFSET: f64 = 3.0;

/// Where along the axis a bot starts, in blocks from the anchor. Five, not three: two bots on
/// opposite sides then open ten blocks apart, which leaves room for `hold_facing`'s four-block
/// shuttle at both ends before either could crowd the other's camera.
const AXIS_START: f64 = 5.0;

/// Dwell at each of the three view stations, in ms, per bot. All clear `hold_facing`'s 2.2 s leg
/// budget — anything shorter fits no leg and degenerates into a stand, which the frozen gate fails
/// outright. Unequal across bots so two POVs never share a phase (the near-clones check).
const STATION_HOLD_MS: [i64; 3] = [4_500, 5_500, 6_000];

/// One shuttle leg, in blocks. 4 clears the key-chatter floor at sprint speed; see `hold_facing`.
///
/// The bearing handed to `hold_facing` alongside it is only the OPENING aim. `move_to` overwrites
/// `camera_intent` with `CameraIntent::Hold`, which points the camera along the path whenever the
/// body is moving, so from the first leg onward a hold does NOT keep a subject framed. Every sight
/// line in this script is carried by a STATIONARY `look_until_facing` — walk, stop, look, walk. Do
/// not "fix" a hold by re-issuing `LookAt` inside it: that fights `Hold` for the camera and was
/// measured pushing three already-passing scripts to 96-100% on the camera/travel gate.
const SHUTTLE_LEG: f64 = 4.0;

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let slot = global % STATION_HOLD_MS.len();
    // Parity picks which way down the axis this bot works, so two bots share the post but never a
    // work site, and their camera sequences point opposite ways from the first frame.
    let direction = if global.is_multiple_of(2) { 1.0 } else { -1.0 };
    let anchor = ctx.anchor();
    let start = ctx.position().unwrap_or((anchor.0, 64.0, anchor.1));
    let ground_y = start.1.round() as i64;
    let anchor_z = anchor.1.round() as i64;

    // One shared post at the anchor. `marker_base_near` walks outward from the same desired cell, so
    // a second contributor lands on a free neighbour: a cluster, not a conflict, and every POV opens
    // on the same subject.
    let raised = raise_landmark(
        ctx,
        (anchor.0.round() as i64, ground_y, anchor_z),
        POST_HEIGHT,
        POST_MATERIAL,
    )
    .await;
    let post_focus = raised
        .map(|(_, point)| point)
        .unwrap_or((anchor.0, start.1 + 1.5, anchor.1));
    ctx.record(
        "observe",
        &format!("vis-loopbuild-axis-setup-{index}"),
        "Raise the shared centre post that both wings are mirrored about.",
        json!({
            "global_index": global,
            "direction": direction,
            "post_placed": raised.is_some(),
            "post_focus": {"x": r2(post_focus.0), "y": r2(post_focus.1), "z": r2(post_focus.2)},
            "wing_offset": WING_OFFSET,
        }),
    );

    let mut wing_y = ground_y;
    let mut cycle = 0usize;
    while !ctx.target_met() {
        // The stand walks one block further down the axis each cycle, so the site is never the same
        // twice and the wings grow into a corridor rather than a single pair of columns. It is also
        // the only translation in this script, which is what keeps displacement negligible.
        let stand_x = anchor.0 + direction * (AXIS_START + cycle as f64);
        // Resolve the moving work station on real support before navigating; the anchor line can cross
        // a shallow hole or a slope even when the build cell itself is placeable.
        let stand = marker_base_near(ctx, (stand_x.round() as i64, ground_y, anchor_z), 1)
            .map(|b| (b.x as f64 + 0.5, b.z as f64 + 0.5))
            .unwrap_or((stand_x, anchor.1));
        travel_to(ctx, stand, TRAVEL_HOP, 1.2).await;

        // Sides are visited near-first, far-second, and the two are a half-circle apart: the second
        // look is a turn well past the 102.5 deg horizontal FOV, and next cycle's first look is the
        // return to the side just left behind.
        for (leg, sign) in [(0usize, 1.0f64), (1usize, -1.0f64)] {
            if ctx.target_met() {
                break;
            }
            let wing_z = anchor.1 + sign * WING_OFFSET;
            let desired = (stand_x.round() as i64, wing_y, wing_z.round() as i64);
            let base = marker_base_near(ctx, desired, WING_HEIGHT);
            if let Some(base) = base {
                wing_y = base.y as i64;
            }
            let focus = base
                .map(|b| (b.x as f64 + 0.5, b.y as f64 + 1.0, b.z as f64 + 0.5))
                .unwrap_or((stand_x, wing_y as f64 + 1.0, wing_z));
            let faced = ctx.look_until_facing(focus, 8.0, 5_000).await;
            let (placed, attempted) = if let Some(base) = base {
                let cells: Vec<((i64, i64, i64), String)> = (0..WING_HEIGHT)
                    .map(|dy| {
                        (
                            (base.x as i64, base.y as i64 + dy, base.z as i64),
                            WING_MATERIAL.to_string(),
                        )
                    })
                    .collect();
                let total = cells.len();
                ctx.build_blocks(&cells, |b, _target, _material| {
                    (
                        format!("vis-loopbuild-axis-place-{index}-c{cycle}-s{leg}-{b}"),
                        format!(
                            "Place wing cell {}/{total} on this side of the axis.",
                            b + 1
                        ),
                        json!({"cycle": cycle, "leg": leg, "sign": sign, "cell_index": b}),
                    )
                })
                .await
            } else {
                (0, 0)
            };
            ctx.record(
                TYPE_PLACE,
                &format!("vis-loopbuild-axis-wing-{index}-c{cycle}-s{leg}"),
                "Wing placed; the camera crosses the axis to the other side next.",
                json!({
                    "cycle": cycle,
                    "leg": leg,
                    "sign": sign,
                    "is_return": cycle > 0,
                    "faced": faced,
                    "exceeds_horizontal_fov": leg > 0,
                    "placed": placed,
                    "attempted": attempted,
                    "wing_resolved": base.is_some(),
                    "stand_x": r2(stand_x),
                }),
            );
            let bearing = ctx
                .position()
                .map(|p| look_angles(head_of(p), focus).0)
                .unwrap_or(0.0);
            hold_facing(
                ctx,
                bearing,
                ctx.elapsed_ms() + STATION_HOLD_MS[slot],
                SHUTTLE_LEG,
            )
            .await;
        }
        if ctx.target_met() {
            break;
        }

        // ---- third station: back along the axis onto the post ----
        // The post is the one subject that does not move as the stand walks away from it, so this is
        // the leg that accumulates a long-baseline revisit: the same object, a block further off
        // every cycle, always re-acquired rather than assumed.
        let faced_post = ctx.look_until_facing(post_focus, 6.0, 6_000).await;
        let post_block = raised.map(|(base, _)| ctx.block_id_at(base));
        let post_distance = ctx
            .position()
            .map(|p| (post_focus.0 - p.0).hypot(post_focus.2 - p.2));
        ctx.record(
            "observe",
            &format!("vis-loopbuild-axis-post-{index}-c{cycle}"),
            "Turn back along the axis onto the shared post: the long-baseline revisit.",
            json!({
                "cycle": cycle,
                "is_return": cycle > 0,
                "faced_post": faced_post,
                "post_block": post_block.flatten(),
                "post_distance": post_distance.map(r2),
                "elapsed_ms": ctx.elapsed_ms(),
            }),
        );
        let post_bearing = ctx
            .position()
            .map(|p| look_angles(head_of(p), post_focus).0)
            .unwrap_or(0.0);
        hold_facing(
            ctx,
            post_bearing,
            ctx.elapsed_ms() + STATION_HOLD_MS[slot],
            SHUTTLE_LEG,
        )
        .await;
        witness_peer_edits(ctx).await;
        cycle += 1;
    }
}
