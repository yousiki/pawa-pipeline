use crate::*;

pub(crate) fn log(message: &str) {
    println!("[minecraft-players] {message}");
}

pub(crate) fn r3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

pub(crate) fn r4(v: f64) -> f64 {
    (v * 10000.0).round() / 10000.0
}

pub(crate) fn r_ratio(v: f64) -> f64 {
    (v * 1_000_000.0).round() / 1_000_000.0
}

pub(crate) fn shortest_arc(mut d: f64) -> f64 {
    while d > std::f64::consts::PI {
        d -= std::f64::consts::TAU;
    }
    while d < -std::f64::consts::PI {
        d += std::f64::consts::TAU;
    }
    d
}

pub(crate) fn shortest_arc_deg(mut d: f64) -> f64 {
    while d > 180.0 {
        d -= 360.0;
    }
    while d < -180.0 {
        d += 360.0;
    }
    d
}

pub(crate) async fn sleep_ms(ms: u64) {
    if ms > 0 {
        tokio::time::sleep(Duration::from_millis(ms)).await;
    }
}

pub(crate) fn signal(cfg: &Cfg, name: &str) {
    let _ = fs::create_dir_all(&cfg.signal_dir);
    let _ = fs::write(
        cfg.signal_dir.join(name),
        pawa_schema::time::EpochMs::now().get().to_string(),
    );
}

fn has_signal(cfg: &Cfg, name: &str) -> bool {
    cfg.signal_dir.join(name).exists()
}

/// The server world age carried in the `go` signal's body ("<epoch_ms> <world_age>").
///
/// The coordinator queried it over RCON the instant it raised `go`; it is the TRUTH the swarm
/// rebases its clock chain on. The bots' own latched age extrapolates from SetTime packets whose
/// transport delay is invisible — measured a flat 64 ticks behind across an 8-bot swarm, which
/// shifted every recorded world age (frames and events alike) by 3.2 s.
pub(crate) fn read_go_age(cfg: &Cfg) -> Option<i64> {
    fs::read_to_string(cfg.signal_dir.join("go"))
        .ok()?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// Poll for a signal file; true if it appeared within `timeout_ms`.
pub(crate) async fn wait_signal(cfg: &Cfg, name: &str, timeout_ms: u64, poll_ms: u64) -> bool {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    while !has_signal(cfg, name) {
        if Instant::now() > deadline {
            return has_signal(cfg, name);
        }
        sleep_ms(poll_ms).await;
    }
    true
}

/// The world anchor written by the server coordinator to `<signal>/anchor` ("x z"): the swarm's
/// spawn point. The seeded scenario plan is origin-relative (0,0-centred), but vanilla spawns the
/// bots at the world spawn — often hundreds of blocks away, in chunks that (0,0) never loads — so
/// every origin goal is unreachable. Shifting the plan onto this anchor puts the goals where the
/// bots actually are (generated terrain). Defaults to (0,0) if absent (older/single-file coordinator).
pub(crate) fn read_anchor(cfg: &Cfg) -> (f64, f64) {
    fs::read_to_string(cfg.signal_dir.join("anchor"))
        .ok()
        .and_then(|s| parse_anchor(&s))
        .unwrap_or((0.0, 0.0))
}

/// Parse the coordinator's `"<x> <z>"` anchor line into world coordinates.
pub(crate) fn parse_anchor(s: &str) -> Option<(f64, f64)> {
    let mut it = s.split_whitespace();
    Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
}

/// Shift an origin-relative plan coordinate onto the world anchor.
pub(crate) fn shift(p: (f64, f64), anchor: (f64, f64)) -> (f64, f64) {
    (p.0 + anchor.0, p.1 + anchor.1)
}

/// Offset the coordinator's anchor onto this shard's slot of a ring so P shards run their
/// origin-relative plans in separate patches instead of piling onto one spawn. The ring formula
/// (radius `shard_spread`, angle 2πk/P) is mirrored by the server coordinator's scenery placement.
pub(crate) fn shard_anchor(cfg: &Cfg, base: (f64, f64)) -> (f64, f64) {
    if cfg.shard_count <= 1 || cfg.shard_spread <= 0.0 {
        return base;
    }
    let angle = std::f64::consts::TAU * (cfg.shard_index as f64) / (cfg.shard_count as f64);
    (
        base.0 + cfg.shard_spread * angle.cos(),
        base.1 + cfg.shard_spread * angle.sin(),
    )
}

/// Mix the shard index into the scenario seed so shards don't run identical choreography.
/// Single-shard swarms keep the raw seed (bit-exact with today's plans).
pub(crate) fn shard_seed(seed: u32, shard_index: usize, shard_count: usize) -> u32 {
    if shard_count <= 1 {
        return seed;
    }
    // splitmix32 finalizer over the shard index.
    let mut z = (shard_index as u32).wrapping_add(0x9e37_79b9);
    z = (z ^ (z >> 16)).wrapping_mul(0x85eb_ca6b);
    z = (z ^ (z >> 13)).wrapping_mul(0xc2b2_ae35);
    seed ^ (z ^ (z >> 16))
}
