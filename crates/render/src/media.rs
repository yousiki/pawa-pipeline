//! Turning what the driver left on disk into described media.
//!
//! The driver writes `<name>.mp4` per projection (or a `<name>/` directory of PNGs) and says
//! nothing about what is inside them. Everything a dataset needs to trust a video — codec,
//! dimensions, frame count, digest, whether the 360 metadata is really there — is measured HERE,
//! from the file, never copied from the request that asked for it.

use std::io::Read;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};
use pawa_schema::dataset::MediaInfo;
use pawa_schema::driver::{RenderEncoding, RenderMethod};
use pawa_schema::time::DurationMs;
use sha2::{Digest, Sha256};

use crate::{ResolvedProjection, spherical::inject_spherical};

/// What one pass over the driver's output found.
pub(crate) struct Collected {
    pub(crate) media: std::collections::BTreeMap<String, MediaInfo>,
    pub(crate) depth: std::collections::BTreeMap<String, pawa_schema::dataset::DepthInfo>,
    pub(crate) all_present: bool,
    pub(crate) all_spherical_ok: bool,
    pub(crate) frame_count: u64,
}

#[derive(Clone, Copy)]
pub(crate) struct DepthRequest {
    pub(crate) enabled: bool,
}

/// Describe every projection the driver was asked for, injecting spherical metadata as it goes.
///
/// Spherical injection happens BEFORE the digest is taken: the metadata is part of the file the
/// dataset ships, so hashing the pre-injection bytes would record a checksum of a file that no
/// longer exists.
pub(crate) fn collect(
    dir: &Path,
    projections: &[ResolvedProjection],
    encoding: RenderEncoding,
    fps: u32,
    depth_request: DepthRequest,
) -> Result<Collected> {
    let mut collected = Collected {
        media: std::collections::BTreeMap::new(),
        depth: std::collections::BTreeMap::new(),
        all_present: !projections.is_empty(),
        all_spherical_ok: true,
        frame_count: 0,
    };
    if depth_request.enabled {
        for projection in projections {
            let dir_path = dir.join(&projection.name);
            let frames = png_count_suffix(&dir_path, ".depth.png");
            if frames == 0 {
                collected.all_present = false;
                continue;
            }
            collected.depth.insert(
                projection.name.clone(),
                pawa_schema::dataset::DepthInfo {
                    path: projection.name.clone(),
                    width: projection.width,
                    height: projection.height,
                    frame_count: frames,
                    format: "float32_bgra_png".into(),
                },
            );
        }
    }
    for projection in projections {
        let path = dir.join(projection.media_name(encoding));
        if encoding == RenderEncoding::Png {
            // PNG output exists so the driver spec can express it; it is not a dataset render, so
            // it is checked for presence and otherwise left alone. `MediaInfo` describes one video
            // file and a frame directory is not one.
            let frames = png_count(&path);
            collected.all_present &= frames > 0;
            if frames > 0 {
                take_representative(projection, frames, &mut collected.frame_count);
            }
            continue;
        }

        let present = path
            .metadata()
            .is_ok_and(|meta| meta.is_file() && meta.len() > 0);
        collected.all_present &= present;
        if !present {
            tracing::warn!(projection = %projection.name, path = %path.display(), "no render output");
            continue;
        }

        let mut spherical = false;
        if projection.method == RenderMethod::Equirectangular {
            match inject_spherical(&path) {
                Ok(injected) => {
                    spherical = true;
                    tracing::info!(
                        projection = %projection.name,
                        verb = if injected { "injected" } else { "already present" },
                        "spherical metadata"
                    );
                }
                Err(err) => {
                    // A 360 video without its projection metadata plays as a distorted flat video
                    // in every consumer, so this fails the render rather than shipping quietly.
                    collected.all_spherical_ok = false;
                    tracing::warn!(projection = %projection.name, %err, "spherical injection failed");
                }
            }
        }

        let probe = ffprobe(&path)?;
        take_representative(projection, probe.frame_count, &mut collected.frame_count);
        collected.media.insert(
            projection.name.clone(),
            MediaInfo {
                // Driver-local name. The caller promotes the file into the episode directory and
                // rewrites this to the episode-relative path it landed at.
                path: projection.media_name(encoding),
                sha256: sha256_file(&path)?,
                container: "mp4".into(),
                codec: probe.codec,
                width: probe.width,
                height: probe.height,
                fps,
                frame_count: probe.frame_count,
                duration_ms: probe.duration_ms.unwrap_or(DurationMs(
                    (probe.frame_count as i64 * 1000) / fps.max(1) as i64,
                )),
                spherical,
            },
        );
    }
    Ok(collected)
}

/// The representative frame count is the flat projection's when the launch rendered one: every
/// projection of a launch shoots the same camera timeline, and `DEFAULT` is the reference.
fn take_representative(projection: &ResolvedProjection, frames: u64, current: &mut u64) {
    if *current == 0 || projection.method == RenderMethod::Default {
        *current = frames;
    }
}
fn png_count_suffix(dir: &Path, suffix: &str) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| {
            let path = entry.path();
            if path.is_dir() {
                png_count_suffix(&path, suffix)
            } else {
                u64::from(
                    path.to_string_lossy().ends_with(suffix)
                        && path.metadata().is_ok_and(|m| m.len() > 0),
                )
            }
        })
        .sum()
}

/// Non-empty PNGs anywhere under `dir`, matching what ReplayMod's frame export produces.
fn png_count(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| {
            let path = entry.path();
            if path.is_dir() {
                png_count(&path)
            } else {
                u64::from(
                    path.extension().is_some_and(|ext| ext == "png")
                        && path.metadata().is_ok_and(|meta| meta.len() > 0),
                )
            }
        })
        .sum()
}

struct Probe {
    codec: String,
    width: u32,
    height: u32,
    frame_count: u64,
    duration_ms: Option<DurationMs>,
}

/// Measure one encoded video with `ffprobe`.
///
/// Shelling out rather than linking a demuxer is deliberate: the render stage already depends on
/// the ffmpeg binaries the driver encodes with, so this adds no deployment surface, and `ffprobe`'s
/// JSON output is a stabler interface than any library's C ABI.
fn ffprobe(video: &Path) -> Result<Probe> {
    let output = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries"])
        .arg("stream=codec_name,width,height,nb_frames,duration:format=duration")
        .args(["-of", "json"])
        .arg(video)
        .output()
        .with_context(|| format!("running ffprobe on {}", video.display()))?;
    if !output.status.success() {
        anyhow::bail!(
            "ffprobe failed on {}: {}",
            video.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let parsed: serde_json::Value = serde_json::from_slice(&output.stdout)
        .with_context(|| format!("parsing ffprobe output for {}", video.display()))?;
    let stream = parsed.pointer("/streams/0").cloned().unwrap_or_default();

    // ffprobe reports numeric-looking fields as JSON strings for some demuxers and as numbers for
    // others, so every read goes through both.
    let number = |value: Option<&serde_json::Value>| -> Option<f64> {
        value.and_then(|value| value.as_f64().or_else(|| value.as_str()?.parse().ok()))
    };
    let seconds =
        number(stream.get("duration")).or_else(|| number(parsed.pointer("/format/duration")));
    Ok(Probe {
        codec: stream
            .get("codec_name")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        width: number(stream.get("width")).unwrap_or(0.0) as u32,
        height: number(stream.get("height")).unwrap_or(0.0) as u32,
        // Absent `nb_frames` means the container did not count them; the render is then reported
        // with zero frames and fails as `render_output_missing` rather than guessing a length.
        frame_count: number(stream.get("nb_frames")).unwrap_or(0.0) as u64,
        duration_ms: seconds.map(|s| DurationMs((s * 1000.0).round() as i64)),
    })
}

/// Describe an encoded video from the file itself: probe + digest. `path` in the result is the
/// file's own name; the caller rewrites it to wherever the file lands relative to its manifest.
pub fn describe_video(video: &Path, fps: u32, spherical: bool) -> Result<MediaInfo> {
    let probe = ffprobe(video)?;
    Ok(MediaInfo {
        path: video
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default(),
        sha256: sha256_file(video)?,
        container: "mp4".into(),
        codec: probe.codec,
        width: probe.width,
        height: probe.height,
        fps,
        frame_count: probe.frame_count,
        duration_ms: probe.duration_ms.unwrap_or(DurationMs(
            (probe.frame_count as i64 * 1000) / fps.max(1) as i64,
        )),
        spherical,
    })
}

/// Cut `source` down to the `frame_len` frames starting at `start_frame`, writing `target`.
///
/// Frame-accurate by construction — a full decode through the `trim` filter, then a re-encode
/// with the run's own encoder settings (the dataset window almost never starts on a keyframe,
/// so a copy-codec cut cannot be exact). Encodes into local scratch and copies out: `target`
/// may sit on a FUSE mount that rejects the mp4 muxer's header seek-back. Re-encoding strips
/// spherical metadata, so it is re-injected when asked, BEFORE the returned digest is taken.
/// Fails when the shipped file does not count exactly `frame_len` frames.
#[allow(clippy::too_many_arguments)]
pub fn trim_video(
    source: &Path,
    target: &Path,
    start_frame: u64,
    frame_len: u64,
    fps: u32,
    encoder: &str,
    preset: &str,
    crf: u8,
    spherical: bool,
) -> Result<MediaInfo> {
    let scratch = tempfile::tempdir().context("trim scratch")?;
    let local = scratch.path().join("trimmed.mp4");
    let encoder = crate::encoder_override().unwrap_or_else(|| encoder.to_string());
    let fragment = crate::encoder_fragment(&encoder, preset, crf);
    let output = Command::new("ffmpeg")
        .args(["-v", "error", "-nostats", "-y", "-i"])
        .arg(source)
        .args([
            "-vf",
            &format!(
                "trim=start_frame={start_frame}:end_frame={},setpts=PTS-STARTPTS",
                start_frame + frame_len
            ),
            "-an",
            "-c:v",
        ])
        .args(fragment.split_whitespace())
        .args(["-pix_fmt", "yuv420p", "-r", &fps.to_string()])
        .arg(&local)
        .output()
        .with_context(|| format!("running ffmpeg trim on {}", source.display()))?;
    if !output.status.success() {
        anyhow::bail!(
            "ffmpeg trim failed on {}: {}",
            source.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    if spherical {
        inject_spherical(&local)?;
    }
    std::fs::copy(&local, target)
        .map(|_| ())
        .with_context(|| format!("promote trimmed video to {}", target.display()))?;
    let described = describe_video(target, fps, spherical)?;
    if described.frame_count != frame_len {
        anyhow::bail!(
            "trim of {} produced {} frames, wanted {frame_len}",
            source.display(),
            described.frame_count
        );
    }
    Ok(described)
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file =
        std::fs::File::open(path).with_context(|| format!("hashing {}", path.display()))?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(hex::encode(digest.finalize()))
}

/// Per-frame mean luma (`signalstats` YAVG, 0-255) of a video, sampled at `samples_per_s` frames
/// per second in presentation order.
///
/// Sampling rather than decoding every frame is a speed trade the caller opts into: darkness QC
/// asks about multi-second stretches, which 2 samples/s resolves at ~1/30 of the decode work.
/// `ffmpeg -i` is used (not an `ffprobe` lavfi `movie=` source) so the path never needs filter-
/// graph escaping; `metadata=print:file=-` puts one `lavfi.signalstats.YAVG=<value>` line per
/// sampled frame on stdout, which is the whole interface.
pub fn sample_luma(video: &Path, samples_per_s: u32) -> Result<Vec<f64>> {
    let output = Command::new("ffmpeg")
        .args(["-v", "error", "-nostats", "-i"])
        .arg(video)
        .args([
            "-vf",
            &format!(
                "fps={},signalstats,metadata=print:key=lavfi.signalstats.YAVG:file=-",
                samples_per_s.max(1)
            ),
            "-f",
            "null",
            "-",
        ])
        .output()
        .with_context(|| format!("running ffmpeg signalstats on {}", video.display()))?;
    if !output.status.success() {
        anyhow::bail!(
            "ffmpeg signalstats failed on {}: {}",
            video.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let samples: Vec<f64> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            line.trim()
                .strip_prefix("lavfi.signalstats.YAVG=")?
                .parse()
                .ok()
        })
        .collect();
    if samples.is_empty() {
        anyhow::bail!("ffmpeg signalstats found no frames in {}", video.display());
    }
    Ok(samples)
}
