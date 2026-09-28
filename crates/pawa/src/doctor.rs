//! Preflight: fail fast, before a scheduler slot is spent on a pod that cannot work.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, bail};

pub fn run() -> Result<serde_json::Value> {
    // Java for the Minecraft server and the render client.
    let output = Command::new("java")
        .arg("-version")
        .output()
        .context("java not runnable")?;
    let text = String::from_utf8_lossy(if output.stderr.is_empty() {
        &output.stdout
    } else {
        &output.stderr
    })
    .to_string();
    let major = text
        .split('"')
        .nth(1)
        .and_then(|v| v.trim_start_matches("1.").split('.').next())
        .and_then(|v| v.parse::<u32>().ok())
        .with_context(|| format!("cannot parse java version from {text:?}"))?;
    if major < 25 {
        bail!("java {major} is too old (need >= 25): {}", text.trim());
    }

    for tool in ["Xvfb", "glxinfo", "vglrun", "ffmpeg", "ffprobe", "hq"] {
        if which(tool).is_none() {
            bail!("{tool} is not on PATH");
        }
    }
    for encoder in ["libx264", "libsvtav1"] {
        pawa_render::assert_encoder(encoder)?;
    }
    // RENDER_ENCODER=<name> must fail here, not as an opaque render_output_missing later.
    // (`auto` cannot fail: it only resolves to an encoder whose probe already passed.)
    if let Some(encoder) = pawa_render::encoder_override() {
        pawa_render::assert_encoder(&encoder)?;
    }
    // The probe itself asserts the right stack: a hardware renderer (never a silent llvmpipe
    // fallback) whenever a GPU is requested or detected, llvmpipe otherwise.
    let (renderer, gl_version) = pawa_render::doctor_gl_probe()?;

    let player_bin = PathBuf::from(
        std::env::var("PAWA_PLAYER_BIN").unwrap_or_else(|_| "/opt/pawa/bin/players".into()),
    );
    if !player_bin.is_file() {
        bail!("player binary missing: {}", player_bin.display());
    }
    let server_pack = PathBuf::from(
        std::env::var("PAWA_SERVER_PACK").unwrap_or_else(|_| "/opt/pawa/server-pack.tar.gz".into()),
    );
    if server_pack.metadata().map(|m| m.len()).unwrap_or(0) == 0 {
        bail!("server pack missing or empty: {}", server_pack.display());
    }

    // A write/read/fsync round trip on the out mount — a FUSE mount can list but not persist.
    let out = PathBuf::from(std::env::var("PAWA_OUT").unwrap_or_else(|_| "/mnt/out".into()));
    std::fs::create_dir_all(&out)?;
    let mut probe = tempfile::NamedTempFile::with_prefix_in(".doctor-", &out)
        .with_context(|| format!("out mount not writable: {}", out.display()))?;
    use std::io::Write;
    probe.write_all(b"pawa-doctor\n")?;
    probe.as_file().sync_all()?;
    if std::fs::read(probe.path())? != b"pawa-doctor\n" {
        bail!("out mount write/read mismatch: {}", out.display());
    }

    Ok(serde_json::json!({
        "status": "ok",
        "java_major": major,
        "renderer": renderer,
        "gl_version": gl_version,
        "player_binary": player_bin.display().to_string(),
        "server_pack": server_pack.display().to_string(),
        "output": out.display().to_string(),
    }))
}

fn which(tool: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(tool))
        .find(|p| p.is_file())
}
