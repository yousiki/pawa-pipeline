//! The file-based handshake between this process and the player shards.
//!
//! Six names, all of them empty-ish files under `<episode>/.signals/` whose EXISTENCE is the whole
//! message; the body is an epoch-millisecond stamp for post-mortems only. The counterpart is
//! `crates/players/src/signal.rs`, which polls for the same names — so the spelling here is a
//! contract, not a convention:
//!
//! ```text
//! server-ready   we write   the Fabric server answers RCON; shards may connect
//! bot-ready[-k]  we wait    shard k has all its bots connected
//! anchor         we write   "<x> <z>", the world position the scenario plans shift onto
//! go             we write   recording is running; play the episode
//! done[-k]       we wait    shard k finished its episode
//! stopped        we write   recording stopped; the swarm may disconnect
//! ```
//!
//! `-k` suffixes appear only when the swarm is sharded, matching `Cfg::shard_signal` on the player
//! side. The single-shard swarm uses the bare names.

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The names a signal takes across `shards` player processes.
pub(crate) fn shard_signal_names(name: &str, shards: u32) -> Vec<String> {
    if shards > 1 {
        (0..shards).map(|index| format!("{name}-{index}")).collect()
    } else {
        vec![name.to_string()]
    }
}

/// Remove every handshake file a previous pass could have left behind.
///
/// The server working directory is re-provisioned from the baked pack each episode, but the signal
/// directory lives beside the episode's artifacts and is not. A stale `go` would release the swarm
/// before recording starts.
pub(crate) fn purge(signal_dir: &Path, shards: u32) {
    let mut names = vec![
        "server-ready".to_string(),
        "go".to_string(),
        "stopped".to_string(),
        "anchor".to_string(),
    ];
    names.extend(shard_signal_names("bot-ready", shards));
    names.extend(shard_signal_names("done", shards));
    for name in names {
        let _ = fs::remove_file(signal_dir.join(name));
    }
}

/// Raise one signal. The body is the epoch millisecond it was raised at — nobody reads it during a
/// run, but it is what tells a post-mortem how long each phase actually took.
pub(crate) fn raise(signal_dir: &Path, name: &str) -> std::io::Result<()> {
    raise_with(signal_dir, name, None)
}

/// Raise a signal whose body carries an extra token after the epoch stamp. The `go` signal uses
/// this to hand the swarm the SERVER world age of the go instant: the bots' own latched age can
/// run a constant transport delay behind (measured 64 ticks flat across an 8-bot swarm), and only
/// the server knows the truth.
pub(crate) fn raise_with(signal_dir: &Path, name: &str, extra: Option<i64>) -> std::io::Result<()> {
    fs::create_dir_all(signal_dir)?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_millis())
        .unwrap_or(0);
    let body = match extra {
        Some(extra) => format!("{stamp} {extra}"),
        None => stamp.to_string(),
    };
    fs::write(signal_dir.join(name), body)
}

/// Wait for ALL of `names` under ONE shared deadline; returns the ones still missing.
///
/// Shared, not per-name: the shards run concurrently, so N sequential per-name timeouts would give a
/// slow last shard N times the budget the operator configured.
pub(crate) fn wait_all(signal_dir: &Path, names: &[String], timeout: Duration) -> Vec<String> {
    let deadline = Instant::now() + timeout;
    let missing = |names: &[String]| -> Vec<String> {
        names
            .iter()
            .filter(|name| !signal_dir.join(name).exists())
            .cloned()
            .collect()
    };
    let mut pending = missing(names);
    while !pending.is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
        pending = missing(&pending);
    }
    pending
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suffixes_appear_only_when_sharded() {
        assert_eq!(shard_signal_names("done", 1), ["done"]);
        assert_eq!(
            shard_signal_names("bot-ready", 3),
            ["bot-ready-0", "bot-ready-1", "bot-ready-2"]
        );
    }

    #[test]
    fn waiting_reports_exactly_what_never_arrived() {
        let dir = tempfile::tempdir().expect("tempdir");
        raise(dir.path(), "done-0").expect("raise");
        let names = shard_signal_names("done", 2);
        let missing = wait_all(dir.path(), &names, Duration::from_millis(300));
        assert_eq!(missing, ["done-1"]);
        // A raised signal carries a plausible epoch-ms stamp.
        let body = fs::read_to_string(dir.path().join("done-0")).expect("read");
        assert!(body.parse::<i64>().expect("stamp") > 1_700_000_000_000);
    }

    #[test]
    fn purging_clears_every_sharded_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        for name in ["go", "bot-ready-0", "done-1", "anchor", "stopped"] {
            raise(dir.path(), name).expect("raise");
        }
        purge(dir.path(), 2);
        assert!(fs::read_dir(dir.path()).expect("read_dir").next().is_none());
    }
}
