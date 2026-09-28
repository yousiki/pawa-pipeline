//! CATALOG: family=construction min_players=1 single_process=true world_edits=true scenery=none far_players=false
//!
//! Raise one beacon first, then build every later cairn at a station out on a ring — and end every
//! step by walking back to the beacon and confirming it is in frame again.
//!
//! Which face of the revisit problem this answers: the TRAJECTORY, with a fixed re-look anchor. v5's build
//! scenarios wander from site to site and each site's view is new, so 26.7% of windows have zero
//! first/last-frame overlap. Here the beacon is a subject every station shares: the bot leaves it,
//! builds, turns back to it (verified, past the horizontal FOV from most stations), then physically
//! returns to a fixed re-entry point beside it before the next station. Two things follow that the
//! audit measures directly — `path_length` is several times `displacement` because the ring is
//! traversed out-and-back rather than orbited, and the same handful of viewpoints recur, so late
//! windows are grounded in early ones.
//!
//! Stations deliberately cycle over a SHORT list of bearings instead of advancing by a golden angle:
//! a fresh bearing every cycle is an orbit again, three repeating bearings mean the bot returns to
//! viewpoints it has already recorded. Occlusion is the beacon itself plus real terrain — from a
//! ring station the cairn under construction and the beacon are often on opposite sides of a rise,
//! and it is the walk back that restores the sight line.
use super::*;

/// The beacon material. `oak_log` reads as a distinct vertical mass against stone terrain, which is
/// what a re-look anchor has to do at 10 blocks.
const BEACON_MATERIAL: &str = "minecraft:oak_log";

/// The cairn material, deliberately different from the beacon so a block probe can tell a station's
/// own work from the shared anchor.
const CAIRN_MATERIAL: &str = "minecraft:cobblestone";

/// Beacon height. Four is the tallest column that is reliably placeable from the ground: the eye
/// sits at 1.62 and `place_reach` is clamped to at most 4.5, so a fifth cell starts failing
/// silently. Going taller would mean standing on top of it, which drags in `build_tower`'s whole
/// walk-off-and-land machinery for no gain here.
const BEACON_HEIGHT: i64 = 4;

/// Cairn footprint, relative to its resolved base cell: a two-high stack with one block leaning off
/// it. Three cells is a short burst, which keeps `build_blocks`' own short nav taps a minority of
/// the cycle's movement presses (the key-chatter gate caps short presses at 35%).
const CAIRN_CELLS: [(i64, i64, i64); 3] = [(0, 0, 0), (0, 1, 0), (1, 0, 0)];

/// Ring radius for the work stations, in blocks. 10 puts the beacon well outside the cairn's own
/// frame, so the turn back to it is a real re-acquisition, while a 4.65 s window that contains the
/// whole walk still lands near the audit's 5-block displacement median.
const STATION_RADIUS: f64 = 10.0;

/// Bearing offsets of the stations, in degrees off this bot's own base bearing. Three widely
/// separated stations, revisited in rotation: the point is that cycle 3 comes back to cycle 0's
/// viewpoint, which is the overlap the audit rewards. Spacing them ~120 deg apart also means
/// consecutive stations are never a small step around a circle, so no cycle reads as an orbit.
const STATION_BEARING_DEG: [f64; 3] = [0.0, 118.0, 236.0];

/// Where a bot stands when it comes back to the beacon: `BEACON_STANDOFF` blocks out along its own
/// base bearing plus 180 deg. Per-bot bearings keep two returning bots apart (at the golden-angle
/// spread below, two bots' re-entry points are 9.3 blocks apart), and a FIXED re-entry point per
/// bot is what makes the beacon view repeat almost pixel-for-pixel across cycles.
const BEACON_STANDOFF: f64 = 5.0;

/// Per-bot rotation of the whole station layout, in degrees. The golden angle spreads any number of
/// bots over the ring without two of them ever sharing a station.
const BOT_BEARING_SPREAD_DEG: f64 = 137.5;

/// Dwell at the cairn and at the beacon, in ms, drawn by bot so two bots are never in phase (the
/// near-clones check reads exactly that). Both clear `hold_facing`'s 2.2 s leg budget, so a dwell
/// really shuttles rather than standing — a stand fails the frozen gate outright.
const CAIRN_HOLD_MS: [i64; 3] = [4_500, 5_500, 6_500];
const BEACON_HOLD_MS: [i64; 3] = [6_500, 5_500, 4_500];

/// One shuttle leg, in blocks. 4 clears the key-chatter floor at sprint speed; see `hold_facing`.
///
/// The bearing handed to `hold_facing` alongside it is only the OPENING aim. `move_to` overwrites
/// `camera_intent` with `CameraIntent::Hold`, which points the camera along the path whenever the
/// body is moving, so from the first leg onward a hold does NOT keep a subject framed. Every sight
/// line in this script is carried by a STATIONARY `look_until_facing` — walk, stop, look, walk. Do
/// not "fix" a hold by re-issuing `LookAt` inside it: that fights `Hold` for the camera and was
/// measured pushing three already-passing scripts to 96-100% on the camera/travel gate.
const SHUTTLE_LEG: f64 = 4.0;

/// World point at `radius` blocks from `origin` on compass `bearing_deg`, measured so 0 is +Z (the
/// same convention as `point_at_yaw`, kept consistent so the recorded bearings mean one thing).
fn ring_point(origin: (f64, f64), bearing_deg: f64, radius: f64) -> (f64, f64) {
    let a = bearing_deg.to_radians();
    (origin.0 - a.sin() * radius, origin.1 + a.cos() * radius)
}

pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let slot = global % CAIRN_HOLD_MS.len();
    let base_bearing = BOT_BEARING_SPREAD_DEG * global as f64;
    let anchor = ctx.anchor();
    let start = ctx.position().unwrap_or((anchor.0, 64.0, anchor.1));
    let ground_y = start.1.round() as i64;

    // Every bot contributes to ONE beacon at the anchor: `marker_base_near` walks outward from the
    // same desired cell, so a second bot lands on a free neighbour and the result is a cluster
    // rather than a conflict. No cross-process coordination needed, and every POV opens on the same
    // subject — compact staging.
    let raised = raise_landmark(
        ctx,
        (anchor.0.round() as i64, ground_y, anchor.1.round() as i64),
        BEACON_HEIGHT,
        BEACON_MATERIAL,
    )
    .await;
    let beacon_focus =
        raised
            .map(|(_, point)| point)
            .unwrap_or((anchor.0, start.1 + 2.0, anchor.1));
    let planned_re_entry = ring_point(anchor, base_bearing + 180.0, BEACON_STANDOFF);
    let re_entry = marker_base_near(
        ctx,
        (
            planned_re_entry.0.round() as i64,
            ground_y,
            planned_re_entry.1.round() as i64,
        ),
        1,
    )
    .map(|b| (b.x as f64 + 0.5, b.z as f64 + 0.5))
    .unwrap_or(planned_re_entry);
    ctx.record(
        "observe",
        &format!("vis-loopbuild-beacon-setup-{index}"),
        "Raise the beacon every later station will be checked against.",
        json!({
            "global_index": global,
            "beacon_placed": raised.is_some(),
            "beacon_focus": {"x": r2(beacon_focus.0), "y": r2(beacon_focus.1), "z": r2(beacon_focus.2)},
            "base_bearing_deg": r2(base_bearing % 360.0),
            "re_entry": {"x": r2(re_entry.0), "z": r2(re_entry.1)},
        }),
    );

    let mut cycle = 0usize;
    while !ctx.target_met() {
        let bearing = base_bearing + STATION_BEARING_DEG[cycle % STATION_BEARING_DEG.len()];
        let planned_station = ring_point(anchor, bearing, STATION_RADIUS);
        let station = marker_base_near(
            ctx,
            (
                planned_station.0.round() as i64,
                ground_y,
                planned_station.1.round() as i64,
            ),
            1,
        )
        .map(|b| (b.x as f64 + 0.5, b.z as f64 + 0.5))
        .unwrap_or(planned_station);

        // ---- out to the station, build the cairn ----
        travel_to(ctx, station, TRAVEL_HOP, 1.5).await;
        let feet_y = ctx
            .position()
            .map(|p| p.1.round() as i64)
            .unwrap_or(ground_y);
        // Stand off the cairn rather than on it: a cell overlapping the placer's own hitbox reads as
        // air and looks supported, yet vanilla always refuses it. Two blocks further along the
        // bearing keeps all three cells inside `place_reach` from where the body already is.
        let cairn_centre = ring_point(anchor, bearing, STATION_RADIUS + 2.5);
        let desired = (
            cairn_centre.0.round() as i64,
            feet_y,
            cairn_centre.1.round() as i64,
        );
        let base = marker_base_near(ctx, desired, 2);
        let cairn_focus = base
            .map(|b| (b.x as f64 + 0.5, b.y as f64 + 1.0, b.z as f64 + 0.5))
            .unwrap_or((cairn_centre.0, feet_y as f64 + 1.0, cairn_centre.1));
        let faced_cairn = ctx.look_until_facing(cairn_focus, 8.0, 4_000).await;
        let (placed, attempted) = if let Some(base) = base {
            let cells: Vec<((i64, i64, i64), String)> = CAIRN_CELLS
                .iter()
                .map(|&(dx, dy, dz)| {
                    (
                        (base.x as i64 + dx, base.y as i64 + dy, base.z as i64 + dz),
                        CAIRN_MATERIAL.to_string(),
                    )
                })
                .collect();
            let total = cells.len();
            ctx.build_blocks(&cells, |b, _target, _material| {
                (
                    format!("vis-loopbuild-beacon-place-{index}-c{cycle}-{b}"),
                    format!("Place cairn cell {}/{total} at this ring station.", b + 1),
                    json!({"cycle": cycle, "cell_index": b, "bearing_deg": r2(bearing % 360.0)}),
                )
            })
            .await
        } else {
            (0, 0)
        };
        ctx.record(
            TYPE_PLACE,
            &format!("vis-loopbuild-beacon-cairn-{index}-c{cycle}"),
            "Cairn done at this station; the beacon is now out of frame behind the bot.",
            json!({
                "cycle": cycle,
                "station": {"x": r2(station.0), "z": r2(station.1)},
                "bearing_deg": r2(bearing % 360.0),
                "faced_cairn": faced_cairn,
                "placed": placed,
                "attempted": attempted,
                "cairn_resolved": base.is_some(),
            }),
        );
        let cairn_bearing = ctx
            .position()
            .map(|p| look_angles(head_of(p), cairn_focus).0)
            .unwrap_or(0.0);
        hold_facing(
            ctx,
            cairn_bearing,
            ctx.elapsed_ms() + CAIRN_HOLD_MS[slot],
            SHUTTLE_LEG,
        )
        .await;
        if ctx.target_met() {
            break;
        }

        // ---- turn back to the beacon from the station: the sight-line half of the loop ----
        let turned = ctx.look_until_facing(beacon_focus, 6.0, 6_000).await;
        let beacon_block = raised.map(|(base, _)| ctx.block_id_at(base));
        ctx.record(
            "observe",
            &format!("vis-loopbuild-beacon-sight-{index}-c{cycle}"),
            "Turn from the fresh cairn back onto the beacon, without moving yet.",
            json!({
                "cycle": cycle,
                "is_return": cycle > 0,
                "turned": turned,
                "exceeds_horizontal_fov": true,
                "beacon_block": beacon_block.flatten(),
            }),
        );

        // ---- walk back to the fixed re-entry point: the trajectory half of the loop ----
        travel_to(ctx, re_entry, TRAVEL_HOP, 1.5).await;
        let arrived = ctx.look_until_facing(beacon_focus, 6.0, 5_000).await;
        ctx.record(
            TYPE_MOVE,
            &format!("vis-loopbuild-beacon-return-{index}-c{cycle}"),
            "Back at this bot's fixed re-entry point beside the beacon.",
            json!({
                "cycle": cycle,
                "arrived_facing": arrived,
                "re_entry": {"x": r2(re_entry.0), "z": r2(re_entry.1)},
                "position": ctx.position().map(|p| json!({"x": r2(p.0), "y": r2(p.1), "z": r2(p.2)})),
                "elapsed_ms": ctx.elapsed_ms(),
            }),
        );
        let beacon_bearing = ctx
            .position()
            .map(|p| look_angles(head_of(p), beacon_focus).0)
            .unwrap_or(0.0);
        hold_facing(
            ctx,
            beacon_bearing,
            ctx.elapsed_ms() + BEACON_HOLD_MS[slot],
            SHUTTLE_LEG,
        )
        .await;
        witness_peer_edits(ctx).await;
        cycle += 1;
    }
}
