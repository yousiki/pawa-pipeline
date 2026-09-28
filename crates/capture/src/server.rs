//! The Fabric capture server: materialize the baked pack, boot the JVM, drive it over RCON.
//!
//! Two choices worth knowing:
//!
//! * The pack is unpacked by shelling out to `tar` rather than by linking a tar+gzip pair. The
//!   image that runs a capture has `tar` (it is how the pack got built), and the alternative is two
//!   dependencies and a stream loop to reproduce one command.
//! * The JVM gets no `ONLINE_MODE`/`DIFFICULTY`/`SEED`/... environment: those names are the
//!   `itzg/minecraft-server` image's vocabulary, and this launcher writes `server.properties`
//!   itself. The only launch-time values are the two memory settings, which are
//!   [`CaptureConfig`](crate::CaptureConfig) fields.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use backon::{BlockingRetryable, ExponentialBuilder};
use pawa_schema::time::WorldAgeTick;
use rand::RngExt;

use crate::proc::Child;
use crate::{CaptureConfig, WorldConfig};

/// A booted (or bootable) Fabric server plus the RCON credentials to drive it.
pub(crate) struct FabricServer {
    workdir: PathBuf,
    log_path: PathBuf,
    address: SocketAddr,
    password: String,
    rcon_timeout: Duration,
    child: Option<Child>,
}

impl FabricServer {
    /// Prepare (but do not start) a server in `config.server_workdir`, logging to `log_path`.
    pub(crate) fn new(config: &CaptureConfig, log_path: PathBuf) -> FabricServer {
        FabricServer {
            workdir: config.server_workdir.clone(),
            log_path,
            address: SocketAddr::from(([127, 0, 0, 1], config.rcon_port)),
            // 24 bytes of randomness, hex-encoded: the password lives for one episode and never
            // leaves this host, but it does go into a file, so it is not a fixed string.
            password: random_password(),
            rcon_timeout: Duration::from_secs_f64(config.rcon_timeout_sec),
            child: None,
        }
    }

    /// Restore the fresh-container invariant: an empty server working directory.
    ///
    /// Every episode unpacks the baked pack again, so
    /// anything left here is a previous episode's world, logs, or recordings — and a leftover
    /// `recordings/players/<uuid>/` tree is the one that silently corrupts a capture, because POV
    /// collection picks the newest file under that UUID.
    pub(crate) fn reset_workdir(&self) -> Result<()> {
        if !self.workdir.is_dir() {
            return Ok(());
        }
        // The one guard between "clear the server dir" and "clear whatever a typoed
        // PAWA_SERVER_WORKDIR points at": refuse anything shallower than two components.
        if self.workdir.components().count() < 3 {
            bail!(
                "refusing to clear {:?}: too close to the filesystem root",
                self.workdir
            );
        }
        for entry in fs::read_dir(&self.workdir)? {
            let path = entry?.path();
            if path.is_dir() && !path.is_symlink() {
                let _ = fs::remove_dir_all(&path);
            } else {
                let _ = fs::remove_file(&path);
            }
        }
        Ok(())
    }

    /// Delete the previous world so every episode honors its seed and generator settings.
    ///
    /// Vanilla/Fabric nests `DIM-1`/`DIM1` under the level dir; the `_nether`/`_the_end` siblings
    /// cover Bukkit-style layouts. A missing dimension is fine and never fails the capture.
    pub(crate) fn reset_world(&self, world: &WorldConfig) {
        for name in [
            world.level_name.clone(),
            format!("{}_nether", world.level_name),
            format!("{}_the_end", world.level_name),
        ] {
            let path = self.workdir.join(name);
            if path.exists() {
                let _ = fs::remove_dir_all(&path);
                tracing::info!(path = %path.display(), "reset world: removed pre-existing level");
            }
        }
    }

    /// Unpack the baked pack, write `server.properties` + `eula.txt`, and start the JVM.
    pub(crate) fn start(
        &mut self,
        config: &CaptureConfig,
        world: &WorldConfig,
        max_players: u32,
    ) -> Result<()> {
        fs::create_dir_all(&self.workdir)?;
        if !config.server_pack.is_file() {
            bail!("server pack not found: {}", config.server_pack.display());
        }
        let status = Command::new("tar")
            .arg("-xzf")
            .arg(&config.server_pack)
            .arg("-C")
            .arg(&self.workdir)
            .status()
            .context("running tar to unpack the server pack")?;
        if !status.success() {
            bail!(
                "unpacking {} failed: {status}",
                config.server_pack.display()
            );
        }
        self.start_prepared(config, world, max_players)
    }

    /// Start the JVM over a workdir that is already populated (the warmup path, which downloads
    /// the server files directly instead of unpacking a pack).
    pub(crate) fn start_prepared(
        &mut self,
        config: &CaptureConfig,
        world: &WorldConfig,
        max_players: u32,
    ) -> Result<()> {
        fs::write(
            self.workdir.join("server.properties"),
            server_properties(config, world, max_players, &self.password),
        )?;
        fs::write(self.workdir.join("eula.txt"), "eula=true\n")?;

        let launcher = self.workdir.join(&config.fabric_launcher);
        if !launcher.is_file() {
            bail!("Fabric launcher not found: {}", launcher.display());
        }
        if let Some(parent) = self.log_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let log = File::create(&self.log_path)?;
        let mut command = Command::new("java");
        command
            .arg(format!("-Xms{}", config.server_xms))
            .arg(format!("-Xmx{}", config.server_xmx))
            .arg("-jar")
            .arg(&launcher)
            .arg("nogui")
            .current_dir(&self.workdir)
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log));
        let child = Child::spawn(&mut command).context("spawning the Fabric server")?;
        tracing::info!(pid = child.pid(), "fabric server started");
        self.child = Some(child);
        Ok(())
    }

    /// Poll `list` over RCON until the server answers, or the JVM dies, or `timeout` elapses.
    pub(crate) fn wait_ready(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some(child) = self.child.as_mut()
                && let Some(code) = child.poll()
            {
                tracing::error!(code, "server process exited early");
                return false;
            }
            if !self.rcon("list").is_empty() {
                return true;
            }
            std::thread::sleep(Duration::from_secs(2));
        }
        false
    }

    /// Run one command, returning the reply — or `""` when RCON could not deliver it.
    ///
    /// Swallowing the error is load-bearing: the coordinator issues
    /// hundreds of best-effort commands (gamerules, effects, hotbar stocking) whose failure must
    /// degrade the episode rather than abort it, and every command whose REPLY matters is parsed by
    /// a caller that already treats "no reply" as "not yet".
    pub(crate) fn rcon(&self, command: &str) -> String {
        tracing::debug!(command, "rcon>");
        let run = || crate::rcon::exec(self.address, &self.password, command, self.rcon_timeout);
        match run
            .retry(
                ExponentialBuilder::default()
                    .with_min_delay(Duration::from_millis(200))
                    .with_max_times(3)
                    .with_jitter(),
            )
            // A rejected password never becomes accepted; retrying it only delays the report.
            .when(|error: &anyhow::Error| !error.to_string().contains("authentication failed"))
            .call()
        {
            Ok(reply) => reply,
            Err(error) => {
                tracing::warn!(command, %error, "rcon command failed");
                String::new()
            }
        }
    }

    /// The world age (`/time query gametime`), polled until it parses or `timeout` elapses.
    pub(crate) fn query_world_age(&self, timeout: Duration) -> Option<WorldAgeTick> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(tick) = parse_world_age(&self.rcon("time query gametime")) {
                return Some(tick);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    /// A player's position, polled until it parses or `timeout` elapses.
    pub(crate) fn query_position(&self, username: &str, timeout: Duration) -> Option<[f64; 3]> {
        let deadline = Instant::now() + timeout;
        loop {
            let reply = self.rcon(&format!("data get entity {username} Pos"));
            if let Some(position) = parse_position(&reply) {
                return Some(position);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    /// `/stop`, then wait; SIGKILL the group if the JVM will not go. Returns its exit code.
    pub(crate) fn stop_and_wait(&mut self, timeout: Duration) -> Option<i32> {
        // Already gone — including "we stopped it earlier". Issuing `/stop` at a dead server would
        // only spend the RCON retry budget on a connection that cannot succeed.
        if let Some(code) = self.child.as_mut()?.poll() {
            return Some(code);
        }
        self.rcon("stop");
        let child = self.child.as_mut().expect("checked above");
        let reaped = child.wait(timeout);
        if reaped.exit_code.is_some() {
            return reaped.exit_code;
        }
        tracing::warn!("server did not exit after /stop; killing its process group");
        child.kill_group().exit_code
    }

    /// The tail of the server log, for a failure report.
    pub(crate) fn recent_log(&self, lines: usize) -> String {
        let Ok(text) = fs::read(&self.log_path) else {
            return String::new();
        };
        let text = String::from_utf8_lossy(&text);
        let all: Vec<&str> = text.lines().collect();
        all[all.len().saturating_sub(lines)..].join("\n")
    }

    /// Where ServerReplay writes each POV, keyed by the player's offline UUID.
    pub(crate) fn recordings_dir(&self) -> PathBuf {
        self.workdir.join("recordings").join("players")
    }

    /// The baked ServerReplay config, snapshotted into the episode for provenance.
    pub(crate) fn replay_config(&self) -> serde_json::Value {
        let path = self
            .workdir
            .join("config")
            .join("server-replay")
            .join("config.json");
        fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_else(|| serde_json::json!({}))
    }
}

fn random_password() -> String {
    let bytes: [u8; 24] = rand::rng().random();
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The `server.properties` document, sorted, exactly as `write_server_properties` produced it.
fn server_properties(
    config: &CaptureConfig,
    world: &WorldConfig,
    max_players: u32,
    rcon_password: &str,
) -> String {
    let mut properties: BTreeMap<&str, String> = BTreeMap::from([
        ("allow-flight", "true".into()),
        ("difficulty", config.difficulty.clone()),
        ("enable-command-block", "false".into()),
        ("enable-rcon", "true".into()),
        ("enforce-secure-profile", "false".into()),
        ("level-name", world.level_name.clone()),
        ("level-seed", world.level_seed.clone()),
        ("level-type", world.level_type.clone()),
        ("max-players", max_players.to_string()),
        ("motd", "PAWA".into()),
        ("online-mode", "false".into()),
        ("rcon.password", rcon_password.to_string()),
        ("rcon.port", config.rcon_port.to_string()),
        ("server-port", config.server_port.to_string()),
        (
            "simulation-distance",
            config.simulation_distance.to_string(),
        ),
        ("spawn-protection", "0".into()),
        ("sync-chunk-writes", "true".into()),
        ("view-distance", config.view_distance.to_string()),
    ]);
    if let Some(settings) = world.generator_settings.as_ref().filter(|s| !s.is_empty()) {
        properties.insert("generator-settings", settings.clone());
    }
    properties
        .into_iter()
        .map(|(key, value)| format!("{key}={value}\n"))
        .collect()
}

/// Parse `/time query gametime`. 26.2's clock overhaul renamed the reply from "The time is N" to
/// "The game time is N tick(s)", so both are read by looking for the shared "time is N".
pub(crate) fn parse_world_age(reply: &str) -> Option<WorldAgeTick> {
    let rest = reply.split_once("time is ")?.1;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok().map(WorldAgeTick)
}

/// Parse `data get entity <player> Pos`, whose reply ends `[-275.5d, 64.0d, -675.5d]`.
pub(crate) fn parse_position(reply: &str) -> Option<[f64; 3]> {
    let inside = reply.split_once('[')?.1.split_once(']')?.0;
    let mut values = inside
        .split(',')
        .map(|part| part.trim().trim_end_matches(['d', 'D']).parse::<f64>().ok());
    let position = [values.next()??, values.next()??, values.next()??];
    values.next().is_none().then_some(position)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn world_age_reads_both_the_pre_and_post_26_2_reply() {
        assert_eq!(
            parse_world_age("The time is 12345"),
            Some(WorldAgeTick(12345))
        );
        assert_eq!(
            parse_world_age("The game time is 1010 ticks"),
            Some(WorldAgeTick(1010))
        );
        assert_eq!(parse_world_age("Unknown command"), None);
        assert_eq!(parse_world_age(""), None);
    }

    #[test]
    fn position_reads_the_entity_data_reply() {
        assert_eq!(
            parse_position("CapBot0 has the following entity data: [-275.5d, 64.0d, -675.5d]"),
            Some([-275.5, 64.0, -675.5])
        );
        // A two-value list is not a position, and neither is prose.
        assert_eq!(parse_position("data: [1.0d, 2.0d]"), None);
        assert_eq!(parse_position("No entity was found"), None);
    }

    #[test]
    fn properties_are_sorted_and_carry_the_world() {
        let config = CaptureConfig::default();
        let world = WorldConfig {
            level_seed: "2026".into(),
            level_type: "minecraft:normal".into(),
            generator_settings: None,
            level_name: "world".into(),
            reset_world: true,
        };
        let text = server_properties(&config, &world, 64, "secret");
        assert!(text.starts_with("allow-flight=true\n"));
        assert!(text.contains("\nrcon.password=secret\n"));
        assert!(text.contains("\nlevel-seed=2026\n"));
        // Absent generator settings must not emit an empty key: vanilla reads "" as a flat-world
        // preset in some level types.
        assert!(!text.contains("generator-settings"));
    }
}
