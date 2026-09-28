//! One ReplayMod render, end to end.
//!
//! [`render`] takes a staged `.mcpr` recording and a resolved plan and gives back described media:
//! it stages the replay into an exclusively held [`Lane`], writes the driver's [`DriverSpec`],
//! launches the headless Minecraft client under Xvfb, polls the driver's status file until the
//! session ends, then measures, hashes and (for 360 output) fixes up what landed on disk.
//!
//! # One launch, one job
//!
//! A call renders exactly ONE recording from ONE camera into N projections, in one Minecraft boot.
//! The driver's [`DriverSpec::jobs`] is a list because the Java side can open several replays per
//! boot, but a batch's failure modes — a per-job status map, partial-batch exit codes, a "some POVs
//! succeeded" verdict — are strictly harder to attribute, so the list here always has exactly one
//! entry and the outcome describes exactly one render.
//!
//! # What is measured versus what is asked for
//!
//! Nothing in [`RenderOutcome`] is copied from [`RenderRequest`]. The codec, dimensions and frame
//! count come from `ffprobe`; the digest comes from the bytes; `spherical` is true because the
//! metadata was injected and verified, not because the projection was configured as 360. A field
//! that echoes the request is a field that agrees with the request even when the render disagreed.
//!
//! # Where the outputs land
//!
//! Everything is written into `work_dir`, and the caller promotes what it wants into the episode
//! directory afterwards:
//!
//! | File | Written by |
//! |---|---|
//! | `<projection>.mp4` | the driver |
//! | `poses.json` | the driver |
//! | `driver-status.json` | the driver |
//! | `render-driver-spec.json` | this crate — the exact configuration the render ran with |
//! | `client.log` | this crate — the client's stdout and stderr |
//! | `glxinfo.txt` | this crate — the GL probe's full report |
//!
//! The spec is ARCHIVED on purpose. It used to be written into a temporary directory that was
//! deleted with the render, so the one document describing what a render was actually asked to do
//! never survived the render — a known defect, and the reason a failed 360 render could not be
//! reproduced from its artifacts.

mod client;
mod lane;
mod media;
pub use media::{describe_video, sample_luma, sha256_file, trim_video};
pub fn hmc_jar_from_env() -> Result<PathBuf> {
    client::hmc_jar_from_env()
}
pub mod spherical;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use pawa_schema::dataset::MediaInfo;
use pawa_schema::driver::{
    CameraArgs, CameraMode, DriverJob, DriverJobStatus, DriverProjection, DriverSpec,
    POSE_TRACE_NAME, RenderEncoding, RenderMethod,
};
use pawa_schema::manifest::{ProjectionSpec, RenderConfig};

pub use lane::{Lane, PinnedOptions, fov_degrees};

/// The driver's status file, polled during the render and kept as evidence afterwards.
pub const DRIVER_STATUS_NAME: &str = "driver-status.json";

/// The archived driver spec — the whole configuration this render ran with.
pub const DRIVER_SPEC_NAME: &str = "render-driver-spec.json";

/// The client's combined stdout and stderr.
pub const CLIENT_LOG_NAME: &str = "client.log";

/// The GL probe's full `glxinfo -B` report.
pub const GL_REPORT_NAME: &str = "glxinfo.txt";

/// In-lane name every render's replay is staged under. Reused deliberately — see
/// [`Lane::prepare`], which purges the ReplayMod cache keyed to it.
const STAGED_REPLAY_NAME: &str = "render-job-0.mcpr";

// Driver knobs that were configuration only for debugging the mod itself. They are constants here
// because nothing in the pipeline ever set them to anything else, and a knob no caller turns is a
// knob that only decays.
//
/// Resolve the followed entity from the job's `player_id` rather than overriding it.
const SPECTATE_ENTITY_ID: i64 = -1;
/// Ticks to wait after reaching the menu before opening the replay.
const OPEN_DELAY_TICKS: u32 = 100;
/// Ticks to wait for the replay to become ready before failing the job.
const READY_TIMEOUT_TICKS: u32 = 6000;
/// Milliseconds of the recording the driver pumps before it starts checking for renderable terrain.
/// A floor, not a horizon: the driver keeps pumping past this until the camera's own chunks arrive.
const WORLD_LOAD_MS: u32 = 3000;

/// One projection with its codec choices already resolved into the ffmpeg fragment the driver
/// passes through verbatim, and the share of the watchdog budget it is allowed.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedProjection {
    /// `[a-z0-9_]+`; becomes the output filename stem and the key in [`RenderOutcome::media`].
    pub name: String,
    pub method: RenderMethod,
    pub width: u32,
    pub height: u32,
    /// A codec-and-options fragment, e.g. `"libx264 -preset veryfast -crf 20"`.
    pub encoder: String,
    pub anti_aliasing: String,
    pub spherical_fov_x: u32,
    pub spherical_fov_y: u32,
    /// This projection's share of the render's wall-clock budget. A 360 render costs an order of
    /// magnitude more than a flat one at the same duration, which is why the budget is per
    /// projection and not per render.
    pub timeout: Duration,
}

impl ResolvedProjection {
    /// Resolve a manifest projection against the run-level codec defaults and timeout table.
    ///
    /// The per-projection `encoder`/`preset`/`crf` overrides collapse here, so the driver never
    /// sees a preset or a CRF — only the fragment it hands to ffmpeg.
    pub fn from_manifest(projection: &ProjectionSpec, config: &RenderConfig) -> ResolvedProjection {
        // The launch-time override outranks both the run default and per-projection overrides:
        // it states what this host's ffmpeg can do, which no manifest field knows.
        let over = encoder_override();
        let encoder = over
            .as_deref()
            .or(projection.encoder.as_deref())
            .unwrap_or(&config.encoder);
        let preset = projection.preset.as_deref().unwrap_or(&config.preset);
        let crf = projection.crf.unwrap_or(config.crf);
        ResolvedProjection {
            name: projection.name.clone(),
            method: projection.method,
            width: projection.width,
            height: projection.height,
            encoder: format!(
                "{} -threads {}",
                encoder_fragment(encoder, preset, crf),
                config.threads_per_render
            ),
            anti_aliasing: projection.anti_aliasing.clone(),
            spherical_fov_x: projection.spherical_fov_x,
            spherical_fov_y: projection.spherical_fov_y,
            timeout: Duration::from_secs(u64::from(
                *config
                    .timeout_sec
                    .get(&projection.name)
                    .or_else(|| config.timeout_sec.get("default"))
                    .unwrap_or(&6000),
            )),
        }
    }

    /// The driver's output name for this projection: `<name>.mp4`, or the `<name>` frame directory
    /// under PNG encoding.
    fn media_name(&self, encoding: RenderEncoding) -> String {
        match encoding {
            RenderEncoding::Mp4 => format!("{}.mp4", self.name),
            RenderEncoding::Png => self.name.clone(),
        }
    }
}

/// Which camera this render is, and the identity it renders under.
///
/// Exactly one of `player_id` and `camera_path` is set. A render is either a player POV — the
/// spectator camera attaches to that player's entity — or an observer shooting a path this pipeline
/// planned. Setting both is a caller error: the camera cannot be two cameras.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct JobSpec {
    /// POV owner, as the username the server assigned. The driver resolves it to an offline-mode
    /// UUID and matches it against the recording's player spawn packets, so a POV LABEL (`p03`)
    /// here can never match and every render would fall through to the positional fallback.
    pub player_id: Option<String>,
    /// An observer's dense camera-path document ([`PoseDoc`] shape). Planning it belongs to the
    /// observer stage; this crate only shoots a path that already exists.
    ///
    /// [`PoseDoc`]: pawa_schema::driver::PoseDoc
    pub camera_path: Option<PathBuf>,
    /// The POV's first recorded position — the driver's second-choice camera anchor when the
    /// recording carries no spawn packet for the POV owner's own UUID.
    pub start_pos: Option<[f64; 3]>,
    pub episode_id: Option<String>,
    pub render_id: Option<String>,
}

impl JobSpec {
    fn is_observer(&self) -> bool {
        self.camera_path.is_some()
    }
}

/// Everything one render needs that is not the lane.
#[derive(Clone, Debug)]
pub struct RenderRequest<'a> {
    /// The recording to render. Copied into the lane's state directory by this crate, because
    /// ReplayMod's caches are keyed on the replay's path and must live inside the lane.
    pub mcpr: &'a Path,
    pub job: JobSpec,
    pub projections: Vec<ResolvedProjection>,
    pub camera: CameraArgs,
    pub fps: u32,
    pub encoding: RenderEncoding,
    /// Request ReplayMod depth buffers in addition to the normal video projections.
    pub depth: bool,
    /// JVM `-XX:ActiveProcessorCount`. Distinct from [`Lane::gl_threads`], which sizes llvmpipe.
    pub threads: u32,
    pub render_distance_chunks: Option<u32>,
    /// Where the driver spec, status, logs and media land before the caller promotes them.
    pub work_dir: &'a Path,
}

impl RenderRequest<'_> {
    fn validate(&self) -> Result<()> {
        if self.projections.is_empty() {
            bail!("a render with no projections would launch Minecraft to produce nothing");
        }
        match (&self.job.player_id, &self.job.camera_path) {
            (Some(_), Some(_)) => bail!("a render is either a player POV or an observer, not both"),
            (None, None) => bail!("a render needs a player_id (POV) or a camera_path (observer)"),
            _ => {}
        }
        if !self.mcpr.is_file() {
            bail!("replay not found: {}", self.mcpr.display());
        }
        Ok(())
    }

    /// Whether a static-camera render counts as valid output. Mirrors the driver's own gate, from
    /// the same field the driver is handed rather than from a second read of the environment.
    fn static_allowed(&self) -> bool {
        self.camera.allow_static || self.camera.mode == CameraMode::Static
    }
}

/// What one render produced, and why it is or is not usable.
#[derive(Clone, Debug)]
pub struct RenderOutcome {
    /// Described media per projection name.
    pub media: BTreeMap<String, MediaInfo>,
    /// Optional ReplayMod depth-buffer sequences keyed by projection name.
    pub depth: BTreeMap<String, pawa_schema::dataset::DepthInfo>,
    /// The field of view the client actually loaded, read back from its `options.txt`. Belongs in
    /// the render manifest's camera intrinsics — the render is the only stage that knows it.
    pub pinned: PinnedOptions,
    /// `None` is a usable render. Anything else is the first thing found wrong, in the order a
    /// diagnosis wants: how the session ended, then what it produced, then whether it was shot
    /// from the right camera.
    pub failure: Option<RenderFailure>,
}

/// Why a render is not usable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RenderFailure {
    /// The watchdog's wall-clock budget expired with the job unfinished.
    ProgressTimeout,
    /// The camera was not the camera this job asked for: a POV whose spectator camera never
    /// attached to its player (so the video shows the world from a static fallback the training
    /// signal cannot use), or an observer that did not shoot its camera path.
    CameraAttachFailed,
    /// A 360 render whose projection metadata could not be written. It would play as a distorted
    /// flat video everywhere, so it fails rather than shipping.
    SphericalMetadataInjectionFailed,
    /// A projection produced no file, or a file with no frames.
    RenderOutputMissing,
    /// The job never reached a terminal state and its outputs are there — the client died under it.
    ClientCrash,
    /// A failure class the driver named that this side does not model. Carried verbatim rather
    /// than collapsed into a generic failure, so a new class from the mod is still legible here.
    Driver(String),
}

impl RenderFailure {
    /// The wire name, as it appears in a render manifest and in the driver's own status.
    pub fn class(&self) -> &str {
        match self {
            RenderFailure::ProgressTimeout => "progress_timeout",
            RenderFailure::CameraAttachFailed => "camera_attach_failed",
            RenderFailure::SphericalMetadataInjectionFailed => {
                "spherical_metadata_injection_failed"
            }
            RenderFailure::RenderOutputMissing => "render_output_missing",
            RenderFailure::ClientCrash => "client_crash",
            RenderFailure::Driver(class) => class,
        }
    }

    fn from_class(class: &str) -> RenderFailure {
        match class {
            "progress_timeout" => RenderFailure::ProgressTimeout,
            "camera_attach_failed" => RenderFailure::CameraAttachFailed,
            "spherical_metadata_injection_failed" => {
                RenderFailure::SphericalMetadataInjectionFailed
            }
            "render_output_missing" => RenderFailure::RenderOutputMissing,
            "client_crash" => RenderFailure::ClientCrash,
            other => RenderFailure::Driver(other.to_string()),
        }
    }
}

impl std::fmt::Display for RenderFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.class())
    }
}

/// Render one recording in one Minecraft launch.
///
/// The lane is prepared here rather than by the caller — [`Lane::prepare`] is idempotent, and
/// making it a precondition would mean a forgotten call renders at whatever field of view the
/// previous render's exit happened to persist, with nothing in the manifest to show for it.
///
/// The `Err` return is reserved for things that stopped the render from being ATTEMPTED honestly:
/// an invalid request, a missing GL stack, a display that could not be taken, a spec that could not
/// be written. A render that ran and produced unusable output returns `Ok` with a
/// [`RenderOutcome::failure`] — that is a result about the recording, not a fault of this stage,
/// and the caller needs the logs and the driver status that come with it.
pub fn render(lane: &Lane, request: &RenderRequest<'_>) -> Result<RenderOutcome> {
    request.validate()?;
    let work_dir = request.work_dir;
    std::fs::create_dir_all(work_dir)
        .with_context(|| format!("creating work dir {}", work_dir.display()))?;
    clear_stale_outputs(work_dir, request);

    let pinned = lane.prepare(request.render_distance_chunks)?;
    std::fs::write(
        work_dir.join("pinned-options.json"),
        serde_json::to_string_pretty(&pinned)? + "\n",
    )?;
    tracing::info!(
        lane = lane.index,
        fov_option = pinned.fov_option,
        fov_degrees = pinned.fov_degrees,
        prior = ?pinned.prior_fov_option,
        options = %pinned.path.display(),
        "pinned camera intrinsics"
    );

    let staged = stage_replay(lane, request.mcpr)?;
    let status_path = work_dir.join(DRIVER_STATUS_NAME);
    let spec_path = work_dir.join(DRIVER_SPEC_NAME);
    let spec = build_spec(request, &staged, &status_path);
    std::fs::write(&spec_path, serde_json::to_string_pretty(&spec)? + "\n")
        .with_context(|| format!("writing driver spec {}", spec_path.display()))?;

    let probe = client::probe_gl(
        &lane.probe_display,
        lane.gpu_device.as_deref(),
        &work_dir.join(GL_REPORT_NAME),
    )?;
    tracing::info!(
        renderer = probe.renderer,
        version = probe.version,
        "GL probe"
    );

    let client_log = work_dir.join(CLIENT_LOG_NAME);
    let supervision = {
        let _xvfb = client::start_xvfb(&lane.display)?;
        let mut child = client::launch(
            &lane.state_dir,
            &lane.display,
            lane.gl_threads,
            request.threads,
            lane.gpu_device.as_deref(),
            &spec_path,
            &client_log,
        )?;
        // Depth adds a second full ReplayMod pass per projection. The watchdog must budget both
        // passes; otherwise a healthy depth render inherits the video-only deadline and times out.
        let pass_count = if request.depth { 2 } else { 1 };
        let deadline: Duration = request.projections.iter().map(|p| p.timeout).sum();
        client::supervise(&mut child, &status_path, deadline * pass_count)?

        // Client and Xvfb are both stopped here, before the post-processing that does not need
        // them — a lane holds its display for exactly as long as something is drawing on it.
    };

    // `None` when the client died before the mod initialised and wrote no parseable status.
    // `classify` reads that absence directly rather than synthesizing a phase no driver reported.
    let driver_status = client::read_status(&status_path);
    let collected = media::collect(
        work_dir,
        &request.projections,
        request.encoding,
        request.fps,
        media::DepthRequest {
            enabled: request.depth,
        },
    )?;
    let mut failure = classify(
        driver_status
            .as_ref()
            .and_then(|status| status.jobs.first()),
        driver_status
            .as_ref()
            .and_then(|status| status.failure_class.as_deref()),
        supervision.timed_out,
        request.job.is_observer(),
        request.static_allowed(),
        &collected,
    );
    if let Some(expected) = request.render_distance_chunks {
        let after = std::fs::read_to_string(&pinned.path).unwrap_or_default();
        let actual = after
            .lines()
            .filter_map(|line| line.split_once(':'))
            .filter(|(key, _)| key.trim() == "renderDistance")
            .filter_map(|(_, value)| value.trim().parse::<u32>().ok())
            .next_back();
        std::fs::write(
            work_dir.join("post-render-options.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "requested_render_distance_chunks": expected,
                "actual_render_distance_chunks": actual,
                "matches": actual == Some(expected)
            }))? + "\n",
        )?;
        if actual != Some(expected) && failure.is_none() {
            failure = Some(RenderFailure::Driver("render_distance_mismatch".into()));
        }
    }
    tracing::info!(
        exit_code = ?supervision.exit_code,
        frames = collected.frame_count,
        projections = ?collected.media.keys().collect::<Vec<_>>(),
        failure = ?failure.as_ref().map(RenderFailure::class),
        "render finished"
    );

    Ok(RenderOutcome {
        media: collected.media,
        depth: collected.depth,
        pinned,
        failure,
    })
}

/// Remove this render's products from an earlier attempt in the same work directory.
fn clear_stale_outputs(work_dir: &Path, request: &RenderRequest<'_>) {
    for name in [DRIVER_STATUS_NAME, POSE_TRACE_NAME] {
        let _ = std::fs::remove_file(work_dir.join(name));
    }
    for projection in &request.projections {
        let path = work_dir.join(projection.media_name(request.encoding));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&path);
        if request.depth {
            let _ = std::fs::remove_dir_all(work_dir.join(&projection.name));
        }
    }
}

/// Copy the recording into the lane, under the one name every render in this lane uses.
///
/// The sibling state ReplayStudio keys to that path goes with it: `<name>.mcpr.cache` (its
/// persisted entity-position cache) and `<name>.mcpr.tmp` (ZipReplayFile's crash-recovery changed
/// entries, which are restored verbatim into the NEXT file opened at this path). Both would
/// otherwise be matched against the next episode's replay.
fn stage_replay(lane: &Lane, mcpr: &Path) -> Result<PathBuf> {
    let staged = lane.state_dir.join(STAGED_REPLAY_NAME);
    let replay_dir = lane.replay_dir();
    std::fs::create_dir_all(&replay_dir)?;
    for path in [
        staged.clone(),
        staged.with_extension("mcpr.cache"),
        staged.with_extension("mcpr.tmp"),
        replay_dir.join(STAGED_REPLAY_NAME),
    ] {
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&path);
    }
    std::fs::copy(mcpr, &staged)
        .with_context(|| format!("staging {} into {}", mcpr.display(), staged.display()))?;
    // ReplayMod's own replay folder needs at least one file present before it will open the replay
    // screen the driver drives.
    std::fs::copy(&staged, replay_dir.join(STAGED_REPLAY_NAME))?;
    Ok(staged)
}

fn build_spec(request: &RenderRequest<'_>, staged: &Path, status_path: &Path) -> DriverSpec {
    DriverSpec {
        status_path: status_path.display().to_string(),
        fps: request.fps,
        encoding: request.encoding,
        depth: request.depth,
        projections: request
            .projections
            .iter()
            .map(|projection| DriverProjection {
                name: projection.name.clone(),
                method: projection.method,
                width: projection.width,
                height: projection.height,
                encoder: projection.encoder.clone(),
                anti_aliasing: projection.anti_aliasing.clone(),
                spherical_fov_x: projection.spherical_fov_x,
                spherical_fov_y: projection.spherical_fov_y,
            })
            .collect(),
        camera: request.camera.clone(),
        spectate_entity_id: SPECTATE_ENTITY_ID,
        open_delay_ticks: OPEN_DELAY_TICKS,
        ready_timeout_ticks: READY_TIMEOUT_TICKS,
        world_load_ms: WORLD_LOAD_MS,
        hud_hotbar: pawa_schema::catalog::hotbar_kit()
            .into_iter()
            .map(|(item, count)| pawa_schema::driver::HudHotbarSlot {
                item: item.to_string(),
                count,
            })
            .collect(),
        jobs: vec![DriverJob {
            replay_path: staged.display().to_string(),
            out_path: request.work_dir.display().to_string(),
            player_id: request.job.player_id.clone(),
            episode_id: request.job.episode_id.clone(),
            render_id: request.job.render_id.clone(),
            start_pos: request.job.start_pos,
            camera_path: request
                .job
                .camera_path
                .as_ref()
                .map(|p| p.display().to_string()),
        }],
    }
}

/// Decide the verdict from what the driver reported and what is on disk.
///
/// Order matters and is the order of a diagnosis: how the session ended outranks what it produced,
/// because a timed-out render's missing file is a symptom of the timeout and reporting it as
/// `render_output_missing` would send the reader looking at the encoder.
fn classify(
    job: Option<&DriverJobStatus>,
    session_failure: Option<&str>,
    timed_out: bool,
    is_observer: bool,
    static_allowed: bool,
    collected: &media::Collected,
) -> Option<RenderFailure> {
    let complete = job.is_some_and(|job| job.status == "complete");
    // A driver that failed before its first job (bad spec, no tick callback) writes jobs:[] with
    // the class at the SESSION level only — without the fallback that class would be lost to the
    // generic client_crash/render_output_missing guess below.
    let driver_class = || {
        job.and_then(|job| job.failure_class.as_deref())
            .or(session_failure)
            .map(RenderFailure::from_class)
    };
    let fallback = || {
        if collected.all_present {
            RenderFailure::ClientCrash
        } else {
            RenderFailure::RenderOutputMissing
        }
    };

    if timed_out && !complete {
        return Some(RenderFailure::ProgressTimeout);
    }
    if !complete {
        // Either the driver failed the job and said why, or it never reached a terminal state at
        // all — a client that died mid-render leaves the job `pending` and no failure class.
        return Some(driver_class().unwrap_or_else(fallback));
    }
    if !collected.all_present {
        return Some(RenderFailure::RenderOutputMissing);
    }
    // A player POV must report `spectator_pov` and an observer must report `camera_path`. Accepting
    // either for both kinds would let a failed observer path silently degrade into whatever camera
    // ReplayMod happened to have, which is the silent-wrong-camera bug this gate exists to catch.
    let want = if is_observer {
        "camera_path"
    } else {
        "spectator_pov"
    };
    let camera_mode = job
        .and_then(|job| job.detail.get("camera_mode"))
        .and_then(|mode| mode.as_str());
    if camera_mode != Some(want) && !static_allowed {
        return Some(RenderFailure::CameraAttachFailed);
    }
    if !collected.all_spherical_ok {
        return Some(RenderFailure::SphericalMetadataInjectionFailed);
    }
    if collected.frame_count == 0 {
        return Some(RenderFailure::RenderOutputMissing);
    }
    None
}

// ---------------------------------------------------------------------------------------------
// preflight (doctor)
// ---------------------------------------------------------------------------------------------

/// Rate control differs per encoder family: NVENC has no `-crf` and its presets are `p1`..`p7`,
/// so an x264 preset or `-crf` leaking through aborts ffmpeg mid-render. The manifest keeps one
/// `preset`/`crf` pair; this maps them onto whichever encoder is in effect.
pub fn encoder_fragment(encoder: &str, preset: &str, crf: u8) -> String {
    if encoder == "h264_nvenc" {
        let preset = if preset.len() == 2 && preset.starts_with('p') {
            preset
        } else {
            "p5"
        };
        format!("h264_nvenc -preset {preset} -rc vbr -cq {crf}")
    } else {
        format!("{encoder} -preset {preset} -crf {crf}")
    }
}

/// Launch-time encoder override, read from `RENDER_ENCODER`: an encoder name forces it,
/// `auto` picks `h264_nvenc` when a real NVENC encode succeeds and falls back to the manifest
/// encoder otherwise. Unset keeps the manifest encoder. Probed once per process.
pub fn encoder_override() -> Option<String> {
    static OVERRIDE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    OVERRIDE
        .get_or_init(|| match std::env::var("RENDER_ENCODER").ok().as_deref() {
            None | Some("") => None,
            Some("auto") => assert_encoder("h264_nvenc")
                .ok()
                .map(|()| "h264_nvenc".into()),
            Some(name) => Some(name.into()),
        })
        .clone()
}

/// Assert an allowed encoder completes a real two-frame encode. The doctor's encoder check —
/// an encoder that lists in `ffmpeg -encoders` can still fail its first real frame (NVENC
/// notably lists fine on a GPU-less ffmpeg build and dies opening the session).
pub fn assert_encoder(name: &str) -> Result<()> {
    let preset = match name {
        "libx264" => "veryfast",
        "libsvtav1" => "8",
        "h264_nvenc" => "p5",
        other => bail!("unsupported encoder {other:?}"),
    };
    let output = std::process::Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=black:s=320x180:r=20:d=0.2",
            "-c:v",
            name,
            "-preset",
            preset,
            "-pix_fmt",
            "yuv420p",
            "-frames:v",
            "2",
            "-f",
            "null",
            "-",
        ])
        .output()
        .context("ffmpeg encoder probe could not start")?;
    if !output.status.success() {
        bail!(
            "ffmpeg encoder {name} failed a real encode: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// GL probe on the doctor display — one past every lane's render AND probe display, so it can run
/// while renders are in flight. Returns `(renderer, version)`.
pub fn doctor_gl_probe() -> Result<(String, String)> {
    let (display, gpu_device) = lane::doctor_display_from_env()?;
    let report = tempfile::NamedTempFile::new()?;
    let probe = client::probe_gl(&display, gpu_device.as_deref(), report.path())?;
    Ok((probe.renderer, probe.version))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn collected(all_present: bool, all_spherical_ok: bool, frame_count: u64) -> media::Collected {
        media::Collected {
            media: BTreeMap::new(),
            depth: BTreeMap::new(),
            all_present,
            all_spherical_ok,
            frame_count,
        }
    }

    fn job_status(
        status: &str,
        camera_mode: Option<&str>,
        failure_class: Option<&str>,
    ) -> DriverJobStatus {
        DriverJobStatus {
            index: 0,
            player_id: Some("Bot03".into()),
            replay_path: "/lane/render-job-0.mcpr".into(),
            out_path: "/work".into(),
            status: status.into(),
            failure_class: failure_class.map(str::to_string),
            failure_message: None,
            detail: camera_mode
                .map(|mode| BTreeMap::from([("camera_mode".to_string(), json!(mode))]))
                .unwrap_or_default(),
        }
    }

    #[test]
    fn a_complete_pov_render_has_no_failure() {
        let done = job_status("complete", Some("spectator_pov"), None);
        assert_eq!(
            classify(
                Some(&done),
                None,
                false,
                false,
                false,
                &collected(true, true, 1200)
            ),
            None
        );
    }

    #[test]
    fn a_timeout_outranks_the_missing_file_it_caused() {
        let stuck = job_status("pending", None, None);
        assert_eq!(
            classify(
                Some(&stuck),
                None,
                true,
                false,
                false,
                &collected(false, true, 0)
            ),
            Some(RenderFailure::ProgressTimeout)
        );
        // A job that finished before the deadline expired keeps its own verdict: the driver had
        // already written the outputs, so the render is usable even though the loop timed out.
        let done = job_status("complete", Some("spectator_pov"), None);
        assert_eq!(
            classify(
                Some(&done),
                None,
                true,
                false,
                false,
                &collected(true, true, 20)
            ),
            None
        );
    }

    #[test]
    fn a_static_fallback_fails_a_pov_and_a_path_miss_fails_an_observer() {
        let fell_back = job_status("complete", Some("static_anchored"), None);
        assert_eq!(
            classify(
                Some(&fell_back),
                None,
                false,
                false,
                false,
                &collected(true, true, 1200)
            ),
            Some(RenderFailure::CameraAttachFailed)
        );
        // ...unless a static render is what was asked for, which is a debugging path.
        assert_eq!(
            classify(
                Some(&fell_back),
                None,
                false,
                false,
                true,
                &collected(true, true, 1200)
            ),
            None
        );

        // An observer that shot a path is fine; a POV that reports one is not, and vice versa —
        // neither camera mode is acceptable for the other kind of job.
        let shot_path = job_status("complete", Some("camera_path"), None);
        assert_eq!(
            classify(
                Some(&shot_path),
                None,
                false,
                true,
                false,
                &collected(true, true, 900)
            ),
            None
        );
        assert_eq!(
            classify(
                Some(&shot_path),
                None,
                false,
                false,
                false,
                &collected(true, true, 900)
            ),
            Some(RenderFailure::CameraAttachFailed)
        );
        let attached = job_status("complete", Some("spectator_pov"), None);
        assert_eq!(
            classify(
                Some(&attached),
                None,
                false,
                true,
                false,
                &collected(true, true, 900)
            ),
            Some(RenderFailure::CameraAttachFailed)
        );
    }

    #[test]
    fn a_driver_failure_class_survives_verbatim() {
        let failed = job_status("failed", None, Some("replay_open_failed"));
        assert_eq!(
            classify(
                Some(&failed),
                None,
                false,
                false,
                false,
                &collected(false, true, 0)
            ),
            Some(RenderFailure::Driver("replay_open_failed".into()))
        );
        assert_eq!(
            RenderFailure::Driver("replay_open_failed".into()).class(),
            "replay_open_failed"
        );
    }

    #[test]
    fn a_preflight_failure_class_survives_from_the_session_level() {
        // A driver that failed before its first job (bad spec, no tick callback) writes jobs:[]
        // with the class at the session level; the classifier must not degrade it into the
        // generic output-missing guess.
        assert_eq!(
            classify(
                None,
                Some("client_launch_failed"),
                false,
                false,
                false,
                &collected(false, true, 0)
            ),
            Some(RenderFailure::Driver("client_launch_failed".into()))
        );
    }

    #[test]
    fn a_client_that_never_wrote_a_status_is_not_a_success() {
        // No status file at all: the mod never initialised, so there is no job to ask.
        assert_eq!(
            classify(None, None, false, false, false, &collected(false, true, 0)),
            Some(RenderFailure::RenderOutputMissing)
        );
        // Outputs present but the job never reached a terminal state: the client died holding them.
        assert_eq!(
            classify(
                None,
                None,
                false,
                false,
                false,
                &collected(true, true, 1200)
            ),
            Some(RenderFailure::ClientCrash)
        );
    }

    #[test]
    fn an_empty_video_and_a_360_without_metadata_both_fail() {
        let done = job_status("complete", Some("spectator_pov"), None);
        assert_eq!(
            classify(
                Some(&done),
                None,
                false,
                false,
                false,
                &collected(true, false, 1200)
            ),
            Some(RenderFailure::SphericalMetadataInjectionFailed)
        );
        assert_eq!(
            classify(
                Some(&done),
                None,
                false,
                false,
                false,
                &collected(true, true, 0)
            ),
            Some(RenderFailure::RenderOutputMissing)
        );
    }

    #[test]
    fn codec_overrides_collapse_into_one_ffmpeg_fragment() {
        let config = RenderConfig::default();
        let spec: ProjectionSpec = serde_json::from_value(json!({
            "name": "equirect", "method": "EQUIRECTANGULAR", "width": 1024, "height": 512,
            "encoder": "libsvtav1", "preset": "8", "crf": 30,
        }))
        .unwrap();
        let resolved = ResolvedProjection::from_manifest(&spec, &config);
        assert_eq!(resolved.encoder, "libsvtav1 -preset 8 -crf 30 -threads 8");
        // 360 renders get their own budget; it is an order of magnitude above the flat one.
        assert_eq!(resolved.timeout, Duration::from_secs(21600));
        assert_eq!(resolved.media_name(RenderEncoding::Mp4), "equirect.mp4");
        assert_eq!(resolved.media_name(RenderEncoding::Png), "equirect");

        // No overrides: the run-level defaults, and the `default` timeout for a name the table has
        // no entry for.
        let plain: ProjectionSpec = serde_json::from_value(json!({"name": "wide"})).unwrap();
        let resolved = ResolvedProjection::from_manifest(&plain, &config);
        assert_eq!(
            resolved.encoder,
            "libx264 -preset veryfast -crf 20 -threads 8"
        );
        assert_eq!(resolved.timeout, Duration::from_secs(6000));
    }

    #[test]
    fn nvenc_fragment_speaks_cq_not_crf() {
        // The manifest's x264-flavoured preset must not leak into an NVENC fragment.
        assert_eq!(
            encoder_fragment("h264_nvenc", "veryfast", 20),
            "h264_nvenc -preset p5 -rc vbr -cq 20"
        );
        assert_eq!(
            encoder_fragment("h264_nvenc", "p4", 23),
            "h264_nvenc -preset p4 -rc vbr -cq 23"
        );
        assert_eq!(
            encoder_fragment("libx264", "veryfast", 20),
            "libx264 -preset veryfast -crf 20"
        );
    }
}
