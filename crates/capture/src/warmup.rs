//! Build-time warmup: download the pinned Fabric server, boot it once so it fetches its own
//! libraries and generates the ServerReplay config, then archive a clean, offline-bootable pack.
//! Runs in the image build only; the runtime always unpacks the resulting tarball.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::server::FabricServer;
use crate::{CaptureConfig, WorldConfig};

const FABRIC_SERVER_URL: &str =
    "https://meta.fabricmc.net/v2/versions/loader/26.2/0.19.3/1.1.2/server/jar";
const MOD_URLS: &[(&str, &str)] = &[
    (
        "fabric-api.jar",
        "https://cdn.modrinth.com/data/P7dR8mSH/versions/vmQp7ixA/fabric-api-0.157.0%2B26.2.jar",
    ),
    (
        "fabric-language-kotlin.jar",
        "https://cdn.modrinth.com/data/Ha28R6CL/versions/bdhiINYC/fabric-language-kotlin-1.13.13%2Bkotlin.2.4.10.jar",
    ),
    (
        "server-replay.jar",
        "https://cdn.modrinth.com/data/qCvSZ8ra/versions/czoujDEC/ServerReplay-3.5.2%2B26.2.jar",
    ),
];

/// Directories a fresh capture regenerates; baking them would ship one build's world to every
/// episode.
const EXCLUDE_TOP: &[&str] = &[
    "logs",
    "recordings",
    "crash-reports",
    "debug",
    "world",
    "world_nether",
    "world_the_end",
];

fn download(url: &str, destination: &Path) -> Result<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    let status = Command::new("curl")
        .args(["-fsSL", "--retry", "3", "-o"])
        .arg(destination)
        .arg(url)
        .status()
        .context("running curl")?;
    if !status.success() {
        bail!("download failed ({status}): {url}");
    }
    Ok(())
}

/// Build the offline server pack at `pack_out`.
pub fn build_server_pack(pack_out: &Path) -> Result<()> {
    let root =
        std::env::var("PAWA_SERVER_WARMUP_ROOT").unwrap_or_else(|_| "/opt/pawa/server".into());
    let root = Path::new(&root);
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root)?;

    let config = CaptureConfig {
        server_workdir: root.to_path_buf(),
        server_xms: "1G".into(),
        server_xmx: "2G".into(),
        ..CaptureConfig::default()
    };
    tracing::info!(
        mc = config.minecraft_version,
        loader = config.fabric_loader_version,
        "downloading Fabric server"
    );
    download(FABRIC_SERVER_URL, &root.join(&config.fabric_launcher))?;
    for (name, url) in MOD_URLS {
        download(url, &root.join("mods").join(name))?;
    }
    fs::write(root.join("eula.txt"), "eula=true\n")?;

    let world = WorldConfig {
        level_seed: "2026".into(),
        level_type: "minecraft:normal".into(),
        generator_settings: None,
        level_name: "world".into(),
        reset_world: true,
    };
    let mut server = FabricServer::new(&config, root.join("logs/warmup.log"));
    server.start_prepared(&config, &world, 8)?;
    if !server.wait_ready(Duration::from_secs(900)) {
        server.stop_and_wait(Duration::from_secs(180));
        bail!("server never reported ready during warmup");
    }
    match server.stop_and_wait(Duration::from_secs(180)) {
        Some(0) => {}
        code => bail!("warmup server exited with {code:?}"),
    }

    // ServerReplay must record in ReplayMod's own container, or the render client cannot open it.
    let replay_config = root.join("config/server-replay/config.json");
    let mut value: serde_json::Value = fs::read_to_string(&replay_config)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    value["default_encoding"] = serde_json::json!("replay_mod");
    fs::create_dir_all(replay_config.parent().unwrap())?;
    fs::write(&replay_config, serde_json::to_string_pretty(&value)? + "\n")?;

    for name in EXCLUDE_TOP {
        let path = root.join(name);
        let _ = fs::remove_dir_all(&path);
        let _ = fs::remove_file(&path);
    }
    // Session locks would make every unpacked copy look already-owned.
    for entry in walk_files(root)? {
        if entry.extension().is_some_and(|ext| ext == "lock") {
            let _ = fs::remove_file(&entry);
        }
    }

    if let Some(parent) = pack_out.parent() {
        fs::create_dir_all(parent)?;
    }
    let status = Command::new("tar")
        .arg("-czf")
        .arg(pack_out)
        .arg("-C")
        .arg(root)
        .arg(".")
        .status()
        .context("running tar to archive the pack")?;
    if !status.success() {
        bail!("archiving the server pack failed: {status}");
    }
    tracing::info!(
        pack = %pack_out.display(),
        bytes = pack_out.metadata().map(|m| m.len()).unwrap_or(0),
        "server pack baked"
    );
    Ok(())
}

fn walk_files(root: &Path) -> Result<Vec<std::path::PathBuf>> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                stack.push(entry.path());
            } else {
                files.push(entry.path());
            }
        }
    }
    Ok(files)
}
