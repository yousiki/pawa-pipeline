//! Build-time render warmup: have HeadlessMC download Minecraft + Fabric into the baked game dir,
//! then pre-fetch every asset object. Without the prefetch the client re-downloads ~4,270
//! sound/texture objects on EVERY render (~120 s each, the single biggest avoidable per-render
//! tax); pure HTTP, no GL, so it runs in the image builder.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

pub fn render_warmup() -> Result<()> {
    let game_dir =
        PathBuf::from(std::env::var("GAME_DIR").unwrap_or_else(|_| "/opt/renderer/gamedir".into()));
    let hmc_jar = pawa_render::hmc_jar_from_env()?;
    let mc = std::env::var("MC_VERSION").unwrap_or_else(|_| "26.2".into());
    let loader = std::env::var("FABRIC_LOADER").unwrap_or_else(|_| "0.19.3".into());
    fs::create_dir_all(&game_dir)?;

    // `--uid` pins the loader: without it the installer takes the newest release, which then
    // disagrees with the `fabric-loader-{FABRIC_LOADER}-{mc}` profile launched below.
    for command in [
        format!("download {mc}"),
        format!("fabric {mc} --uid {loader}"),
    ] {
        tracing::info!(%command, "headlessmc warmup step");
        let status = Command::new("java")
            .arg(format!("-Dhmc.mcdir={}", game_dir.display()))
            .arg(format!("-Dhmc.gamedir={}", game_dir.display()))
            .args(["-Dhmc.store.accounts=false", "-Dhmc.offline=true"])
            .arg("-jar")
            .arg(&hmc_jar)
            .args(["--command", &command])
            .status()
            .context("running headlessmc")?;
        if !status.success() {
            bail!("headlessmc {command:?} failed: {status}");
        }
    }
    prefetch_libraries(&game_dir, &hmc_jar, &mc)?;
    prefetch_assets(&game_dir, &mc)
}

/// Bake the game libraries — and the first-launch state the renders start from — the way the
/// assets are baked. `download`/`fabric` do NOT fetch libraries or the profile's client jar:
/// HeadlessMC downloads those at LAUNCH time. So launch once. The game gets all the way to the main
/// menu (writing `options.txt`, mod configs and Fabric's processed-mod cache on the way) and then
/// idles forever, so stop it once `options.txt` exists and the log has gone quiet. Without this
/// every render pays the download, and a render inside a read-only container root (e.g.
/// Apptainer) fails outright: the lane's `libraries` symlink points back into the baked game dir.
fn prefetch_libraries(game_dir: &Path, hmc_jar: &Path, mc: &str) -> Result<()> {
    use std::os::unix::process::CommandExt;
    use std::time::{Duration, Instant};

    let loader = std::env::var("FABRIC_LOADER").unwrap_or_else(|_| "0.19.3".into());
    let command = format!("launch fabric-loader-{loader}-{mc} -offline --jvm -Xmx1G");
    tracing::info!(%command, "library prefetch launch");
    let mut child = Command::new("java")
        .arg(format!("-Dhmc.mcdir={}", game_dir.display()))
        .arg(format!("-Dhmc.gamedir={}", game_dir.display()))
        .args(["-Dhmc.store.accounts=false", "-Dhmc.offline=true"])
        .arg("-jar")
        .arg(hmc_jar)
        .args(["--command", &command])
        // Own process group: HeadlessMC spawns the game as a separate JVM, and both must go.
        .process_group(0)
        .spawn()
        .context("running headlessmc launch for library prefetch")?;

    let options = game_dir.join("options.txt");
    let log = game_dir.join("logs").join("latest.log");
    let started = Instant::now();
    let (mut last_len, mut quiet_since) = (0, Instant::now());
    let outcome = loop {
        if let Some(status) = child.try_wait()? {
            break format!("launch exited on its own: {status}");
        }
        let len = log.metadata().map(|m| m.len()).unwrap_or(0);
        if len != last_len {
            (last_len, quiet_since) = (len, Instant::now());
        }
        if options.is_file() && quiet_since.elapsed() >= Duration::from_secs(5) {
            break format!("game reached the main menu after {:?}", started.elapsed());
        }
        if started.elapsed() >= Duration::from_secs(900) {
            break "gave up after 900 s".to_string();
        }
        std::thread::sleep(Duration::from_millis(500));
    };
    // SAFETY: plain syscall on the process group this function created.
    unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
    let _ = child.wait();
    // A killed launcher leaves its log lock behind; the renders' own launches must not inherit it.
    let _ = fs::remove_file(
        hmc_jar
            .with_file_name("HeadlessMC")
            .join("headlessmc.log.lck"),
    );
    tracing::info!(%outcome, "library prefetch launch finished");

    // The launch's exit proves nothing, so assert the thing the step exists for: LWJGL — the
    // largest launch-time download — and the profile's client jar are now baked.
    let profile = format!("fabric-loader-{loader}-{mc}");
    let client = game_dir
        .join("versions")
        .join(&profile)
        .join(format!("{profile}.jar"));
    if !client.is_file() {
        bail!(
            "library prefetch launch left no client jar at {}",
            client.display()
        );
    }
    let mut lwjgl = false;
    let mut stack = vec![game_dir.join("libraries")];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
            let path = entry?.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.to_string_lossy().contains("lwjgl") {
                lwjgl = true;
            }
        }
    }
    if !lwjgl {
        bail!("library prefetch launch left no lwjgl jar under libraries/");
    }
    Ok(())
}

/// Download `(url, destination)` pairs with ONE curl invocation: many small files are
/// latency-bound, and curl's own `--parallel` is the pool.
fn curl_batch(game_dir: &Path, label: &str, items: &[(String, PathBuf)]) -> Result<()> {
    if items.is_empty() {
        return Ok(());
    }
    let mut config = String::new();
    for (url, dest) in items {
        fs::create_dir_all(dest.parent().context("download destination")?)?;
        config.push_str(&format!(
            "url = \"{url}\"\noutput = \"{}\"\n",
            dest.display()
        ));
    }
    let config_path = game_dir.join(format!("{label}-prefetch.curl"));
    fs::write(&config_path, config)?;
    let status = Command::new("curl")
        .args([
            "-fsS",
            "--retry",
            "3",
            "--parallel",
            "--parallel-max",
            "16",
            "--config",
        ])
        .arg(&config_path)
        .status()?;
    fs::remove_file(&config_path).ok();
    if !status.success() {
        bail!("{label} prefetch failed: {status}");
    }
    Ok(())
}

fn prefetch_assets(game_dir: &Path, mc: &str) -> Result<()> {
    // The version JSON HeadlessMC just wrote names the asset index.
    let mut asset_index: Option<serde_json::Value> = None;
    for entry in fs::read_dir(game_dir.join("versions").join(mc))? {
        let path = entry?.path();
        if path.extension().is_some_and(|ext| ext == "json")
            && let Ok(value) =
                serde_json::from_str::<serde_json::Value>(&fs::read_to_string(&path)?)
            && value.get("assetIndex").is_some_and(|v| v.is_object())
        {
            asset_index = Some(value["assetIndex"].clone());
            break;
        }
    }
    let Some(index) = asset_index else {
        tracing::warn!("no assetIndex in the version JSON; renders will lazy-download");
        return Ok(());
    };

    let assets = game_dir.join("assets");
    let index_id = index["id"].as_str().context("assetIndex.id")?;
    let index_path = assets.join("indexes").join(format!("{index_id}.json"));
    fs::create_dir_all(index_path.parent().unwrap())?;
    let status = Command::new("curl")
        .args(["-fsSL", "--retry", "3", "-o"])
        .arg(&index_path)
        .arg(index["url"].as_str().context("assetIndex.url")?)
        .status()?;
    if !status.success() {
        bail!("asset index download failed: {status}");
    }

    let objects: serde_json::Value = serde_json::from_str(&fs::read_to_string(&index_path)?)?;
    let objects = objects["objects"].as_object().cloned().unwrap_or_default();
    let objects_dir = assets.join("objects");

    // Deduplicate by hash — one hash can be indexed under several virtual paths — and skip
    // objects already present at their indexed size.
    let mut wanted = Vec::new();
    let mut present = 0usize;
    let mut seen = std::collections::BTreeSet::new();
    for meta in objects.values() {
        let hash = meta["hash"].as_str().unwrap_or_default();
        let size = meta["size"].as_u64().unwrap_or(0);
        if hash.len() < 2 || !seen.insert(hash.to_string()) {
            continue;
        }
        let destination = objects_dir.join(&hash[..2]).join(hash);
        if destination.metadata().map(|m| m.len()).unwrap_or(u64::MAX) == size {
            present += 1;
            continue;
        }
        let url = format!(
            "https://resources.download.minecraft.net/{}/{hash}",
            &hash[..2]
        );
        wanted.push((url, destination));
    }
    tracing::info!(wanted = wanted.len(), present, "asset prefetch plan");
    curl_batch(game_dir, "asset", &wanted)?;
    Ok(())
}
