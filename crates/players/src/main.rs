//! player SWARM capture bot (production runtime): hosts B logical players in ONE
//! process (one tokio `current_thread` runtime + one Bevy ECS via azalea's `SwarmBuilder`,
//! the validated swarm model) against the server container.
//!
//! Cross-container handshake (shared signal dir):
//!   all B bots connect (offline auth, usernames {BASENAME}{0..B-1}) + settle; when ALL B
//!   are ready the swarm writes `<signal>/bot-ready`; each bot waits for `<signal>/go`
//!   (touched by the coordinator after `replay start players ...`), samples its tick
//!   base, all B play the seeded scenario episode concurrently, and when ALL B finish the
//!   swarm writes `<signal>/done`. In multi-bot mode the bots stay connected until
//!   `<signal>/stopped` appears (or 30 s) so `replay stop players @a` catches every player
//!   online. Exit code 0 ONLY if all B bots completed ok.
//!
//! Per-bot artifacts are written directly into `<out_dir>` with canonical pNN names:
//! `<pNN>-actions.jsonl`, `<pNN>-events.jsonl`, and `<pNN>-meta.json`.
//! The swarm summary is `<out_dir>/swarm-summary.json` (sharded variants add `-shardN`).
//!
//! Tick model parity notes: `server_tick` is the per-bot client `Event::Tick` offset from
//! the `go` handshake (the same physics-tick proxy Mineflayer uses); `world_age_tick` is
//! the authoritative server world age from `ClientboundSetTime.game_time`, extrapolated
//! between time packets with the client tick counter. Dense `keys` are derived from the
//! live movement intent (azalea `ClientMovementState.move_direction` + jump/sprint/sneak state).

mod artifacts;
mod config;
mod control;
mod ctx;
mod handlers;
mod nav;
mod plan;
mod scenarios;
mod signal;
mod state;

use std::{
    collections::HashMap,
    env, fs,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use azalea::{
    BlockPos,
    core::direction::Direction,
    ecs::entity::Entity,
    entity::{EntityKindComponent, Position, inventory::Inventory},
    inventory::ItemStack,
    physics::client_movement::WalkDirection,
    physics::collision::BlockWithShape,
    prelude::*,
    registry::builtin::EntityKind,
    swarm::prelude::*,
};
use azalea_protocol::packets::game::ClientboundGamePacket;
use parking_lot::Mutex;
use serde_json::{Value, json};

use artifacts::*;
use config::*;
use ctx::*;
use handlers::*;
use pawa_schema::capture::{ActionEvent, ActionFrame};
use pawa_schema::time::{ClientTick, PlayerTimeMs, ServerTick, WorldAgeTick};
use scenarios::*;
use signal::*;
use state::*;

use control::{CameraIntent, ControlPlugin, LastAction, LocoIntent, look_angles};
use plan::{
    BuiltLandmark, EngineConfig, FormationLeg, Landmark, Mulberry32, RecentEdit, ScenarioEngine,
    r2, resolve_seed,
};

pub(crate) const RUNTIME_FAMILY: &str = "azalea";
const AZALEA_VERSION: &str = "0.16.0+mc26.2";

/// Waits for every bot to finish, signals `done`, holds the swarm online until the
/// coordinator's `stopped` (multi-bot), writes the swarm summary, then exits the process
/// (0 only if ALL bots completed ok).
async fn coordinator(shared: Shared) {
    let cfg = shared.inner.cfg.clone();
    loop {
        sleep_ms(200).await;
        if shared.inner.done_count.load(Ordering::SeqCst) >= cfg.bot_count {
            break;
        }
    }
    signal(&cfg, &cfg.shard_signal("done"));
    let ok = shared
        .inner
        .states
        .iter()
        .filter(|s| s.status.lock().as_str() == "ok")
        .count();
    log(&format!("done: {ok}/{} bots ok", cfg.bot_count));
    // Stay connected until the coordinator has stopped recording (so `replay stop players @a`
    // catches every bot online), then disconnect by exiting.
    wait_signal(&cfg, "stopped", 30000, 100).await;
    write_swarm_summary(&shared, ok);
    sleep_ms(600).await; // flush stdout / fs
    std::process::exit(if ok == cfg.bot_count { 0 } else { 1 });
}

async fn bot_watchdog(inner: Arc<BotInner>, shared: Shared) {
    sleep_ms(shared.inner.cfg.effective_hard_timeout_ms).await;
    if !inner.finished.load(Ordering::SeqCst) {
        if inner.spawned.load(Ordering::SeqCst) {
            finish(&inner, &shared, "partial", Some("progress_timeout"));
        } else {
            finish(&inner, &shared, "error", Some("server_connection_failed"));
        }
    }
}

// ---------------------------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------------------------

fn main() {
    // A bad config is fatal and named: a silent fallback's only symptom would be a bot behaving
    // unlike its manifest.
    let cfg = match Cfg::load() {
        Ok(cfg) => cfg,
        Err(err) => {
            eprintln!("players: invalid configuration: {err}");
            std::process::exit(2);
        }
    };
    // current_thread is the validated default; tokio_threads>1 opts a shard into a
    // multi-thread runtime (connection servicing spread over cores — unvalidated with azalea).
    let runtime = if cfg.tokio_threads <= 1 {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
    } else {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(cfg.tokio_threads)
            .enable_all()
            .build()
    }
    .expect("failed to build tokio runtime");
    runtime.block_on(async_main(cfg));
}

async fn async_main(cfg: Cfg) {
    let seed = shard_seed(
        resolve_seed(cfg.scenario_seed_raw.as_deref(), &cfg.episode_id),
        cfg.shard_index,
        cfg.shard_count,
    );
    let engine = ScenarioEngine::create(
        &cfg.scenario_name,
        seed,
        cfg.bot_count,
        &cfg.engine_config(),
    );
    if engine.name != engine.requested {
        log(&format!(
            "scenario {:?} resolved to {:?} (azalea implements: {})",
            engine.requested,
            engine.name,
            pawa_schema::catalog::SCENARIOS
                .iter()
                .map(|info| info.name)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    let states: Vec<Arc<BotInner>> = (0..cfg.bot_count)
        .map(|i| Arc::new(BotInner::new(i, cfg.username(i), cfg.player_id(i))))
        .collect();
    let _ = fs::create_dir_all(&cfg.out_dir);

    let shared = Shared {
        inner: Arc::new(SharedInner {
            cfg: cfg.clone(),
            engine,
            states: states.clone(),
            ready_count: AtomicUsize::new(0),
            done_count: AtomicUsize::new(0),
        }),
    };

    log(&format!(
        "hosting {} capture bot(s) -> {} (scenario={} requested={} seed={}, target_duration_ms={}, hard_timeout_ms={}, swarm={}, behavior={})",
        cfg.bot_count,
        cfg.out_dir.display(),
        shared.inner.engine.name,
        shared.inner.engine.requested,
        shared.inner.engine.seed,
        if cfg.episode_duration_ms > 0 {
            cfg.episode_duration_ms.to_string()
        } else {
            "default".to_string()
        },
        cfg.effective_hard_timeout_ms,
        serde_json::to_string(&resolved_swarm(&cfg, &shared.inner.engine)).unwrap_or_default(),
        serde_json::to_string(&resolved_behavior(&cfg)).unwrap_or_default(),
    ));

    tokio::spawn(coordinator(shared.clone()));
    for state in &states {
        tokio::spawn(bot_watchdog(state.clone(), shared.clone()));
    }

    let mut builder = SwarmBuilder::new()
        .add_plugins(ControlPlugin)
        .set_handler(handle)
        .set_swarm_handler(|_swarm, _event, _state| async { Ok::<(), anyhow::Error>(()) });
    for state in &states {
        let username = state.username.clone();
        builder = builder.add_account_with_state(
            Account::offline(&username),
            BotState {
                inner: state.clone(),
            },
        );
    }

    let address = format!("{}:{}", cfg.host, cfg.port);
    log(&format!("connecting swarm to {address}"));
    let exit = builder
        .join_delay(Duration::from_millis(cfg.stagger_ms))
        .set_swarm_state(shared.clone())
        .start(address.as_str())
        .await;
    // `start` no longer returns the error itself, only Bevy's process exit code — azalea has
    // already logged the cause through `tracing` by the time we get here.
    if let azalea::app::AppExit::Error(code) = exit {
        log(&format!("swarm start error: exit code {code}"));
        for state in &states {
            finish(state, &shared, "error", Some("server_connection_failed"));
        }
        write_swarm_summary(&shared, 0);
        signal(&cfg, &cfg.shard_signal("done"));
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::{Cfg, move_churn, parse_anchor, shard_anchor, shard_seed, shift};

    fn sharded_cfg(shard_index: usize, shard_count: usize, index_offset: usize) -> Cfg {
        Cfg {
            bot_count: 10,
            shard_index,
            shard_count,
            index_offset,
            total_bot_count: 10 * shard_count,
            shard_spread: 24.0,
            ..Cfg::default()
        }
    }

    #[test]
    fn sharded_usernames_are_globally_unique_and_contiguous() {
        // Shard 2 of 5 with B=10 owns global indices 20..29 → CapBot20..CapBot29.
        let cfg = sharded_cfg(2, 5, 20);
        assert_eq!(cfg.username(0), "CapBot20");
        assert_eq!(cfg.username(9), "CapBot29");
        assert_eq!(cfg.player_id(0), "player-20");
        // Unsharded multi keeps today's names.
        let cfg = sharded_cfg(0, 1, 0);
        assert_eq!(cfg.username(3), "CapBot3");
    }

    #[test]
    fn bot_count_1_uses_canonical_flat_pov_name() {
        assert_eq!(pawa_schema::catalog::pov_name(0, 1), "p00");
        assert_eq!(pawa_schema::catalog::pov_name(7, 50), "p07");
    }

    #[test]
    fn shard_signals_are_suffixed_only_when_sharded() {
        assert_eq!(
            sharded_cfg(2, 5, 20).shard_signal("bot-ready"),
            "bot-ready-2"
        );
        assert_eq!(sharded_cfg(2, 5, 20).shard_signal("done"), "done-2");
        assert_eq!(sharded_cfg(0, 1, 0).shard_signal("bot-ready"), "bot-ready");
    }

    #[test]
    fn shard_anchor_offsets_onto_a_ring_and_collapses_when_disabled() {
        let base = (-272.5, -698.5);
        // P=1 or spread 0 → base anchor untouched.
        assert_eq!(shard_anchor(&sharded_cfg(0, 1, 0), base), base);
        let mut flat = sharded_cfg(1, 4, 10);
        flat.shard_spread = 0.0;
        assert_eq!(shard_anchor(&flat, base), base);
        // Shard 0 of 4 sits at angle 0 → +R on x; shard 1 at 90° → +R on z.
        let a0 = shard_anchor(&sharded_cfg(0, 4, 0), base);
        assert!((a0.0 - (base.0 + 24.0)).abs() < 1e-9 && (a0.1 - base.1).abs() < 1e-9);
        let a1 = shard_anchor(&sharded_cfg(1, 4, 10), base);
        assert!((a1.0 - base.0).abs() < 1e-9 && (a1.1 - (base.1 + 24.0)).abs() < 1e-9);
        // Distinct shards get distinct anchors.
        assert_ne!(a0, a1);
    }

    #[test]
    fn shard_seed_is_identity_unsharded_and_distinct_per_shard() {
        assert_eq!(shard_seed(12345, 0, 1), 12345);
        let s0 = shard_seed(12345, 0, 5);
        let s1 = shard_seed(12345, 1, 5);
        let s2 = shard_seed(12345, 2, 5);
        assert_ne!(s0, s1);
        assert_ne!(s1, s2);
        assert_ne!(s0, 12345); // even shard 0 mixes so P=5 ≠ five copies of P=1
    }

    #[test]
    fn parse_anchor_reads_xz_and_rejects_garbage() {
        assert_eq!(parse_anchor("-272.5 -698.5"), Some((-272.5, -698.5)));
        assert_eq!(parse_anchor("0 0"), Some((0.0, 0.0)));
        assert_eq!(parse_anchor("  12   -3  \n"), Some((12.0, -3.0)));
        assert_eq!(parse_anchor("12"), None); // missing z
        assert_eq!(parse_anchor(""), None);
        assert_eq!(parse_anchor("a b"), None);
    }

    #[test]
    fn move_churn_measures_short_nonidle_runs() {
        // Synthetic dense move_dir stream. Non-idle runs: forward x2, left x5, forward x1 —
        // "none" runs never count (idle is not churn).
        let dirs = [
            "none", "forward", "forward", "left", "left", "left", "left", "left", "none",
            "forward", "none",
        ];
        let (median, frac) = move_churn(&dirs);
        // sorted run lengths [1, 2, 5]: median 2 ticks; 2 of 3 runs are <4 ticks (<200 ms).
        assert_eq!(median, Some(2.0));
        assert!((frac.unwrap() - 2.0 / 3.0).abs() < 1e-9);
        // Even run count -> median is the midpoint average: runs [2, 5] -> 3.5.
        let (median, _) = move_churn(&[
            "back", "back", "none", "right", "right", "right", "right", "right",
        ]);
        assert_eq!(median, Some(3.5));
        // All-idle (or empty) stream: no non-idle runs, both stats are null.
        assert_eq!(move_churn(&["none", "none", "none"]), (None, None));
        assert_eq!(move_churn(&[]), (None, None));
        // One long smooth run (what the dwell latch should produce): nothing under 200 ms.
        let (median, frac) = move_churn(&["forward"; 10]);
        assert_eq!(median, Some(10.0));
        assert_eq!(frac, Some(0.0));
    }

    #[test]
    fn shift_moves_origin_relative_plan_onto_the_anchor() {
        // A landmark at origin-relative (9, 0) lands on the swarm's spawn, not world (0,0).
        let anchor = (-272.5, -698.5);
        assert_eq!(shift((9.0, 0.0), anchor), (-263.5, -698.5));
        assert_eq!(shift((0.0, 0.0), anchor), anchor);
        // A zero anchor (older/single-file coordinator) leaves the plan at world origin.
        assert_eq!(shift((9.0, -4.0), (0.0, 0.0)), (9.0, -4.0));
    }
}
