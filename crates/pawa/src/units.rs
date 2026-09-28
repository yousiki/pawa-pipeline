//! The schedulable units: what one HyperQueue task actually does.
//!
//! # The out-root layout the units share
//!
//! ```text
//! <out>/work/<ep>/capture/            capture unit's raw artifacts (players + coordinator + .mcpr)
//! <out>/work/<ep>/render/<target>/    one render's products + render.json (RenderRecord)
//! <out>/dataset/<ts>-<scenario>-<Np>-<hash8>/   the final layout, assembled by finalize
//!                                     (docs/dataset-spec.md; name derived from the capture record)
//! <out>/status/<ep>/*.json            UnitStatus per unit, atomic
//! <out>/runs/, <out>/logs/            run report, task stdout/stderr
//! ```
//!
//! `work/` is the staging tier on the shared mount; finalize moves the dataset files out of it and
//! parks EVERYTHING left over under the episode's `diagnostics/` — full retention, wholesale
//! deletable, never read by training.
//!
//! # Exit codes
//!
//! `0` = complete or skipped-by-design (the skip is in the status file; a skip must not break the
//! static task graph). `2` = contract error, never retried. `3` = stage failure, retryable by the
//! head script's resubmission round. Anything else is a crash.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use pawa_align::frames::{EpisodeRowCtx, FramesOutcome, assemble_frames, frame_time_ms, trim_rows};
use pawa_align::join::{Anchors, metadata_join};
use pawa_align::observer::{PlayerTrack, plan_observer};
use pawa_align::qc::{Thresholds, check_episode, check_pov_anchor_spread, check_pov_darkness};
use pawa_capture::{CaptureConfig, EpisodeRequest as CaptureRequest, capture_episode};
use pawa_render::{JobSpec, Lane, RenderRequest, ResolvedProjection, render};
use pawa_schema::capture::{
    ActionEvent, ActionFrame, CaptureMeta, CapturePov, CaptureRecord, ShardRecord, SwarmSummary,
};
use pawa_schema::dataset::{
    self, EpisodeManifest, EventRow, MediaInfo, PovEntry, PovKind, PovQc, PovStatus, QcReport,
    QcVerdict,
};
use pawa_schema::driver::{CameraArgs, CameraMode, PoseDoc, RenderEncoding};
use pawa_schema::manifest::{EpisodeSpec, RunManifest};
use pawa_schema::status::{FailureClass, UnitKind, UnitOutcome, UnitStatus};
use pawa_schema::time::{DurationMs, EpochMs};

/// Media, depth and optional source POV promoted from one observer render.
type PromotedObserverMedia = (
    BTreeMap<String, MediaInfo>,
    BTreeMap<String, pawa_schema::dataset::DepthInfo>,
    Option<String>,
);

/// A unit failure that must not be retried: the input contract is broken.
#[derive(Debug)]
pub struct ContractError(pub anyhow::Error);

impl std::fmt::Display for ContractError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
impl std::error::Error for ContractError {}

fn contract(error: anyhow::Error) -> anyhow::Error {
    anyhow::Error::new(ContractError(error))
}

/// Paths for one episode under the out root.
pub struct EpisodePaths {
    pub out_root: PathBuf,
    pub episode_id: String,
}

impl EpisodePaths {
    pub fn capture_dir(&self) -> PathBuf {
        self.out_root
            .join("work")
            .join(&self.episode_id)
            .join("capture")
    }
    pub fn render_dir(&self, target: &str) -> PathBuf {
        self.out_root
            .join("work")
            .join(&self.episode_id)
            .join("render")
            .join(target)
    }
    pub fn work_dir(&self) -> PathBuf {
        self.out_root.join("work").join(&self.episode_id)
    }
    /// Parent of every episode's dataset directory. The per-episode directory name is derived by
    /// finalize (`dataset_dir_name`) from the capture record, not from the episode id — use
    /// [`find_dataset_dir`] to locate an assembled episode.
    pub fn dataset_root(&self) -> PathBuf {
        self.out_root.join("dataset")
    }
    pub fn status_path(&self, name: &str) -> PathBuf {
        self.out_root
            .join("status")
            .join(&self.episode_id)
            .join(name)
    }
    pub fn capture_record(&self) -> Result<CaptureRecord> {
        let path = self.capture_dir().join("capture.json");
        let text = fs::read_to_string(&path)
            .with_context(|| format!("capture record missing: {}", path.display()))
            .map_err(contract)?;
        serde_json::from_str(&text)
            .with_context(|| format!("capture record unparseable: {}", path.display()))
            .map_err(contract)
    }
}

/// The work-tier record one render unit leaves for finalize. Internal to this binary (same
/// version writes and reads it within one run), so it lives here rather than in the schema crate.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct RenderRecord {
    pub target: String,
    pub kind: PovKind,
    pub media: BTreeMap<String, MediaInfo>,
    pub depth: BTreeMap<String, pawa_schema::dataset::DepthInfo>,
    pub fov_deg: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_pov: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
}

pub const RENDER_RECORD_NAME: &str = "render.json";

fn read_jsonl<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Vec<T>> {
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    text.lines()
        .enumerate()
        .map(|(idx, line)| {
            serde_json::from_str(line).with_context(|| format!("{}:{}", path.display(), idx + 1))
        })
        .collect()
}

fn write_jsonl<T: serde::Serialize>(path: &Path, rows: &[T]) -> Result<()> {
    let mut text = String::new();
    for row in rows {
        text.push_str(&serde_json::to_string(row)?);
        text.push('\n');
    }
    fs::write(path, text).with_context(|| format!("write {}", path.display()))
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    serde_json::from_str(
        &fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?,
    )
    .with_context(|| format!("parse {}", path.display()))
}

fn episode_spec<'m>(manifest: &'m RunManifest, episode_id: &str) -> Result<&'m EpisodeSpec> {
    manifest
        .episodes
        .iter()
        .find(|e| e.episode_id == episode_id)
        .ok_or_else(|| contract(anyhow!("episode {episode_id} is not in the manifest")))
}

/// Sum of per-tick capture anchors for one POV, from its meta plus the episode record.
fn anchors_for(record: &CaptureRecord, meta: &CaptureMeta) -> Anchors {
    Anchors {
        go_world_age_tick: meta.go_world_age_tick,
        recording_start_world_age_tick: record.recording_start_world_age_tick,
        recording_stop_world_age_tick: record
            .recording_stop_world_age_by_player
            .get(&meta.username)
            .copied()
            .or(record.recording_stop_world_age_tick),
    }
}

// ---------------------------------------------------------------------------------------------
// capture unit
// ---------------------------------------------------------------------------------------------

/// Deployment knobs for the capture stage: defaults, overlaid by an optional JSON file named in
/// `PAWA_CAPTURE_CONFIG`, overlaid by the handful of environment variables the image bakes.
/// One JSON document beats reintroducing an ambient env vocabulary; the env layer is the three
/// paths the Dockerfile bakes, plus the heap size.
fn capture_config_from_env() -> Result<CaptureConfig> {
    let mut config = match std::env::var("PAWA_CAPTURE_CONFIG") {
        Ok(path) => read_json(Path::new(&path))?,
        Err(_) => CaptureConfig::default(),
    };
    if let Ok(pack) = std::env::var("PAWA_SERVER_PACK") {
        config.server_pack = pack.into();
    }
    if let Ok(workdir) = std::env::var("PAWA_SERVER_WORKDIR") {
        config.server_workdir = workdir.into();
    }
    if let Ok(bin) = std::env::var("PAWA_PLAYER_BIN") {
        config.player_bin = bin.into();
    }
    if let Ok(xmx) = std::env::var("SERVER_XMX") {
        config.server_xmx = xmx;
    }
    let slot = match std::env::var(CAPTURE_SLOT_ENV) {
        Ok(raw) => raw
            .trim()
            .parse::<u16>()
            .with_context(|| format!("{CAPTURE_SLOT_ENV}={raw:?} is not a slot index"))?,
        Err(_) => 0,
    };
    if !(2..=32).contains(&config.view_distance) {
        bail!("capture view_distance must be between 2 and 32");
    }
    derive_capture_slot(&mut config, slot)?;
    Ok(config)
}

/// The exclusive capture slot HyperQueue granted this task, as a decimal string. Absent in a
/// single-slot local run, which is slot 0.
const CAPTURE_SLOT_ENV: &str = "HQ_RESOURCE_VALUES_captureslot";

/// Give one capture everything it owns exclusively, derived from its slot index.
///
/// Same doctrine as the render lane (see `pawa_render::lane`): a capture is not a pure
/// function of its inputs. It binds two fixed ports, and `reset_workdir` WIPES `server_workdir`
/// at the start of every episode — so two captures pointed at one workdir delete each other's
/// world mid-episode, which no port change would save them from. Deriving both from the single
/// index the scheduler granted is what makes it impossible to configure two captures onto one.
///
/// Slot 0 is a subdirectory too, deliberately: leaving it at the parent would put every other
/// slot's directory *inside* the tree slot 0 wipes.
fn derive_capture_slot(config: &mut CaptureConfig, slot: u16) -> Result<()> {
    let span = config.rcon_port.abs_diff(config.server_port);
    if slot >= span {
        bail!(
            "capture slot {slot} does not fit between the server port ({}) and the rcon port ({}), \
             which are only {span} apart",
            config.server_port,
            config.rcon_port
        );
    }
    config.server_port = config
        .server_port
        .checked_add(slot)
        .context("capture server port overflow")?;
    config.rcon_port = config
        .rcon_port
        .checked_add(slot)
        .context("capture rcon port overflow")?;
    config.server_workdir = config.server_workdir.join(format!("slot-{slot}"));
    Ok(())
}

pub fn unit_capture(manifest: &RunManifest, paths: &EpisodePaths) -> Result<UnitResult> {
    let spec = episode_spec(manifest, &paths.episode_id)?;
    let config = capture_config_from_env()?;
    let capture_dir = paths.capture_dir();
    fs::create_dir_all(&capture_dir)?;
    let outcome = capture_episode(
        &config,
        &CaptureRequest {
            spec,
            run_id: &manifest.run_id,
            episode_dir: &capture_dir,
        },
    )?;

    // The bridge record downstream units navigate by. Written whether or not the episode passed —
    // a failed capture's inventory is exactly what a human debugs from.
    let mut povs = Vec::new();
    let mut runtime: Option<dataset::RuntimeInfo> = None;
    let mut scenario_name = spec.scenario.name.clone();
    let mut scenario_seed = spec.scenario.seed.unwrap_or(0);
    let mut actual_duration = 0i64;
    for (pov, username) in &outcome.pov_map {
        let meta: Option<CaptureMeta> =
            read_json(&capture_dir.join(format!("{pov}-meta.json"))).ok();
        // This is a renderable-input inventory, not the capture success verdict. A timed-out
        // bot can still have a valid replay and metadata; preserve that evidence for explicit
        // operator recovery while the capture unit itself continues to report failure.
        let connected = meta.is_some() && outcome.recordings.contains_key(pov);
        if let Some(meta) = &meta {
            actual_duration = actual_duration.max(meta.actual_duration_ms.get());
            scenario_name = meta.scenario.clone();
            scenario_seed = meta.scenario_seed;
            if runtime.is_none() {
                runtime = Some(dataset::RuntimeInfo {
                    family: meta.runtime_family.clone(),
                    topology: String::new(), // filled from the swarm summary below
                    azalea_version: meta.azalea_version.clone(),
                });
            }
        }
        povs.push(CapturePov {
            name: pov.clone(),
            username: username.clone(),
            player_id: meta
                .map(|m| m.player_id)
                .unwrap_or_else(|| username.clone()),
            connected,
        });
    }
    let mut runtime = runtime.unwrap_or_else(|| dataset::RuntimeInfo {
        family: "azalea".into(),
        topology: String::new(),
        azalea_version: String::new(),
    });
    if let Ok(swarm) = read_json::<SwarmSummary>(&capture_dir.join("swarm-summary.json")) {
        runtime.topology = swarm.runtime_topology;
    }
    let family = pawa_schema::catalog::scenario(&scenario_name)
        .map(|info| info.family.to_string())
        .unwrap_or_default();

    let record = CaptureRecord {
        episode_id: paths.episode_id.clone(),
        run_id: manifest.run_id.clone(),
        started_epoch_ms: outcome.started_epoch_ms,
        mc_version: config.minecraft_version.clone(),
        tick_rate_hz: 20,
        world: dataset::WorldInfo {
            seed: outcome.world.level_seed.clone(),
            level_type: outcome.world.level_type.clone(),
        },
        scenario: dataset::ScenarioRef {
            name: scenario_name,
            family,
            seed: scenario_seed,
            scenery: spec.scenario.scenery.clone(),
        },
        runtime,
        target_duration_ms: Some(pawa_schema::time::DurationMs(
            (spec.episode_duration_sec * 1000.0) as i64,
        )),
        actual_duration_ms: pawa_schema::time::DurationMs(actual_duration),
        recording_start_world_age_tick: outcome.recording_start_world_age_tick,
        recording_stop_world_age_tick: outcome.recording_stop_world_age_tick,
        recording_stop_world_age_by_player: outcome.stop_world_age_tick_by_player.clone(),
        povs,
        shards: outcome
            .shard_usage
            .iter()
            .map(|usage| ShardRecord {
                index: usage.shard,
                exit_code: usage.exit_code.unwrap_or(-1),
                peak_rss_kib: usage.peak_rss_kib.unwrap_or(0),
            })
            .collect(),
    };
    fs::write(
        capture_dir.join("capture.json"),
        serde_json::to_string_pretty(&record)? + "\n",
    )?;

    if !outcome.ok {
        bail!(
            "capture failed ({}): {}",
            outcome.failure_class.as_deref().unwrap_or("unknown"),
            outcome
                .failures
                .iter()
                .map(|f| f.code.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(UnitResult::Complete)
}

// ---------------------------------------------------------------------------------------------
// render units
// ---------------------------------------------------------------------------------------------

/// Outcome the unit wrapper turns into a status file + exit code.
pub enum UnitResult {
    Complete,
    SkippedNoConnect,
}

pub fn unit_render_pov(
    manifest: &RunManifest,
    paths: &EpisodePaths,
    pov: &str,
) -> Result<UnitResult> {
    let record = paths.capture_record()?;
    let entry = record
        .povs
        .iter()
        .find(|p| p.name == pov)
        .ok_or_else(|| contract(anyhow!("pov {pov} is not in the capture inventory")))?;
    if !entry.connected {
        return Ok(UnitResult::SkippedNoConnect);
    }
    let capture_dir = paths.capture_dir();
    let mcpr = capture_dir.join(format!("{pov}.mcpr"));
    let actions: Vec<ActionFrame> = read_jsonl(&capture_dir.join(format!("{pov}-actions.jsonl")))?;
    let start_pos = actions.first().map(|row| [row.x, row.y, row.z]);

    let job = JobSpec {
        player_id: Some(entry.username.clone()),
        camera_path: None,
        start_pos,
        episode_id: Some(paths.episode_id.clone()),
        render_id: Some(format!("{}-{pov}", paths.episode_id)),
    };
    run_render(manifest, paths, pov, PovKind::Player, None, &mcpr, job)
}

pub fn unit_render_observer(
    manifest: &RunManifest,
    paths: &EpisodePaths,
    observer: &str,
) -> Result<UnitResult> {
    let record = paths.capture_record()?;
    let spec = manifest
        .render
        .observers
        .iter()
        .find(|o| o.name == observer)
        .ok_or_else(|| contract(anyhow!("observer {observer} is not in the manifest")))?;
    let capture_dir = paths.capture_dir();

    // Player tracks for the crowd geometry, from every connected POV's dense stream.
    let mut tracks = Vec::new();
    for entry in record.povs.iter().filter(|p| p.connected) {
        let actions: Vec<ActionFrame> =
            read_jsonl(&capture_dir.join(format!("{}-actions.jsonl", entry.name)))?;
        tracks.push(PlayerTrack {
            pov: entry.name.clone(),
            positions: actions
                .iter()
                .filter_map(|row| row.world_age_tick.map(|age| (age, row.x, row.y, row.z)))
                .collect(),
        });
    }

    // The observer shoots inside a source POV's recording and inherits that POV's clock: the
    // join delta and the frame count both come from the source render, which the task graph
    // guarantees has already run. That guarantee only covers SAMPLED povs — under `pov_sample`
    // most povs never render, so the source must be drawn from the sampled set even though the
    // crowd geometry above rightly uses every connected track.
    let sampled: Vec<String> = crate::plan::sample_povs(
        &paths.episode_id,
        episode_spec(manifest, &paths.episode_id)?.player_count,
        manifest.render.pov_sample,
    );
    let source = match &spec.source_pov {
        Some(name) => name.clone(),
        // Preliminary choice by the planner below; it needs a frame count first, so bootstrap
        // from the most central rendered POV and let plan_observer confirm centrality.
        None => {
            let mut best: Option<(f64, String)> = None;
            for track in tracks.iter().filter(|t| sampled.contains(&t.pov)) {
                let (mut sx, mut sz, mut n) = (0.0, 0.0, 0.0);
                for &(_, x, _, z) in &track.positions {
                    sx += x;
                    sz += z;
                    n += 1.0;
                }
                if n == 0.0 {
                    continue;
                }
                let spread = (sx / n).hypot(sz / n);
                if best.as_ref().is_none_or(|(s, _)| spread < *s) {
                    best = Some((spread, track.pov.clone()));
                }
            }
            best.map(|(_, pov)| pov)
                .ok_or_else(|| anyhow!("no connected POV can host observer {observer}"))?
        }
    };
    let source_render: RenderRecord =
        read_json(&paths.render_dir(&source).join(RENDER_RECORD_NAME))?;
    let Some(source_media) = source_render.media.get("default") else {
        // The source POV failed or was skipped. An observer is additive — report and stand down
        // rather than failing the episode or burning retries on an impossible render.
        bail!("observer {observer}: source pov {source} has no usable default render");
    };
    let frame_count = source_media.frame_count;

    let meta: CaptureMeta = read_json(&capture_dir.join(format!("{source}-meta.json")))?;
    let events: Vec<ActionEvent> = read_jsonl(&capture_dir.join(format!("{source}-events.jsonl")))?;
    let join = metadata_join(
        &events,
        frame_count,
        manifest.render.fps,
        record.tick_rate_hz,
        6,
        anchors_for(&record, &meta),
    )
    .map_err(|e| anyhow!("observer {observer}: join unavailable: {}", e.reason))?;

    let plan = plan_observer(
        spec,
        &tracks,
        frame_count,
        manifest.render.fps,
        record.tick_rate_hz,
        join.display_delta,
    )
    .map_err(|e| anyhow!("observer {observer}: {e}"))?;

    let render_dir = paths.render_dir(observer);
    fs::create_dir_all(&render_dir)?;
    let camera_path = render_dir.join("camera-path.json");
    // Written BEFORE the render, so a failed render still leaves the requested path as evidence.
    fs::write(&camera_path, serde_json::to_string(&plan.path)? + "\n")?;

    let mcpr = capture_dir.join(format!("{}.mcpr", plan.source_pov));
    let job = JobSpec {
        player_id: None,
        camera_path: Some(camera_path),
        start_pos: None,
        episode_id: Some(paths.episode_id.clone()),
        render_id: Some(format!("{}-{observer}", paths.episode_id)),
    };
    run_render(
        manifest,
        paths,
        observer,
        PovKind::Observer,
        Some(plan.source_pov.clone()),
        &mcpr,
        job,
    )
}

fn run_render(
    manifest: &RunManifest,
    paths: &EpisodePaths,
    target: &str,
    kind: PovKind,
    source_pov: Option<String>,
    mcpr: &Path,
    job: JobSpec,
) -> Result<UnitResult> {
    let lane = Lane::from_env()?;
    let scratch = tempfile::tempdir().context("render scratch")?;
    let projections: Vec<ResolvedProjection> = manifest
        .render
        .projections
        .iter()
        .map(|p| ResolvedProjection::from_manifest(p, &manifest.render))
        .collect();
    let request = RenderRequest {
        mcpr,
        job,
        projections,
        camera: pov_camera(),
        fps: manifest.render.fps,
        encoding: RenderEncoding::Mp4,
        depth: manifest.render.depth,
        threads: manifest.render.threads_per_render,
        render_distance_chunks: manifest.render.render_distance_chunks,
        work_dir: scratch.path(),
    };
    let outcome = render(&lane, &request)?;

    // Promote everything the render produced into the shared work tier, then the record last —
    // its presence marks the copy complete.
    let render_dir = paths.render_dir(target);
    fs::create_dir_all(&render_dir)?;
    for entry in fs::read_dir(scratch.path())? {
        let entry = entry?;
        let destination = render_dir.join(entry.file_name());
        if entry.file_type()?.is_file() {
            fs::copy(entry.path(), destination)?;
        } else if entry.file_type()?.is_dir() {
            copy_dir_all(&entry.path(), &destination)?;
        }
    }
    let record = RenderRecord {
        target: target.to_string(),
        kind,
        media: outcome.media.clone(),
        depth: outcome.depth.clone(),
        fov_deg: outcome.pinned.fov_degrees,
        source_pov,
        failure: outcome.failure.as_ref().map(|f| f.class().to_string()),
    };
    fs::write(
        render_dir.join(RENDER_RECORD_NAME),
        serde_json::to_string_pretty(&record)? + "\n",
    )?;

    match outcome.failure {
        None => Ok(UnitResult::Complete),
        Some(failure) => bail!("render {target} failed: {failure}"),
    }
}

/// The POV camera: spectator attach, no static fallback — a video whose camera never follows the
/// player is a failed render, not a degraded one. Free-camera pose fields are driver-required
/// but inert outside static mode.
fn pov_camera() -> CameraArgs {
    CameraArgs {
        mode: CameraMode::Auto,
        allow_static: false,
        x: 0.0,
        y: 80.0,
        z: 0.0,
        yaw: 0.0,
        pitch: 30.0,
        dy: 4.0,
        dz: 6.0,
    }
}

// ---------------------------------------------------------------------------------------------
// qc-actions: the scenario dev-loop instrument
// ---------------------------------------------------------------------------------------------

/// Recursively copy one render output directory into the shared work tier.
fn copy_dir_all(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let destination = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_all(&entry.path(), &destination)?;
        } else {
            fs::copy(entry.path(), destination)?;
        }
    }
    Ok(())
}

/// Run the finalize-stage action QC directly over a capture directory and print the verdict.
/// Same [`check_episode`], same [`Thresholds::for_scenario`] — a scenario that passes here passes
/// finalize, by construction rather than by keeping two rulers in sync. The one finalize check
/// missing here is video darkness: it reads pixels, and a capture directory has no video yet.
pub fn qc_actions(capture_dir: &Path, scenario_override: Option<&str>) -> Result<bool> {
    let record: Option<CaptureRecord> = read_json(&capture_dir.join("capture.json")).ok();
    let scenario = scenario_override
        .map(str::to_string)
        .or_else(|| record.as_ref().map(|r| r.scenario.name.clone()))
        .unwrap_or_default();
    let tick_rate_hz = record.as_ref().map(|r| r.tick_rate_hz).unwrap_or(20);

    let mut povs: BTreeMap<String, Vec<ActionFrame>> = BTreeMap::new();
    for entry in fs::read_dir(capture_dir)
        .with_context(|| format!("read capture dir {}", capture_dir.display()))?
    {
        let name = entry?.file_name().to_string_lossy().into_owned();
        if let Some(pov) = name.strip_suffix("-actions.jsonl")
            && pov.len() >= 3
            && pov.starts_with('p')
            && pov[1..].bytes().all(|b| b.is_ascii_digit())
        {
            povs.insert(pov.to_string(), read_jsonl(&capture_dir.join(&name))?);
        }
    }

    let outcome = check_episode(&povs, &Thresholds::for_scenario(&scenario), tick_rate_hz);
    println!(
        "scenario {scenario} — {} povs at {tick_rate_hz} Hz",
        outcome.povs.len()
    );
    for (pov, pov_outcome) in &outcome.povs {
        let m = &pov_outcome.metrics;
        println!(
            "{pov}: frames {} moving {} | idle {:.1}% longest {:.1}s | frozen {:.1}% longest \
             {:.1}s stuck {:.1}s | camera {:.3}",
            m.frames,
            m.moving,
            m.idle_frac * 100.0,
            m.longest_idle_s,
            m.frozen_frac * 100.0,
            m.longest_frozen_s,
            m.stuck_total_s,
            m.camera_travel_agreement,
        );
    }
    for failure in &outcome.failures {
        println!("FAIL {failure}");
    }
    println!("{}", if outcome.ok() { "PASS" } else { "FAIL" });
    Ok(outcome.ok())
}

// ---------------------------------------------------------------------------------------------
// finalize
// ---------------------------------------------------------------------------------------------

/// Frames per second sampled for the video-darkness check. Darkness QC asks about multi-second
/// stretches (the failing bound is 45 s), which 2 samples/s resolves at ~1/30 of a full decode.
const DARK_SAMPLES_PER_S: u32 = 2;

/// The episode's dataset directory name — the HF layout syntax:
/// `<UTC start %Y%m%dT%H%M%SZ>-<scenario>-<N>p-<hash8>`, where `hash8` is the first 8 hex digits
/// of the capture record file's sha256. capture.json is written exactly once by the capture unit,
/// so a finalize retry recomputes the identical name.
fn dataset_dir_name(record: &CaptureRecord, bot_count: u32, capture_json: &Path) -> Result<String> {
    let ts = chrono::DateTime::from_timestamp_millis(record.started_epoch_ms.get())
        .unwrap_or_default()
        .format("%Y%m%dT%H%M%SZ");
    let digest = sha256_file(capture_json)?;
    Ok(format!(
        "{ts}-{}-{}p-{}",
        record.scenario.name,
        bot_count,
        &digest[..8]
    ))
}

/// Locate an assembled episode under the dataset root by its string id. The directory name is
/// content-derived, so identity comes from the `episode.json` inside — a linear scan over the
/// run's episodes, which number tens.
pub fn find_dataset_dir(out_root: &Path, episode_id: &str) -> Result<PathBuf> {
    let root = out_root.join("dataset");
    for entry in fs::read_dir(&root).with_context(|| format!("read {}", root.display()))? {
        let dir = entry?.path();
        if !dir.is_dir() {
            continue;
        }
        if let Ok(episode) = read_json::<EpisodeManifest>(&dir.join("episode.json"))
            && episode.episode_id == episode_id
        {
            return Ok(dir);
        }
    }
    bail!("no assembled episode {episode_id} under {}", root.display())
}

/// Layout file name for one projection of one POV: `<pov>-<projection>.mp4`, with the `equirect`
/// manifest shorthand spelled out the way the HF/solaris layout names it.
fn projection_file_name(pov: &str, projection: &str) -> String {
    let spelled = if projection == "equirect" {
        "equirectangular"
    } else {
        projection
    };
    format!("{pov}-{spelled}.mp4")
}

pub fn unit_finalize(manifest: &RunManifest, paths: &EpisodePaths) -> Result<UnitResult> {
    let record = paths.capture_record()?;
    let spec = episode_spec(manifest, &paths.episode_id)?;
    let capture_dir = paths.capture_dir();
    let dataset_dir = paths.dataset_root().join(dataset_dir_name(
        &record,
        spec.player_count,
        &capture_dir.join("capture.json"),
    )?);
    let fps = manifest.render.fps;
    // Episode-level constants the v3 rows repeat: synthetic wall-clock base, the row-level
    // integer episode id (manifest position — the string id stays in episode.json), and the
    // world seed (0 when non-numeric; the authoritative string lives in episode.json).
    let row_ctx = EpisodeRowCtx {
        fps,
        tick_rate_hz: record.tick_rate_hz,
        started_epoch_ms: record.started_epoch_ms,
        episode_seq: manifest
            .episodes
            .iter()
            .position(|e| e.episode_id == paths.episode_id)
            .unwrap_or(0) as i64,
        seed: record.world.seed.parse().unwrap_or(0),
    };

    // Which renders were planned: sampled POVs + observers, same pure functions as the plan.
    let planned_povs = crate::plan::sample_povs(
        &paths.episode_id,
        spec.player_count,
        manifest.render.pov_sample,
    );

    // ---- Phase A: read and judge. NOTHING is written or moved until the episode is known to
    // assemble — a finalize that fails after moving files destroys its own retry's inputs, which
    // is exactly what happened on the first cluster shakedown.
    // A handful of instances per finalize (one per POV), so the size skew is irrelevant.
    #[allow(clippy::large_enum_variant)]
    enum Prepared {
        Skipped,
        Ready {
            render_record: RenderRecord,
            join: pawa_align::join::Join,
            assembled: FramesOutcome,
            events: Vec<ActionEvent>,
            actions: Vec<ActionFrame>,
        },
    }
    let mut prepared: Vec<(String, String, Prepared)> = Vec::new(); // (pov, username, ...)
    let mut failures: Vec<String> = Vec::new();

    for pov in &planned_povs {
        let entry =
            record.povs.iter().find(|p| &p.name == pov).ok_or_else(|| {
                contract(anyhow!("planned pov {pov} missing from capture inventory"))
            })?;
        if !entry.connected {
            // Confirm the render unit agreed (its status file must exist and say skipped).
            let status =
                crate::status::read_status(&paths.status_path(&format!("{pov}.render.json")))
                    .map_err(contract)?;
            if !matches!(status.outcome, UnitOutcome::SkippedNoConnect) {
                return Err(contract(anyhow!(
                    "pov {pov}: capture says never-connected but its render status disagrees"
                )));
            }
            prepared.push((pov.clone(), entry.username.clone(), Prepared::Skipped));
            continue;
        }

        let render_dir = paths.render_dir(pov);
        let render_record: RenderRecord = read_json(&render_dir.join(RENDER_RECORD_NAME))?;
        if let Some(class) = &render_record.failure {
            failures.push(format!("{pov}: render failed ({class})"));
            continue;
        }

        let meta: CaptureMeta = read_json(&capture_dir.join(format!("{pov}-meta.json")))?;
        let events: Vec<ActionEvent> =
            read_jsonl(&capture_dir.join(format!("{pov}-events.jsonl")))?;
        let actions: Vec<ActionFrame> =
            read_jsonl(&capture_dir.join(format!("{pov}-actions.jsonl")))?;
        let frame_count = render_record
            .media
            .get("default")
            .map(|m| m.frame_count)
            .ok_or_else(|| anyhow!("{pov}: render record has no default projection"))?;

        match metadata_join(
            &events,
            frame_count,
            fps,
            record.tick_rate_hz,
            6,
            anchors_for(&record, &meta),
        ) {
            Err(unavailable) => {
                failures.push(format!(
                    "{pov}: alignment unavailable: {}",
                    unavailable.reason
                ));
            }
            Ok(join) => {
                if !join.failures.is_empty() {
                    failures.extend(
                        join.failures
                            .iter()
                            .map(|i| format!("{pov}: {}", i.message)),
                    );
                    continue;
                }
                let poses: PoseDoc = read_json(&render_dir.join("poses.json")).unwrap_or(PoseDoc {
                    source: None,
                    keyframes: Vec::new(),
                });
                let assembled = assemble_frames(
                    frame_count,
                    &row_ctx,
                    &poses,
                    &actions,
                    join.display_delta,
                    render_record.fov_deg,
                );
                prepared.push((
                    pov.clone(),
                    entry.username.clone(),
                    Prepared::Ready {
                        render_record,
                        join,
                        assembled,
                        events,
                        actions,
                    },
                ));
            }
        }
    }

    // A failed player POV fails the episode, and it fails HERE — the work tier is untouched, so
    // a retry (after a render resubmission) starts from intact inputs.
    if !failures.is_empty() {
        bail!("episode failed: {}", failures.join("; "));
    }

    // The common complete window: every SHIPPED player row must carry the full solaris payload
    // (a consumer indexes `row["action"]` unconditionally), but assembly leaves honest holes —
    // the render's lead-in frames before capture matched, and each bot's tail after its capture
    // stopped. The intersection is taken in WORLD-TICK space, not frame-index space: sharded
    // captures start their recordings ~14 ticks apart, so equal frame indices sit on different
    // world ticks across POVs. Each POV converts the common tick span to its own frame window,
    // which puts EVERY file's frame 0 on the same world tick — frame numbers are directly
    // comparable across POVs, and one wall base serves every file. A hole INSIDE the window is
    // a capture defect, not a boundary artifact, and fails the episode rather than shipping a
    // row a loader would crash on.
    let mut tick_start = i64::MIN;
    let mut tick_end = i64::MAX;
    for (pov, _, item) in &prepared {
        if let Prepared::Ready { assembled, .. } = item {
            let first = assembled.rows.iter().position(|row| row.action.is_some());
            let last = assembled.rows.iter().rposition(|row| row.action.is_some());
            match (first, last) {
                (Some(first), Some(last)) => {
                    tick_start = tick_start.max(assembled.rows[first].world_age_ticks.get());
                    tick_end = tick_end.min(assembled.rows[last].world_age_ticks.get());
                }
                _ => failures.push(format!("{pov}: no action-matched rows at all")),
            }
        }
    }
    if failures.is_empty() && (tick_end == i64::MAX || tick_start > tick_end) {
        // MAX means the loop above never saw a Ready player — an all-skipped episode has no
        // dataset to assemble.
        failures.push(format!(
            "no usable common window across player POVs (ticks {tick_start}..{tick_end})"
        ));
    }
    // A POV's frame window for the common tick span. Rows are one per frame with consecutive
    // world ticks (fps == tick rate), so the check on both endpoints proves the whole mapping;
    // it also rejects a POV whose rows don't cover the span (an observer render that started
    // too late or stopped too early).
    let pov_window = |rows: &[dataset::FrameRow]| -> Result<(u64, u64)> {
        let base = rows.first().map(|row| row.world_age_ticks.get());
        let (start, end) = match base {
            Some(base) => (tick_start - base, tick_end - base),
            None => (-1, -1),
        };
        if start < 0
            || rows.get(start as usize).map(|r| r.world_age_ticks.get()) != Some(tick_start)
            || rows.get(end as usize).map(|r| r.world_age_ticks.get()) != Some(tick_end)
        {
            bail!(
                "rows (first tick {base:?}, {} rows) do not cover the common tick window \
                 {tick_start}..{tick_end} one-per-tick",
                rows.len()
            );
        }
        Ok((start as u64, end as u64))
    };
    // `window_start_ref` is the largest per-POV frame offset: the earliest-started recording's
    // distance to the window, the best estimate of how long after the episode's wall start the
    // first shipped frame happened.
    let mut window_start_ref = 0u64;
    if failures.is_empty() {
        for (pov, _, item) in &prepared {
            if let Prepared::Ready { assembled, .. } = item {
                match pov_window(&assembled.rows) {
                    Ok((start, end)) => {
                        window_start_ref = window_start_ref.max(start);
                        if let Some(hole) = assembled.rows[start as usize..=end as usize]
                            .iter()
                            .position(|row| row.action.is_none())
                        {
                            failures.push(format!(
                                "{pov}: incomplete row inside the common window at frame {}",
                                start + hole as u64
                            ));
                        }
                    }
                    Err(err) => failures.push(format!("{pov}: {err}")),
                }
            }
        }
    }
    if !failures.is_empty() {
        bail!("episode failed: {}", failures.join("; "));
    }
    let window_len = (tick_end - tick_start + 1) as u64;
    // The dataset's clock starts at its first shipped frame. Every POV's frame 0 is the same
    // world tick, so this ONE base serves every file: renderTime stays a single linear function
    // of worldAgeTicks across the episode, and equal wall clocks mean the same world moment.
    let dataset_started =
        EpochMs(record.started_epoch_ms.get() + frame_time_ms(window_start_ref, fps));
    let dataset_duration = DurationMs(frame_time_ms(window_len, fps));

    // ---- Phase B: assemble. Everything below is writes over judged-good inputs.
    fs::create_dir_all(&dataset_dir)?;
    let diagnostics = dataset_dir.join("diagnostics");
    fs::create_dir_all(&diagnostics)?;

    let mut pov_entries = Vec::new();
    let mut pov_qc: BTreeMap<String, PovQc> = BTreeMap::new();
    let mut first_shipped_tick: BTreeMap<String, i64> = BTreeMap::new();
    let mut all_events: Vec<(String, ActionEvent)> = Vec::new();
    let mut actions_by_pov: BTreeMap<String, Vec<ActionFrame>> = BTreeMap::new();
    let mut soft_failures: Vec<String> = Vec::new();

    for (pov, username, item) in prepared {
        match item {
            Prepared::Skipped => pov_entries.push(PovEntry {
                name: pov,
                kind: PovKind::Player,
                status: PovStatus::SkippedNoConnect,
                username: Some(username),
                source_pov: None,
                videos: BTreeMap::new(),
                depth: BTreeMap::new(),
            }),

            Prepared::Ready {
                render_record,
                join,
                mut assembled,
                events,
                actions,
            } => {
                let render_dir = paths.render_dir(&pov);
                let (win_start, win_end) =
                    pov_window(&assembled.rows).with_context(|| format!("{pov}: trim window"))?;
                trim_rows(
                    &mut assembled.rows,
                    win_start,
                    win_end,
                    fps,
                    dataset_started,
                );
                // Measured from the shipped rows, not from `tick_start`: the spread gate is a
                // tripwire on the artifact itself, 0 unless the tick-aligned trim regresses.
                if let Some(row) = assembled.rows.first() {
                    first_shipped_tick.insert(pov.clone(), row.world_age_ticks.get());
                }
                let (videos, depth) = promote_pov(
                    &dataset_dir,
                    &pov,
                    &render_dir,
                    &render_record,
                    &assembled.rows,
                    (win_start, window_len),
                    &manifest.render,
                )?;

                fs::write(
                    diagnostics.join(format!("{pov}-alignment-events.json")),
                    serde_json::to_string_pretty(&join.events)? + "\n",
                )?;
                pov_qc.insert(
                    pov.clone(),
                    PovQc {
                        alignment: Some(join.qc.clone()),
                        degeneracy: None,
                        motion: None,
                    },
                );
                pov_entries.push(PovEntry {
                    name: pov.clone(),
                    kind: PovKind::Player,
                    status: PovStatus::Complete,
                    username: Some(username),
                    source_pov: None,
                    videos,
                    depth,
                });
                for event in events.iter().cloned() {
                    all_events.push((pov.clone(), event));
                }
                actions_by_pov.insert(pov, actions);
            }
        }
    }

    // Observers: additive. A broken observer is a failed PovEntry, never a failed episode.
    for observer in &manifest.render.observers {
        let render_dir = paths.render_dir(&observer.name);
        let status = crate::status::read_status(
            &paths.status_path(&format!("{}.render.json", observer.name)),
        )
        .map_err(contract)?;
        let record_path = render_dir.join(RENDER_RECORD_NAME);
        let render_record: Option<RenderRecord> = record_path
            .is_file()
            .then(|| read_json(&record_path))
            .transpose()?;
        let usable = matches!(status.outcome, UnitOutcome::Complete)
            && render_record.as_ref().is_some_and(|r| r.failure.is_none());
        // The whole promotion is one fallible block: any surprise inside it (missing source pov,
        // a render shorter than the players' common window, a failed trim) downgrades this
        // observer, never the episode.
        let promoted = match (usable, render_record) {
            (true, Some(render_record)) => {
                (|| -> Result<PromotedObserverMedia> {
                    let source = render_record.source_pov.clone().unwrap_or_default();
                    let source_meta: CaptureMeta =
                        read_json(&capture_dir.join(format!("{source}-meta.json")))?;
                    let source_events: Vec<ActionEvent> =
                        read_jsonl(&capture_dir.join(format!("{source}-events.jsonl")))?;
                    let frame_count = render_record
                        .media
                        .get("default")
                        .map(|m| m.frame_count)
                        .unwrap_or(0);
                    let join = metadata_join(
                        &source_events,
                        frame_count,
                        fps,
                        record.tick_rate_hz,
                        6,
                        anchors_for(&record, &source_meta),
                    )
                    .map_err(|e| anyhow!("observer {}: {}", observer.name, e.reason))?;
                    let poses: PoseDoc = read_json(&render_dir.join("poses.json"))?;
                    let mut assembled = assemble_frames(
                        frame_count,
                        &row_ctx,
                        &poses,
                        &[],
                        join.display_delta,
                        render_record.fov_deg,
                    );
                    // `pov_window` rejects a render that started after or stopped before the
                    // players' common tick span, downgrading the observer.
                    let (win_start, win_end) = pov_window(&assembled.rows)
                        .map_err(|e| anyhow!("observer {}: {e}", observer.name))?;
                    trim_rows(
                        &mut assembled.rows,
                        win_start,
                        win_end,
                        fps,
                        dataset_started,
                    );
                    let (videos, depth) = promote_pov(
                        &dataset_dir,
                        &observer.name,
                        &render_dir,
                        &render_record,
                        &assembled.rows,
                        (win_start, window_len),
                        &manifest.render,
                    )?;
                    Ok((videos, depth, render_record.source_pov.clone()))
                })()
            }
            _ => Err(anyhow!("unusable render")),
        };
        match promoted {
            Ok((videos, depth, source_pov)) => {
                pov_entries.push(PovEntry {
                    name: observer.name.clone(),
                    kind: PovKind::Observer,
                    status: PovStatus::Complete,
                    username: None,
                    source_pov,
                    videos,
                    depth,
                });
            }
            Err(err) => {
                soft_failures.push(format!(
                    "observer {}: {err:#} (additive, episode continues)",
                    observer.name
                ));
                pov_entries.push(PovEntry {
                    name: observer.name.clone(),
                    kind: PovKind::Observer,
                    status: PovStatus::Failed,
                    username: None,
                    source_pov: None,
                    videos: BTreeMap::new(),
                    depth: BTreeMap::new(),
                });
            }
        }
    }

    // <pov>-episode_info.json: the solaris per-bot manifest projection, one per complete POV,
    // written once the whole POV inventory is known (peer_encountered_error looks sideways).
    // `encountered_error: false` is DERIVED, not asserted: the window checks above already
    // failed the episode if any shipped player row were incomplete.
    let started_rfc3339 = chrono::DateTime::from_timestamp_millis(dataset_started.get())
        .unwrap_or_default()
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    for entry in pov_entries
        .iter()
        .filter(|p| p.status == PovStatus::Complete)
    {
        let peer_error = pov_entries
            .iter()
            .any(|other| other.name != entry.name && other.status == PovStatus::Failed);
        let info = dataset::SolarisEpisodeInfo {
            timestamp: started_rfc3339.clone(),
            bot_name: entry.username.clone().unwrap_or_else(|| entry.name.clone()),
            episode_number: row_ctx.episode_seq,
            episode_type: record.scenario.name.clone(),
            instance_id: paths.episode_id.clone(),
            encountered_error: false,
            peer_encountered_error: peer_error,
            bot_died: false,
            episode_recording_started: true,
            eval_metadata: BTreeMap::new(),
        };
        fs::write(
            dataset_dir.join(format!("{}-episode_info.json", entry.name)),
            serde_json::to_string_pretty(&info)? + "\n",
        )?;
    }

    // events.jsonl: the union of every POV's discrete events, ordered by world age. Events without
    // a world-age tick cannot be placed on the shared clock and stay in diagnostics only.
    let mut event_rows: Vec<EventRow> = Vec::new();
    let mut unplaced_events = 0u64;
    for (pov, event) in &all_events {
        match event.world_age_tick {
            None => unplaced_events += 1,
            Some(age) => event_rows.push(EventRow {
                seq: 0, // assigned after the sort below
                event_id: event
                    .visible_event_id
                    .clone()
                    .unwrap_or_else(|| event.action_id.clone()),
                pov: pov.clone(),
                action_type: event.action_type.clone(),
                world_age_tick: age,
                server_tick: event.server_tick,
                description: event.payload.description.clone(),
                payload: event.payload.extra.clone().into_iter().collect(),
            }),
        }
    }
    event_rows.sort_by(|a, b| {
        (a.world_age_tick, &a.pov, &a.event_id).cmp(&(b.world_age_tick, &b.pov, &b.event_id))
    });
    for (seq, row) in event_rows.iter_mut().enumerate() {
        row.seq = seq as u64;
    }
    write_jsonl(&dataset_dir.join("events.jsonl"), &event_rows)?;

    // qc.json: degeneracy over the dense streams + motion/separation from the swarm summary +
    // the per-POV alignment blocks gathered above.
    let thresholds = Thresholds::for_scenario(&record.scenario.name);
    let episode_qc = check_episode(&actions_by_pov, &thresholds, record.tick_rate_hz);
    soft_failures.extend(episode_qc.failures.iter().cloned());
    // Cross-POV time-base guard: the shipped files' first rows must all sit on the same world
    // tick (the tick-aligned trim guarantees it by construction; a spread here means the trim
    // regressed, e.g. back to frame-index windows — sharded captures start ~14 ticks apart).
    let (anchor_spread, spread_failures) =
        check_pov_anchor_spread(&first_shipped_tick, &thresholds);
    soft_failures.extend(spread_failures);
    let swarm: Option<SwarmSummary> = read_json(&capture_dir.join("swarm-summary.json")).ok();
    for (pov, outcome) in &episode_qc.povs {
        let slot = pov_qc.entry(pov.clone()).or_insert(PovQc {
            alignment: None,
            degeneracy: None,
            motion: None,
        });
        let mut degeneracy = outcome.metrics.clone();
        // Video darkness, the one check the action stream cannot carry: measured on the promoted
        // default projection only (equirect shares the same world light, and its spherical pixel
        // distribution would need its own calibration). Sampled at DARK_SAMPLES_PER_S.
        let video = dataset_dir.join(format!("{pov}-default.mp4"));
        if video.is_file() {
            let luma = pawa_render::sample_luma(&video, DARK_SAMPLES_PER_S)?;
            let (dark_frac, longest_dark_s, dark_failures) =
                check_pov_darkness(pov, &luma, f64::from(DARK_SAMPLES_PER_S), &thresholds);
            degeneracy.dark_frame_frac = Some(dark_frac);
            degeneracy.longest_dark_run_s = Some(longest_dark_s);
            soft_failures.extend(dark_failures);
        }
        slot.degeneracy = Some(degeneracy);
        if let (Some(swarm), Some(entry)) = (&swarm, record.povs.iter().find(|p| &p.name == pov)) {
            slot.motion = swarm
                .players
                .iter()
                .find(|p| p.username == entry.username)
                .map(|p| p.motion_metrics.clone());
        }
    }
    let qc = QcReport {
        verdict: if soft_failures.is_empty() {
            QcVerdict::Pass
        } else {
            QcVerdict::Fail
        },
        failures: soft_failures.clone(),
        dead_keys: episode_qc.dead_keys.clone(),
        pair_agreement_active: episode_qc.pair_agreement_active,
        camera_travel_agreement: episode_qc.camera_travel_agreement,
        pov_anchor_spread_ticks: (!first_shipped_tick.is_empty()).then_some(anchor_spread),
        separation: swarm
            .as_ref()
            .map(|s| s.separation_metrics.clone())
            .unwrap_or_else(empty_separation),
        povs: pov_qc,
    };
    fs::write(
        dataset_dir.join("qc.json"),
        serde_json::to_string_pretty(&qc)? + "\n",
    )?;

    // episode.json last: its presence marks the dataset dir assembled.
    let episode = EpisodeManifest {
        schema_version: dataset::DATASET_SCHEMA_VERSION,
        episode_id: paths.episode_id.clone(),
        run_id: record.run_id.clone(),
        scenario: record.scenario.clone(),
        mc_version: record.mc_version.clone(),
        runtime: record.runtime.clone(),
        tick_rate_hz: record.tick_rate_hz,
        fps,
        // The trimmed dataset's own clock, not capture's: rows, videos and this manifest all
        // describe the common complete window. The trim aligns every POV's frame 0 to the same
        // world tick, so this is EVERY complete file's `renderTime[0]`.
        started_epoch_ms: dataset_started,
        target_duration_ms: record.target_duration_ms.unwrap_or_default(),
        actual_duration_ms: dataset_duration,
        world: record.world.clone(),
        povs: pov_entries,
    };
    fs::write(
        dataset_dir.join("episode.json"),
        serde_json::to_string_pretty(&episode)? + "\n",
    )?;
    if unplaced_events > 0 {
        fs::write(
            diagnostics.join("unplaced-events.json"),
            serde_json::json!({"count": unplaced_events}).to_string() + "\n",
        )?;
    }

    // Full diagnostics retention: everything left in the work tier moves under diagnostics/.
    move_tree(&paths.work_dir(), &diagnostics)?;
    Ok(UnitResult::Complete)
}

/// Promote one POV's dataset files out of its render dir.
fn promote_pov(
    dataset_dir: &Path,
    pov: &str,
    render_dir: &Path,
    record: &RenderRecord,
    rows: &[pawa_schema::dataset::FrameRow],
    window: (u64, u64),
    render_cfg: &pawa_schema::manifest::RenderConfig,
) -> Result<(
    BTreeMap<String, MediaInfo>,
    BTreeMap<String, pawa_schema::dataset::DepthInfo>,
)> {
    let (window_start, window_len) = window;
    let mut videos = BTreeMap::new();
    for (projection, media) in &record.media {
        let file_name = projection_file_name(pov, projection);
        let source = render_dir.join(&media.path);
        let target = dataset_dir.join(&file_name);
        let described = if !source.is_file() && target.is_file() {
            let described = pawa_render::describe_video(&target, render_cfg.fps, media.spherical)?;
            if described.frame_count != window_len {
                bail!(
                    "{pov}/{projection}: promoted video has {} frames, window is {window_len}",
                    described.frame_count
                );
            }
            described
        } else {
            let described = pawa_render::trim_video(
                &source,
                &target,
                window_start,
                window_len,
                render_cfg.fps,
                &render_cfg.encoder,
                &render_cfg.preset,
                render_cfg.crf,
                media.spherical,
            )?;
            fs::remove_file(&source)?;
            described
        };
        let mut described = described;
        described.path = file_name;
        videos.insert(projection.clone(), described);
    }
    let mut depth = BTreeMap::new();
    for (projection, info) in &record.depth {
        if info.frame_count < window_start + window_len {
            bail!(
                "{pov}/{projection}: depth sequence has {} frames, trim needs {}",
                info.frame_count,
                window_start + window_len
            );
        }
        let source = render_dir.join(&info.path);
        let target_name = format!("{pov}-{projection}-depth");
        let target = dataset_dir.join(&target_name);
        trim_depth_sequence(&source, &target, window_start, window_len)?;
        let mut promoted = info.clone();
        promoted.path = target_name;
        promoted.frame_count = window_len;
        depth.insert(projection.clone(), promoted);
    }
    write_jsonl(&dataset_dir.join(format!("{pov}-frames.jsonl")), rows)?;
    Ok((videos, depth))
}

fn trim_depth_sequence(source: &Path, target: &Path, start: u64, len: u64) -> Result<()> {
    if target.is_dir() {
        let existing = fs::read_dir(target)?.filter_map(|entry| entry.ok()).count() as u64;
        if existing == len {
            return Ok(());
        }
        fs::remove_dir_all(target)?;
    }
    fs::create_dir_all(target)?;
    for output_index in 0..len {
        let input_index = start + output_index;
        let input = source.join(format!("{input_index}.depth.png"));
        let output = target.join(format!("{output_index}.depth.png"));
        fs::copy(&input, &output)
            .with_context(|| format!("copy depth frame {}", input.display()))?;
    }
    fs::remove_dir_all(source)?;
    Ok(())
}

#[cfg(test)]
mod depth_tests {
    use super::*;

    #[test]
    fn trims_and_renumbers_depth_frames() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let target = root.path().join("target");
        fs::create_dir_all(&source).unwrap();
        for index in 0..5 {
            fs::write(source.join(format!("{index}.depth.png")), [index as u8]).unwrap();
        }
        trim_depth_sequence(&source, &target, 2, 2).unwrap();
        assert_eq!(fs::read(target.join("0.depth.png")).unwrap(), [2]);
        assert_eq!(fs::read(target.join("1.depth.png")).unwrap(), [3]);
        assert!(!target.join("2.depth.png").exists());
        assert!(!source.exists());
    }
}

fn empty_separation() -> dataset::SeparationMetrics {
    dataset::SeparationMetrics {
        evaluated_player_frames: 0,
        crowd_frame_count: 0,
        crowd_frame_ratio: None,
        overlap_frame_count: 0,
        overlap_frame_ratio: None,
        min_nearest_neighbor_2d: None,
        median_nearest_neighbor_2d: None,
        p05_nearest_neighbor_2d: None,
        p95_nearest_neighbor_2d: None,
    }
}

use pawa_render::sha256_file;

fn move_tree(from: &Path, to: &Path) -> Result<()> {
    fn walk(from: &Path, to: &Path) -> Result<()> {
        fs::create_dir_all(to)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            let target = to.join(entry.file_name());
            if entry.file_type()?.is_dir() {
                walk(&entry.path(), &target)?;
            } else {
                fs::rename(entry.path(), &target).or_else(|_| {
                    fs::copy(entry.path(), &target)
                        .map(|_| ())
                        .and_then(|_| fs::remove_file(entry.path()))
                })?;
            }
        }
        Ok(())
    }
    walk(from, to)?;
    fs::remove_dir_all(from).ok();
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// hud (review runs only)
// ---------------------------------------------------------------------------------------------

/// Burn the action HUD into every complete player POV of an assembled episode, as
/// `<pov>-hud.mp4` beside the clean video. Runs after finalize; a review instrument, so observers
/// (no action rows) are left alone.
pub fn unit_hud(paths: &EpisodePaths) -> Result<UnitResult> {
    let dataset_dir = find_dataset_dir(&paths.out_root, &paths.episode_id)?;
    let episode: EpisodeManifest = read_json(&dataset_dir.join("episode.json"))?;
    for pov in &episode.povs {
        if pov.kind != PovKind::Player || pov.status != PovStatus::Complete {
            continue;
        }
        // Encode into local scratch, then copy — the shared mount is FUSE, and ffmpeg's mp4
        // muxer seeks back to write the header on finish, which FUSE rejects (the whole encode
        // succeeds and then exits non-zero). Same pattern as the render unit's scratch dir.
        let scratch = tempfile::tempdir().context("hud scratch")?;
        let local_out = scratch.path().join("hud.mp4");
        let stats = pawa_hud::render_hud(&pawa_hud::HudJob {
            video: &dataset_dir.join(format!("{}-default.mp4", pov.name)),
            frames: &dataset_dir.join(format!("{}-frames.jsonl", pov.name)),
            out: &local_out,
        })?;
        fs::copy(
            &local_out,
            dataset_dir.join(format!("{}-hud.mp4", pov.name)),
        )
        .with_context(|| format!("promote hud video for {}", pov.name))?;
        if stats.frames != stats.rows {
            bail!(
                "{}: hud drew {} frames over {} rows — video and frame table disagree",
                pov.name,
                stats.frames,
                stats.rows
            );
        }
    }
    Ok(UnitResult::Complete)
}

// ---------------------------------------------------------------------------------------------
// the wrapper: status + exit codes
// ---------------------------------------------------------------------------------------------

/// Run a unit body, write its status file, and return the process exit code.
pub fn run_unit(
    kind: UnitKind,
    paths: &EpisodePaths,
    target: Option<&str>,
    status_name: &str,
    body: impl FnOnce() -> Result<UnitResult>,
) -> i32 {
    let started = EpochMs::now();
    let (outcome, error, code) = match body() {
        Ok(UnitResult::Complete) => (UnitOutcome::Complete, None, 0),
        Ok(UnitResult::SkippedNoConnect) => (UnitOutcome::SkippedNoConnect, None, 0),
        Err(error) => {
            let class = if error.chain().any(|cause| cause.is::<ContractError>()) {
                FailureClass::Contract
            } else {
                FailureClass::Stage
            };
            let code = match class {
                FailureClass::Contract => 2,
                FailureClass::Stage => 3,
            };
            tracing::error!(unit = ?kind, %error, "unit failed");
            (
                UnitOutcome::Failed { class },
                Some(format!("{error:#}")),
                code,
            )
        }
    };
    let status = UnitStatus {
        unit: kind,
        episode_id: paths.episode_id.clone(),
        target: target.map(str::to_string),
        outcome,
        attempt: std::env::var("HQ_INSTANCE_ID")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .map(|v| v + 1)
            .unwrap_or(1),
        started_epoch_ms: started,
        finished_epoch_ms: EpochMs::now(),
        error,
    };
    if let Err(error) = crate::status::write_status(&paths.status_path(status_name), &status) {
        tracing::error!(%error, "could not write status file");
        return 3;
    }
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two capture slots must not share a port or a world directory — and slot 0's directory must
    /// not CONTAIN slot 1's, because every episode starts by wiping it.
    #[test]
    fn capture_slots_share_no_port_and_no_world() {
        let mut zero = CaptureConfig::default();
        let mut one = CaptureConfig::default();
        derive_capture_slot(&mut zero, 0).unwrap();
        derive_capture_slot(&mut one, 1).unwrap();

        assert_ne!(zero.server_port, one.server_port);
        assert_ne!(zero.rcon_port, one.rcon_port);
        assert_ne!(zero.server_port, one.rcon_port);
        assert_ne!(zero.rcon_port, one.server_port);
        assert_ne!(zero.server_workdir, one.server_workdir);
        assert!(
            !one.server_workdir.starts_with(&zero.server_workdir),
            "slot 0 wipes {:?}, which would take slot 1's {:?} with it",
            zero.server_workdir,
            one.server_workdir
        );

        // The slot index may not walk the server port into the rcon port's range.
        let span = CaptureConfig::default()
            .rcon_port
            .abs_diff(CaptureConfig::default().server_port);
        assert!(derive_capture_slot(&mut CaptureConfig::default(), span).is_err());
    }

    /// Round trip of the video-darkness gate: synthesize a clip whose luma is known by
    /// construction (4 s of black, then 4 s of mid-gray), sample it the way finalize does, judge
    /// it. The one check that fails if the ffmpeg plumbing, the YAVG parsing, or the run
    /// accounting breaks.
    #[test]
    fn dark_video_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let video = dir.path().join("video.mp4");
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=64x64:d=4:r=10",
                "-f",
                "lavfi",
                "-i",
                "color=c=gray:s=64x64:d=4:r=10",
                "-filter_complex",
                "[0][1]concat=n=2:v=1",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&video)
            .status()
            .expect("ffmpeg is already a hard dependency of the render stage");
        assert!(status.success());

        let luma = pawa_render::sample_luma(&video, DARK_SAMPLES_PER_S).unwrap();
        assert_eq!(luma.len(), 16, "8 s at 2 samples/s, got {luma:?}");

        let thresholds = Thresholds::default();
        let (dark_frac, longest_dark_s, failures) =
            check_pov_darkness("p00", &luma, f64::from(DARK_SAMPLES_PER_S), &thresholds);
        // Half black in one 4 s run: limited-range black encodes at Y=16 (under the 24 bound),
        // 0x808080 gray at ~126 (far over it).
        assert!((0.4..=0.6).contains(&dark_frac), "dark_frac {dark_frac}");
        assert!(
            (3.0..=5.0).contains(&longest_dark_s),
            "longest_dark_run_s {longest_dark_s}"
        );
        // The tightened default (0.25 frac) rejects a half-black POV; the 4 s run stays under
        // the 20 s run bound, so exactly the frac violation fires.
        assert_eq!(failures.len(), 1, "got {failures:?}");
        assert!(failures[0].contains("near-black"), "got {failures:?}");

        // The same clip against bounds it satisfies must pass clean.
        let lenient = Thresholds {
            max_dark_frame_frac: 0.6,
            max_dark_run_s: 10.0,
            ..thresholds.clone()
        };
        let (_, _, failures) =
            check_pov_darkness("p00", &luma, f64::from(DARK_SAMPLES_PER_S), &lenient);
        assert!(failures.is_empty(), "got {failures:?}");

        // And against bounds it exceeds on both axes it must report both violations.
        let strict = Thresholds {
            max_dark_frame_frac: 0.25,
            max_dark_run_s: 2.0,
            ..thresholds
        };
        let (_, _, failures) =
            check_pov_darkness("p00", &luma, f64::from(DARK_SAMPLES_PER_S), &strict);
        assert_eq!(failures.len(), 2, "got {failures:?}");
    }
}
