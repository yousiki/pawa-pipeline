//! Isolation lanes for concurrent ReplayMod renders inside one container.
//!
//! A render is not a pure function of its inputs. It drives a real X display and it mutates a
//! HeadlessMC game directory in place: ReplayMod's `.replay_cache`, its `replay_recordings/` folder
//! and the staged `render-job-0.mcpr` all live there and are all keyed by PATH. Two renders sharing
//! either resource corrupt each other — the second Xvfb is refused the display, and one render's
//! cache entry answers the other render's entity lookup.
//!
//! Everything a lane owns is DERIVED from a single integer index, so no configuration can put two
//! lanes on one display:
//!
//! ```text
//! render displays  :BASE + 0        ..  :BASE + N-1
//! probe displays   :BASE + N        ..  :BASE + 2N-1
//! ```
//!
//! Which index a render gets is decided by the scheduler, not by this process. HyperQueue grants an
//! exclusive `render_slot` resource and names the granted index in [`LANE_INDEX_ENV`]; holding it is
//! the scheduler's guarantee, so there is no lock here. The `flock`-per-lane arrangement this
//! replaces existed to keep two Ray tasks off one display, and it could only ever fail closed AFTER
//! the scheduler had already overcommitted the node.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;

/// The exclusive lane index HyperQueue granted this task, as a decimal string. Absent in a
/// single-lane local run, which is index 0.
pub const LANE_INDEX_ENV: &str = "HQ_RESOURCE_VALUES_renderslot";

/// How many lanes this node serves. Sets the probe-display offset and each lane's CPU share.
pub const LANE_COUNT_ENV: &str = "RENDER_SLOTS";

/// Base X display, e.g. `":99"`. Lane 0 renders on it.
pub const DISPLAY_BASE_ENV: &str = "RENDER_DISPLAY";

/// GPU rasterization: the GL processes run under VirtualGL's EGL back end (`vglrun -d eglN`)
/// instead of llvmpipe. Unset (the default) auto-detects: GPU when the node exposes NVIDIA
/// devices, software otherwise. `1` requires the GPU, `0` forces software, `eglN` pins a device.
pub const GPU_ENV: &str = "RENDER_GPU";

/// How many GPUs this node exposes; lane `i` renders on EGL device `i % count`. Default: the
/// number of `/dev/nvidiaN` devices, or 1.
pub const GPU_COUNT_ENV: &str = "RENDER_GPUS";

/// Writable root each lane's HeadlessMC state directory is created under. Defaults to the baked
/// game directory, which is what a single-lane developer run wants.
pub const STATE_ROOT_ENV: &str = "RENDER_STATE_DIR";

/// The image's baked game directory: mods, versions and assets, static after the warmup build step.
/// Renders never write here — each lane symlinks it into its own state directory.
pub const GAME_DIR_ENV: &str = "GAME_DIR";

const DEFAULT_GAME_DIR: &str = "/work/gamedir";
const DEFAULT_DISPLAY_BASE: u32 = 99;

/// Minecraft stores `fov` in `options.txt` as a normalised SLIDER value, not as degrees:
/// `degrees = 70 + 40 * value` over `value` in `[-1, 1]`, i.e. 30° to 110°. `0.0` is the vanilla
/// default of 70°, which is what every render on this branch was taken at — pinning to it changes
/// no existing geometry, it only stops the value from being able to change without saying so.
pub const PINNED_FOV_OPTION: f64 = 0.0;

/// `ChatVisiblity.HIDDEN`, spelled the way `options.txt` carries it: an ordinal, because the option's
/// codec is `Codec.INT` over the enum's ids (`FULL` 0, `SYSTEM` 1, `HIDDEN` 2). The value is JSON-
/// parsed and then decoded by that codec, so the enum's *name* is not an accepted spelling —
/// `chatVisibility:"hidden"` decodes to nothing, Minecraft logs one `Error parsing option value`
/// line, keeps the `FULL` default, and rewrites the file with `chatVisibility:0`. That silently
/// shipped a cloud run with pipeline chatter burned into every frame; a read-back check cannot catch
/// it, because the file would read back exactly what we wrote.
const CHAT_VISIBILITY_HIDDEN: u8 = 2;

const GAME_OPTIONS_FILENAME: &str = "options.txt";

/// Slider value to (vertical) FOV in degrees, per Minecraft's own mapping.
pub fn fov_degrees(option_value: f64) -> f64 {
    70.0 + 40.0 * option_value
}

/// One render lane. Values are snapshotted at construction so a later change to the environment
/// cannot move a render half-way through.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Lane {
    pub index: u32,
    /// X display the client renders on, e.g. `":99"`.
    pub display: String,
    /// X display the GL probe uses, past every lane's render display so a probe never disturbs a
    /// render in flight.
    pub probe_display: String,
    /// This lane's HeadlessMC game directory — `-Dhmc.gamedir`, and everything ReplayMod mutates.
    pub state_dir: PathBuf,
    /// Rasterizer threads this lane may use, i.e. its equal share of the node's CPUs. llvmpipe
    /// sizes its pool from the CPUs it can SEE, not from what the scheduler reserved, so without
    /// this every lane would start a full-node thread pool and spend the gain on context switching.
    pub gl_threads: u32,
    /// VirtualGL EGL device this lane rasterizes on (e.g. `"egl0"`), or `None` for llvmpipe.
    pub gpu_device: Option<String>,
}

impl Lane {
    /// Read the lane the scheduler granted. See the module docs for the variables involved.
    pub fn from_env() -> Result<Lane> {
        let (total, base) = topology_from_env()?;
        let index = lane_index_from_env(total)?;
        let cpus = std::thread::available_parallelism().map_or(1, |n| n.get() as u32);
        let mut lane = Lane::derive(index, total, base, &state_root(), cpus)?;
        lane.gpu_device = gpu_device_from_env(index)?;
        Ok(lane)
    }

    /// Derive a lane from explicit values. `from_env` is this function plus the environment; tests
    /// and any caller that already knows its index use it directly.
    pub fn derive(
        index: u32,
        total: u32,
        display_base: u32,
        state_root: &Path,
        cpus: u32,
    ) -> Result<Lane> {
        if total == 0 {
            bail!("render lane count must be positive");
        }
        if index >= total {
            bail!("render lane index must be in 0..{total}; got {index}");
        }
        let display = display_base
            .checked_add(index)
            .context("render display number overflow")?;
        let probe_display = display_base
            .checked_add(total)
            .and_then(|value| value.checked_add(index))
            .context("render probe display number overflow")?;
        Ok(Lane {
            index,
            display: format!(":{display}"),
            probe_display: format!(":{probe_display}"),
            state_dir: state_root.join(format!("slot-{index}")),
            gl_threads: (cpus / total).max(1),
            gpu_device: None,
        })
    }

    /// ReplayMod's own replay folder for this lane. It refuses to open the replay screen with an
    /// empty one, so the staging step drops a copy here.
    pub fn replay_dir(&self) -> PathBuf {
        self.state_dir.join("replay_recordings")
    }

    /// Make this lane's state directory a usable, isolated HeadlessMC gamedir and pin the field of
    /// view the client will load.
    ///
    /// Idempotent, and cheap enough to call before every render in a long-lived worker — which is
    /// what [`render`](crate::render) does, so the FOV can never be left to whatever the last
    /// render's exit wrote.
    pub fn prepare(&self, render_distance_chunks: Option<u32>) -> Result<PinnedOptions> {
        let base = game_dir();
        ensure_render_state_dir(&base, &self.state_dir)
            .with_context(|| format!("preparing lane state dir {}", self.state_dir.display()))?;

        // ReplayMod persists its entity-position cache under `<gamedir>/.replay_cache/<escaped
        // replay path>`, keyed purely on the replay's PATH, and capture-produced .mcpr files all
        // carry the literal placeholder "invalid" in recording.tmcpr.crc32 — so ZipReplayFile's
        // cache-freshness check can never invalidate a stale entry. The staged name is REUSED by
        // every render in this lane, so without this purge episode N+1's camera attach reads
        // episode N's entity positions: `camera_attach_failed` when the POV's entity id is absent
        // from the stale set, and silently wrong camera poses when it happens to be present.
        let _ = std::fs::remove_dir_all(self.state_dir.join(".replay_cache"));

        // Must run AFTER ensure_render_state_dir, which is what can put a shared symlink at that
        // path for pin_game_options to detach.
        pin_game_options(&self.state_dir, PINNED_FOV_OPTION, render_distance_chunks)
    }
}

/// What the game will actually load, read back from the file it will load it from.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PinnedOptions {
    /// The slider value as re-read from `options.txt`.
    pub fov_option: f64,
    /// `70 + 40 * fov_option`, Minecraft's slider mapping. DERIVED from the option value, not
    /// measured from pixels — a lens check against rendered geometry is a separate instrument and
    /// has not been run.
    pub fov_degrees: f64,
    /// The value the file held before, when it held one. Diagnostic: a non-zero prior means some
    /// earlier render in this lane ran at intrinsics nobody declared.
    pub prior_fov_option: Option<f64>,
    /// Actual chunk radius re-read from the isolated renderer options.
    pub render_distance_chunks: Option<u32>,
    /// Whether the path was a symlink into the shared baked gamedir before being replaced.
    pub was_symlink: bool,
    pub path: PathBuf,
}

fn game_dir() -> PathBuf {
    PathBuf::from(std::env::var(GAME_DIR_ENV).unwrap_or_else(|_| DEFAULT_GAME_DIR.into()))
}

fn state_root() -> PathBuf {
    std::env::var(STATE_ROOT_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|_| game_dir())
}

fn parse_positive(name: &str, raw: &str) -> Result<u32> {
    raw.parse::<u32>()
        .ok()
        .filter(|value| *value > 0)
        .with_context(|| format!("{name} must be a positive integer; got {raw:?}"))
}

fn topology_from_env() -> Result<(u32, u32)> {
    let total = match env_value(LANE_COUNT_ENV) {
        Some(raw) => parse_positive(LANE_COUNT_ENV, &raw)?,
        None => 1,
    };
    let base = match env_value(DISPLAY_BASE_ENV) {
        Some(raw) => raw
            .strip_prefix(':')
            .unwrap_or(&raw)
            .parse::<u32>()
            .with_context(|| {
                format!("{DISPLAY_BASE_ENV} is not a nonnegative display like ':99'; got {raw:?}")
            })?,
        None => DEFAULT_DISPLAY_BASE,
    };
    base.checked_add(total)
        .and_then(|value| value.checked_add(total))
        .context("render display range overflow")?;
    Ok((total, base))
}

fn lane_index_from_env(total: u32) -> Result<u32> {
    match std::env::var(LANE_INDEX_ENV) {
        Ok(raw) => {
            if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
                bail!("{LANE_INDEX_ENV} must be exactly one decimal lane index; got {raw:?}");
            }
            let index = raw
                .parse::<u32>()
                .with_context(|| format!("{LANE_INDEX_ENV} is not a lane index: {raw:?}"))?;
            if index >= total {
                bail!("render lane index must be in 0..{total}; got {index}");
            }
            Ok(index)
        }
        Err(std::env::VarError::NotPresent) if total == 1 => Ok(0),
        Err(std::env::VarError::NotPresent) => {
            bail!("{LANE_INDEX_ENV} is required when {LANE_COUNT_ENV}={total}")
        }
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn doctor_display_from_env() -> Result<(String, Option<String>)> {
    let (total, base) = topology_from_env()?;
    let display = base
        .checked_add(total)
        .and_then(|value| value.checked_add(total))
        .context("doctor display number overflow")?;
    Ok((format!(":{display}"), gpu_device_from_env(0)?))
}

/// `/dev/nvidia0`, `/dev/nvidia1`, ... — present on the host and inside a container given GPUs.
fn detected_gpus() -> u32 {
    std::fs::read_dir("/dev")
        .map(|dir| {
            dir.flatten()
                .filter(|e| {
                    let name = e.file_name();
                    let name = name.to_string_lossy();
                    name.strip_prefix("nvidia")
                        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
                })
                .count() as u32
        })
        .unwrap_or(0)
}

/// The EGL device lane `index` renders on, or `None` for software rendering. See [`GPU_ENV`].
fn gpu_device_from_env(index: u32) -> Result<Option<String>> {
    let mode = env_value(GPU_ENV);
    if mode
        .as_deref()
        .is_some_and(|v| v == "0" || v.eq_ignore_ascii_case("false"))
    {
        return Ok(None);
    }
    let count = match env_value(GPU_COUNT_ENV) {
        Some(raw) => Some(parse_positive(GPU_COUNT_ENV, &raw)?),
        None => None,
    };
    Ok(gpu_device(
        env_value(GPU_ENV),
        count,
        detected_gpus(),
        index,
    ))
}

fn gpu_device(
    mode: Option<String>,
    count: Option<u32>,
    detected: u32,
    index: u32,
) -> Option<String> {
    let spread = |n: u32| Some(format!("egl{}", index % count.unwrap_or(n.max(1))));
    match mode {
        Some(v) if v == "0" || v.eq_ignore_ascii_case("false") => None,
        Some(v) if v.starts_with("egl") => Some(v),
        Some(_) => spread(detected),
        None if detected > 0 => spread(detected),
        None => {
            tracing::warn!("no GPU detected; rendering with software GL (llvmpipe), which is slow");
            None
        }
    }
}

/// The trimmed value of `name`, or `None` when it is unset or blank — an empty variable is how a
/// shell passes "not set", and parsing it as a number would fail the run for no reason.
fn env_value(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(raw) if !raw.trim().is_empty() => Some(raw.trim().to_string()),
        _ => None,
    }
}

/// Make `state_dir` a usable HeadlessMC gamedir for one render lane.
///
/// `-Dhmc.gamedir` understands exactly ONE directory tree: mods, versions and assets AND ReplayMod's
/// mutable `.replay_cache` / `replay_recordings` all live under it. Passing the shared `base` to
/// every lane would make them race on the same mutable state; giving each lane a fully separate tree
/// would lose the image's baked-in multi-GB static content. So every entry ALREADY in `base` is
/// symlinked into `state_dir`, while anything the lane creates later lands for real, only in
/// `state_dir`. A no-op when the two are the same directory, and idempotent.
fn ensure_render_state_dir(base: &Path, state_dir: &Path) -> Result<()> {
    if let (Ok(state), Ok(base)) = (state_dir.canonicalize(), base.canonicalize())
        && state == base
    {
        return Ok(());
    }
    std::fs::create_dir_all(state_dir)?;
    let resolved_state = state_dir.canonicalize()?;
    for entry in std::fs::read_dir(base)? {
        let entry = entry?;
        // `state_dir` may itself live under `base` — it does whenever RENDER_STATE_DIR is unset —
        // and linking a directory into a descendant of itself makes a cycle that any recursive walk
        // falls into.
        if let Ok(target) = entry.path().canonicalize()
            && resolved_state.starts_with(&target)
        {
            continue;
        }
        let link = state_dir.join(entry.file_name());
        // `symlink_metadata` and not `exists`, so a broken symlink counts as present rather than
        // being replaced on every call.
        if link.symlink_metadata().is_ok() {
            continue;
        }
        std::os::unix::fs::symlink(entry.path(), &link)?;
    }
    Ok(())
}

/// Pin this lane's field of view and report what the game will actually load.
///
/// Three things here are deliberate, and each one is a failure this codebase has already paid for
/// somewhere else.
///
/// **Other keys are preserved, not overwritten.** Writing a file containing only `fov` would drop
/// render distance, graphics mode, particles and every other persisted option back to its default.
/// That silently changes what is IN the picture while claiming to pin only how wide the picture is.
///
/// **A symlink is replaced, never written through.** `ensure_render_state_dir` links every entry of
/// the shared baked gamedir into the lane, so an `options.txt` shipped in the image is a symlink
/// into content every OTHER lane also links to. Opening it for writing would pin one lane by
/// mutating all of them.
///
/// **The returned value is RE-READ from the emitted file**, not echoed from the argument. What
/// belongs in a manifest is the value the game will load, and the only thing that knows that is the
/// file on disk. A mismatch is an error rather than a record, because a manifest field asserting a
/// FOV the file does not carry is worse than no field at all.
fn pin_game_options(
    state_dir: &Path,
    fov_option: f64,
    render_distance_chunks: Option<u32>,
) -> Result<PinnedOptions> {
    if render_distance_chunks.is_some_and(|n| !(2..=32).contains(&n)) {
        bail!("render distance must be between 2 and 32 chunks");
    }
    let path = state_dir.join(GAME_OPTIONS_FILENAME);
    let was_symlink = path
        .symlink_metadata()
        .is_ok_and(|meta| meta.file_type().is_symlink());

    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut prior = None;
    let mut lines: Vec<String> = Vec::new();
    for line in existing.lines() {
        if let Some((key, value)) = line.split_once(':') {
            if key.trim() == "fov" {
                prior = value.trim().parse::<f64>().ok();
                continue;
            }
            // Chat is pinned hidden alongside fov: with the HUD now rendered into flat frames,
            // pipeline chatter (rcon confirmations, "Starting replay for ...") would otherwise
            // burn into the first seconds of every video. So it is hidden.
            if matches!(key.trim(), "renderDistance" | "graphicsPreset")
                && render_distance_chunks.is_some()
            {
                continue;
            }
            if key.trim() == "chatVisibility" {
                continue;
            }
        }
        lines.push(line.to_string());
    }
    if let Some(distance) = render_distance_chunks {
        // Minecraft 26.2 applies its named graphics preset after loading options.
        // Fancy silently replaces a requested radius with 16 unless the preset is custom.
        lines.push("graphicsPreset:\"custom\"".to_string());
        lines.push(format!("renderDistance:{distance}"));
    }
    lines.push(format!("fov:{fov_option:?}"));
    lines.push(format!("chatVisibility:{CHAT_VISIBILITY_HIDDEN}"));

    // Unlink first: on a symlink this detaches the lane from the shared file before any write
    // touches it, and on a real file it replaces rather than truncates.
    if path.symlink_metadata().is_ok() {
        std::fs::remove_file(&path)?;
    }
    std::fs::write(&path, lines.join("\n") + "\n")
        .with_context(|| format!("pinning fov in {}", path.display()))?;

    let resolved = std::fs::read_to_string(&path)?
        .lines()
        .filter_map(|line| line.split_once(':'))
        .filter(|(key, _)| key.trim() == "fov")
        .filter_map(|(_, value)| value.trim().parse::<f64>().ok())
        .next_back();
    if resolved != Some(fov_option) {
        bail!(
            "pinned fov:{fov_option} to {} but reading it back gave {resolved:?}; refusing to \
             render with intrinsics the manifest cannot state",
            path.display()
        );
    }

    let resolved_distance = std::fs::read_to_string(&path)?
        .lines()
        .filter_map(|line| line.split_once(':'))
        .filter(|(key, _)| key.trim() == "renderDistance")
        .filter_map(|(_, value)| value.trim().parse::<u32>().ok())
        .next_back();
    if render_distance_chunks.is_some() && resolved_distance != render_distance_chunks {
        bail!("pinned render distance did not survive options readback");
    }
    Ok(PinnedOptions {
        fov_option,
        fov_degrees: fov_degrees(fov_option),
        prior_fov_option: prior,
        render_distance_chunks: resolved_distance,
        was_symlink,
        path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpu_mode_auto_detects_and_honours_overrides() {
        let s = |v: &str| Some(v.to_string());
        assert_eq!(gpu_device(None, None, 0, 3), None);
        assert_eq!(gpu_device(None, None, 2, 3), s("egl1"));
        assert_eq!(gpu_device(None, Some(1), 2, 3), s("egl0"));
        assert_eq!(gpu_device(s("0"), None, 8, 0), None);
        assert_eq!(gpu_device(s("1"), None, 0, 5), s("egl0"));
        assert_eq!(gpu_device(s("egl2"), None, 8, 5), s("egl2"));
    }
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());
    const TEST_ENV: [&str; 6] = [
        LANE_COUNT_ENV,
        LANE_INDEX_ENV,
        DISPLAY_BASE_ENV,
        GPU_ENV,
        GPU_COUNT_ENV,
        STATE_ROOT_ENV,
    ];

    struct EnvRestore(Vec<(&'static str, Option<std::ffi::OsString>)>);

    impl EnvRestore {
        fn clear() -> Self {
            let old = TEST_ENV
                .into_iter()
                .map(|name| (name, std::env::var_os(name)))
                .collect();
            for name in TEST_ENV {
                unsafe { std::env::remove_var(name) };
            }
            Self(old)
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            for (name, value) in self.0.drain(..) {
                match value {
                    Some(value) => unsafe { std::env::set_var(name, value) },
                    None => unsafe { std::env::remove_var(name) },
                }
            }
        }
    }

    fn set_env(name: &str, value: &str) {
        unsafe { std::env::set_var(name, value) };
    }

    #[test]
    fn common_lane_counts_have_unique_state_and_displays() {
        let root = Path::new("/scratch");
        for total in [1, 2, 3, 8] {
            let lanes: Vec<Lane> = (0..total)
                .map(|i| Lane::derive(i, total, 99, root, 32).unwrap())
                .collect();
            let displays: std::collections::BTreeSet<&str> = lanes
                .iter()
                .flat_map(|lane| [lane.display.as_str(), lane.probe_display.as_str()])
                .collect();
            let state_dirs: std::collections::BTreeSet<&Path> =
                lanes.iter().map(|lane| lane.state_dir.as_path()).collect();
            assert_eq!(displays.len(), (2 * total) as usize);
            assert_eq!(state_dirs.len(), total as usize);
            assert_eq!(lanes[0].display, ":99");
            assert_eq!(lanes[0].probe_display, format!(":{}", 99 + total));
            assert_eq!(
                lanes[total as usize - 1].probe_display,
                format!(":{}", 98 + 2 * total)
            );
        }
    }

    #[test]
    fn a_single_lane_keeps_the_whole_node() {
        let lane = Lane::derive(0, 1, 99, Path::new("/scratch"), 32).unwrap();
        assert_eq!(lane.gl_threads, 32);
        assert_eq!(lane.probe_display, ":100");
        // More lanes than CPUs still leaves each one a usable rasterizer.
        assert_eq!(
            Lane::derive(5, 8, 99, Path::new("/s"), 4)
                .unwrap()
                .gl_threads,
            1
        );
    }

    #[test]
    fn bad_lane_environment_fails_closed() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
        let _restore = EnvRestore::clear();

        assert_eq!(Lane::from_env().unwrap().index, 0);
        for bad in ["0", "-1", "two"] {
            set_env(LANE_COUNT_ENV, bad);
            assert!(Lane::from_env().is_err(), "accepted {LANE_COUNT_ENV}={bad}");
        }
        set_env(LANE_COUNT_ENV, "2");
        for bad in ["", "0,1", " 1 ", "2", "-1", "one"] {
            if bad.is_empty() {
                unsafe { std::env::remove_var(LANE_INDEX_ENV) };
            } else {
                set_env(LANE_INDEX_ENV, bad);
            }
            assert!(
                Lane::from_env().is_err(),
                "accepted {LANE_INDEX_ENV}={bad:?}"
            );
        }
        set_env(LANE_INDEX_ENV, "1");
        assert_eq!(Lane::from_env().unwrap().index, 1);
    }

    #[test]
    fn display_and_gpu_counts_are_checked() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
        let _restore = EnvRestore::clear();

        set_env(DISPLAY_BASE_ENV, "-1");
        assert!(Lane::from_env().is_err());
        set_env(DISPLAY_BASE_ENV, &u32::MAX.to_string());
        assert!(Lane::from_env().is_err());
        set_env(DISPLAY_BASE_ENV, "99");
        set_env(GPU_ENV, "1");
        for bad in ["0", "-1", "two"] {
            set_env(GPU_COUNT_ENV, bad);
            assert!(Lane::from_env().is_err(), "accepted {GPU_COUNT_ENV}={bad}");
        }
        set_env(GPU_ENV, "0");
        set_env(GPU_COUNT_ENV, "0");
        assert_eq!(Lane::from_env().unwrap().gpu_device, None);
    }

    #[test]
    fn doctor_needs_topology_but_not_a_scheduler_lane() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
        let _restore = EnvRestore::clear();
        set_env(LANE_COUNT_ENV, "8");
        set_env(DISPLAY_BASE_ENV, ":99");
        set_env(GPU_ENV, "0"); // host-independent: unset would auto-detect this machine's GPUs
        let (display, gpu) = doctor_display_from_env().unwrap();
        assert_eq!(display, ":115");
        assert_eq!(gpu, None);
        assert!(Lane::from_env().is_err());
    }

    #[test]
    fn an_index_outside_the_lane_count_is_refused() {
        // Fails closed: an index past the declared count means the scheduler was told this node has
        // more render_slot capacity than it has lanes, and the silent alternative is two clients on
        // one display.
        assert!(Lane::derive(4, 4, 99, Path::new("/scratch"), 8).is_err());
        assert!(Lane::derive(0, 0, 99, Path::new("/scratch"), 8).is_err());
    }

    #[test]
    fn pinning_keeps_every_other_option() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("options.txt"),
            "renderDistance:12\nfov:0.5\nparticles:0\n",
        )
        .unwrap();

        let pinned = pin_game_options(dir.path(), PINNED_FOV_OPTION, None).unwrap();
        assert_eq!(pinned.fov_option, 0.0);
        assert_eq!(pinned.fov_degrees, 70.0);
        assert_eq!(pinned.prior_fov_option, Some(0.5));
        assert!(!pinned.was_symlink);

        let written = std::fs::read_to_string(dir.path().join("options.txt")).unwrap();
        assert_eq!(
            written,
            "renderDistance:12\nparticles:0\nfov:0.0\nchatVisibility:2\n"
        );
    }

    #[test]
    fn pinning_replaces_a_shared_symlink_instead_of_writing_through_it() {
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("baked-options.txt");
        std::fs::write(&shared, "graphicsPreset:\"fancy\"\nrenderDistance:12\n").unwrap();
        let lane = dir.path().join("slot-0");
        std::fs::create_dir(&lane).unwrap();
        std::os::unix::fs::symlink(&shared, lane.join("options.txt")).unwrap();

        let pinned = pin_game_options(&lane, PINNED_FOV_OPTION, Some(24)).unwrap();
        assert!(pinned.was_symlink);
        assert_eq!(pinned.render_distance_chunks, Some(24));
        assert!(pin_game_options(&lane, PINNED_FOV_OPTION, Some(33)).is_err());
        assert!(
            !lane
                .join("options.txt")
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
        // The file every other lane links to is exactly as it was.
        assert_eq!(
            std::fs::read_to_string(&shared).unwrap(),
            "graphicsPreset:\"fancy\"\nrenderDistance:12\n"
        );
        assert_eq!(
            std::fs::read_to_string(lane.join("options.txt")).unwrap(),
            "graphicsPreset:\"custom\"\nrenderDistance:24\nfov:0.0\nchatVisibility:2\n"
        );
    }

    #[test]
    fn a_lane_under_the_baked_gamedir_does_not_link_itself() {
        // The default RENDER_STATE_DIR is the baked gamedir itself, so slot-0 lives inside the very
        // directory being linked in. Linking it into itself would make a cycle.
        let base = tempfile::tempdir().unwrap();
        std::fs::create_dir(base.path().join("mods")).unwrap();
        let state = base.path().join("slot-0");
        std::fs::create_dir(&state).unwrap();

        ensure_render_state_dir(base.path(), &state).unwrap();
        assert!(
            state
                .join("mods")
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(state.join("slot-0").symlink_metadata().is_err());

        // Idempotent: a second call adds nothing and replaces nothing.
        ensure_render_state_dir(base.path(), &state).unwrap();
        assert_eq!(std::fs::read_dir(&state).unwrap().count(), 1);
    }
}
