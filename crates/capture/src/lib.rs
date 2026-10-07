//! One capture episode: the Fabric server, the player shards, and the choreography between them.
//!
//! # Process layout
//!
//! Supervision and choreography are ONE process: [`capture_episode`] boots the server, runs the
//! thirteen-step episode inline, and reaps its children. The only child processes are the ones that
//! genuinely must be separate: the JVM, and one `players` process per shard.
//!
//! The shards are separate processes, so the handshake with them goes through a signal directory
//! on the filesystem — see [`signals`] for the protocol.
//!
//! # What the caller supplies
//!
//! [`CaptureConfig`] is the deployment: where the baked server pack is, how much heap the JVM gets,
//! which timeouts apply. [`EpisodeRequest`] is the episode: a manifest [`EpisodeSpec`] and where its
//! artifacts go.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, bail};
use md5::{Digest, Md5};
use pawa_schema::manifest::{EpisodeSpec, WorldSpec};
use pawa_schema::time::{EpochMs, WorldAgeTick};
use serde::{Deserialize, Serialize};

mod coordinator;
mod proc;
mod rcon;
pub mod scenery;
mod server;
mod shards;
pub mod signals;
pub mod warmup;

pub use shards::ShardUsage;

/// Deployment-level capture settings: paths, memory, ports, timeouts.
///
/// Nothing here reads the environment — the caller fills this in. `pawa` builds it from these
/// defaults overlaid by the JSON document `PAWA_CAPTURE_CONFIG` names, in which every field is
/// optional, then by the handful of variables the image bakes (named below where one applies).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CaptureConfig {
    /// Baked server pack tarball. `PAWA_SERVER_PACK`.
    pub server_pack: PathBuf,
    /// Where the pack is unpacked and the world lives. `PAWA_SERVER_WORKDIR`.
    pub server_workdir: PathBuf,
    /// Launcher jar inside the pack.
    pub fabric_launcher: String,
    /// The player runtime binary, one process per shard. `PAWA_PLAYER_BIN`.
    pub player_bin: PathBuf,

    /// JVM initial heap (`-Xms`).
    pub server_xms: String,
    /// JVM maximum heap (`-Xmx`). `SERVER_XMX`. Raised from the launcher's own 4G default before
    /// every episode, so 8G is the value captures actually run on.
    pub server_xmx: String,

    /// Minecraft server port the shards connect to, bound to loopback (`server-ip=127.0.0.1`).
    pub server_port: u16,
    /// RCON port, bound to loopback like the game port, with a per-episode random password.
    pub rcon_port: u16,
    /// Socket timeout for one RCON command, seconds.
    pub rcon_timeout_sec: f64,

    /// `difficulty` in `server.properties`. Scenarios that need hostiles raise it themselves.
    pub difficulty: String,
    pub simulation_distance: u32,
    pub view_distance: u32,

    /// Delay between successive bot joins, milliseconds. Decided here and used on BOTH sides: it
    /// paces shard launches, sizes the connect timeout, and is forwarded to the runtime as the
    /// swarm's own join delay — two sides pacing at different rates is a desync.
    pub stagger_ms: u64,
    /// Slack added to the episode duration before a shard is considered hung, seconds.
    pub episode_timeout_margin_sec: f64,
    /// How long the JVM has to answer RCON.
    pub ready_timeout_sec: f64,
    /// Mean per-player capture rate below which the episode is rejected, Hz.
    /// A server that fell behind produces sparse action rows that no alignment can rescue.
    pub min_capture_tick_hz: f64,

    /// Recording lead-in between `replay start` and `go`, seconds. Every POV therefore opens with
    /// stationary frames, which is what makes the start anchor visible by eye.
    pub pre_roll_sec: f64,
    /// Wait after the last recording stops, seconds — ServerReplay flushes asynchronously and a
    /// truncated `.mcpr` is indistinguishable from a short episode.
    pub flush_sec: f64,
    /// Grace period for a shard to exit on its own after the episode, seconds; then SIGKILL.
    pub shard_grace_sec: f64,
    /// How long `/stop` has to bring the JVM down before its process group is killed, seconds.
    pub server_stop_timeout_sec: f64,

    /// Minimum distance between spread players, blocks — used when the manifest states none.
    pub spread_distance: f64,
    /// Radius of the spread area, blocks — used when the manifest states none.
    pub spread_range: f64,
    /// How far `locate biome` may reach for a dry-land spread centre, blocks. Chunks generate on
    /// demand, so a centre thousands of blocks out is slow AND drifts the episode away from the
    /// seed's characteristic terrain.
    pub max_locate_distance: f64,

    /// Version provenance recorded into the episode. These describe the BAKED pack — changing them
    /// here relabels artifacts, it does not change what runs.
    pub minecraft_version: String,
    pub fabric_loader_version: String,
    pub server_replay_version: String,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        CaptureConfig {
            server_pack: "/opt/pawa/server-pack.tar.gz".into(),
            server_workdir: "/tmp/pawa-server".into(),
            fabric_launcher: "fabric-server-launch.jar".into(),
            player_bin: "/opt/pawa/bin/players".into(),
            server_xms: "1G".into(),
            server_xmx: "8G".into(),
            server_port: 25565,
            rcon_port: 25575,
            rcon_timeout_sec: 30.0,
            difficulty: "peaceful".into(),
            simulation_distance: 8,
            view_distance: 8,
            stagger_ms: 300,
            episode_timeout_margin_sec: 120.0,
            ready_timeout_sec: 300.0,
            min_capture_tick_hz: 18.0,
            pre_roll_sec: 2.0,
            flush_sec: 8.0,
            shard_grace_sec: 20.0,
            server_stop_timeout_sec: 180.0,
            spread_distance: 8.0,
            spread_range: 32.0,
            max_locate_distance: 3000.0,
            minecraft_version: "26.2".into(),
            fabric_loader_version: "0.19.3".into(),
            server_replay_version: "3.5.2+26.2".into(),
        }
    }
}

/// One episode to capture.
#[derive(Clone, Copy, Debug)]
pub struct EpisodeRequest<'a> {
    pub spec: &'a EpisodeSpec,
    pub run_id: &'a str,
    /// The canonical flat episode directory: every artifact this stage writes lands here.
    pub episode_dir: &'a Path,
}

/// World generation as this episode configures it. Written to `world-config.json` for provenance.
///
/// Not a schema type: no other stage reads it today. It is the first candidate if one is ever needed
/// — see the crate's report for the full list of locally-typed diagnostics.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorldConfig {
    pub level_seed: String,
    pub level_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generator_settings: Option<String>,
    pub level_name: String,
    /// Always true today: every episode deletes the previous world so its seed is authoritative.
    pub reset_world: bool,
}

impl WorldConfig {
    /// Read the manifest's world block, rejecting a level name that would escape the workdir.
    pub fn from_spec(spec: &WorldSpec) -> Result<WorldConfig> {
        let level_name = spec.level_name.trim();
        let path = Path::new(level_name);
        if level_name.is_empty()
            || path.is_absolute()
            || path.components().any(|c| c.as_os_str() == "..")
        {
            bail!("unsafe level-name: {level_name:?}");
        }
        Ok(WorldConfig {
            level_seed: spec.seed.clone(),
            level_type: spec.level_type.clone(),
            generator_settings: spec.generator_settings.clone().filter(|s| !s.is_empty()),
            level_name: level_name.to_string(),
            reset_world: true,
        })
    }
}

/// One thing that went wrong. A non-empty list fails the episode.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CaptureFailure {
    /// Stable machine code, e.g. `recorder_not_ready`.
    pub code: String,
    pub message: String,
}

/// Everything one capture episode produced, and everything it is judged on.
#[derive(Clone, Debug, Serialize)]
pub struct CaptureOutcome {
    pub episode_id: String,
    pub run_id: String,
    pub bot_count: u32,
    pub shards: u32,
    /// True when validation passed: every shard exited 0, every POV artifact is present and
    /// non-empty, and the capture rate held up.
    pub ok: bool,
    /// The first thing that failed, as a stable code; `None` when `ok`.
    pub failure_class: Option<String>,
    pub failures: Vec<CaptureFailure>,

    pub world: WorldConfig,
    /// The `(x, z)` written to the `anchor` signal — the origin every scenario plan shifts onto.
    pub anchor: Option<(f64, f64)>,
    /// World age the instant `replay start` returned: recording tick 0, the pixel-free anchor the
    /// align stage joins the replay timeline against.
    pub recording_start_world_age_tick: Option<WorldAgeTick>,
    /// World age the instant `go` was raised — the server-truth anchor the swarm rebases its
    /// clock chain on (the go signal body carries it).
    pub go_world_age_tick: Option<WorldAgeTick>,
    /// World age when the episode ended, before the per-player stops.
    pub recording_stop_world_age_tick: Option<WorldAgeTick>,
    /// World age sampled right after EACH player's recording stopped. `replay stop players @a` is
    /// processed per player, not simultaneously, so one shared stop tick is wrong for everyone the
    /// server got to later.
    pub stop_world_age_tick_by_player: BTreeMap<String, WorldAgeTick>,

    /// Usernames the swarm summary reports as `ok`.
    pub connected_players: Vec<String>,
    /// Expected usernames that did not finish `ok`.
    pub missing_players: Vec<String>,
    /// `pNN` -> username, the canonical POV naming shared with every other stage.
    pub pov_map: BTreeMap<String, String>,
    /// `pNN` -> the `.mcpr` copied into the episode directory.
    pub recordings: BTreeMap<String, PathBuf>,

    pub shard_usage: Vec<ShardUsage>,
    /// The largest peak RSS any shard reached, KiB.
    pub peak_rss_kib: Option<u64>,
    pub server_exit_code: Option<i32>,
    /// Mean and minimum per-player capture rate, Hz, from the merged swarm summary.
    pub capture_tick_hz_mean: Option<f64>,
    pub capture_tick_hz_min: Option<f64>,

    pub started_epoch_ms: EpochMs,
    pub finished_epoch_ms: EpochMs,
    /// The untyped record of what the coordinator did to the world — spread and wet-player fix-ups,
    /// scenery commands and their replies, PvE summons, the server log tail. Written verbatim to
    /// `capture-summary.json`; nothing downstream parses it, it is read by a human after a bad
    /// episode.
    pub diagnostics: serde_json::Value,
}

/// Capture one episode.
///
/// Returns `Err` only for problems that are not about this episode (a missing server pack, an
/// unwritable episode directory). An episode that ran and failed comes back as `Ok` with
/// `ok == false` and a `failure_class` — the caller schedules retries off that, and the artifacts on
/// disk are the evidence.
pub fn capture_episode(
    config: &CaptureConfig,
    request: &EpisodeRequest<'_>,
) -> Result<CaptureOutcome> {
    if !(2..=32).contains(&config.view_distance) {
        anyhow::bail!("capture view_distance must be between 2 and 32");
    }
    let spec = request.spec;
    let shards = spec.resolved_shards();
    let episode_dir = request.episode_dir;
    fs::create_dir_all(episode_dir)?;
    let signal_dir = episode_dir.join(".signals");
    fs::create_dir_all(&signal_dir)?;

    let world = WorldConfig::from_spec(&spec.world)?;
    let usernames = expected_usernames(spec.player_count);
    let episode = Episode {
        config,
        request,
        signal_dir,
        world: world.clone(),
        usernames,
    };

    let mut outcome = CaptureOutcome {
        episode_id: spec.episode_id.clone(),
        run_id: request.run_id.to_string(),
        bot_count: spec.player_count,
        shards,
        ok: false,
        failure_class: None,
        failures: Vec::new(),
        world,
        anchor: None,
        recording_start_world_age_tick: None,
        go_world_age_tick: None,
        recording_stop_world_age_tick: None,
        stop_world_age_tick_by_player: BTreeMap::new(),
        connected_players: Vec::new(),
        missing_players: episode.usernames.clone(),
        pov_map: episode.pov_map(),
        recordings: BTreeMap::new(),
        shard_usage: Vec::new(),
        peak_rss_kib: None,
        server_exit_code: None,
        capture_tick_hz_mean: None,
        capture_tick_hz_min: None,
        started_epoch_ms: EpochMs::now(),
        finished_epoch_ms: EpochMs(0),
        diagnostics: serde_json::Value::Null,
    };

    let result = coordinator::run(&episode, &mut outcome);
    // The signal directory is scratch. Removing it is also what stops a rerun of this episode from
    // reading a previous pass's `go`.
    let _ = fs::remove_dir_all(&episode.signal_dir);
    result?;

    shards::finish(&episode, &mut outcome)?;
    outcome.finished_epoch_ms = EpochMs::now();
    write_json(&episode_dir.join("capture-summary.json"), &outcome)?;
    tracing::info!(
        episode = %outcome.episode_id,
        ok = outcome.ok,
        failure_class = ?outcome.failure_class,
        "capture finished"
    );
    Ok(outcome)
}

/// Everything one episode needs, resolved once.
pub(crate) struct Episode<'a> {
    pub(crate) config: &'a CaptureConfig,
    pub(crate) request: &'a EpisodeRequest<'a>,
    pub(crate) signal_dir: PathBuf,
    pub(crate) world: WorldConfig,
    pub(crate) usernames: Vec<String>,
}

impl Episode<'_> {
    pub(crate) fn spec(&self) -> &EpisodeSpec {
        self.request.spec
    }

    pub(crate) fn episode_dir(&self) -> &Path {
        self.request.episode_dir
    }

    pub(crate) fn shards(&self) -> u32 {
        self.spec().resolved_shards()
    }

    /// `pNN` -> username for this swarm.
    pub(crate) fn pov_map(&self) -> BTreeMap<String, String> {
        let total = self.usernames.len();
        self.usernames
            .iter()
            .enumerate()
            .map(|(index, user)| (pawa_schema::catalog::pov_name(index, total), user.clone()))
            .collect()
    }

    /// How long the shards have to all report ready. Sized by the join pacing, because a large swarm
    /// legitimately takes `bot_count * stagger` just to connect.
    pub(crate) fn connect_timeout(&self) -> Duration {
        let stagger_s = self.config.stagger_ms as f64 / 1000.0;
        Duration::from_secs_f64(f64::max(
            150.0,
            self.spec().player_count as f64 * stagger_s + 120.0,
        ))
    }

    /// How long the episode itself may take before the shards are declared hung.
    pub(crate) fn episode_timeout(&self) -> Duration {
        let duration = self.spec().episode_duration_sec;
        if duration > 0.0 {
            Duration::from_secs_f64(f64::max(
                duration + self.config.episode_timeout_margin_sec,
                1.0,
            ))
        } else {
            Duration::from_secs(150)
        }
    }

    /// Vanilla needs headroom above the swarm for the odd operator join.
    pub(crate) fn max_players(&self) -> u32 {
        u32::max(64, self.spec().player_count + 4)
    }
}

/// The usernames the swarm connects with. A single bot is a swarm of one and uses the same
/// `CapBot<i>` naming as fifty — the runtime always joins this way, regardless of `bot_count`.
pub(crate) fn expected_usernames(bot_count: u32) -> Vec<String> {
    (0..bot_count)
        .map(|index| format!("CapBot{index}"))
        .collect()
}

/// Split `bot_count` bots across `shards` processes: `(bot_index_offset, count)` per shard.
///
/// The first `bot_count % shards` shards carry one extra bot. The offsets are what let each shard
/// name its bots with GLOBAL indices.
pub fn split_bots(bot_count: u32, shards: u32) -> Vec<(u32, u32)> {
    let shards = shards.max(1).min(bot_count.max(1));
    let (base, extra) = (bot_count / shards, bot_count % shards);
    let mut offset = 0;
    (0..shards)
        .map(|index| {
            let count = base + u32::from(index < extra);
            let plan = (offset, count);
            offset += count;
            plan
        })
        .collect()
}

/// A username's Minecraft offline-mode UUID (Java `UUID.nameUUIDFromBytes`, type-3 MD5).
///
/// ServerReplay stores each POV under `recordings/players/<uuid>/`, and an offline server derives
/// the player's UUID deterministically from the name — so this maps a bot's username back to its
/// recording directory without parsing replay metadata.
pub fn offline_uuid(username: &str) -> String {
    let mut digest = Md5::digest(format!("OfflinePlayer:{username}").as_bytes());
    digest[6] = (digest[6] & 0x0f) | 0x30; // version 3
    digest[8] = (digest[8] & 0x3f) | 0x80; // IETF variant
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

pub(crate) fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_string_pretty(value)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The known vector: an offline server puts CapBot0 under this directory, and the golden
    /// corpus's `recordings/players/` tree is named with it.
    #[test]
    fn offline_uuids_match_javas_name_uuid_from_bytes() {
        assert_eq!(
            offline_uuid("CapBot0"),
            "77027fc6-289b-387b-b291-94d40a5b9569"
        );
        assert_eq!(
            offline_uuid("Notch"),
            "b50ad385-829d-3141-a216-7e7d7539ba7f"
        );
        // Version 3 and the IETF variant are pinned by the two nibbles Java rewrites.
        assert_eq!(offline_uuid("CapBot1").as_bytes()[14], b'3');
        assert!(["8", "9", "a", "b"].contains(&&offline_uuid("CapBot1")[19..20]));
    }

    #[test]
    fn bots_split_evenly_with_the_remainder_up_front() {
        assert_eq!(
            split_bots(50, 5),
            [(0, 10), (10, 10), (20, 10), (30, 10), (40, 10)]
        );
        assert_eq!(split_bots(7, 3), [(0, 3), (3, 2), (5, 2)]);
        assert_eq!(split_bots(4, 1), [(0, 4)]);
        // More shards than bots is one shard per bot, never an empty process.
        assert_eq!(split_bots(2, 8), [(0, 1), (1, 1)]);
        // Every bot lands in exactly one shard.
        let plan = split_bots(23, 4);
        assert_eq!(plan.iter().map(|(_, count)| count).sum::<u32>(), 23);
        assert_eq!(plan[0].0, 0);
    }

    #[test]
    fn level_names_may_not_escape_the_workdir() {
        let mut spec = WorldSpec::default();
        assert_eq!(
            WorldConfig::from_spec(&spec).expect("default").level_name,
            "world"
        );
        spec.level_name = "../etc".into();
        assert!(WorldConfig::from_spec(&spec).is_err());
        spec.level_name = "/absolute".into();
        assert!(WorldConfig::from_spec(&spec).is_err());
        // An empty generator-settings string is dropped, not written as an empty property.
        spec.level_name = "world".into();
        spec.generator_settings = Some(String::new());
        assert_eq!(
            WorldConfig::from_spec(&spec)
                .expect("empty")
                .generator_settings,
            None
        );
    }
}
