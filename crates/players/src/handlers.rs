use crate::*;

// ---------------------------------------------------------------------------------------------
// Event handler: tick counting, world-age anchoring, dense per-tick recording.
// ---------------------------------------------------------------------------------------------

pub(crate) async fn handle(bot: Client, event: Event, state: BotState) -> anyhow::Result<()> {
    let shared = bot.resource::<Shared>();
    match event {
        Event::Init => {
            // The server caps delivery by the client request as well as server.properties.
            // Set this before login so replays contain the requested distant terrain.
            bot.set_client_information(azalea::ClientInformation {
                view_distance: shared.inner.cfg.view_distance,
                ..Default::default()
            })?;
        }
        Event::Spawn => {
            state.inner.spawned.store(true, Ordering::SeqCst);
            if !state.inner.episode_started.swap(true, Ordering::SeqCst) {
                let b = bot.clone();
                let s = state.clone();
                let sh = shared.clone();
                tokio::spawn(async move { run_bot(b, s, sh).await });
                // Planner-owned navigation loop (nav.rs) — plans corridors for move_to goals.
                tokio::spawn(nav::nav_driver(bot.clone(), state.inner.clone()));
            }
        }
        // Per-tick control + dense capture run as ordered ECS systems (see control.rs), not here,
        // so the recorded (s_t, a_t) lands deterministically around PhysicsSet.
        Event::Packet(packet) => {
            if let ClientboundGamePacket::SetTime(p) = packet.as_ref() {
                *state.inner.last_world_age.lock() = Some(p.game_time as i64);
                state.inner.world_age_client_tick.store(
                    state.inner.client_ticks.load(Ordering::SeqCst),
                    Ordering::SeqCst,
                );
            }
        }
        Event::Disconnect(reason) if !state.inner.finished.load(Ordering::SeqCst) => {
            state
                .inner
                .step_errors
                .lock()
                .push(format!("disconnect: {reason:?}"));
            if !state.inner.spawned.load(Ordering::SeqCst) {
                finish(
                    &state.inner,
                    &shared,
                    "error",
                    Some("server_connection_failed"),
                );
            }
        }
        _ => {}
    }
    Ok(())
}

pub(crate) fn item_name(stack: &ItemStack) -> Option<String> {
    if stack.is_empty() {
        return None;
    }
    let raw = format!("{:?}", stack.kind());
    // azalea-registry Debug gives the variant name (e.g. Glowstone); normalize to the
    // lowercase snake_case registry-ish name Mineflayer records (e.g. glowstone).
    let mut out = String::with_capacity(raw.len() + 4);
    for (i, ch) in raw.chars().enumerate() {
        if ch.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------------------------
// Episode lifecycle.
// ---------------------------------------------------------------------------------------------

pub(crate) async fn run_bot(bot: Client, state: BotState, shared: Shared) {
    let inner = state.inner.clone();
    let cfg = &shared.inner.cfg;
    // Settle in-world before declaring ready (chunk warm-up; mirrors the Mineflayer bot).
    sleep_ms(1500).await;
    // ...and wait until this client is REAL-TIME. A large swarm connects with a packet backlog
    // and azalea skips ticks to catch up; while it does, the latched world age is stale — the
    // second cluster shakedown measured it 123 ticks behind at `go` on an 8-bot swarm, putting
    // every early event before the recording even started. "Real-time" is observable: over a
    // one-second window the latch advances at roughly the tick rate, while a catching-up client
    // bursts far faster. Cap at two minutes so a genuinely broken clock still surfaces later
    // (as an alignment failure) instead of hanging the swarm here.
    for _ in 0..120 {
        let before = inner.world_age_tick();
        sleep_ms(1000).await;
        let after = inner.world_age_tick();
        if let (Some(before), Some(after)) = (before, after) {
            let advanced = after - before;
            if (1..=25).contains(&advanced) {
                break;
            }
        }
    }
    let ready = shared.inner.ready_count.fetch_add(1, Ordering::SeqCst) + 1;
    log(&format!(
        "{} ready ({}/{})",
        inner.username, ready, cfg.bot_count
    ));
    if ready >= cfg.bot_count {
        let name = cfg.shard_signal("bot-ready");
        signal(cfg, &name);
        log(&format!("all bots ready; {name} signalled"));
    }
    if !wait_signal(cfg, "go", cfg.go_timeout_ms, 50).await {
        finish(&inner, &shared, "error", Some("go_timeout"));
        return;
    }
    // Anchor `go` on a FRESH SetTime, not on whatever the latch happens to hold. Under load the
    // client falls behind and skips ticks (azalea logs "GameTick is more than 10 ticks behind"),
    // so at `go` the latched world age can still be the connection-era value — measured 113 ticks
    // stale on an 8-bot swarm, which put every first event before the recording even started.
    // The server broadcasts time every 20 ticks, so waiting for the latch to MOVE bounds the
    // staleness at one broadcast interval; the 5 s cap only fires if time packets stop entirely.
    let stale_age = inner.world_age_tick();
    for _ in 0..100 {
        if inner.world_age_tick() != stale_age {
            break;
        }
        sleep_ms(50).await;
    }
    inner
        .go_tick_base
        .store(inner.client_ticks.load(Ordering::SeqCst), Ordering::SeqCst);
    *inner.t0.lock() = Instant::now();
    if let Some(go_age) = read_go_age(cfg) {
        inner.go_server_age.store(go_age, Ordering::SeqCst);
    }
    *inner.go_world_age.lock() = inner.world_age_tick();
    let look = bot.direction().unwrap_or_default();
    let (yaw, pitch) = (look.y_rot(), look.x_rot());
    *inner.last_yaw.lock() = shortest_arc((yaw as f64).to_radians());
    *inner.last_pitch.lock() = (pitch as f64).to_radians();
    *inner.cam_yaw.lock() = yaw as f64;
    *inner.cam_pitch.lock() = pitch as f64;
    inner.recording.store(true, Ordering::SeqCst); // start dense per-tick capture

    let mut ctx = Ctx {
        bot,
        inner: inner.clone(),
        shared: shared.clone(),
        rng: shared.inner.engine.bot_rng(inner.index),
        // The coordinator wrote the anchor before releasing `go`, so it is ready now.
        anchor: shard_anchor(cfg, read_anchor(cfg)),
        witness_cursor: 0,
        last_witness_ms: 0,
    };
    run_scenario(&mut ctx).await;
    sleep_ms(cfg.inter_round_still_ms).await;
    finish(&inner, &shared, "ok", None);
}

/// Idempotent per-bot completion: freeze status/durations, write artifacts, count done.
/// NB: bots do NOT disconnect here — the process stays online until the coordinator's
/// `stopped` signal (multi-bot) so `replay stop players @a` catches every player.
pub(crate) fn finish(
    inner: &Arc<BotInner>,
    shared: &Shared,
    status: &str,
    error_code: Option<&str>,
) {
    if inner.finished.swap(true, Ordering::SeqCst) {
        return;
    }
    let cfg = &shared.inner.cfg;
    *inner.status.lock() = status.to_string();
    *inner.error_code.lock() = error_code.map(|s| s.to_string());
    let actual = if inner.recording.load(Ordering::SeqCst) {
        inner.elapsed_ms().max(0)
    } else {
        0
    };
    inner
        .episode_actual_duration_ms
        .store(actual, Ordering::SeqCst);
    *inner.duration_met.lock() = if cfg.episode_duration_ms > 0 {
        Some(actual >= cfg.episode_duration_ms)
    } else {
        None
    };
    write_bot_artifacts(inner, shared);
    shared.inner.done_count.fetch_add(1, Ordering::SeqCst);
    log(&format!(
        "{} finished status={} error={:?} ({} events, {} dense frames)",
        inner.username,
        status,
        error_code,
        inner.events.lock().len(),
        inner.frames.lock().len()
    ));
}
