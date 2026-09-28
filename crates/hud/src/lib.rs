//! Burn an action HUD into one POV's video, so a human can *see* whether the actions line up with
//! the pixels.
//!
//! This is a diagnostic instrument, not a pipeline stage. Nothing downstream reads its output; a
//! person watches it and decides whether the episode's action↔video alignment is real. That is the
//! whole reason it draws faithfully rather than quickly, and the reason rows with no action are
//! drawn as a conspicuous dead panel instead of being skipped — where coverage ends is exactly the
//! thing the viewer is looking for.
//!
//! # Input is already aligned
//!
//! It takes a POV's `<pov>-default.mp4` and `<pov>-frames.jsonl` (v3,
//! [`pawa_schema::dataset::FrameRow`]) and joins them by position: row `i` describes video
//! frame `i`, and each row already carries the action sampled at its own world-age tick. There is no
//! delta solver here and there must never be one. The dataset bakes the chosen alignment into
//! every row and keeps the rejected anchor candidates in `qc.json` where they cannot be mistaken for
//! the answer — a HUD that re-derived its own alignment could disagree with the corpus it is
//! supposed to be auditing and draw the wrong frame's keys.
//!
//! # How a frame is made
//!
//! ffmpeg decodes to raw `rgb24` and a second ffmpeg re-encodes from raw `rgb24`; in between, each
//! frame gets two RGBA pixmaps alpha-composited into its bottom-left corner (see [`overlay`]). The
//! static, state-driven layer is cached — a bot holds one key for hundreds of frames, so the same
//! panel is asked for over and over.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};
use ffmpeg_sidecar::command::FfmpegCommand;
use pawa_schema::dataset::FrameRow;
use resvg::tiny_skia::{Pixmap, Transform};
use resvg::usvg;

pub mod overlay;

/// The ffmpeg to run. Resolved through `PATH` — the `download_ffmpeg` feature is disabled in
/// `Cargo.toml`, so this crate can never substitute a downloaded binary for the system one.
const FFMPEG: &str = "ffmpeg";

/// Panel width as a fraction of the video width, and the floor below which it stops shrinking.
///
/// A HUD that scales purely with the video becomes unreadable on a small render, and the point of
/// the thing is to be read. On a 640-wide video the floor makes the panel half the frame; that is
/// the correct trade for an instrument.
const PANEL_WIDTH_FRACTION: u32 = 3;
const PANEL_MIN_WIDTH: u32 = 320;

/// Ceiling on cached panel pixmaps.
// ponytail: fixed-cap cache, no eviction — once full, later states just re-render. With the camera
// dial kept in the uncached layer the distinct state count is small (key combinations × hotbar slot
// × held item), so this is reached only by a POV that cycles held items constantly. Swap in an LRU
// if a real episode ever tops it out.
const CACHE_CAP: usize = 512;

/// One POV's HUD render.
pub struct HudJob<'a> {
    /// The POV's default-projection video.
    pub video: &'a Path,
    /// The POV's `frames.jsonl` — one [`FrameRow`] per line, in video-frame order.
    pub frames: &'a Path,
    /// Destination mp4. Overwritten if it exists.
    pub out: &'a Path,
}

/// What the render actually did. Read it: `rows` and `frames` disagreeing means the video and the
/// frame table describe different things, which invalidates every judgement made from the output.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HudStats {
    /// Video frames decoded, drawn and written.
    pub frames: u64,
    /// Rows read from `frames.jsonl`.
    pub rows: u64,
    /// Of those, rows whose `action` was `None` — drawn as the dead `NO ACTION` panel. This is the
    /// size of the coverage hole the viewer is about to see.
    pub rows_missing_action: u64,
    /// Frames whose static panel came from the cache rather than being rasterized again.
    pub cache_hits: u64,
}

/// Render `job.video` with the action HUD burned in, writing `job.out`.
///
/// Fails if `frames.jsonl` has fewer rows than the video has frames: a HUD drawn from a short frame
/// table would silently show stale state for the tail, which is precisely the class of error this
/// tool exists to catch.
pub fn render_hud(job: &HudJob) -> Result<HudStats> {
    let rows = read_frames(job.frames)?;
    if rows.is_empty() {
        bail!("{} has no rows", job.frames.display());
    }
    let full_scale = overlay::camera_full_scale(&rows);

    let mut options = usvg::Options {
        font_family: "DejaVu Sans".to_string(),
        ..usvg::Options::default()
    };
    options.fontdb_mut().load_system_fonts();

    let video = path_str(job.video)?;
    let out = path_str(job.out)?;

    let mut decoder = FfmpegCommand::new_with_path(FFMPEG)
        .hide_banner()
        .input(video)
        .rawvideo()
        .spawn()
        .with_context(|| format!("spawning {FFMPEG} to decode {video}"))?;
    let mut events = decoder.iter().context("reading decoder output")?;
    // Drains stderr up to the point ffmpeg has described its streams; the frame thread only starts
    // afterwards, so no frame can be lost to this call.
    let meta = events
        .collect_metadata()
        .with_context(|| format!("{video} did not report a decodable video stream"))?;
    let fps = meta
        .output_streams
        .iter()
        .find_map(|stream| stream.video_data())
        .map(|video| video.fps)
        .filter(|fps| *fps > 0.0)
        .with_context(|| format!("{video} reported no video frame rate"))?;

    let mut cache: HashMap<String, Pixmap> = HashMap::new();
    let mut stats = HudStats {
        rows: rows.len() as u64,
        rows_missing_action: rows.iter().filter(|row| row.action.is_none()).count() as u64,
        ..HudStats::default()
    };
    // The encoder cannot be built until the first decoded frame tells us the true frame size, so it
    // and its stdin arrive together, lazily.
    let mut encoder = None;

    for frame in events.filter_frames() {
        let index = stats.frames as usize;
        let Some(row) = rows.get(index) else {
            bail!(
                "{} has {} rows but the video has more frames (at frame {index}); \
                 the frame table and the video are not the same recording",
                job.frames.display(),
                rows.len()
            );
        };
        let (width, height) = (frame.width, frame.height);
        let mut pixels = frame.data;

        let (panel_w, panel_h) = panel_size(width);
        let margin = (12.0 * panel_w as f32 / overlay::PANEL_W).round() as u32;
        let origin_y = height.saturating_sub(panel_h + margin);

        // The SVG source doubles as the cache key. Keying on the drawing itself means the key can
        // never drift out of step with what gets drawn, which a hand-maintained key struct would.
        let key = overlay::static_layer(row);
        // Holds the panel only when the cache is full and this one is therefore not kept.
        let mut uncached = None;
        let panel = if cache.contains_key(&key) {
            stats.cache_hits += 1;
            &cache[&key]
        } else {
            let panel = rasterize(&key, &options, panel_w, panel_h)?;
            if cache.len() < CACHE_CAP {
                cache.entry(key).or_insert(panel)
            } else {
                uncached.insert(panel)
            }
        };
        composite(&mut pixels, width, height, panel, margin, origin_y);

        let dial = overlay::dynamic_layer(row, full_scale);
        let dial = rasterize(&dial, &options, panel_w, panel_h)?;
        composite(&mut pixels, width, height, &dial, margin, origin_y);

        // The encoder needs the true frame size, which only a decoded frame can give, so it starts
        // on the first one rather than from the container's declared dimensions.
        if encoder.is_none() {
            encoder = Some(start_encoder(out, width, height, fps)?);
        }
        encoder
            .as_mut()
            .expect("encoder was just started")
            .stdin
            .write_all(&pixels)
            .with_context(|| format!("writing frame {index} to {FFMPEG}"))?;
        stats.frames += 1;
    }

    let Some(encoder) = encoder else {
        bail!("{video} decoded zero frames");
    };
    finish(encoder)?;

    if stats.frames < stats.rows {
        bail!(
            "{} has {} rows but only {} video frames were decoded; the frame table and the video \
             are not the same recording",
            job.frames.display(),
            stats.rows,
            stats.frames
        );
    }
    Ok(stats)
}

/// Read `frames.jsonl` whole. A row is ~200 bytes and an episode is minutes long, so even an hour of
/// 20 FPS footage is a handful of megabytes — worth it to get the row count and the camera
/// calibration ([`overlay::camera_full_scale`]) before the first frame is drawn.
fn read_frames(path: &Path) -> Result<Vec<FrameRow>> {
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut rows = Vec::new();
    for (number, line) in BufReader::new(file).lines().enumerate() {
        let line = line.with_context(|| format!("reading {}", path.display()))?;
        if line.trim().is_empty() {
            continue;
        }
        rows.push(
            serde_json::from_str(&line)
                .with_context(|| format!("{}:{}", path.display(), number + 1))?,
        );
    }
    Ok(rows)
}

/// Pixel size of the panel for a given video width, preserving the logical canvas aspect.
fn panel_size(video_width: u32) -> (u32, u32) {
    let width = (video_width / PANEL_WIDTH_FRACTION)
        .max(PANEL_MIN_WIDTH)
        .min(video_width);
    let height = (width as f32 * overlay::PANEL_H / overlay::PANEL_W).round() as u32;
    (width, height)
}

/// Parse an SVG document and render it into a fresh RGBA pixmap of the requested pixel size.
fn rasterize(svg: &str, options: &usvg::Options, width: u32, height: u32) -> Result<Pixmap> {
    let tree = usvg::Tree::from_str(svg, options).context("parsing the HUD SVG")?;
    let mut pixmap = Pixmap::new(width, height)
        .with_context(|| format!("allocating a {width}x{height} HUD pixmap"))?;
    let scale = Transform::from_scale(
        width as f32 / overlay::PANEL_W,
        height as f32 / overlay::PANEL_H,
    );
    resvg::render(&tree, scale, &mut pixmap.as_mut());
    Ok(pixmap)
}

/// Alpha-composite a HUD pixmap onto an `rgb24` frame at `(origin_x, origin_y)`, clipped to the
/// frame.
///
/// [`Pixmap`] stores colours *premultiplied* by alpha, which is exactly the form `src + dst·(1-a)`
/// wants — no division back out, and no rounding drift from doing so.
fn composite(
    frame: &mut [u8],
    frame_w: u32,
    frame_h: u32,
    hud: &Pixmap,
    origin_x: u32,
    origin_y: u32,
) {
    let (hud_w, hud_h) = (hud.width() as usize, hud.height() as usize);
    let (frame_w, frame_h) = (frame_w as usize, frame_h as usize);
    let src = hud.data();
    for y in 0..hud_h {
        let fy = origin_y as usize + y;
        if fy >= frame_h {
            break;
        }
        for x in 0..hud_w {
            let fx = origin_x as usize + x;
            if fx >= frame_w {
                break;
            }
            let s = (y * hud_w + x) * 4;
            let alpha = u32::from(src[s + 3]);
            if alpha == 0 {
                continue;
            }
            let d = (fy * frame_w + fx) * 3;
            let inverse = 255 - alpha;
            for channel in 0..3 {
                let under = u32::from(frame[d + channel]) * inverse;
                frame[d + channel] =
                    (u32::from(src[s + channel]) + (under + 127) / 255).min(255) as u8;
            }
        }
    }
}

/// The encoder process and the pipe we feed it. Its stderr is drained on a thread from the moment it
/// starts: ffmpeg writes progress there continuously, and a full stderr pipe deadlocks a process
/// that is also waiting on our stdin writes.
struct Encoder {
    child: ffmpeg_sidecar::child::FfmpegChild,
    stdin: std::process::ChildStdin,
    log: std::thread::JoinHandle<String>,
}

fn start_encoder(out: &str, width: u32, height: u32, fps: f32) -> Result<Encoder> {
    let mut child = FfmpegCommand::new_with_path(FFMPEG)
        .hide_banner()
        // Input side: raw frames exactly as `composite` left them.
        .format("rawvideo")
        .pix_fmt("rgb24")
        .size(width, height)
        .rate(fps)
        .input("-")
        // Output side. crf 18 because the HUD's thin strokes and 8px hotbar digits are the first
        // thing a lossier setting smears, and the smeared thing is the measurement.
        .codec_video("libx264")
        .preset("medium")
        .crf(18)
        .pix_fmt("yuv420p")
        .no_audio()
        .overwrite()
        .output(out)
        .spawn()
        .with_context(|| format!("spawning {FFMPEG} to encode {out}"))?;
    let stdin = child.take_stdin().context("encoder stdin was not piped")?;
    let mut stderr = child
        .take_stderr()
        .context("encoder stderr was not piped")?;
    let log = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });
    Ok(Encoder { child, stdin, log })
}

/// Close the pipe, wait for the encoder to finish the file, and surface its log if it failed.
fn finish(encoder: Encoder) -> Result<()> {
    let Encoder {
        mut child,
        stdin,
        log,
    } = encoder;
    drop(stdin);
    let status = child.wait().context("waiting for the encoder")?;
    let log = log.join().unwrap_or_default();
    if !status.success() {
        let tail: Vec<&str> = log.lines().rev().take(20).collect();
        bail!(
            "{FFMPEG} exited with {status}:\n{}",
            tail.into_iter().rev().collect::<Vec<_>>().join("\n")
        );
    }
    Ok(())
}

fn path_str(path: &Path) -> Result<&str> {
    path.to_str()
        .with_context(|| format!("{} is not valid UTF-8", path.display()))
}

#[cfg(test)]
mod tests {
    use pawa_schema::dataset::{CameraPose, ExtraInfo, FrameRow, InventorySnapshot, SolarisAction};
    use pawa_schema::time::{EpochMs, PlayerTimeMs, ReplayTimeMs, ServerTick, WorldAgeTick};

    use super::*;

    fn row(action: Option<SolarisAction>) -> FrameRow {
        let with_action = action.is_some();
        FrameRow {
            x: with_action.then_some(1.5),
            y: with_action.then_some(64.0),
            z: with_action.then_some(-2.5),
            yaw: with_action.then_some(0.0),
            pitch: with_action.then_some(0.0),
            action,
            inventory: with_action.then(|| InventorySnapshot {
                quick_bar_slot: 3,
                main_inventory: vec![],
                hotbar: vec![],
                held_item: None,
                off_hand_item: None,
            }),
            render_time: EpochMs(1_755_600_002_100),
            epoch_time: 1_755_600_002.1,
            relative_time_ms: ReplayTimeMs(2100),
            physics_time: with_action.then_some(PlayerTimeMs(2100)),
            world_age_ticks: WorldAgeTick(133_742),
            is_in_water: with_action.then_some(false),
            frame_count: 42,
            episode_id: 0,
            extra_info: ExtraInfo { seed: 2026 },
            camera_pose: Some(CameraPose {
                x: 1.5,
                y: 64.0,
                z: -2.5,
                yaw: 90.0,
                pitch: -10.0,
                roll: 0.0,
                fov_deg: 70.0,
            }),
            server_tick: with_action.then_some(ServerTick(900)),
            // An id with an XML metacharacter, to prove escaping keeps the document parseable.
            held_item: with_action.then(|| "minecraft:tools & torches".into()),
        }
    }

    /// Both layers must parse and put ink on the canvas at the requested size. No ffmpeg here — this
    /// covers the half of the pipeline that can be wrong silently, since a malformed SVG renders as
    /// a blank panel rather than an error the viewer would notice.
    #[test]
    fn both_layers_rasterize_to_a_non_empty_panel() {
        let mut options = usvg::Options::default();
        options.fontdb_mut().load_system_fonts();

        let sample = SolarisAction {
            forward: true,
            sprint: true,
            // Mineflayer radians; the dial converts back to notchian degrees for display.
            camera: [-0.001096, 0.000686],
            attack: true,
            ..SolarisAction::default()
        };
        let (width, height) = panel_size(1280);
        assert_eq!((width, height), (426, 256));

        for svg in [
            overlay::static_layer(&row(Some(sample.clone()))),
            overlay::static_layer(&row(None)),
            overlay::dynamic_layer(&row(Some(sample)), 3.6),
            overlay::dynamic_layer(&row(None), 3.6),
        ] {
            let pixmap = rasterize(&svg, &options, width, height).expect("rasterize");
            assert_eq!((pixmap.width(), pixmap.height()), (width, height));
            assert!(
                pixmap.data().as_chunks::<4>().0.iter().any(|px| px[3] > 0),
                "layer rendered fully transparent: {svg}"
            );
        }
    }
}
