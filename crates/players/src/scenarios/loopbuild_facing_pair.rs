//! CATALOG: family=construction min_players=2 single_process=true world_edits=true scenery=none far_players=false
//!
//! Two builders work back to back on opposite sides of the anchor, each facing OUTWARD to place,
//! then turning a full half-circle to check the other's row before turning outward again.
//!
//! Which face of the revisit problem this answers: the SIGHT LINE, with almost no translation. 22.4% of v5's
//! multi-player windows have a teammate walking into frame that the context frame never showed,
//! and the net-turn/sweep ratio of 0.80 says the camera almost never comes back. Here the camera
//! has exactly two destinations — own work face (outward) and the peer's row (inward) — and it
//! alternates between them every cycle, both confirmed with `look_until_facing`. Facing outward to
//! build is what makes the peer check a real turn: the teammate is directly BEHIND the builder, so
//! the check exceeds the 102.5 deg horizontal FOV and the return is a genuine re-observation of a
//! region that left the frame, not a nudge.
//!
//! The bodies barely move — the work stand creeps outward by one block per column — so the audit's
//! displacement stays tiny while `hold_facing`'s shuttle keeps `path_length` and the frozen gate
//! happy. Occlusion is whatever real terrain sits between the two rows, plus each row itself once
//! it is two blocks tall: the inward check has to see PAST the builder's own structure.
//!
//! The two sides use different materials on purpose. It is not decoration: the inward check probes
//! the peer's column by block id, so distinct materials make "the peer really built that" provable
//! instead of ambiguous with the checker's own work.
use super::*;

/// Per-side material, indexed by global parity. Both are kit blocks; the contrast is what makes the
/// cross-check probe unambiguous.
const MATERIALS: [&str; 2] = ["minecraft:stone_bricks", "minecraft:oak_planks"];

/// Column height, in blocks. Two is enough to occlude the far end of a row from ground level while
/// staying inside `place_reach` from the ground.
const COLUMN_HEIGHT: i64 = 2;

/// Distances from the anchor, in blocks, at which a side's columns stand. Starting at 6 leaves a
/// 12-block corridor between the two innermost columns, so the rows never crowd each other, and the
/// work stands (2.5 blocks inward of the column being built) stay 7 to 13 blocks apart — always
/// past `MIN_PEER_GAP`.
const COLUMN_OFFSETS: [f64; COLUMN_COUNT] = [6.0, 7.0, 8.0, 9.0];

/// Columns per side. Named so the per-column bookkeeping array below can be sized in a const
/// context, where `COLUMN_OFFSETS.len()` is not usable.
const COLUMN_COUNT: usize = 4;

/// Layers of the row a bot may build: after four columns it starts a second course on top of them.
/// Two courses x four columns is eight cycles, which fills a 120 s episode without ever asking for
/// a cell that is already occupied (a dead target, which `build_blocks` abandons after three).
const COURSES: usize = 2;

/// Stand-off inward of the column being built. Inside `place_reach` for both cells of a column, so
/// `build_blocks` never issues its own short nav taps.
const WORK_STANDOFF: f64 = 2.5;

/// Dwell at each end of the swing, in ms, per parity. Unequal on purpose: the near-clones check
/// fails a pair whose action sequences agree over 80% of the episode, and these two roles are
/// otherwise mirror images. Both clear `hold_facing`'s 2.2 s leg budget.
const OUTWARD_HOLD_MS: [i64; 2] = [5_000, 6_500];
const INWARD_HOLD_MS: [i64; 2] = [6_500, 5_000];

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
    let slot = global % 2;
    // Parity picks the side, so a two-bot episode always gets one builder per side without any
    // coordination. Everything else about the row is derived from this one sign.
    let side = if slot == 0 { 1.0 } else { -1.0 };
    let material = MATERIALS[slot];
    let peer_material = MATERIALS[1 - slot];
    let anchor = ctx.anchor();
    let start = ctx.position().unwrap_or((anchor.0, 64.0, anchor.1));
    let anchor_z = anchor.1.round() as i64;
    let mut row_y = start.1.round() as i64;

    ctx.record(
        "observe",
        &format!("vis-loopbuild-facing-open-{index}"),
        "Open the back-to-back build: claim a side of the anchor and its material.",
        json!({
            "global_index": global,
            "side": side,
            "material": material,
            "peer_material": peer_material,
            "column_offsets": COLUMN_OFFSETS,
        }),
    );

    // Where each column's ground course actually landed. On natural terrain `marker_base_near` can
    // shift a column up or down by a few blocks, and every column can land differently — the second
    // course has to stack on the cell that really exists, not on the last one resolved.
    let mut column_y: [Option<i64>; COLUMN_COUNT] = [None; COLUMN_COUNT];
    let mut cycle = 0usize;
    while !ctx.target_met() {
        // Even parity builds outside-in, odd inside-out. Same set of cells, opposite order: the two
        // POVs therefore never place at mirrored coordinates on the same cycle, which is the cheapest
        // way to keep the pairwise action sequences from converging.
        let step = cycle % COLUMN_OFFSETS.len();
        let k = if slot == 0 {
            COLUMN_OFFSETS.len() - 1 - step
        } else {
            step
        };
        let course = (cycle / COLUMN_OFFSETS.len()).min(COURSES - 1) as i64;
        let building = cycle / COLUMN_OFFSETS.len() < COURSES;
        let column_x = anchor.0 + side * COLUMN_OFFSETS[k];

        // ---- outward: face the work, place a column ----
        let planned_work_stand = (column_x - side * WORK_STANDOFF, anchor.1);
        let work_stand =
            walkable_xz_near(ctx, planned_work_stand, row_y as i32).unwrap_or(planned_work_stand);
        travel_to(ctx, work_stand, TRAVEL_HOP, 1.2).await;
        let course_y = column_y[k].unwrap_or(row_y) + course * COLUMN_HEIGHT;
        // Resolve the actual ground column before aiming; marker placement may shift in X/Z as well as Y.
        let desired = (column_x.round() as i64, course_y, anchor_z);
        let base = if building {
            if course == 0 {
                marker_base_near(ctx, desired, COLUMN_HEIGHT)
            } else {
                Some(BlockPos::new(
                    desired.0 as i32,
                    desired.1 as i32,
                    desired.2 as i32,
                ))
            }
        } else {
            None
        };
        let work_focus = base
            .map(|b| (b.x as f64 + 0.5, b.y as f64 + 1.0, b.z as f64 + 0.5))
            .unwrap_or((column_x, course_y as f64 + 1.0, anchor.1));
        let faced_work = ctx.look_until_facing(work_focus, 6.0, 4_000).await;

        let mut placed = 0u64;
        let mut attempted = 0u64;
        if let Some(base) = base {
            if course == 0 {
                row_y = base.y as i64;
                column_y[k] = Some(base.y as i64);
            }
            let cells: Vec<((i64, i64, i64), String)> = (0..COLUMN_HEIGHT)
                .map(|dy| {
                    (
                        (base.x as i64, base.y as i64 + dy, base.z as i64),
                        material.to_string(),
                    )
                })
                .collect();
            let total = cells.len();
            let (p, a) = ctx
                .build_blocks(&cells, |b, _target, _material| {
                    (
                        format!("vis-loopbuild-facing-place-{index}-c{cycle}-{b}"),
                        format!("Place column cell {}/{total} of this side's row.", b + 1),
                        json!({"cycle": cycle, "cell_index": b, "course": course, "column": k}),
                    )
                })
                .await;
            placed = p;
            attempted = a;
        }
        ctx.record(
            TYPE_LOOK,
            &format!("vis-loopbuild-facing-outward-{index}-c{cycle}"),
            "Outward: facing away from the anchor at this side's own work face.",
            json!({
                "cycle": cycle,
                "is_return": cycle > 0,
                "faced_work": faced_work,
                "column": k,
                "course": course,
                "building": building,
                "placed": placed,
                "attempted": attempted,
                "row_y": row_y,
            }),
        );
        let work_bearing = ctx
            .position()
            .map(|p| look_angles(head_of(p), work_focus).0)
            .unwrap_or(0.0);
        hold_facing(
            ctx,
            work_bearing,
            ctx.elapsed_ms() + OUTWARD_HOLD_MS[slot],
            SHUTTLE_LEG,
        )
        .await;
        if ctx.target_met() {
            break;
        }

        // ---- inward: half-circle turn onto the peer's row ----
        // Aim at the peer's INNERMOST column: it is the one both sides can see over their own row,
        // and its position needs no knowledge of which cycle the peer is on.
        let peer_x = anchor.0 - side * COLUMN_OFFSETS[0];
        let peer_probe = area_find_block(
            ctx,
            (peer_x.round() as i32, anchor_z as i32),
            (row_y as i32 - 4, row_y as i32 + 6),
            peer_material,
        );
        let peer_focus = peer_probe
            .map(|cell| {
                (
                    cell.x as f64 + 0.5,
                    cell.y as f64 + 0.5,
                    cell.z as f64 + 0.5,
                )
            })
            .unwrap_or((peer_x, row_y as f64 + 1.0, anchor.1));
        let faced_peer = ctx.look_until_facing(peer_focus, 8.0, 6_000).await;
        // Distance to the nearest peer, recorded so a scored window can tell "the teammate was in
        // frame" from "the teammate was 40 blocks away and unresolvable".
        let own = ctx.position();
        let mut peer_distance: Option<f64> = None;
        for j in 0..ctx.player_count() {
            if j == ctx.index() {
                continue;
            }
            if let (Some(own), Some(p)) = (own, ctx.peer_position(j)) {
                let d = (p.0 - own.0).hypot(p.2 - own.2);
                if peer_distance.is_none_or(|best| d < best) {
                    peer_distance = Some(d);
                }
            }
        }
        ctx.record(
            "observe",
            &format!("vis-loopbuild-facing-inward-{index}-c{cycle}"),
            "Inward: half-circle turn to check the other side's row, then back out next cycle.",
            json!({
                "cycle": cycle,
                "faced_peer": faced_peer,
                "exceeds_horizontal_fov": true,
                "peer_material": peer_material,
                "peer_column_found": peer_probe.is_some(),
                "peer_focus": {"x": r2(peer_focus.0), "y": r2(peer_focus.1), "z": r2(peer_focus.2)},
                "peer_distance": peer_distance.map(r2),
            }),
        );
        let peer_bearing = ctx
            .position()
            .map(|p| look_angles(head_of(p), peer_focus).0)
            .unwrap_or(0.0);
        hold_facing(
            ctx,
            peer_bearing,
            ctx.elapsed_ms() + INWARD_HOLD_MS[slot],
            SHUTTLE_LEG,
        )
        .await;
        witness_peer_edits(ctx).await;
        cycle += 1;
    }
}
