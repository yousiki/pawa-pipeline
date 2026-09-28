//! CATALOG: family=many_player_spatial min_players=2 single_process=true world_edits=false scenery=none far_players=false
//!
//! Two players take turns being each other's landmark: one walks away and navigates back BY MEMORY
//! of where the other was standing, while the other holds a single remembered bearing and waits.
//!
//! Every other revisit script in this family anchors on a block the script placed. A player is a
//! harder and more useful anchor for the revisit problem's purposes: it is the thing 22.4% of v5's multi-player
//! windows failed to keep in frame at all, and unlike a block it MOVES a little, so the return has a
//! non-trivial answer. The rover walks back to the coordinates it remembers, not to the peer's live
//! position, and only then turns to find the peer — so the recorded pair (`remembered_anchor`,
//! `observed_anchor`, `anchor_drift`) measures exactly how wrong a memory-driven return is.
//!
//! The sentry's half is the mirror image: it looks at the departing rover ONCE, converts that gaze
//! into a fixed bearing, and holds that bearing for the whole watch without tracking. Holding rather
//! than tracking is not a stylistic choice — a tracked hold turns the camera continuously to keep the
//! subject centred, the turn direction follows the body, and the camera/travel-independence gate
//! measured exactly that shape at 96-100% against a 95% ceiling. It is also the more informative
//! behaviour: because the bearing is frozen, whether the rover reappears inside the frame is itself
//! the memory result, and the closing gaze records the angular error.
//!
//! Roles swap every cycle, so both POVs contain travel, a departure and a verified return, and the
//! two action streams never coincide (the near-clones check is pairwise).
//!
//! This is the only script in the family that edits nothing: the anchor is a player, so there is
//! no world_edits term to confound the return with.
use super::*;

/// How far the rover goes and how long it stays away. 15 blocks puts the sentry down to a handful of
/// pixels without leaving render range, and 12 s is long enough that departure and return fall in
/// different training windows while a full swap still fits several times into an episode.
const EXCURSION: f64 = 15.0;
const AWAY_MS: i64 = 12_000;

/// The sentry's watch, sized to roughly cover a rover cycle (walk out ~8 s, dwell 12 s, walk back
/// ~8 s). It cannot be exact — the two bots share no clock — which is precisely why the interesting
/// quantities are recorded per side rather than assumed to line up.
const WATCH_MS: i64 = 26_000;

/// Tolerance for the gaze that finds a player. Wider than a block landmark's 5 deg because a player
/// is a moving 0.6-block-wide target and `look_until_facing` recomputes its bearing every poll.
const PEER_FACE_TOL_DEG: f64 = 8.0;

/// Half the horizontal field of view at `fov_deg=70` and 16:9 (102.5 deg), so a bearing error under
/// this means the rover came back inside the frame the sentry never stopped looking at.
const HALF_FOV_DEG: f64 = 51.25;

const SHUTTLE_LEG: f64 = 4.0;
const REUNION_HOLD_MS: i64 = 4_000;
const FACE_MAX_WAIT_MS: u64 = 5_000;
const ARRIVE_RADIUS: f64 = 3.0;

/// Pair adjacent local players first, falling back to the nearest available player for odd-N swarms.
/// This keeps each rover's sentry paired at N=4+ instead of letting both choose the same nearest peer.
fn nearest_peer(ctx: &Ctx) -> Option<usize> {
    let paired = if ctx.index().is_multiple_of(2) {
        ctx.index() + 1
    } else {
        ctx.index() - 1
    };
    if paired < ctx.player_count() {
        return Some(paired);
    }
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

    ctx.record(
        "observe",
        &format!("vis-revisit-sentry-setup-{index}"),
        "Alternate rover and sentry: one navigates back by memory, one holds a remembered bearing.",
        json!({
            "global_index": global,
            "role": "mutual_sentry",
            "excursion": EXCURSION,
            "away_ms": AWAY_MS,
            "watch_ms": WATCH_MS,
        }),
    );

    let mut cycle = 0usize;
    while !ctx.target_met() {
        let peer = nearest_peer(ctx);
        let peer_pos = peer.and_then(|j| ctx.peer_position(j));
        // At N=2 this makes one bot the rover and the other the sentry, and swaps them every cycle.
        let roving = (global + cycle).is_multiple_of(2);

        if roving {
            // Leave straight away from the sentry, so the corridor the sentry watches is the corridor
            // the rover returns down and "did it come back where I was looking" is a real question.
            let own = ctx.position();
            let (ux, uz) = away_unit(own, peer_pos.unwrap_or((anchor.0, 64.0, anchor.1)));
            let base = peer_pos.map(|p| (p.0, p.2)).unwrap_or((anchor.0, anchor.1));
            let far = (base.0 + ux * EXCURSION, base.1 + uz * EXCURSION);
            let away_yaw = look_angles((base.0, 0.0, base.1), (far.0, 0.0, far.1)).0;
            let out = travel_to(ctx, far, TRAVEL_HOP, ARRIVE_RADIUS).await;
            ctx.record(
                TYPE_MOVE,
                &format!("vis-revisit-sentry-depart-{index}-{cycle}"),
                "Rover leaves its human anchor, memorising where the anchor was standing.",
                json!({
                    "cycle": cycle,
                    "peer": peer,
                    "remembered_anchor": peer_pos.map(|p| json!({"x": r2(p.0), "z": r2(p.2)})),
                    "away_yaw_deg": r2(away_yaw),
                    "away_ms": AWAY_MS,
                    "arrival_distance": r2(out),
                }),
            );
            if ctx.target_met() {
                break;
            }
            hold_facing(ctx, away_yaw, ctx.elapsed_ms() + AWAY_MS, SHUTTLE_LEG).await;
            if ctx.target_met() {
                break;
            }

            // Walk back along the REMEMBERED bearing, not toward the peer's live position. Homing on
            // the live position would make the return trivially correct and would erase the quantity
            // this script exists to measure.
            //
            // Stop short of the remembered point rather than on it: the sentry is still standing
            // there, shuttling up to `SHUTTLE_LEG` blocks around it, so arriving on the point would
            // script an approach to nose-to-nose range — the "a face fills the frame" defect
            // `MIN_PEER_GAP` exists to prevent. `MIN_PEER_GAP + SHUTTLE_LEG` still clears the gap on
            // the pass where the sentry's shuttle leg swings toward the rover. Nothing about the
            // measurement changes: `anchor_drift` is computed from positions, not from standing on
            // the spot.
            let stop_short = MIN_PEER_GAP + SHUTTLE_LEG;
            let stop = (base.0 + ux * stop_short, base.1 + uz * stop_short);
            let back = travel_to(ctx, stop, TRAVEL_HOP, ARRIVE_RADIUS).await;
            let live = peer.and_then(|j| ctx.peer_position(j));
            let faced = match live {
                Some(p) => {
                    ctx.look_until_facing(head_of(p), PEER_FACE_TOL_DEG, FACE_MAX_WAIT_MS)
                        .await
                }
                None => false,
            };
            let drift = live
                .zip(peer_pos)
                .map(|(now, then)| (now.0 - then.0).hypot(now.2 - then.2));
            ctx.record(
                "observe",
                &format!("vis-revisit-sentry-return-{index}-{cycle}"),
                "Rover arrives at the remembered anchor point and looks for the anchor itself.",
                json!({
                    "cycle": cycle,
                    "peer": peer,
                    "remembered_anchor": peer_pos.map(|p| json!({"x": r2(p.0), "z": r2(p.2)})),
                    "observed_anchor": live.map(|p| json!({"x": r2(p.0), "z": r2(p.2)})),
                    "anchor_drift": drift.map(r2),
                    "anchor_found": faced,
                    "stop_short_blocks": r2(stop_short),
                    "return_distance": r2(back),
                    "elapsed_ms": ctx.elapsed_ms(),
                }),
            );
            // Hold the bearing the reunion ended on, fixed. One gaze found the peer; holding on it
            // keeps the peer in frame through the following window without tracking it.
            let reunion_yaw = ctx
                .position()
                .zip(live)
                .map(|(own, p)| look_angles(head_of(own), head_of(p)).0)
                .unwrap_or(0.0);
            hold_facing(
                ctx,
                reunion_yaw,
                ctx.elapsed_ms() + REUNION_HOLD_MS,
                SHUTTLE_LEG,
            )
            .await;
        } else {
            // Look at the rover ONCE, freeze that into a bearing, and hold it for the whole watch.
            let watched = match peer_pos {
                Some(p) => {
                    ctx.look_until_facing(head_of(p), PEER_FACE_TOL_DEG, FACE_MAX_WAIT_MS)
                        .await
                }
                None => false,
            };
            let watch_yaw = ctx
                .position()
                .zip(peer_pos)
                .map(|(own, p)| look_angles(head_of(own), head_of(p)).0)
                .unwrap_or(0.0);
            ctx.record(
                TYPE_LOOK,
                &format!("vis-revisit-sentry-watch-{index}-{cycle}"),
                "Sentry commits to one remembered bearing and holds it while the rover is away.",
                json!({
                    "cycle": cycle,
                    "peer": peer,
                    "peer_acquired": watched,
                    "watch_yaw_deg": r2(watch_yaw),
                    "watch_ms": WATCH_MS,
                    "tracking": false,
                }),
            );
            hold_facing(ctx, watch_yaw, ctx.elapsed_ms() + WATCH_MS, SHUTTLE_LEG).await;
            if ctx.target_met() {
                break;
            }
            // Close the watch by measuring how far off the frozen bearing the rover actually is. This
            // is the sentry's half of the same memory error the rover records from its own side.
            let live = peer.and_then(|j| ctx.peer_position(j));
            let actual_yaw = ctx
                .position()
                .zip(live)
                .map(|(own, p)| look_angles(head_of(own), head_of(p)).0);
            let error = actual_yaw.map(|a| ((a - watch_yaw + 540.0) % 360.0) - 180.0);
            let faced = match live {
                Some(p) => {
                    ctx.look_until_facing(head_of(p), PEER_FACE_TOL_DEG, FACE_MAX_WAIT_MS)
                        .await
                }
                None => false,
            };
            ctx.record(
                "observe",
                &format!("vis-revisit-sentry-close-{index}-{cycle}"),
                "Sentry checks whether the rover came back inside the bearing it kept holding.",
                json!({
                    "cycle": cycle,
                    "peer": peer,
                    "watch_yaw_deg": r2(watch_yaw),
                    "peer_yaw_deg": actual_yaw.map(r2),
                    "bearing_error_deg": error.map(r2),
                    "returned_inside_fov": error.map(|e| e.abs() <= HALF_FOV_DEG),
                    "peer_reacquired": faced,
                    "elapsed_ms": ctx.elapsed_ms(),
                }),
            );
            let reunion_yaw = actual_yaw.unwrap_or(watch_yaw);
            hold_facing(
                ctx,
                reunion_yaw,
                ctx.elapsed_ms() + REUNION_HOLD_MS,
                SHUTTLE_LEG,
            )
            .await;
        }
        cycle += 1;
    }
}
