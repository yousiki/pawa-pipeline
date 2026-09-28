use crate::*;

// ---------------------------------------------------------------------------------------------
// Per-bot state (shared between the event handler, the episode task, and the watchdog).
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
pub(crate) struct Pending {
    pub(crate) attack: bool,
    pub(crate) use_item: bool,
    pub(crate) place_block: bool,
    pub(crate) mine: bool,
}

pub(crate) struct BotInner {
    pub(crate) index: usize,
    pub(crate) username: String,
    pub(crate) player_id: String,
    pub(crate) spawned: AtomicBool,
    pub(crate) episode_started: AtomicBool,
    pub(crate) finished: AtomicBool,
    pub(crate) recording: AtomicBool,
    pub(crate) client_ticks: AtomicI64,
    pub(crate) go_tick_base: AtomicI64,
    pub(crate) t0: Mutex<Instant>,
    pub(crate) last_world_age: Mutex<Option<i64>>,
    pub(crate) world_age_client_tick: AtomicI64,
    pub(crate) go_world_age: Mutex<Option<i64>>,
    /// Server world age at `go`, from the go signal body. i64::MIN = unset.
    pub(crate) go_server_age: AtomicI64,
    pub(crate) seq: AtomicU64,
    pub(crate) events: Mutex<Vec<ActionEvent>>,
    pub(crate) frames: Mutex<Vec<ActionFrame>>,
    // (server_tick, x, z) per dense frame — feeds the swarm separation metrics.
    pub(crate) sep_samples: Mutex<Vec<(i64, f64, f64)>>,
    pub(crate) step_errors: Mutex<Vec<String>>,
    pub(crate) pending: Mutex<Pending>,
    pub(crate) last_yaw: Mutex<f64>,
    pub(crate) last_pitch: Mutex<f64>,
    pub(crate) live_pos: Mutex<Option<(f64, f64, f64)>>,
    pub(crate) status: Mutex<String>,
    pub(crate) error_code: Mutex<Option<String>>,
    pub(crate) episode_actual_duration_ms: AtomicI64,
    pub(crate) duration_met: Mutex<Option<bool>>,
    // Decoupled quantized control intents (set by the scenario task, read by the ECS systems).
    pub(crate) camera_intent: Mutex<CameraIntent>,
    pub(crate) loco_intent: Mutex<LocoIntent>,
    pub(crate) last_action: Mutex<LastAction>,
    pub(crate) camera_busy: AtomicBool,
    // Scenario-commanded jump request, OR-ed into the controller's `want_jump` each tick. The
    // controller is the sole authority on the wire (azalea's executor jump never reaches the
    // server — see control.rs module docs), so world-action primitives that need a jump (e.g.
    // `build_tower_underneath`'s pillar-jump) raise this flag instead of calling `bot.jump()`.
    pub(crate) jump_request: AtomicBool,
    // Our authoritative camera heading (degrees). apply_intents owns it and writes LookDirection
    // from it every tick, so the live pathfinder executor's look-at snap can never leak into the
    // wire or the recording.
    pub(crate) cam_yaw: Mutex<f64>,
    pub(crate) cam_pitch: Mutex<f64>,
    // Movement latch state (control::apply_intents): identity-dwell + overshoot-settle for the
    // APPLIED WalkDirection. Feeds `control::latch_step`.
    pub(crate) move_latch: Mutex<control::MoveLatch>,
    // Sprint is decided once per movement bout, not per tick (control::sprint_for_bout). Holding
    // the decision here is what keeps the recorded sprint bit independent of the navigation state
    // instead of being a restatement of "forward and far from the goal".
    pub(crate) sprint_bout: Mutex<control::SprintBout>,
    // Where this bot stood when its script finished, or when it stalled mid-script. The ambient
    // wander is leashed to it, so an idle bot stays roughly where the scenario left it.
    pub(crate) ambient_anchor: Mutex<Option<(f64, f64, f64)>>,
    // Consecutive ticks the scenario has issued NO intent at all (Idle loco + Hold camera). The
    // controller takes over with the ambient wander past `ambient_idle_ticks`. This is the
    // mid-script half of the dead-air problem: the post-finish path only helps a bot whose script
    // ended, and the worst offenders never end — they reach a terminal branch and keep looping in
    // it until the episode duration is met (mob_chase's post-capture tableau, the construction
    // scenarios once the structure is up).
    pub(crate) stall_ticks: AtomicU32,
    // Consecutive `move_to*` calls that failed without displacing the body (Ctx::unpin_after_move).
    pub(crate) move_stalls: AtomicU32,
    // Planner-owned navigation state (see nav.rs): the scenario sets the goal, the per-bot
    // nav_driver task plans corridors via azalea's public `calculate_path`, and the controller
    // follows/replans/gives-up in nav::nav_tick. azalea's own goto/executor are never engaged.
    pub(crate) nav: Mutex<nav::NavState>,
}

impl BotInner {
    pub(crate) fn new(index: usize, username: String, player_id: String) -> Self {
        BotInner {
            index,
            username,
            player_id,
            spawned: AtomicBool::new(false),
            episode_started: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            recording: AtomicBool::new(false),
            client_ticks: AtomicI64::new(0),
            go_tick_base: AtomicI64::new(0),
            t0: Mutex::new(Instant::now()),
            last_world_age: Mutex::new(None),
            world_age_client_tick: AtomicI64::new(0),
            go_world_age: Mutex::new(None),
            go_server_age: AtomicI64::new(i64::MIN),
            seq: AtomicU64::new(0),
            events: Mutex::new(Vec::new()),
            frames: Mutex::new(Vec::new()),
            sep_samples: Mutex::new(Vec::new()),
            step_errors: Mutex::new(Vec::new()),
            pending: Mutex::new(Pending::default()),
            last_yaw: Mutex::new(0.0),
            last_pitch: Mutex::new(0.0),
            live_pos: Mutex::new(None),
            status: Mutex::new("pending".to_string()),
            error_code: Mutex::new(None),
            episode_actual_duration_ms: AtomicI64::new(0),
            duration_met: Mutex::new(None),
            camera_intent: Mutex::new(CameraIntent::Hold),
            loco_intent: Mutex::new(LocoIntent::Idle),
            last_action: Mutex::new(LastAction::default()),
            camera_busy: AtomicBool::new(false),
            jump_request: AtomicBool::new(false),
            cam_yaw: Mutex::new(0.0),
            cam_pitch: Mutex::new(0.0),
            move_latch: Mutex::new(control::MoveLatch::default()),
            sprint_bout: Mutex::new(control::SprintBout::default()),
            ambient_anchor: Mutex::new(None),
            stall_ticks: AtomicU32::new(0),
            move_stalls: AtomicU32::new(0),
            nav: Mutex::new(nav::NavState::default()),
        }
    }

    pub(crate) fn offset_tick(&self) -> i64 {
        (self.client_ticks.load(Ordering::SeqCst) - self.go_tick_base.load(Ordering::SeqCst)).max(0)
    }

    pub(crate) fn world_age_tick(&self) -> Option<i64> {
        // After `go`, the age is DERIVED from the server-truth anchor the go signal carried:
        // go_age + client ticks since go. The SetTime latch extrapolates from (age_at_send,
        // client_tick_at_receive), so a constant transport delay shifts every latched age by that
        // delay — measured 64 ticks flat on a loaded 8-bot swarm. The client tick advances in
        // lockstep with the server once connected, so the derived clock has no such term.
        let go_age = self.go_server_age.load(Ordering::SeqCst);
        if go_age != i64::MIN {
            return Some(
                go_age
                    + (self.client_ticks.load(Ordering::SeqCst)
                        - self.go_tick_base.load(Ordering::SeqCst)),
            );
        }
        let last = *self.last_world_age.lock();
        last.map(|age| {
            age + (self.client_ticks.load(Ordering::SeqCst)
                - self.world_age_client_tick.load(Ordering::SeqCst))
        })
    }

    pub(crate) fn elapsed_ms(&self) -> i64 {
        (self.t0.lock().elapsed().as_secs_f64() * 1000.0) as i64
    }
}

#[derive(Component, Clone)]
pub(crate) struct BotState {
    pub(crate) inner: Arc<BotInner>,
}

impl Default for BotState {
    fn default() -> Self {
        BotState {
            inner: Arc::new(BotInner::new(0, String::new(), String::new())),
        }
    }
}

// SwarmBuilder requires the swarm-state type to be `Default` (it seeds an initial value
// before `set_swarm_state` replaces it with the real one); the Default values are unused.
#[derive(Resource, Clone)]
pub(crate) struct Shared {
    pub(crate) inner: Arc<SharedInner>,
}

pub(crate) struct SharedInner {
    pub(crate) cfg: Cfg,
    pub(crate) engine: ScenarioEngine,
    pub(crate) states: Vec<Arc<BotInner>>,
    pub(crate) ready_count: AtomicUsize,
    pub(crate) done_count: AtomicUsize,
}

impl Default for Shared {
    fn default() -> Self {
        Shared {
            inner: Arc::new(SharedInner {
                cfg: Cfg::default(),
                engine: ScenarioEngine::create("landmark_tour", 0, 0, &EngineConfig::default()),
                states: Vec::new(),
                ready_count: AtomicUsize::new(0),
                done_count: AtomicUsize::new(0),
            }),
        }
    }
}
