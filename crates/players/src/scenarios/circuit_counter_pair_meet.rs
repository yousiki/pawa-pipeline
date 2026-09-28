//! CATALOG: family=group_motion min_players=2 single_process=true world_edits=false scenery=none far_players=false

use super::*;

const TAU: f64 = std::f64::consts::TAU;

/// Stations on the shared loop. Six gives a 7-block side at the radius below — one or two
/// `TRAVEL_HOP` legs — and puts the two counter-running bots exactly three stations apart, so a
/// head-on pass lands near a station rather than always in the middle of a leg.
const STATIONS: i64 = 6;
/// Radius of the shared loop. Small on purpose: the whole point is that the same ground and the
/// same views come back every ~40 s, and a wide ring spends the episode on scenery nobody revisits.
/// It also keeps both bots inside each other's render distance for the entire lap.
const LOOP_RADIUS: f64 = 7.0;
/// Radial lane offset by index parity, so two bots closing head-on pass BESIDE each other instead
/// of trying to occupy the same station cell. 1.2 is the figure-eight's measured lane width; the
/// resulting 2.4-block separation at the pass is deliberate — it is under `MIN_PEER_GAP`, which is a
/// station-spacing rule, and a close pass is the event this scenario exists to film.
const LANE: f64 = 1.2;
/// Both bots reverse the whole loop this often. Long enough that a reversal is not the dominant
/// event (a lap is ~40 s), short enough that a 120 s episode contains several.
const REVERSE_PERIOD_MS: i64 = 24_000;
/// Below this 2D distance the peer counts as MET, and the station beat becomes a look at them.
const MEET_GAP: f64 = 12.0;
/// Legs of a dolly dwell, in blocks.
const DOLLY_LEG: f64 = 4.0;

/// Draw in `[lo, hi)` from the per-bot stream, so a number this scenario could have hard-coded
/// varies per bot and per station and still replays from the seed.
fn vary(ctx: &mut Ctx, lo: f64, hi: f64) -> f64 {
    lo + ctx.rng_next() * (hi - lo)
}

/// Station `slot` of the shared loop, pushed `lane` blocks outward (or inward) from the ring.
///
/// Every bot must agree on this geometry, so it takes only the anchor, and the orientation is
/// derived from the anchor's own coordinates rather than drawn — `rng_next` is a PER-BOT stream and
/// two bots drawing their own axis would be running two different loops.
fn station(anchor: (f64, f64), axis: f64, radius: f64, lane: f64, slot: i64) -> (f64, f64) {
    let theta = axis + slot.rem_euclid(STATIONS) as f64 * TAU / STATIONS as f64;
    let r = radius + lane;
    (anchor.0 + theta.cos() * r, anchor.1 + theta.sin() * r)
}

/// Nearest live peer's index and 2D distance on this shard's roster. Peer geometry only exists for
/// the bots this process hosts, hence `single_process=true`.
fn nearest_peer(ctx: &Ctx) -> Option<(usize, f64)> {
    let own = ctx.position()?;
    (0..ctx.player_count())
        .filter(|&j| j != ctx.index())
        .filter_map(|j| {
            ctx.peer_position(j)
                .map(|p| (j, (p.0 - own.0).hypot(p.2 - own.2)))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
}

/// Dwell on the CURRENT view by dollying: walk `leg` blocks along the facing, then the same
/// distance backward, leaving the camera exactly where the caller pointed it, until `deadline_ms`.
/// Returns how many halves actually displaced the body.
///
/// `hold_visual` pins the camera to the angle it finds, so across a dwell the view only
/// TRANSLATES along the sight line. That is what the frustum audit rewards: `overlap_mean` is
/// measured against the window's FIRST frame and a window with no rotation scores 1.0, while
/// `net_over_sweep` is 0.0 whenever the yaw range is empty. A dwell longer than one window
/// (93 frames = 4.65 s) therefore contributes whole windows of pure revisit.
///
/// Each half is ONE press of 0.9-1.4 s, over the `SHORT_RUN_FRAMES` (0.5 s) chatter floor, and a
/// blocked half is cut by `hold_visual`'s stall guard only after 1.2 s, so even walking into a tree
/// cannot produce a short press. The two halves cancel, so the dwell covers ground for the frozen
/// gate without inflating the window displacement median.
///
/// Turns are never taken during a dolly. `qc.rs`'s camera/travel gate is TWO-SIDED (`agreement()`
/// is `max(toward, away) / total`), so a turn while translating is evidence against the episode
/// whichever way it points, while a turn below `CAMERA_SPEED_FLOOR` (0.05 blocks/frame) is not
/// evidence at all — which is why every scripted turn here happens standing still.
async fn dolly_dwell(ctx: &mut Ctx, leg: f64, deadline_ms: i64) -> u32 {
    let hold_ms = ((leg / 4.3 * 1_000.0) as u64).clamp(900, 2_600);
    let sprint_prob = ctx.cfg().sprint_bout_prob;
    let mut moved = 0u32;
    let mut outbound = true;
    while !ctx.target_met() && ctx.elapsed_ms() + hold_ms as i64 + 200 < deadline_ms {
        let before = ctx.position();
        // Sprint only on forward halves (it does nothing backward) and only sometimes: `qc.rs`
        // fails an episode whose sustained forward runs all agree on the sprint bit.
        let sprint = outbound && ctx.rng_next() < sprint_prob;
        let dir = if outbound {
            WalkDirection::Forward
        } else {
            WalkDirection::Backward
        };
        ctx.hold_visual(dir, sprint, hold_ms, 0).await;
        if let (Some(a), Some(b)) = (before, ctx.position())
            && (b.0 - a.0).hypot(b.2 - a.2) > 1.0
        {
            moved += 1;
        }
        outbound = !outbound;
    }
    moved
}

/// Two bots run the SAME small closed loop in OPPOSITE directions from antipodal stations, so they
/// meet head-on twice a lap, and both reverse the whole loop together on a shared clock.
///
/// The revisit problem, the trajectory face and the teammate face at once. The trajectory is a
/// closed ring walked over and over, so every station's view returns every ~40 s and `path_length`
/// runs away from displacement. The teammate face is the reason the loop is SHARED and the
/// directions are OPPOSED: counter-running bots close on each other along the path they are already
/// looking down, so a peer enters the frame from the middle of the frame and grows, rather than
/// sliding in from an edge the model never saw. That is the 22.4% of multi-player windows the v5
/// audit found had a teammate intruding mid-window with no first-frame evidence.
///
/// Station beats alternate by parity, and both halves of the alternation are frozen-camera dollies:
/// an ONWARD beat looks down the stretch the peer is coming up, and a BACK-SIGHT beat looks at the
/// station just left. A station where the peer is already inside [`MEET_GAP`] switches to looking at
/// the peer itself, which is what films the pass.
///
/// The reversal is on a SHARED wall clock (`elapsed_ms / REVERSE_PERIOD_MS`), not a per-bot draw:
/// both bots flip together, so the counter-running relationship and the meeting cadence survive the
/// flip, while the sign of every station turn inverts. A one-way ring is the v5 defect in miniature
/// — all its turns share a sign, so net rotation grows as fast as sweep.
///
/// Distinct from `counter_ring_flow`, which splits a swarm across CONCENTRIC rings (traffic passes
/// abeam at a fixed radial gap, on the flat `build_arena` slab, with free camera sweeps): here both
/// bots share ONE ring on natural terrain, the passes are genuinely head-on, and the camera only
/// ever holds a loop bearing or the peer.
pub(super) async fn run(ctx: &mut Ctx) {
    settle_on_ground(ctx).await;
    let index = ctx.index();
    let global = global_index(ctx);
    let anchor = ctx.anchor();
    // Brief rule 5: open facing the SHARED subject. Both bots enter the loop at an anchor-derived
    // angle, which on its own can put them back to back at spawn — and this is the one script whose
    // headline claim is that a peer never enters the frame unannounced. Turning onto the anchor
    // first makes the opening frame — the only context frame a 93-frame training window gets —
    // contain the middle of the loop, where the other bot is. The turn is stationary, so it is
    // invisible to the camera/travel gate.
    let opened = if let Some(p) = ctx.position() {
        ctx.look_until_facing((anchor.0, p.1 + 1.4, anchor.1), 10.0, 4_000)
            .await
    } else {
        false
    };
    // Shared, bot-independent orientation — the same derivation `figure_eight_weave` uses, and for
    // the same reason: a drawn axis would give each bot its own loop.
    let axis = ((anchor.0 * 0.37 + anchor.1 * 0.61).sin() * TAU).rem_euclid(TAU);
    // Antipodal start and opposite direction by parity, so at N=2 the pair is set up to meet.
    let base_dir: i64 = if global.is_multiple_of(2) { 1 } else { -1 };
    let lane = if global.is_multiple_of(2) {
        LANE
    } else {
        -LANE
    };
    let mut slot: i64 = if global.is_multiple_of(2) {
        0
    } else {
        STATIONS / 2
    };
    let dwell_ms = vary(ctx, 6_500.0, 8_500.0) as i64;

    ctx.record(
        "observe",
        &format!("vis-counter-pair-open-{index}"),
        "Open the counter-run: shared loop, opposite direction, antipodal start.",
        json!({
            "global_index": global,
            "role": if base_dir > 0 { "counter_clockwise" } else { "clockwise" },
            "opened_on_anchor": opened,
            "axis_deg": r2(axis.to_degrees()),
            "loop_radius": r2(LOOP_RADIUS),
            "lane": r2(lane),
            "start_slot": slot,
            "stations": STATIONS,
            "dwell_ms": dwell_ms,
        }),
    );
    let entry = station(anchor, axis, LOOP_RADIUS, lane, slot);
    travel_to(ctx, entry, TRAVEL_HOP, 2.0).await;

    let mut visits = 0u32;
    while !ctx.target_met() {
        // Shared reversal: both bots read the same episode clock, so they flip together and stay
        // counter-running. Derived, never stored, so a bot that fell behind cannot desynchronise.
        let flip = (ctx.elapsed_ms() / REVERSE_PERIOD_MS).rem_euclid(2);
        let dir = if flip == 0 { base_dir } else { -base_dir };
        let from = slot;
        slot += dir;
        let target = station(anchor, axis, LOOP_RADIUS, lane, slot);
        let gap = travel_to(ctx, target, TRAVEL_HOP, 1.6).await;
        if ctx.target_met() {
            break;
        }

        let feet = ctx.position().map(|p| p.1).unwrap_or(64.0);
        let peer = nearest_peer(ctx);
        let (beat, confirmed) = if let Some((j, d)) = peer
            && d < MEET_GAP
        {
            // The pass itself. Looking at the peer while STANDING is the safe way to keep them
            // framed: `move_to_watching_peer` would track them while the body translates, which is
            // the camera/travel trap measured at 96-100% agreement.
            ("meet", look_at_peer(ctx, j, 400).await.is_some())
        } else if visits.is_multiple_of(2) {
            // ONWARD: hold the stretch the oncoming bot will come up, so when they appear they
            // appear inside an already-observed frustum.
            let onward = station(anchor, axis, LOOP_RADIUS, lane, slot + dir);
            (
                "onward",
                ctx.look_until_facing((onward.0, feet + 1.4, onward.1), 8.0, 5_000)
                    .await,
            )
        } else {
            // BACK-SIGHT: the station just left, which this POV rendered seconds ago from the other
            // end of the same leg.
            let behind = station(anchor, axis, LOOP_RADIUS, lane, from);
            (
                "backsight",
                ctx.look_until_facing((behind.0, feet + 1.4, behind.1), 8.0, 5_000)
                    .await,
            )
        };
        let leg = vary(ctx, DOLLY_LEG - 0.5, DOLLY_LEG + 0.8);
        let halves = dolly_dwell(ctx, leg, ctx.elapsed_ms() + dwell_ms).await;
        ctx.record(
            TYPE_LOOK,
            &format!("vis-counter-pair-{index}-{visits}"),
            &format!("Station {slot}: {beat} beat, then dolly on that view."),
            json!({
                "visit": visits,
                "beat": beat,
                "direction": dir,
                "reversed": flip == 1,
                "from_slot": from.rem_euclid(STATIONS),
                "at_slot": slot.rem_euclid(STATIONS),
                "arrival_gap": r2(gap),
                "facing_confirmed": confirmed,
                "peer_distance": peer.map(|(_, d)| r2(d)),
                "dolly_halves_moved": halves,
                "elapsed_ms": ctx.elapsed_ms(),
            }),
        );
        visits += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::{LANE, LOOP_RADIUS, STATIONS, station};

    /// The bug this pins: the two bots share this geometry, so a slot that is not a pure function
    /// of (anchor, axis, radius, lane, slot) — or a lane that shifts tangentially instead of
    /// radially — puts them on two different loops and the "head-on meeting" never happens. Also
    /// pins that opposite lanes stay a fixed radial distance apart at every station, which is what
    /// makes a pass a pass rather than a collision.
    #[test]
    fn opposite_lanes_ride_the_same_ring_at_a_fixed_gap() {
        let anchor = (31.0, -77.0);
        for &axis in &[0.0_f64, 1.1, 4.7] {
            let mut previous: Option<(f64, f64)> = None;
            for slot in 0..STATIONS {
                let out = station(anchor, axis, LOOP_RADIUS, LANE, slot);
                let inn = station(anchor, axis, LOOP_RADIUS, -LANE, slot);
                // Same bearing from the anchor, radii differing by exactly the two lane widths.
                let (ro, ri) = (
                    (out.0 - anchor.0).hypot(out.1 - anchor.1),
                    (inn.0 - anchor.0).hypot(inn.1 - anchor.1),
                );
                assert!(
                    (ro - (LOOP_RADIUS + LANE)).abs() < 1e-9,
                    "slot={slot} ro={ro}"
                );
                assert!(
                    (ri - (LOOP_RADIUS - LANE)).abs() < 1e-9,
                    "slot={slot} ri={ri}"
                );
                assert!(
                    ((out.0 - inn.0).hypot(out.1 - inn.1) - 2.0 * LANE).abs() < 1e-9,
                    "lanes must separate radially, not tangentially"
                );
                // Sides are equal, so the leg budget holds at every station.
                if let Some(p) = previous {
                    let side = (out.0 - p.0).hypot(out.1 - p.1);
                    let want =
                        2.0 * (LOOP_RADIUS + LANE) * (std::f64::consts::PI / STATIONS as f64).sin();
                    assert!((side - want).abs() < 1e-9, "slot={slot} side={side}");
                }
                previous = Some(out);
            }
            // The loop closes, and a bot three stations along is antipodal.
            let zero = station(anchor, axis, LOOP_RADIUS, LANE, 0);
            let wrapped = station(anchor, axis, LOOP_RADIUS, LANE, STATIONS);
            assert!((zero.0 - wrapped.0).abs() < 1e-9 && (zero.1 - wrapped.1).abs() < 1e-9);
            let half = station(anchor, axis, LOOP_RADIUS, LANE, STATIONS / 2);
            let span = (zero.0 - half.0).hypot(zero.1 - half.1);
            assert!(
                (span - 2.0 * (LOOP_RADIUS + LANE)).abs() < 1e-9,
                "span={span}"
            );
        }
    }
}
