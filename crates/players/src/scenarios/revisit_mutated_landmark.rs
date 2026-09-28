//! CATALOG: family=many_player_spatial min_players=2 single_process=true world_edits=true scenery=none far_players=false
//!
//! Two bots each keep a column, and each one's column is edited BY THE OTHER while its owner is
//! away. On return the owner reads its column back and records what it expected next to what it
//! observed, so "the model misremembered" and "the world really changed" stay separable.
//!
//! This is the change-detection face of the revisit problem. A return whose subject is guaranteed unchanged
//! teaches the model that memory is enough; a return whose subject changed WITHOUT the change being
//! recorded teaches it that memory is unreliable. Only a return where the change is recorded in the
//! event stream carries both signals, and that is what the `expected_height` / `observed_height`
//! pair here is for: a downstream scorer can ask whether a discrepancy is attributable to a logged
//! peer edit before calling it a hallucination.
//!
//! Roles alternate every cycle, so each POV produces both the excursion (travel frames, a real
//! return) and the editing beat, and the two bots never run the same sequence — the near-clones
//! check is pairwise over action streams.
//!
//! Distinct from `rendezvous_after_edits`, which needs `far_players` and scores a MEETING rather
//! than a re-observation, and from `depart_and_revisit`, whose marker nobody ever touches, so its
//! verification can only ever come back "persisted".
use super::*;

/// One column per bot, keyed by parity, so a keeper can tell its own column from its peer's by
/// material alone — the two processes share no coordinates, only the catalog's material vocabulary.
const MATERIALS: [&str; 2] = ["minecraft:stone_bricks", "minecraft:bricks"];

/// Starting column height, and the ceiling a keeper may grow it to. 4 is a reach limit, not a taste
/// one: placing onto the top of a 4-tall column from a 3-block standoff is a 3.8-block reach against
/// the 4.5-block maximum, and a fifth layer would silently fail.
const START_HEIGHT: i64 = 2;
const MAX_HEIGHT: i32 = 4;

/// Column sites sit on a ring around the anchor at this radius, one bearing step apart. 97 deg is
/// chosen so that in the first two-player tier the two sites are 9.0 blocks apart (chord `2 r sin(48.5 deg)`), clearing
/// `MIN_PEER_GAP` with margin, while both stay well inside each other's opening frame.
const SITE_RADIUS: f64 = 6.0;
const SITE_FAN_DEG: f64 = 97.0;

/// How far the traveler goes, and for how long. 13 blocks is far enough that the column is a small
/// patch rather than the subject, and the round trip plus the dwell is ~25 s, so a keeper cycle has
/// room to notice the absence and act inside it.
const EXCURSION: f64 = 13.0;
const AWAY_MS: i64 = 12_000;

/// A keeper only edits while the owner is genuinely away: the column's owner must be at least this
/// far from its own site. Below it the keeper would be walking into an occupied site, which both
/// breaks the peer gap and makes the edit a witnessed one rather than a remembered-vs-real test.
const OWNER_AWAY_MIN: f64 = 9.0;

/// Standoff the keeper edits from. Inside placing reach of the column top, outside the owner's body.
const EDIT_STANDOFF: f64 = 3.0;

/// Facing tolerance for the edit itself. Looser than the 5 deg a landmark check uses, because the
/// target is a single block within arm's reach where a small standoff drift is a large angular
/// error, but tight enough that the footage shows the bot facing what it edits — the server judges
/// `use_item_on` on the packet's hit position, so nothing else would catch a sideways placement.
const EDIT_FACE_TOL_DEG: f64 = 8.0;

const SHUTTLE_LEG: f64 = 4.0;
const HOME_HOLD_MS: i64 = 4_000;
const FACE_MAX_WAIT_MS: u64 = 5_000;
const ARRIVE_RADIUS: f64 = 2.0;

/// Site of the bot with global index `g`. Both roles compute it from the same formula, which is what
/// lets a keeper find a peer's column without any shared state — the only thing it needs is the
/// peer's global index, and in a single-process swarm that is `index_offset + local index`.
fn site_of(anchor: (f64, f64), g: usize) -> (f64, f64) {
    let bearing = (g as f64 * SITE_FAN_DEG).to_radians();
    (
        anchor.0 - bearing.sin() * SITE_RADIUS,
        anchor.1 + bearing.cos() * SITE_RADIUS,
    )
}

/// Consecutive cells of `material` upward from `base`. The column's height IS the memory under test,
/// so it has to be read the same way by the owner (before and after) and by the keeper (before
/// editing) — a single reader keeps the two sides from drifting apart.
fn stack_height(ctx: &Ctx, base: BlockPos, material: &str) -> i32 {
    let wanted = material.strip_prefix("minecraft:").unwrap_or(material);
    (0..=MAX_HEIGHT + 1)
        .take_while(|&dy| ctx.block_id_at(base.up(dy)).as_deref() == Some(wanted))
        .count() as i32
}

/// Nearest peer, by local index — the only peer identity available inside one process.
fn nearest_peer(ctx: &Ctx) -> Option<usize> {
    let own = ctx.position()?;
    (0..ctx.player_count())
        .filter(|&j| j != ctx.index())
        .filter_map(|j| {
            ctx.peer_position(j)
                .map(|p| (j, (p.0 - own.0).powi(2) + (p.2 - own.2).powi(2)))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(j, _)| j)
}

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let anchor = ctx.anchor();
    let start = ctx.position().unwrap_or((anchor.0, 64.0, anchor.1));
    let material = MATERIALS[global % MATERIALS.len()];
    let planned_site = site_of(anchor, global);
    let site = marker_base_near(
        ctx,
        (
            planned_site.0.round() as i64,
            start.1.round() as i64,
            planned_site.1.round() as i64,
        ),
        1,
    )
    .map(|b| (b.x as f64 + 0.5, b.z as f64 + 0.5))
    .unwrap_or(planned_site);

    travel_to(ctx, site, TRAVEL_HOP, ARRIVE_RADIUS).await;
    let desired = (
        site.0.round() as i64,
        start.1.round() as i64,
        site.1.round() as i64,
    );
    let raised = raise_landmark(ctx, desired, START_HEIGHT, material).await;
    let own_base = raised.map(|(base, _)| base);
    let own_focus =
        raised
            .map(|(_, point)| point)
            .unwrap_or((site.0, start.1 + START_HEIGHT as f64, site.1));
    ctx.record(
        "observe",
        &format!("vis-revisit-mutated-setup-{index}"),
        "Raise this bot's column: the subject a peer will edit while its owner is away.",
        json!({
            "global_index": global,
            "role": "mutation_pair",
            "material": material,
            "site": {"x": r2(site.0), "z": r2(site.1)},
            "column_placed": raised.is_some(),
            "start_height": START_HEIGHT,
        }),
    );

    let mut cycle = 0usize;
    while !ctx.target_met() {
        // Alternate: at N=2 one bot travels while the other keeps, and they swap every cycle. There
        // is no cross-process clock, so the overlap is approximate — which is exactly why both the
        // edit and the observation are timestamped into the event stream instead of being assumed
        // to line up.
        let traveling = (global + cycle).is_multiple_of(2);
        if traveling {
            let remembered = own_base.map(|b| stack_height(ctx, b, material));
            let bearing = ((global + cycle) as f64 * 61.0).to_radians();
            let far = (
                site.0 - bearing.sin() * EXCURSION,
                site.1 + bearing.cos() * EXCURSION,
            );
            let out = travel_to(ctx, far, TRAVEL_HOP, ARRIVE_RADIUS).await;
            ctx.record(
                TYPE_MOVE,
                &format!("vis-revisit-mutated-depart-{index}-{cycle}"),
                "Leave the column behind, facing away, so the return is a real re-acquisition.",
                json!({
                    "cycle": cycle,
                    "remembered_height": remembered,
                    "away_yaw_deg": r2(bearing.to_degrees()),
                    "away_ms": AWAY_MS,
                    "arrival_distance": r2(out),
                    "exceeds_horizontal_fov": true,
                }),
            );
            if ctx.target_met() {
                break;
            }
            hold_facing(
                ctx,
                bearing.to_degrees(),
                ctx.elapsed_ms() + AWAY_MS,
                SHUTTLE_LEG,
            )
            .await;

            travel_to(ctx, site, TRAVEL_HOP, ARRIVE_RADIUS).await;
            let faced = ctx
                .look_until_facing(own_focus, 5.0, FACE_MAX_WAIT_MS)
                .await;
            let observed = own_base.map(|b| stack_height(ctx, b, material));
            // Attribute the discrepancy before recording it: a witnessed peer edit turns "the world
            // changed" into a fact the footage can be scored against, instead of a mismatch a
            // downstream metric would have to guess about.
            let witnessed = witness_peer_edits(ctx).await;
            ctx.record(
                "observe",
                &format!("vis-revisit-mutated-return-{index}-{cycle}"),
                "Return to the column and compare its remembered height against its real height.",
                json!({
                    "cycle": cycle,
                    "expected_height": remembered,
                    "observed_height": observed,
                    "changed": remembered != observed,
                    "peer_edit_witnessed": witnessed,
                    "faced_column": faced,
                    "material": material,
                    "elapsed_ms": ctx.elapsed_ms(),
                }),
            );
            let home_yaw = ctx
                .position()
                .map(|p| look_angles(head_of(p), own_focus).0)
                .unwrap_or(0.0);
            hold_facing(ctx, home_yaw, ctx.elapsed_ms() + HOME_HOLD_MS, SHUTTLE_LEG).await;
        } else {
            let peer = nearest_peer(ctx);
            let peer_global = peer.map(|j| ctx.cfg().index_offset + j);
            let peer_site = peer_global.map(|g| site_of(anchor, g));
            // Only edit an unattended column. `peer_position` is live, so this is a real absence
            // check rather than a hope that the roles are in phase.
            let owner_gap = peer
                .zip(peer_site)
                .and_then(|(j, s)| ctx.peer_position(j).map(|p| (p.0 - s.0).hypot(p.2 - s.1)));
            let unattended = owner_gap.is_some_and(|gap| gap >= OWNER_AWAY_MIN);
            let mut edit = json!(null);
            if let (Some(g), Some(s), true) = (peer_global, peer_site, unattended) {
                let peer_material = MATERIALS[g % MATERIALS.len()];
                // Stand off on the side of the peer's site that faces this bot's own site, so the
                // approach never crosses the column and the walk home is a straight retrace.
                let (dx, dz) = (site.0 - s.0, site.1 - s.1);
                let span = dx.hypot(dz).max(0.01);
                let approach = (
                    s.0 + dx / span * EDIT_STANDOFF,
                    s.1 + dz / span * EDIT_STANDOFF,
                );
                travel_to(ctx, approach, TRAVEL_HOP, ARRIVE_RADIUS).await;
                let found = area_find_block(
                    ctx,
                    (s.0.round() as i32, s.1.round() as i32),
                    (
                        start.1.round() as i32 - 3,
                        start.1.round() as i32 + MAX_HEIGHT + 2,
                    ),
                    peer_material,
                );
                if let Some(top) = found {
                    // The found cell can be any layer of the column; walk down to its base so the
                    // height reading and the edit both address the same column.
                    let mut base = top;
                    let wanted = peer_material
                        .strip_prefix("minecraft:")
                        .unwrap_or(peer_material);
                    while ctx.block_id_at(base.down(1)).as_deref() == Some(wanted) {
                        base = base.down(1);
                    }
                    let height = stack_height(ctx, base, peer_material);
                    // Grow while there is headroom, otherwise take the top layer off. Both are
                    // one-block edits, so the owner's height reading changes by exactly one and the
                    // discrepancy is unambiguous.
                    let grow = height < MAX_HEIGHT;
                    let cell = if grow {
                        base.up(height)
                    } else {
                        base.up(height - 1)
                    };
                    ctx.look_until_facing(
                        (
                            cell.x as f64 + 0.5,
                            cell.y as f64 + 0.5,
                            cell.z as f64 + 0.5,
                        ),
                        EDIT_FACE_TOL_DEG,
                        FACE_MAX_WAIT_MS,
                    )
                    .await;
                    let applied = if grow {
                        ctx.equip(peer_material).await && ctx.place_block(cell, peer_material).await
                    } else {
                        ctx.dig(cell).await
                    };
                    edit = json!({
                        "peer_global_index": g,
                        "peer_material": peer_material,
                        "height_before": height,
                        "operation": if grow { "grow" } else { "shorten" },
                        "cell": {"x": cell.x, "y": cell.y, "z": cell.z},
                        "applied": applied,
                    });
                }
            }
            // Why this cycle produced no edit, as one field. The roles alternate on per-bot cycle
            // counters that share no clock, so a smoke run can legitimately show few `applied: true`
            // records; without this a rare edit is indistinguishable from broken logic.
            let blocked_by = if peer.is_none() {
                "no_peer"
            } else if owner_gap.is_none() {
                "no_peer_position"
            } else if !unattended {
                "owner_present"
            } else if edit.is_null() {
                "column_not_found"
            } else {
                "none"
            };
            ctx.record(
                if edit.is_null() {
                    "observe"
                } else {
                    TYPE_PLACE
                },
                &format!("vis-revisit-mutated-edit-{index}-{cycle}"),
                "Edit the peer's column by one layer, but only while its owner is away.",
                json!({
                    "cycle": cycle,
                    "role": "keeper",
                    "peer": peer,
                    "owner_gap": owner_gap.map(r2),
                    "owner_unattended": unattended,
                    "edit_blocked_by": blocked_by,
                    "edit": edit,
                    "elapsed_ms": ctx.elapsed_ms(),
                }),
            );
            // Walk home and re-verify the keeper's OWN column too: the keeper made an excursion of
            // its own, so its return is another scored re-observation rather than dead time.
            travel_to(ctx, site, TRAVEL_HOP, ARRIVE_RADIUS).await;
            let faced = ctx
                .look_until_facing(own_focus, 5.0, FACE_MAX_WAIT_MS)
                .await;
            let own_height = own_base.map(|b| stack_height(ctx, b, material));
            ctx.record(
                "observe",
                &format!("vis-revisit-mutated-keeper-home-{index}-{cycle}"),
                "Keeper returns to its own column and reads its height back.",
                json!({
                    "cycle": cycle,
                    "observed_height": own_height,
                    "faced_column": faced,
                }),
            );
            let home_yaw = ctx
                .position()
                .map(|p| look_angles(head_of(p), own_focus).0)
                .unwrap_or(0.0);
            hold_facing(ctx, home_yaw, ctx.elapsed_ms() + HOME_HOLD_MS, SHUTTLE_LEG).await;
        }
        cycle += 1;
    }
}
