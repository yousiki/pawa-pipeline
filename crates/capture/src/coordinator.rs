//! The thirteen-step episode lifecycle, in order.
//!
//! ```text
//!  1  purge the previous pass's handshake files
//!  2  reset the server working directory and the world; write world-config.json
//!  3  boot the JVM and wait for RCON
//!  4  raise `server-ready`, then launch the player shards (staggered)
//!  5  wait for `bot-ready[-k]` from every shard
//!  6  op the swarm, freeze the scene, stock the hotbars
//!  7  choose a dry-land centre, spread the swarm there, dry out whoever landed in water
//!  8  build the scenery and place every bot on its verified geometry
//!  9  write the `anchor` signal — the origin every scenario plan shifts onto
//! 10  scenario setup: far-region players, PvP resistance, PvE hostiles
//! 11  `replay start`, latch the recording-start world age, pre-roll, raise `go`
//! 12  wait for `done[-k]`, stop each recording with its own world age, raise `stopped`, flush
//! 13  copy each POV's `.mcpr` into the flat episode namespace
//! ```
//!
//! The order is load-bearing. Two constraints explain most of it: every world edit must land BEFORE
//! `replay start`, or it is not in the recording; and `go` must come AFTER the recording is
//! running, or the first seconds of the episode are not captured. The sleeps between steps exist
//! because vanilla applies most of these commands over the following tick, and a command issued
//! into the same tick as its predecessor is silently the loser.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use pawa_schema::catalog;
use serde_json::{Map, Value, json};

use crate::server::FabricServer;
use crate::{
    CaptureFailure, CaptureOutcome, Episode, offline_uuid, scenery, shards, signals, write_json,
};

/// ServerReplay's output extension. `replay_mod` is the only encoding the stage ever produced.
const RECORDING_EXT: &str = ".mcpr";

/// Random seeds routinely centre the world spawn in or near an ocean, and vanilla then drops the
/// whole swarm underwater or underground — the capture videos are bobbing blue frames or black
/// caves. Instead of trusting the world spawn, the spread centre moves to the nearest open-land
/// biome: flat walkable terrain first, then progressively busier but still dry biomes.
const SPAWN_BIOME_PREFERENCE: [&str; 6] = [
    "minecraft:plains",
    "minecraft:sunflower_plains",
    "minecraft:meadow",
    "minecraft:savanna",
    "minecraft:snowy_plains",
    "minecraft:forest",
];

/// Run the episode. Failures that belong to the episode are recorded in `outcome.failures` and
/// return `Ok`; only a broken environment returns `Err`.
pub(crate) fn run(episode: &Episode<'_>, outcome: &mut CaptureOutcome) -> Result<()> {
    let mut server = FabricServer::new(episode.config, episode.episode_dir().join("server.log"));
    let mut shard_set = Vec::new();
    let result = choreograph(episode, outcome, &mut server, &mut shard_set);

    // Reap the shards whatever happened. Their peak RSS is the only record of an OOM, and the kernel
    // holds it only until the child is reaped.
    shards::reap(episode, &mut shard_set, outcome);
    let stop_timeout = Duration::from_secs_f64(episode.config.server_stop_timeout_sec);
    let code = server.stop_and_wait(stop_timeout);
    if outcome.server_exit_code.is_none() {
        outcome.server_exit_code = code;
    }
    result
}

fn choreograph(
    episode: &Episode<'_>,
    outcome: &mut CaptureOutcome,
    server: &mut FabricServer,
    shard_set: &mut Vec<shards::Shard>,
) -> Result<()> {
    let config = episode.config;
    let signal_dir = episode.signal_dir.clone();
    let out_dir = episode.episode_dir().to_path_buf();
    let usernames = episode.usernames.clone();
    let multi = usernames.len() > 1;
    let mut diagnostics = Map::new();

    // ---- 1. A fresh working directory is provisioned per episode, but the signal directory lives
    // beside the artifacts. A stale `go` would release the swarm before recording started.
    signals::purge(&signal_dir, episode.shards());

    // ---- 2. The world is deleted before Fabric starts, so the configured seed and type are
    // authoritative rather than "whatever the last episode generated".
    tracing::info!(
        level_type = %episode.world.level_type,
        seed = %episode.world.level_seed,
        level = %episode.world.level_name,
        "world config"
    );
    server.reset_workdir()?;
    if episode.world.reset_world {
        server.reset_world(&episode.world);
    }
    write_json(&out_dir.join("world-config.json"), &episode.world)?;

    // ---- 3. Boot.
    server.start(config, &episode.world, episode.max_players())?;
    if !server.wait_ready(Duration::from_secs_f64(config.ready_timeout_sec)) {
        fail(
            outcome,
            "recorder_not_ready",
            "server did not report ready in time",
        );
        diagnostics.insert("recent_log_tail".into(), json!(server.recent_log(40)));
        outcome.diagnostics = Value::Object(diagnostics);
        return Ok(());
    }
    sleep(2.0);
    write_json(
        &out_dir.join("serverreplay-config.json"),
        &server.replay_config(),
    )?;

    // ---- 4. Tell the player side the server is up, then launch its processes. The signal goes
    // through the filesystem because its READER is a separate process.
    signals::raise(&signal_dir, "server-ready")?;
    *shard_set = shards::spawn(episode)?;

    // ---- 5. Wait for every shard's bots to be connected.
    let ready_names = signals::shard_signal_names("bot-ready", episode.shards());
    tracing::info!(waiting_for = ?ready_names, "server ready; waiting for the player side");
    let missing = signals::wait_all(&signal_dir, &ready_names, episode.connect_timeout());
    if !missing.is_empty() {
        fail(
            outcome,
            "player_missing",
            format!(
                "bot never reported ready ({} missing); see player logs",
                missing.join(", ")
            ),
        );
        outcome.diagnostics = Value::Object(diagnostics);
        return Ok(());
    }

    // ---- 6. Op every player and FREEZE the scene, so each placed block is the only thing changing
    // during its still phase (clean frame-diff downstream).
    for user in &usernames {
        server.rcon(&format!("op {user}"));
        sleep(0.3);
    }
    for gamerule in [
        "gamerule doDaylightCycle false",
        "gamerule doWeatherCycle false",
        "gamerule doMobSpawning false",
        "gamerule fallDamage false",
        // A death anywhere between kit stocking and episode end would silently empty the hotbar
        // and leave the rest of the episode's held_item stream unlearnable (drowning during a wet
        // spawn fix-up, a PvE hit). Inventory survives every death path instead.
        "gamerule keepInventory true",
        "weather clear",
        "time set day",
    ] {
        server.rcon(gamerule);
        sleep(0.3);
    }
    // Default capture visibility: underground POVs stay usable without particle clutter.
    let night_vision: Vec<Value> = usernames
        .iter()
        .map(|user| {
            let command = format!("effect give {user} minecraft:night_vision 1000000 0 true");
            let reply = server.rcon(&command);
            sleep(0.1);
            json!({"command": command, "reply": reply})
        })
        .collect();
    diagnostics.insert("night_vision".into(), Value::Array(night_vision));
    // Stock every bot's HOTBAR so episodes perform REAL in-game place/dig actions instead of the old
    // dropped `/setblock` chat. `item replace` guarantees the slot; `give` does not fill hotbar-first.
    stock_hotbars(server);

    // ---- 7. Spawn quality.
    let mut relocation = Map::new();
    let mut attempts = Vec::new();
    let located = locate_land_anchor(server, config.max_locate_distance, &mut attempts);
    let (center_x, center_z) = match located {
        Some((biome, x, z, distance)) => {
            tracing::info!(%biome, x, z, distance, "spawn relocation: spreading at located biome");
            relocation.insert("status".into(), json!("located_biome"));
            relocation.insert("biome".into(), json!(biome));
            relocation.insert("distance".into(), json!(distance));
            (round2(x), round2(z))
        }
        None => {
            let spawn = server
                .query_position(&usernames[0], Duration::from_secs(10))
                .unwrap_or([0.0, 0.0, 0.0]);
            tracing::warn!(
                "spawn relocation: no land biome within range; falling back to swarm spawn"
            );
            relocation.insert("status".into(), json!("fallback_world_spawn"));
            (round2(spawn[0]), round2(spawn[2]))
        }
    };
    relocation.insert("center".into(), json!({"x": center_x, "z": center_z}));
    relocation.insert("attempts".into(), Value::Array(attempts));
    let spread = spread_and_fix_water(
        server,
        &usernames,
        center_x,
        center_z,
        episode
            .spec()
            .swarm
            .spread_distance
            .unwrap_or(config.spread_distance),
        episode
            .spec()
            .swarm
            .spread_range
            .unwrap_or(config.spread_range),
    );
    let wet_after: Vec<String> = spread.wet_after.clone();
    relocation.extend(spread.into_diagnostics());
    // A still-wet swarm degrades the capture but must NOT fail the episode — the caller treats
    // failures as fatal, and this is a tuning signal for downstream filtering.
    if !wet_after.is_empty() {
        tracing::warn!(players = ?wet_after, "degenerate spawn: still in water after fix-ups");
    }
    diagnostics.insert(
        "spawn_quality".into(),
        json!({"wet_players": wet_after, "degenerate": !wet_after.is_empty()}),
    );

    // ---- 8. One immutable centre serves the bot signal, the scenery builder and the final layout.
    // The preliminary spread chose a safe surface Y; scenery episodes finish by putting every bot on
    // known support inside the geometry, before replay and `go`.
    let spawn = server
        .query_position(&usernames[0], Duration::from_secs(10))
        .unwrap_or([center_x, 0.0, center_z]);
    let scenery_name = episode.spec().scenario.scenery.clone();
    let builds_scenery = scenery_name != "none";
    let origin = (
        spawn[0].round() as i64,
        spawn[1].round() as i64,
        spawn[2].round() as i64,
    );
    let (anchor_x, anchor_z) = if builds_scenery {
        (origin.0 as f64, origin.2 as f64)
    } else if episode.spec().scenario.name == pawa_schema::regions::SCENARIO {
        // The preliminary vanilla spread is not seeded. Anchor this scenario's grid to the
        // located biome instead of its randomly chosen first player's preliminary position.
        (center_x, center_z)
    } else {
        (round2(spawn[0]), round2(spawn[2]))
    };

    if builds_scenery {
        let seed = scenery::resolve_seed(
            episode
                .spec()
                .scenario
                .seed
                .map(|seed| seed.to_string())
                .as_deref(),
        );
        let commands = scenery::build_commands(&scenery_name, seed, origin);
        if commands.is_empty() {
            tracing::warn!(scenery = %scenery_name, "not a known builder; nothing built");
            diagnostics.insert(
                "scenery".into(),
                json!({"name": scenery_name, "seed": seed, "command_count": 0, "verified": false}),
            );
        } else {
            tracing::info!(scenery = %scenery_name, seed, ?origin, commands = commands.len(), "building scenery");
            let replies: Vec<String> = commands
                .iter()
                .map(|command| {
                    let reply = server.rcon(command);
                    sleep(0.1);
                    reply
                })
                .collect();
            let verified = scenery::replies_ok(&commands, &replies);
            let final_relocation =
                relocate_players_to_scenery(server, &usernames, &scenery_name, seed, origin);
            diagnostics.insert(
                "scenery".into(),
                json!({
                    "name": scenery_name,
                    "seed": seed,
                    "origin": [origin.0, origin.1, origin.2],
                    "command_count": commands.len(),
                    "commands": commands,
                    "replies": replies,
                    "verified": verified,
                    "final_relocation": final_relocation.diagnostics,
                }),
            );
            if !verified {
                fail(
                    outcome,
                    "scenery_build_failed",
                    format!("scenery {scenery_name:?} did not place blocks in-world"),
                );
            }
            if !final_relocation.verified {
                fail(
                    outcome,
                    "scenery_player_relocation_failed",
                    format!("not every player reached scenery {scenery_name:?} geometry"),
                );
            }
        }
    }
    diagnostics.insert("spawn_relocation".into(), Value::Object(relocation));

    // ---- 9. The anchor: origin-relative scenario plans are shifted onto this point, because the
    // bots are wherever vanilla spawned them, not at (0,0).
    std::fs::write(
        signal_dir.join("anchor"),
        format!("{} {}", coordinate(anchor_x), coordinate(anchor_z)),
    )?;
    outcome.anchor = Some((anchor_x, anchor_z));
    tracing::info!(
        x = anchor_x,
        z = anchor_z,
        "scenario anchor and scenery centre"
    );

    // ---- 10. Scenario setup.
    if episode.spec().scenario.name == pawa_schema::regions::SCENARIO {
        let traffic = pawa_schema::regions::Traffic::new(usernames.len());
        let mut placements = Vec::new();
        let mut occupied: Vec<(f64, f64)> = Vec::new();
        let mut verified = true;
        for (index, username) in usernames.iter().enumerate() {
            let address = traffic.homes[index];
            let offset = traffic.point(address);
            let x = anchor_x + offset.0;
            let z = anchor_z + offset.1;
            // Vanilla's highest safe Y can be a treetop. Try small inward offsets and require
            // actual ground support before recording; never repair a bad spawn by teleporting
            // during gameplay. Inward alternatives keep the four starting quadrants separated.
            let local = pawa_schema::regions::SLOTS[address.slot];
            let inward = (-local.0.signum() * 3.0, -local.1.signum() * 3.0);
            let mut attempts = Vec::new();
            let mut placed = false;
            let mut position = None;
            let mut candidates: Vec<_> = [(0.0, 0.0), (inward.0, 0.0), (0.0, inward.1), inward]
                .into_iter()
                .map(|(dx, dz)| (x + dx, z + dz))
                .collect();
            // Larger grids can include forest edges. Search other ground pockets in the SAME
            // cell when one quadrant is entirely canopy; preserve four-block staging separation.
            let center = traffic.center(address.region);
            for dz in [-6.0, -3.0, 0.0, 3.0, 6.0] {
                for dx in [-6.0, -3.0, 0.0, 3.0, 6.0] {
                    let candidate = (anchor_x + center.0 + dx, anchor_z + center.1 + dz);
                    if !candidates.contains(&candidate) {
                        candidates.push(candidate);
                    }
                }
            }
            for (sx, sz) in candidates {
                if occupied
                    .iter()
                    .any(|&(ox, oz)| (sx - ox).hypot(sz - oz) < 4.0)
                {
                    continue;
                }
                // Decimal coordinates avoid Minecraft's implicit +0.5 centering for integers.
                let command = format!("spreadplayers {sx:.3} {sz:.3} 0 1 false {username}");
                let reply = server.rcon(&command);
                sleep(0.1);
                position = server.query_position(username, Duration::from_secs(10));
                let mut supports = Vec::new();
                let mut ground = false;
                for support in [
                    "minecraft:grass_block",
                    "minecraft:dirt",
                    "minecraft:stone",
                    "minecraft:sand",
                    "minecraft:gravel",
                    "minecraft:podzol",
                    "minecraft:coarse_dirt",
                ] {
                    let check = format!(
                        "execute as {username} at @s if block ~ ~-0.2 ~ {support} run data get entity @s Pos"
                    );
                    let response = server.rcon(&check);
                    ground =
                        response.starts_with(&format!("{username} has the following entity data:"));
                    supports.push(json!({"command": check, "reply": response}));
                    if ground {
                        break;
                    }
                }
                placed = ground
                    && position.is_some_and(|p| {
                        (p[0] - sx).hypot(p[2] - sz) <= 2.0
                            && traffic.region_at(p[0] - anchor_x, p[2] - anchor_z)
                                == Some(address.region)
                            && occupied
                                .iter()
                                .all(|&(ox, oz)| (p[0] - ox).hypot(p[2] - oz) >= 4.0)
                    });
                attempts.push(
                    json!({"command": command, "reply": reply, "position": position,
                    "ground": ground, "supports": supports, "verified": placed}),
                );
                if placed {
                    let p = position.expect("verified position");
                    occupied.push((p[0], p[2]));
                    break;
                }
            }
            verified &= placed;
            placements.push(json!({"username": username, "address": address,
                "target": [x, z], "position": position, "attempts": attempts, "verified": placed}));
        }
        diagnostics.insert(
            "regional_staging".into(),
            json!({
                "verified": verified, "side": traffic.side, "pitch": pawa_schema::regions::PITCH,
                "capacity": pawa_schema::regions::CAPACITY, "placements": placements,
            }),
        );
        if !verified {
            fail(
                outcome,
                "regional_staging_failed",
                "A regional player did not reach its assigned surface slot",
            );
        }
    }
    let far_players = episode.spec().scenario.far_players as usize;
    if far_players > 0 && usernames.len() > 1 {
        let take = far_players.min(usernames.len() - 1);
        let far: Vec<String> = usernames[usernames.len() - take..].to_vec();
        diagnostics.insert(
            "far_region".into(),
            relocate_far_players(
                server,
                &far,
                anchor_x,
                anchor_z,
                episode.spec().scenario.far_distance,
            ),
        );
    }
    let scenario = episode.spec().scenario.name.clone();
    if scenario == "pvp" {
        // PvP must keep issuing real melee packets, but a death and respawn relocates players far
        // apart and destroys the rest of the interaction. One selector-wide, particle-hidden
        // resistance prevents death without faking any attack.
        let command = "effect give @a minecraft:resistance 999999 4 true";
        let reply = server.rcon(command);
        diagnostics.insert(
            "pvp_safety".into(),
            json!({"target": "@a", "commands": [command], "replies": [reply]}),
        );
        tracing::info!("pvp: applied long-lived resistance V to all capture players");
    }
    if scenario == "pve" {
        // The bots have no RCON, so the coordinator summons the hostiles they fight. WORLD-relative
        // offsets (`~`), not facing-relative (`^ ^ ^4`): bots commonly face the spawn centre at
        // episode start, so facing-relative summons converge every bot's mobs into one pile.
        server.rcon("difficulty easy");
        sleep(0.2);
        for (mob, dx, dz) in [("zombie", 3, 0), ("skeleton", -3, 0), ("spider", 0, 3)] {
            server.rcon(&format!(
                "execute as @a at @s run summon minecraft:{mob} ~{dx} ~ ~{dz}"
            ));
            sleep(0.2);
        }
        tracing::info!("pve: summoned hostiles around each bot; difficulty easy");
        diagnostics.insert(
            "pve_summon".into(),
            json!({
                "mobs": ["zombie", "skeleton", "spider"],
                "per_bot": 3,
                "difficulty": "easy",
                "placement": "world_offset_spread",
            }),
        );
    }

    // ---- 10.9. Re-issue the hotbar kit now that every setup step that can disturb an inventory
    // (spread fix-ups, scenery relocation, PvE hostiles) has run. `item replace` is idempotent, so
    // for a healthy bot this is a no-op that also resets counts; for one that lost its kit it is
    // the difference between a learnable episode and a hotbar that starts empty. Still BEFORE
    // `replay start`, keeping the "no world edits after recording starts" doctrine intact.
    stock_hotbars(server);

    // ---- 11. `replay start/stop players <target>` takes a vanilla entity selector: `@a` records
    // every online player in ONE command (a space-joined name list is rejected), a single bot uses
    // its explicit name. The world age latched here is recording tick 0 — the pixel-free anchor the
    // align stage joins the replay timeline against.
    //
    // Wait for the server to be REAL-TIME first. `spreadplayers` sends bots into ungenerated
    // terrain and the server catches up for seconds ("Running 4377ms or 87 ticks behind" was
    // measured landing on the same second as `replay start`); a recording started inside that
    // catch-up opens with a starved chunk stream for whichever player's backlog drained last.
    // Real-time is observable the same way the bots observe it: the world age advancing at
    // roughly tick rate over a one-second window.
    for _ in 0..60 {
        let before = server.query_world_age(Duration::from_secs(10));
        sleep(1.0);
        let after = server.query_world_age(Duration::from_secs(10));
        match (before, after) {
            (Some(before), Some(after)) if (1..=25).contains(&(after - before)) => break,
            _ => {}
        }
    }
    let target = if multi { "@a" } else { usernames[0].as_str() };
    server.rcon(&format!("replay start players {target}"));
    let Some(start_tick) = server.query_world_age(Duration::from_secs(60)) else {
        fail(
            outcome,
            "recording_start_gametime_missing",
            "could not read the world age after replay start",
        );
        outcome.diagnostics = Value::Object(diagnostics);
        return Ok(());
    };
    outcome.recording_start_world_age_tick = Some(start_tick);
    diagnostics.insert(
        "start_command".into(),
        json!(format!("replay start players {target}")),
    );
    tracing::info!(start_tick = start_tick.get(), "recording started");
    sleep(config.pre_roll_sec);
    // The go body carries the server's world age of this instant: the swarm anchors its whole
    // clock chain on it (see crates/players signal.rs / state.rs).
    let go_age = server.query_world_age(Duration::from_secs(10));
    signals::raise_with(&signal_dir, "go", go_age.map(|age| age.get()))?;
    outcome.go_world_age_tick = go_age;
    tracing::info!(povs = usernames.len(), %target, go_age = ?go_age, "go signalled");

    // ---- 12. The episode itself, then the per-player stop.
    let done_names = signals::shard_signal_names("done", episode.shards());
    let missing_done = signals::wait_all(&signal_dir, &done_names, episode.episode_timeout());
    if !missing_done.is_empty() {
        fail(
            outcome,
            "progress_timeout",
            format!(
                "bot(s) did not finish the episode in time ({} missing)",
                missing_done.join(", ")
            ),
        );
    }
    outcome.recording_stop_world_age_tick = server.query_world_age(Duration::from_secs(5));
    if multi {
        outcome.stop_world_age_tick_by_player = stop_recordings_per_player(server, &usernames);
    } else {
        server.rcon(&format!("replay stop players {target} true"));
    }
    // Release the swarm to disconnect now that recording has stopped.
    signals::raise(&signal_dir, "stopped")?;
    sleep(config.flush_sec);
    diagnostics.insert("recent_log_tail".into(), json!(server.recent_log(40)));
    outcome.server_exit_code =
        server.stop_and_wait(Duration::from_secs_f64(config.server_stop_timeout_sec));

    // ---- 13. Collect each POV into the flat episode namespace. ServerReplay writes an internal
    // UUID tree under the working directory; the bots' own artifacts already land in `out_dir`.
    let recordings_dir = server.recordings_dir();
    for (pov, username) in episode.pov_map() {
        let Some(source) = find_player_recording(&recordings_dir, &username, RECORDING_EXT) else {
            fail(
                outcome,
                "replaymod_output_missing",
                format!("no *{RECORDING_EXT} for {username} under recordings/players"),
            );
            continue;
        };
        let dest = out_dir.join(format!("{pov}{RECORDING_EXT}"));
        if let Err(error) = std::fs::copy(&source, &dest) {
            // A POV that cannot be copied is a failed episode, not a broken environment: the other
            // POVs are still worth collecting and the failure belongs in the record.
            fail(
                outcome,
                "replaymod_output_missing",
                format!("copying {} failed: {error}", source.display()),
            );
            continue;
        }
        outcome.recordings.insert(pov, dest);
    }
    if outcome.recordings.len() != usernames.len() {
        diagnostics.insert("recordings_tree".into(), json!(tree(&recordings_dir)));
    }
    diagnostics.insert(
        "matrix".into(),
        json!({
            "minecraft": config.minecraft_version,
            "loader": {"family": "fabric", "version": config.fabric_loader_version},
            "server_replay": config.server_replay_version,
        }),
    );
    outcome.diagnostics = Value::Object(diagnostics);
    Ok(())
}

/// Stock every online bot's hotbar with [`catalog::hotbar_kit`], slot by slot.
///
/// The kit definition lives in the catalog because THREE parties must agree on it: `Ctx::equip`
/// searches the hotbar for exactly these names, this function issues the `item replace` commands,
/// and the render driver paints the same kit into the replay HUD (the protocol never syncs a
/// spectated player's container to the replay client). The two Rust sides drifted twice while
/// they were separate lists.
fn stock_hotbars(server: &FabricServer) {
    for (slot, (item, count)) in catalog::hotbar_kit().into_iter().enumerate() {
        server.rcon(&format!(
            "item replace entity @a hotbar.{slot} with minecraft:{item} {count}"
        ));
        sleep(0.1);
    }
}

fn fail(outcome: &mut CaptureOutcome, code: &str, message: impl Into<String>) {
    let message = message.into();
    tracing::error!(code, %message, "capture failure");
    outcome.failures.push(CaptureFailure {
        code: code.to_string(),
        message,
    });
}

fn sleep(seconds: f64) {
    if seconds > 0.0 {
        std::thread::sleep(Duration::from_secs_f64(seconds));
    }
}

/// Two decimals: the precision every recorded coordinate is stored at.
fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

/// A stable Minecraft coordinate without unnecessary trailing zeroes.
fn coordinate(value: f64) -> String {
    let text = format!("{value:.3}");
    if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        text
    }
}

/// Parse a `locate biome` reply into `(x, z, distance)`; `None` when the biome was not found.
///
/// Vanilla answers `The nearest <biome> is at [X, ~, Z] (D blocks away)`. Y is usually the literal
/// `~` (the biome search is columnar) but a real Y can appear, and every coordinate can be negative.
fn parse_locate_reply(reply: &str) -> Option<(f64, f64, f64)> {
    let (_, rest) = reply.split_once("is at [")?;
    let (inside, after) = rest.split_once(']')?;
    let mut parts = inside.split(',');
    let x: f64 = parts.next()?.trim().parse().ok()?;
    let _y = parts.next()?;
    let z: f64 = parts.next()?.trim().parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    let (_, tail) = after.split_once('(')?;
    let digits: String = tail
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let distance: f64 = digits.parse().ok()?;
    tail.contains("block").then_some((x, z, distance))
}

/// Find a dry-land spread centre near the world spawn, as `(biome, x, z, distance)`.
///
/// Walks [`SPAWN_BIOME_PREFERENCE`] and takes the first hit within `max_distance`. `None` when
/// nothing preferred is close enough (a deep-ocean seed) — the caller then falls back to wherever
/// the swarm actually spawned.
fn locate_land_anchor(
    server: &FabricServer,
    max_distance: f64,
    attempts: &mut Vec<Value>,
) -> Option<(String, f64, f64, f64)> {
    for biome in SPAWN_BIOME_PREFERENCE {
        let reply = server.rcon(&format!("locate biome {biome}"));
        attempts.push(json!({"biome": biome, "reply": reply}));
        if let Some((x, z, distance)) = parse_locate_reply(&reply)
            && distance <= max_distance
        {
            return Some((biome.to_string(), x, z, distance));
        }
    }
    None
}

/// The players currently standing in water — the underwater-spawn symptom.
///
/// `execute … if block ~ ~-0.2 ~ minecraft:water` probes just below the player's feet: vanilla
/// replies "Test passed" (sometimes with a count) on a match and "Test failed" otherwise.
fn check_wet_players(server: &FabricServer, usernames: &[String]) -> Vec<String> {
    usernames
        .iter()
        .filter(|user| {
            server
                .rcon(&format!(
                    "execute as {user} at @s if block ~ ~-0.2 ~ minecraft:water"
                ))
                .to_lowercase()
                .contains("passed")
        })
        .cloned()
        .collect()
}

/// `tp` each wet player onto the first dry one; returns the commands issued.
///
/// Entity-target `tp` inherits the dry player's exact position — the one spot we KNOW is on land —
/// so there is no Y to guess (a bare `tp x ~ z` keeps the wet player's current Y at the new column:
/// underground, or still underwater). Empty when nobody is dry; the caller handles that case.
fn teleport_wet_players(
    server: &FabricServer,
    usernames: &[String],
    wet: &[String],
) -> Vec<String> {
    let Some(dry) = usernames.iter().find(|user| !wet.contains(user)) else {
        return Vec::new();
    };
    wet.iter()
        .map(|user| {
            let command = format!("tp {user} {dry}");
            server.rcon(&command);
            command
        })
        .collect()
}

/// What the spread did, for the episode's spawn-relocation record.
struct Spread {
    command: String,
    reply: String,
    retry: Option<Value>,
    wet_before: Vec<String>,
    wet_after: Vec<String>,
    tp_commands: Vec<String>,
}

impl Spread {
    fn into_diagnostics(self) -> Map<String, Value> {
        let mut map = Map::new();
        map.insert("spread_command".into(), json!(self.command));
        map.insert("spread_reply".into(), json!(self.reply));
        if let Some(retry) = self.retry {
            map.insert("spread_retry".into(), retry);
        }
        map.insert("wet_before".into(), json!(self.wet_before));
        map.insert("wet_after".into(), json!(self.wet_after));
        map.insert("tp_commands".into(), json!(self.tp_commands));
        map
    }
}

/// Spread the swarm around `(center_x, center_z)`, then dry out anyone who landed in water.
///
/// `spreadplayers` does the Y work a bare `tp` cannot — it computes a safe SURFACE Y at the target
/// column — which is why the single-bot case goes through it too. But it happily drops players onto
/// shoreline water inside the ring, hence the wet check afterwards: if only SOME are wet, `tp` them
/// onto a dry teammate; if ALL are wet the ring itself sits in water, so the spread is retried once
/// with a doubled range to reach past the shore before giving up.
fn spread_and_fix_water(
    server: &FabricServer,
    usernames: &[String],
    center_x: f64,
    center_z: f64,
    spread_distance: f64,
    spread_range: f64,
) -> Spread {
    let target = if usernames.len() > 1 {
        "@a".to_string()
    } else {
        usernames[0].clone()
    };
    let command = format!(
        "spreadplayers {center_x} {center_z} {spread_distance} {spread_range} false {target}"
    );
    let reply = server.rcon(&command);
    sleep(1.0);
    // Non-fatal: the anchored plan still works if the spread is rejected (players start stacked and
    // disperse as the scenario runs), but it is surfaced rather than swallowed.
    if reply.starts_with("Could not spread") {
        tracing::warn!(%reply, "spreadplayers rejected");
    }

    let wet_before = check_wet_players(server, usernames);
    let mut wet = wet_before.clone();
    let mut retry = None;
    if !wet.is_empty() && wet.len() == usernames.len() {
        let command = format!(
            "spreadplayers {center_x} {center_z} {spread_distance} {} false {target}",
            spread_range * 2.0
        );
        let reply = server.rcon(&command);
        retry = Some(json!({"command": command, "reply": reply}));
        sleep(1.0);
        wet = check_wet_players(server, usernames);
    }
    let mut tp_commands = Vec::new();
    if !wet.is_empty() && wet.len() < usernames.len() {
        tp_commands = teleport_wet_players(server, usernames, &wet);
        sleep(0.5);
        // Re-check rather than assume: the fix-up must actually have dried them.
        wet = check_wet_players(server, usernames);
    }
    Spread {
        command,
        reply,
        retry,
        wet_before,
        wet_after: wet,
        tp_commands,
    }
}

/// Place each far player alone at `(center_x + far_distance, center_z)`.
///
/// One `spreadplayers` per player, because a vanilla `<targets>` is a single selector and a bare
/// `tp x ~ z` would keep the player's current Y at the new column. A still-wet far player is recorded
/// as degenerate and is NEVER teleported onto a teammate: that would undo the separation the
/// scenario depends on.
fn relocate_far_players(
    server: &FabricServer,
    far_usernames: &[String],
    center_x: f64,
    center_z: f64,
    far_distance: f64,
) -> Value {
    let far_x = round2(center_x + far_distance);
    let far_z = round2(center_z);
    let mut players = Vec::new();
    let mut degenerate = false;
    for username in far_usernames {
        let mut commands = vec![format!(
            "spreadplayers {far_x} {far_z} 1 8 false {username}"
        )];
        let mut replies = vec![server.rcon(&commands[0])];
        sleep(1.0);
        let mut wet = !check_wet_players(server, std::slice::from_ref(username)).is_empty();
        if wet {
            commands.push(format!(
                "spreadplayers {far_x} {far_z} 1 24 false {username}"
            ));
            replies.push(server.rcon(&commands[1]));
            sleep(1.0);
            wet = !check_wet_players(server, std::slice::from_ref(username)).is_empty();
        }
        let achieved = server
            .query_position(username, Duration::from_secs(10))
            .map(|position| {
                ((position[0] - center_x).powi(2) + (position[2] - center_z).powi(2)).sqrt()
            });
        degenerate |= wet || achieved.is_none_or(|distance| distance < 0.5 * far_distance);
        tracing::info!(%username, wet, ?achieved, "far-region placement");
        players.push(json!({
            "username": username,
            "commands": commands,
            "replies": replies,
            "wet": wet,
            "achieved_distance": achieved,
        }));
    }
    let report = json!({
        "center": {"x": far_x, "z": far_z},
        "requested_distance": far_distance,
        "players": players,
        "degenerate": degenerate,
    });
    if degenerate {
        tracing::warn!(%report, "far-region placement degenerate");
    }
    report
}

struct SceneryRelocation {
    verified: bool,
    diagnostics: Value,
}

/// Teleport every player onto verified scenery support and confirm the resulting positions.
///
/// Vanilla teleport preserves the exact X/Z but the server may settle the entity a little above the
/// requested support Y on the following tick. Horizontal placement is verified tightly and only a
/// bounded upward Y settle is accepted; falling below, or landing on another level, still fails.
fn relocate_players_to_scenery(
    server: &FabricServer,
    usernames: &[String],
    name: &str,
    seed: u64,
    origin: scenery::Origin,
) -> SceneryRelocation {
    let expected = scenery::player_positions(name, seed, origin, usernames.len());
    if expected.len() != usernames.len() {
        return SceneryRelocation {
            verified: name == "none",
            diagnostics: json!({
                "commands": [],
                "expected_positions": [],
                "verified_players": [],
                "verified": name == "none",
            }),
        };
    }
    let mut commands = Vec::new();
    let mut replies = Vec::new();
    for (username, (x, y, z)) in usernames.iter().zip(&expected) {
        let command = format!(
            "tp {username} {} {} {}",
            coordinate(*x),
            coordinate(*y),
            coordinate(*z)
        );
        replies.push(server.rcon(&command));
        commands.push(command);
    }
    sleep(0.5);

    let actual: Vec<Option<[f64; 3]>> = usernames
        .iter()
        .map(|username| server.query_position(username, Duration::from_secs(2)))
        .collect();
    let verified_players: Vec<&String> = usernames
        .iter()
        .zip(&expected)
        .zip(&actual)
        .filter(|((_, wanted), observed)| {
            observed.is_some_and(|observed| {
                (observed[0] - wanted.0).abs() <= 0.05
                    && (-0.05..=0.25).contains(&(observed[1] - wanted.1))
                    && (observed[2] - wanted.2).abs() <= 0.05
            })
        })
        .map(|((username, _), _)| username)
        .collect();
    let verified = verified_players.len() == usernames.len();
    SceneryRelocation {
        verified,
        diagnostics: json!({
            "commands": commands,
            "replies": replies,
            "expected_positions": expected.iter().map(|p| [p.0, p.1, p.2]).collect::<Vec<_>>(),
            "actual_positions": actual,
            "verified_players": verified_players,
            "verified": verified,
        }),
    }
}

/// Stop each player's recording individually and sample the world age right after.
///
/// `replay stop players @a true` is processed per player, not simultaneously (the same staggering as
/// `replay start players @a`), so a single shared world-age query taken once for the whole batch is
/// wrong for everyone the server got to later. A missed query (an RCON hiccup) just omits that
/// player; the caller falls back to the shared episode-level stop tick.
fn stop_recordings_per_player(
    server: &FabricServer,
    usernames: &[String],
) -> BTreeMap<String, pawa_schema::time::WorldAgeTick> {
    let mut out = BTreeMap::new();
    for user in usernames {
        server.rcon(&format!("replay stop players {user} true"));
        if let Some(tick) = server.query_world_age(Duration::from_secs(10)) {
            out.insert(user.clone(), tick);
        }
    }
    out
}

/// A player's newest ServerReplay output, located by deterministic offline UUID.
fn find_player_recording(recordings: &Path, username: &str, extension: &str) -> Option<PathBuf> {
    let target = offline_uuid(username).replace('-', "").to_lowercase();
    let entry = std::fs::read_dir(recordings)
        .ok()?
        .filter_map(Result::ok)
        .find(|entry| {
            entry.path().is_dir()
                && entry
                    .file_name()
                    .to_string_lossy()
                    .replace('-', "")
                    .to_lowercase()
                    == target
        })?;
    let mut candidates: Vec<(std::time::SystemTime, PathBuf)> = walk(&entry.path())
        .into_iter()
        .filter(|path| path.to_string_lossy().ends_with(extension))
        .filter_map(|path| Some((path.metadata().ok()?.modified().ok()?, path)))
        .collect();
    candidates.sort();
    candidates.pop().map(|(_, path)| path)
}

/// Every file under `root`, recursively.
fn walk(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return found;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            found.extend(walk(&path));
        } else {
            found.push(path);
        }
    }
    found
}

/// The recordings tree, sorted, for a failure report.
fn tree(root: &Path) -> Vec<String> {
    let mut names: Vec<String> = walk(root)
        .iter()
        .filter_map(|path| path.strip_prefix(root).ok())
        .map(|path| path.display().to_string())
        .collect();
    names.sort();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hotbar_is_exactly_the_equip_vocabulary() {
        let kit = catalog::hotbar_kit();
        let names: Vec<&str> = kit.iter().map(|(item, _)| *item).collect();
        assert_eq!(names, catalog::EQUIP_ITEMS);
        // Tools are single items; everything else is a stack.
        assert_eq!(kit.iter().filter(|(_, count)| *count == 1).count(), 2);
        assert!(kit.iter().all(|(_, count)| *count == 1 || *count == 64));
    }

    #[test]
    fn coordinates_lose_their_trailing_zeroes() {
        assert_eq!(coordinate(-675.0), "-675");
        assert_eq!(coordinate(-674.5), "-674.5");
        assert_eq!(coordinate(12.3456), "12.346");
        assert_eq!(coordinate(0.0), "0");
    }

    #[test]
    fn locate_replies_carry_a_column_and_a_distance() {
        assert_eq!(
            parse_locate_reply(
                "The nearest minecraft:plains is at [-275, ~, -675] (312 blocks away)"
            ),
            Some((-275.0, -675.0, 312.0))
        );
        // A concrete Y appears sometimes, and the distance can be fractional.
        assert_eq!(
            parse_locate_reply("is at [8, 64, -16] (20.5 blocks away)"),
            Some((8.0, -16.0, 20.5))
        );
        assert_eq!(
            parse_locate_reply("Could not find a biome of type minecraft:meadow"),
            None
        );
        assert_eq!(parse_locate_reply(""), None);
    }
}
