//! Planner-owned navigation: ONE brain for progress, replanning, and giving up.
//!
//! The old design left azalea's pathfinder LIVE as a waypoint provider (`goto` +
//! `ExecutingPath.front()`), which split navigation across two brains: our controller decided
//! motion, azalea's executor bookkeeping decided path progression (feet-block node pops, the
//! centered-and-slow final-node pop, `path.len() < 5` partial recalcs). Every quality fix ended
//! up bridging that split with another heuristic — node-eps tuned to azalea's pop condition,
//! degenerate-frontier detection, goal-push fallbacks — and each bridge leaked somewhere new
//! (learned over several churn-test iterations).
//!
//! Here WE own the loop and azalea is reduced to a planning pure function plus the
//! protocol/physics client:
//!
//!   * a per-bot driver task calls [`azalea::pathfinder::calculate_path`] (public API) with
//!     SHORT timeouts and writes the resulting corridor into [`NavState`];
//!   * the controller ([`nav_tick`], called from `control::apply_intents` each tick) follows the
//!     corridor loosely — nodes are consumed by PROXIMITY (window-scanned, so cutting near a
//!     later node consumes earlier ones) and steering aims at the farthest in-range node, which
//!     rounds corners into single long key presses (data-collection stance: NEARBY is enough,
//!     never an exact xyz);
//!   * progress, replanning, and giving up live in one place: no positional progress for
//!     [`NO_PROGRESS_REPLAN_TICKS`] triggers a replan from the CURRENT position; repeated
//!     no-progress replans or repeated empty corridors mark the goal [`NavState::unreachable`]
//!     and `move_to` returns immediately instead of dead-staring at an obstacle.
//!
//! azalea's `GotoEvent`/executor are never engaged, so their wire-ordering invariants and event
//! floods are out of the picture entirely.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use azalea::{
    BlockPos, Client,
    pathfinder::{
        CalculatePathCtx, PathfinderOpts,
        astar::PathfinderTimeout,
        calculate_path,
        custom_state::CustomPathfinderState,
        goals::{BlockPosGoal, XZGoal},
        mining::MiningCache,
        moves,
    },
    physics::collision::BlockWithShape,
};

use crate::BotInner;
use pawa_schema::regions::Bounds;

#[derive(Clone, Copy)]
struct RegionalSearch {
    bounds: Bounds,
    origin: BlockPos,
    parkour: bool,
}

fn regional_moves(ctx: &mut moves::MovesCtx, node: azalea::pathfinder::positions::RelBlockPos) {
    let search = *ctx
        .custom_state
        .get::<RegionalSearch>()
        .expect("regional search bounds");
    if search.parkour {
        moves::default_move(ctx, node);
    } else {
        moves::basic::basic_move(ctx, node);
    }
    ctx.edges.retain(|edge| {
        let p = edge.movement.target.apply(search.origin);
        search.bounds.contains(p.x as f64 + 0.5, p.z as f64 + 0.5)
    });
}

/// A corridor node passes when the bot is within this XZ distance of it (blocks). Well above the
/// per-tick displacement (walk ~0.22, sprint ~0.28) so passing is never missed between ticks,
/// and generous on purpose: the dataset needs "near", not exact node occupancy.
const ADVANCE_RADIUS: f64 = 1.5;
/// How many upcoming nodes the pass-by scan looks at: brushing near node i+k consumes i..=i+k.
const SKIP_WINDOW: usize = 8;
/// Steering aims at the FARTHEST corridor node within this range (blocks) among the next
/// [`SKIP_WINDOW`] — corners round off into single long presses.
const LOOKAHEAD_DIST: f64 = 4.5;
/// No positional progress for this many ticks (3 s) → replan from the current position.
const NO_PROGRESS_REPLAN_TICKS: i64 = 60;
/// Positional progress = moving this far (blocks) from the last checkpoint. Above the shove
/// radius of a crowd of bots pushing each other around, far below real navigation speed.
const PROGRESS_EPS: f64 = 1.5;
/// This many consecutive no-progress replans → the goal is unreachable from here; give up.
const MAX_STALLED_REPLANS: u32 = 3;
/// This many consecutive EMPTY corridors (astar's best frontier is where we stand) → give up.
const MAX_EMPTY_PLANS: u32 = 2;
/// Planning timeouts: partial paths are perfectly fine (we replan leg by leg), so keep the
/// astar budget small — azalea's defaults (1 s / 5 s) would stall a swarm on unreachable goals.
const CALC_MIN_TIMEOUT_MS: u64 = 250;
const CALC_MAX_TIMEOUT_MS: u64 = 1500;
/// Driver poll cadence (ms).
const DRIVER_POLL_MS: u64 = 100;

/// One planned foot position (block center) plus the edge that reaches it — `prev_block` is kept
/// so the parkour take-off check (`control::parkour_step`) still knows its jump edge.
#[derive(Clone, Copy, Debug)]
pub struct NavNode {
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub block: (i32, i32, i32),
    pub prev_block: (i32, i32, i32),
}

#[derive(Clone, Copy, Debug)]
pub struct NavGoal {
    /// Used only by explicit regional traffic; ordinary scenario routes remain unbounded.
    pub bounds: Option<Bounds>,
    pub x: f64,
    pub z: f64,
    pub radius: f64,
    pub allow_parkour: bool,
    /// Surface goals must also reach this feet Y; `None` preserves ordinary XZ navigation.
    pub target_y: Option<i32>,
}

/// Shared per-bot navigation state: the scenario sets `goal` (via `Ctx::move_to`), the driver
/// task fills `corridor`, and [`nav_tick`] (controller) advances/replans/gives up.
#[derive(Default)]
pub struct NavState {
    pub goal: Option<NavGoal>,
    /// Monotonic goal generation; a calc result tagged with an older generation is dropped.
    pub generation: u64,
    pub corridor: Vec<NavNode>,
    pub cursor: usize,
    pub is_partial: bool,
    pub planning: bool,
    pub want_replan: bool,
    /// Consecutive replans that produced an empty corridor.
    pub empty_plans: u32,
    /// Consecutive no-progress replan triggers since the last real displacement.
    pub stalled_replans: u32,
    pub unreachable: bool,
    /// Progress checkpoint (position + client tick), maintained by [`nav_tick`].
    pub progress_pos: Option<(f64, f64)>,
    pub progress_tick: i64,
}

impl NavState {
    /// Install a new goal (from `move_to`): bumps the generation so stale calc results and the
    /// old corridor are discarded, and resets all give-up bookkeeping.
    pub fn set_goal(&mut self, goal: NavGoal) {
        self.generation += 1;
        self.goal = Some(goal);
        self.corridor.clear();
        self.cursor = 0;
        self.is_partial = false;
        self.want_replan = false;
        self.empty_plans = 0;
        self.stalled_replans = 0;
        self.unreachable = false;
        self.progress_pos = None;
    }

    /// Clear the goal (move finished/aborted): the driver stops planning for it.
    pub fn clear_goal(&mut self) {
        self.generation += 1;
        self.goal = None;
        self.corridor.clear();
        self.cursor = 0;
        self.want_replan = false;
        self.unreachable = false;
    }
}

/// What the controller steers at this tick.
#[derive(Clone, Copy, Debug, Default)]
pub struct NavTickOut {
    /// XZ steering target (a lookahead corridor node), `None` = stand (arrived at the corridor
    /// end / waiting for a plan / unreachable).
    pub waypoint: Option<(f64, f64)>,
    /// The upcoming corridor node is a full block above us → pre-contact ascent jump.
    pub uphill: bool,
    /// Live parkour edge `(start, target)` (horizontal span >= 2) when parkour is enabled.
    #[allow(clippy::type_complexity)]
    // two bare block coords; a type alias would just rename them
    pub parkour_edge: Option<((i32, i32, i32), (i32, i32, i32))>,
}

fn dist_xz(a: (f64, f64), b: (f64, f64)) -> f64 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
}

fn goal_reached(goal: NavGoal, pos: (f64, f64, f64)) -> bool {
    dist_xz((goal.x, goal.z), (pos.0, pos.2)) <= goal.radius
        && goal
            .target_y
            .is_none_or(|target_y| (pos.1 - target_y as f64).abs() <= 1.0)
}

/// Consume every corridor node the bot has passed: scan the next [`SKIP_WINDOW`] nodes and jump
/// the cursor past the FARTHEST one within [`ADVANCE_RADIUS`] (loose skip-ahead).
pub fn advance_cursor(corridor: &[NavNode], cursor: usize, pos: (f64, f64)) -> usize {
    let mut new_cursor = cursor;
    for (k, node) in corridor.iter().enumerate().skip(cursor).take(SKIP_WINDOW) {
        if dist_xz((node.x, node.z), pos) <= ADVANCE_RADIUS {
            new_cursor = k + 1;
        }
    }
    new_cursor
}

/// The FARTHEST corridor node within [`LOOKAHEAD_DIST`] among the next [`SKIP_WINDOW`], falling
/// back to the immediate cursor node (even if it is farther than the lookahead range).
pub fn steer_index(corridor: &[NavNode], cursor: usize, pos: (f64, f64)) -> Option<usize> {
    if cursor >= corridor.len() {
        return None;
    }
    let mut best = cursor;
    for (k, node) in corridor.iter().enumerate().skip(cursor).take(SKIP_WINDOW) {
        if dist_xz((node.x, node.z), pos) <= LOOKAHEAD_DIST {
            best = k;
        }
    }
    Some(best)
}

/// One controller tick of corridor-following: advance by proximity, keep the progress/replan
/// bookkeeping, and emit the steering target. Pure over [`NavState`] (unit-tested).
pub fn nav_tick(nav: &mut NavState, pos: (f64, f64, f64), tick: i64) -> NavTickOut {
    let Some(goal) = nav.goal else {
        return NavTickOut::default();
    };
    if nav.unreachable {
        return NavTickOut::default();
    }
    let pos_xz = (pos.0, pos.2);

    // ---- progress bookkeeping (drives replan + give-up) ----
    match nav.progress_pos {
        None => {
            nav.progress_pos = Some(pos_xz);
            nav.progress_tick = tick;
        }
        Some(cp) if dist_xz(cp, pos_xz) >= PROGRESS_EPS => {
            nav.progress_pos = Some(pos_xz);
            nav.progress_tick = tick;
            nav.stalled_replans = 0;
            nav.empty_plans = 0;
        }
        Some(_) => {
            if tick - nav.progress_tick > NO_PROGRESS_REPLAN_TICKS
                && !nav.planning
                && !nav.want_replan
            {
                nav.want_replan = true;
                nav.progress_tick = tick; // space out the triggers
                nav.stalled_replans += 1;
                if nav.stalled_replans >= MAX_STALLED_REPLANS {
                    nav.unreachable = true;
                    return NavTickOut::default();
                }
            }
        }
    }

    nav.cursor = if goal.target_y.is_some() {
        // Surface routes must touch each validated feet block; the ordinary 1.5-block pass radius
        // consumes adjacent nodes before arrival and lets steering cut a corner through a cave mouth.
        if nav.cursor < nav.corridor.len()
            && nav.corridor[nav.cursor].block
                == (
                    pos.0.floor() as i32,
                    pos.1.floor() as i32,
                    pos.2.floor() as i32,
                )
        {
            nav.cursor + 1
        } else {
            nav.cursor
        }
    } else {
        advance_cursor(&nav.corridor, nav.cursor, pos_xz)
    };

    // ---- corridor consumed: arrived, or ask for the next leg (partial paths replan leg by leg)
    if nav.cursor >= nav.corridor.len() {
        if !goal_reached(goal, pos) && !nav.planning && !nav.want_replan {
            nav.want_replan = true;
        }
        return NavTickOut::default();
    }

    let idx = if goal.target_y.is_some() {
        nav.cursor
    } else {
        steer_index(&nav.corridor, nav.cursor, pos_xz).unwrap_or(nav.cursor)
    };
    let node = nav.corridor[idx];
    let near = nav.corridor[nav.cursor];
    let uphill = (near.y - pos.1) > 0.5 && dist_xz((near.x, near.z), pos_xz) < 2.0;
    let parkour_edge = if goal.allow_parkour {
        let (sx, _, sz) = near.prev_block;
        let (tx, _, tz) = near.block;
        if (tx - sx).abs().max((tz - sz).abs()) >= 2 {
            Some((near.prev_block, near.block))
        } else {
            None
        }
    } else {
        None
    };
    NavTickOut {
        waypoint: Some((node.x, node.z)),
        uphill,
        parkour_edge,
    }
}

/// A feet block is on an exposed natural surface when its support is solid and the whole column
/// above it is air. This rejects routes through cave floors while retaining open ravines and valleys.
fn exposed_surface(world: &azalea::world::World, feet: BlockPos) -> bool {
    const WORLD_TOP: i32 = 320;
    let Some(support) = world.get_block_state(BlockPos::new(feet.x, feet.y - 1, feet.z)) else {
        return false;
    };
    !support.is_air()
        && support.is_collision_shape_full()
        && (feet.y..WORLD_TOP).all(|y| {
            world
                .get_block_state(BlockPos::new(feet.x, y, feet.z))
                .is_some_and(|state| state.is_air())
        })
}

/// Per-bot planning driver: watches [`NavState`] and runs azalea's `calculate_path` (public
/// planning API, short timeouts) on a blocking worker whenever a (re)plan is wanted. Spawned
/// once per bot at Spawn; exits when the episode finishes.
pub async fn nav_driver(bot: Client, inner: Arc<BotInner>) {
    let goto_id = Arc::new(AtomicUsize::new(0));
    loop {
        tokio::time::sleep(Duration::from_millis(DRIVER_POLL_MS)).await;
        if inner.finished.load(Ordering::SeqCst) {
            break;
        }
        let job = {
            let mut nav = inner.nav.lock();
            match nav.goal {
                Some(goal)
                    if !nav.planning
                        && !nav.unreachable
                        && (nav.want_replan || nav.corridor.is_empty()) =>
                {
                    nav.planning = true;
                    nav.want_replan = false;
                    Some((goal, nav.generation))
                }
                _ => None,
            }
        };
        let Some((goal, generation)) = job else {
            continue;
        };
        // Pose and world only exist while the bot is actually in an instance. Missing either is
        // transient (pre-spawn / respawn), so hand the planning flag back and retry next poll
        // rather than leaving the goal stuck in `planning`.
        let (Ok(pos), Ok(world_lock)) = (bot.position(), bot.world()) else {
            inner.nav.lock().planning = false;
            continue;
        };
        let validation_world = world_lock.clone();
        let start = BlockPos::new(
            pos.x.floor() as i32,
            pos.y.floor() as i32,
            pos.z.floor() as i32,
        );
        let custom_state = CustomPathfinderState::default();
        if let Some(bounds) = goal.bounds {
            custom_state.0.write().insert(RegionalSearch {
                bounds,
                origin: start,
                parkour: goal.allow_parkour,
            });
        }
        let successors: moves::SuccessorsFn = if goal.bounds.is_some() {
            regional_moves
        } else if goal.allow_parkour {
            moves::default_move
        } else {
            moves::basic::basic_move
        };
        let surface_only = goal.target_y.is_some();
        let goal_block = BlockPos::new(
            goal.x.floor() as i32,
            goal.target_y.unwrap_or(start.y),
            goal.z.floor() as i32,
        );
        let goal: Arc<dyn azalea::pathfinder::goals::Goal> = match goal.target_y {
            Some(_) => Arc::new(BlockPosGoal(goal_block)),
            None => Arc::new(XZGoal {
                x: goal_block.x,
                z: goal_block.z,
            }),
        };
        let ctx = CalculatePathCtx {
            entity: bot.entity,
            start,
            goal,
            world_lock,
            goto_id_atomic: goto_id.clone(),
            mining_cache: MiningCache::new(None),
            custom_state,
            opts: PathfinderOpts::new()
                .allow_mining(false)
                .successors_fn(successors)
                .min_timeout(PathfinderTimeout::Time(Duration::from_millis(
                    CALC_MIN_TIMEOUT_MS,
                )))
                .max_timeout(PathfinderTimeout::Time(Duration::from_millis(
                    CALC_MAX_TIMEOUT_MS,
                ))),
        };
        let found = tokio::task::spawn_blocking(move || calculate_path(ctx))
            .await
            .ok()
            .flatten();
        let mut nav = inner.nav.lock();
        nav.planning = false;
        if nav.generation != generation || nav.goal.is_none() {
            continue; // goal changed while we were planning — result is stale
        }
        let Some(found) = found else {
            continue; // interrupted by a newer calculation
        };
        if surface_only {
            let world = validation_world.read();
            let path_is_exposed = found.path.as_ref().is_none_or(|path| {
                path.iter()
                    .all(|edge| exposed_surface(&world, edge.movement.target))
            });
            if !path_is_exposed {
                // A shortest path through a cave is not a usable v6 route. Treat it like an
                // unreachable goal so the scenario can try its bounded sidestep instead of filming
                // an underground detour. Drop the old corridor too: it may be the very route that
                // triggered this replan.
                nav.corridor.clear();
                nav.cursor = 0;
                nav.want_replan = true;
                nav.empty_plans += 1;
                if nav.empty_plans >= MAX_EMPTY_PLANS {
                    nav.unreachable = true;
                }
                continue;
            }
        }
        let mut prev = (found.start.x, found.start.y, found.start.z);
        let corridor: Vec<NavNode> = found
            .path
            .unwrap_or_default()
            .iter()
            .map(|edge| {
                let t = edge.movement.target;
                let node = NavNode {
                    x: t.x as f64 + 0.5,
                    y: t.y as f64,
                    z: t.z as f64 + 0.5,
                    block: (t.x, t.y, t.z),
                    prev_block: prev,
                };
                prev = (t.x, t.y, t.z);
                node
            })
            .collect();
        nav.is_partial = found.is_partial;
        if corridor.is_empty() {
            nav.empty_plans += 1;
            if nav.empty_plans >= MAX_EMPTY_PLANS {
                nav.unreachable = true;
            }
        } else {
            nav.empty_plans = 0;
        }
        nav.corridor = corridor;
        nav.cursor = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corridor(points: &[(f64, f64)]) -> Vec<NavNode> {
        let mut prev = (0, 64, 0);
        points
            .iter()
            .map(|&(x, z)| {
                let block = (x.floor() as i32, 64, z.floor() as i32);
                let node = NavNode {
                    x,
                    y: 64.0,
                    z,
                    block,
                    prev_block: prev,
                };
                prev = block;
                node
            })
            .collect()
    }

    fn goal_state(corr: Vec<NavNode>) -> NavState {
        let mut nav = NavState::default();
        nav.set_goal(NavGoal {
            bounds: None,
            x: 100.0,
            z: 0.5,
            radius: 2.0,
            allow_parkour: false,
            target_y: None,
        });
        nav.corridor = corr;
        nav
    }

    #[test]
    fn cursor_advances_by_proximity_and_skips_passed_nodes() {
        let c = corridor(&[(1.5, 0.5), (2.5, 0.5), (3.5, 0.5), (10.5, 0.5)]);
        // Standing near node 2 consumes nodes 0..=2 in one step (loose skip-ahead).
        assert_eq!(advance_cursor(&c, 0, (3.4, 0.5)), 3);
        // Nothing in range: cursor stays.
        assert_eq!(advance_cursor(&c, 3, (5.0, 0.5)), 3);
    }

    #[test]
    fn steering_aims_at_farthest_in_range_node() {
        let c = corridor(&[(1.5, 0.5), (2.5, 0.5), (3.5, 0.5), (10.5, 0.5)]);
        // From the start, node 2 (3.5) is within 4.5 but node 3 (10.5) is not.
        assert_eq!(steer_index(&c, 0, (0.5, 0.5)), Some(2));
        // Cursor node beyond lookahead range still returned as fallback.
        assert_eq!(steer_index(&c, 3, (0.5, 0.5)), Some(3));
    }

    #[test]
    fn consumed_corridor_requests_next_leg_until_goal() {
        let mut nav = goal_state(corridor(&[(1.5, 0.5)]));
        // Walk past the single node: corridor consumed, goal still 90+ blocks away -> replan.
        let out = nav_tick(&mut nav, (1.5, 64.0, 0.5), 10);
        assert!(out.waypoint.is_none());
        assert!(nav.want_replan);
        // Within the goal radius: consumed corridor does NOT request another leg.
        let mut nav = goal_state(corridor(&[(99.5, 0.5)]));
        let out = nav_tick(&mut nav, (99.4, 64.0, 0.5), 10);
        assert!(out.waypoint.is_none());
        assert!(!nav.want_replan);
    }

    #[test]
    fn surface_route_advances_only_after_touching_the_feet_block() {
        let mut nav = NavState::default();
        nav.set_goal(NavGoal {
            bounds: None,
            x: 10.0,
            z: 0.5,
            radius: 2.0,
            allow_parkour: false,
            target_y: Some(64),
        });
        nav.corridor = corridor(&[(1.5, 0.5), (2.5, 0.5)]);
        nav_tick(&mut nav, (0.5, 64.0, 0.5), 1);
        assert_eq!(nav.cursor, 0);
        nav_tick(&mut nav, (1.5, 64.0, 0.5), 2);
        assert_eq!(nav.cursor, 1);
    }

    #[test]
    fn surface_goal_does_not_finish_on_lower_cave_floor() {
        let mut nav = NavState::default();
        nav.set_goal(NavGoal {
            bounds: None,
            x: 10.0,
            z: 0.5,
            radius: 2.0,
            allow_parkour: false,
            target_y: Some(70),
        });
        nav.corridor = corridor(&[(10.5, 0.5)]);
        nav_tick(&mut nav, (10.5, 64.0, 0.5), 1);
        assert!(nav.want_replan);
        nav.want_replan = false;
        nav.corridor.clear();
        assert!(goal_reached(nav.goal.unwrap(), (10.5, 70.0, 0.5)));
    }

    #[test]
    fn no_progress_replans_then_gives_up() {
        let mut nav = goal_state(corridor(&[(10.5, 0.5)]));
        let pos = (0.5, 64.0, 0.5);
        let mut tick = 0;
        let mut replans = 0;
        // Never moving: each NO_PROGRESS window triggers one replan; after
        // MAX_STALLED_REPLANS the goal is declared unreachable.
        while !nav.unreachable {
            tick += 1;
            let before = nav.want_replan;
            nav_tick(&mut nav, pos, tick);
            if nav.want_replan && !before {
                replans += 1;
                nav.want_replan = false; // driver consumes the request
            }
            assert!(tick < 10_000, "must converge");
        }
        assert_eq!(replans, MAX_STALLED_REPLANS as usize); // the last trigger also flips unreachable
        assert!(nav_tick(&mut nav, pos, tick + 1).waypoint.is_none());
    }

    #[test]
    fn moving_resets_the_stall_and_empty_counters() {
        let mut nav = goal_state(corridor(&[(10.5, 0.5)]));
        nav.stalled_replans = 2;
        nav.empty_plans = 1;
        nav_tick(&mut nav, (0.5, 64.0, 0.5), 1);
        // Displace beyond PROGRESS_EPS: counters reset.
        nav_tick(&mut nav, (3.0, 64.0, 0.5), 2);
        assert_eq!(nav.stalled_replans, 0);
        assert_eq!(nav.empty_plans, 0);
    }

    #[test]
    fn parkour_edge_reported_only_for_gaps() {
        let mut c = corridor(&[(5.5, 0.5)]);
        c[0].prev_block = (2, 64, 0); // span 3 => parkour edge
        let mut nav = NavState::default();
        nav.set_goal(NavGoal {
            bounds: None,
            x: 100.0,
            z: 0.5,
            radius: 2.0,
            allow_parkour: true,
            target_y: None,
        });
        nav.corridor = c;
        let out = nav_tick(&mut nav, (2.5, 64.0, 0.5), 1);
        assert_eq!(out.parkour_edge, Some(((2, 64, 0), (5, 64, 0))));
    }
}
