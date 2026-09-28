//! The three processes one render drives: an Xvfb display, a GL probe, and the Minecraft client.
//!
//! Every one of them is spawned as its own process group leader. A render that is killed — by the
//! watchdog here, by the scheduler, or by a panic unwinding past a [`Proc`] — must not leave a 4 GB
//! JVM or an X server holding this lane's display, because the next render in the lane would then
//! fail to take it and every render after that would too.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use pawa_schema::driver::{DRIVER_SPEC_ENV, DriverPhase, DriverStatus};
use process_wrap::std::{ChildWrapper, CommandWrap, ProcessGroup};
use serde::Serialize;

/// How long Xvfb gets to publish its socket before the render gives up on the display.
const XVFB_START_TIMEOUT: Duration = Duration::from_secs(15);

/// Gap between reads of the driver's status file.
const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// How long a client that has reported a terminal phase gets to shut itself down cleanly before it
/// is signalled.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(120);

/// How long a signalled client gets before it is killed outright.
const TERMINATE_GRACE: Duration = Duration::from_secs(30);

const SIGTERM: i32 = 15;

/// HeadlessMC's offline identity. Fixed so a replay's viewer is never a new player each render.
const OFFLINE_USERNAME: &str = "mwrenderer";
const OFFLINE_UUID: &str = "00000000000040008000000000002006";

/// Where the image puts the launcher wrapper.
const DEFAULT_HMC_JAR: &str = "/opt/renderer/headlessmc-launcher-wrapper.jar";

/// A spawned process that is signalled and reaped when it goes out of scope.
///
/// `process-wrap`'s own kill-on-drop wrapper is Tokio-only, so this is the synchronous equivalent:
/// the group is killed on drop unless it already exited.
#[derive(Debug)]
pub(crate) struct Proc {
    name: &'static str,
    child: Box<dyn ChildWrapper>,
    exited: bool,
}

impl Proc {
    fn spawn(name: &'static str, command: Command) -> Result<Proc> {
        let mut wrapped = CommandWrap::from(command);
        wrapped.wrap(ProcessGroup::leader());
        let child = wrapped
            .spawn()
            .with_context(|| format!("spawning {name}"))?;
        Ok(Proc {
            name,
            child,
            exited: false,
        })
    }

    /// The exit code, if the process has already exited.
    pub(crate) fn try_wait(&mut self) -> Result<Option<i32>> {
        match self.child.try_wait() {
            Ok(Some(status)) => {
                self.exited = true;
                Ok(Some(status.code().unwrap_or(-1)))
            }
            Ok(None) => Ok(None),
            // ECHILD means the process group is already reaped — the state every stop asks for.
            // Treating it as an error once voided a render that had its mp4 on disk.
            Err(error) if error.raw_os_error() == Some(10) => {
                self.exited = true;
                Ok(Some(0))
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Wait up to `grace` for the process to exit on its own.
    fn wait_for(&mut self, grace: Duration) -> Result<Option<i32>> {
        let deadline = Instant::now() + grace;
        loop {
            if let Some(code) = self.try_wait()? {
                return Ok(Some(code));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(200).min(grace));
        }
    }

    /// SIGTERM the group, then SIGKILL it if it is still there. Returns the exit code when one was
    /// collected. Idempotent — a process that already exited is left alone.
    fn stop(&mut self) -> Result<Option<i32>> {
        if self.exited {
            return self.try_wait();
        }
        if let Some(code) = self.try_wait()? {
            return Ok(Some(code));
        }
        tracing::info!(process = self.name, "terminating");
        let _ = self.child.signal(SIGTERM);
        if let Some(code) = self.wait_for(TERMINATE_GRACE)? {
            return Ok(Some(code));
        }
        tracing::warn!(process = self.name, "did not exit after SIGTERM; killing");
        let _ = self.child.kill();
        self.exited = true;
        Ok(self.child.try_wait()?.and_then(|status| status.code()))
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        if let Err(err) = self.stop() {
            tracing::warn!(process = self.name, %err, "could not stop process on drop");
        }
    }
}

/// Start Xvfb on `x_display` and confirm it is actually serving before returning.
///
/// Xvfb exits immediately when the display is already in use ("Server is already active for display
/// N"). Sleeping and returning the handle unexamined — which is what this used to do — produced a
/// render that quietly ran against somebody else's X server, exactly the failure per-lane displays
/// exist to prevent. A dead Xvfb is therefore fatal.
///
/// (The parameter is `x_display` and not `display` because `tracing`'s macros import a helper of
/// that name into the block they expand into, which shadows any local called `display`.)
pub(crate) fn start_xvfb(x_display: &str) -> Result<Proc> {
    tracing::info!(display = x_display, "starting Xvfb");
    let mut command = Command::new("Xvfb");
    command
        .args([x_display, "-screen", "0", "1280x720x24", "-ac"])
        .args(["+extension", "GLX", "+render", "-noreset"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut xvfb = Proc::spawn("Xvfb", command)?;

    let socket =
        PathBuf::from("/tmp/.X11-unix").join(format!("X{}", x_display.trim_start_matches(':')));
    let deadline = Instant::now() + XVFB_START_TIMEOUT;
    while Instant::now() < deadline {
        if let Some(code) = xvfb.try_wait()? {
            bail!("Xvfb for {x_display} exited with code {code}; that display is already in use");
        }
        if socket.exists() {
            return Ok(xvfb);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // The process is alive but its socket never showed up where Debian's Xvfb puts it. That is far
    // more likely to be an unusual socket path than a dead server, and the exit-code check above
    // already covers the collision case, so warn and let the launch decide.
    tracing::warn!(display = x_display, socket = %socket.display(), "Xvfb is running but its socket did not appear");
    Ok(xvfb)
}

/// What the software GL stack reports on a display.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct GlProbe {
    pub(crate) renderer: String,
    pub(crate) version: String,
}

/// Verify the GL stack on a display that belongs to nobody else: llvmpipe by default, or the
/// hardware renderer behind `gpu_device` when one is given.
///
/// `x_display` is the caller's own probe display, so a probe never disturbs a render in flight. The
/// full `glxinfo` output is written to `report` whatever the outcome — a failed probe is exactly
/// when somebody wants to read it.
pub(crate) fn probe_gl(
    x_display: &str,
    gpu_device: Option<&str>,
    report: &Path,
) -> Result<GlProbe> {
    let mut command = Command::new("Xvfb");
    command
        .args([
            x_display,
            "-screen",
            "0",
            "640x480x24",
            "-ac",
            "+extension",
            "GLX",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let _xvfb = Proc::spawn("Xvfb (GL probe)", command)?;
    std::thread::sleep(Duration::from_millis(1500));

    let mut glxinfo = gl_command("glxinfo", gpu_device);
    let output = glxinfo
        .arg("-B")
        .env("DISPLAY", x_display)
        .output()
        .context("running glxinfo")?;
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    if let Some(parent) = report.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(report, &text)?;

    if !output.status.success() {
        bail!("glxinfo exited {:?} on {x_display}", output.status.code());
    }
    let field = |needle: &str| {
        text.lines()
            .find(|line| line.contains(needle))
            .and_then(|line| line.split_once(':'))
            .map(|(_, value)| value.trim().to_string())
    };
    let renderer = field("OpenGL renderer").unwrap_or_default();
    let version = field("OpenGL version").unwrap_or_default();
    let is_llvmpipe = renderer.to_lowercase().contains("llvmpipe");
    match gpu_device {
        None if !is_llvmpipe => {
            bail!("OpenGL renderer on {x_display} is not llvmpipe: {renderer:?}")
        }
        // A GPU lane that quietly fell back to software would render, slowly, and nobody would
        // know — fail it here instead.
        Some(device) if is_llvmpipe => {
            bail!("RENDER_GPU asked for {device} but {x_display} still renders on {renderer:?}")
        }
        _ => {}
    }
    // ReplayMod's shaders need 4.4; an older stack renders black frames rather than failing, which
    // is the worst way to find out.
    if gl_version(&version).is_none_or(|v| v < (4, 4)) {
        bail!("OpenGL version on {x_display} is below 4.4: {version:?}");
    }
    Ok(GlProbe { renderer, version })
}

/// A command that talks GL: `program` as-is for llvmpipe, or wrapped in `vglrun -d <device>` so
/// VirtualGL's EGL back end rasterizes it on the GPU. The image's software-GL environment
/// (`LIBGL_ALWAYS_SOFTWARE`, `GALLIUM_DRIVER`) is dropped from a GPU command so Mesa cannot
/// interfere with the readback path.
fn gl_command(program: &str, gpu_device: Option<&str>) -> Command {
    match gpu_device {
        Some(device) => {
            let mut command = Command::new("vglrun");
            command.args(["-d", device, program]);
            command.env_remove("LIBGL_ALWAYS_SOFTWARE");
            command.env_remove("GALLIUM_DRIVER");
            command
        }
        None => Command::new(program),
    }
}

/// The leading `<major>.<minor>` of a GL version string like `"4.5 (Core Profile) Mesa 24.0"`.
fn gl_version(version: &str) -> Option<(u32, u32)> {
    let start = version.find(|c: char| c.is_ascii_digit())?;
    let rest = &version[start..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(rest.len());
    let mut parts = rest[..end].split('.');
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

pub(crate) fn hmc_jar_from_env() -> Result<PathBuf> {
    let hmc_jar =
        PathBuf::from(std::env::var("HMC_JAR").unwrap_or_else(|_| DEFAULT_HMC_JAR.into()));
    if !hmc_jar.is_absolute() || !hmc_jar.is_file() {
        bail!(
            "HMC_JAR must be an absolute path to a regular file; got {}",
            hmc_jar.display()
        );
    }
    Ok(hmc_jar)
}

fn launch_command(
    state_dir: &Path,
    x_display: &str,
    gl_threads: u32,
    jvm_threads: u32,
    gpu_device: Option<&str>,
    spec_path: &Path,
) -> Result<(Command, String)> {
    let mc = std::env::var("MC_VERSION").unwrap_or_else(|_| "26.2".into());
    let loader = std::env::var("FABRIC_LOADER").unwrap_or_else(|_| "0.19.3".into());
    let hmc_jar = hmc_jar_from_env()?;

    // One `--command` string, the way HeadlessMC's console reads it: the launch target, then each
    // JVM argument introduced by its own `--jvm`.
    let launch = format!(
        "launch fabric-loader-{loader}-{mc} -offline --jvm -Xmx4G --jvm -Djava.awt.headless=false \
         --jvm -XX:ActiveProcessorCount={jvm_threads}"
    );
    let mut command = gl_command("java", gpu_device);
    command
        .arg(format!("-Dhmc.mcdir={}", state_dir.display()))
        .arg(format!("-Dhmc.gamedir={}", state_dir.display()))
        .args([
            "-Dhmc.check.xvfb=true",
            "-Dhmc.store.accounts=false",
            "-Dhmc.offline=true",
        ])
        .arg(format!("-Dhmc.offline.username={OFFLINE_USERNAME}"))
        .arg(format!("-Dhmc.offline.uuid={OFFLINE_UUID}"))
        .arg("-jar")
        .arg(&hmc_jar)
        .args(["--command", &launch])
        .current_dir(state_dir)
        .env("DISPLAY", x_display)
        // Keep this lane's software rasterizer inside its share of the node; see `Lane::gl_threads`.
        .env("LP_NUM_THREADS", gl_threads.to_string())
        // The driver's whole configuration, and the flag that activates it, are this one path.
        .env(DRIVER_SPEC_ENV, spec_path)
        .stdin(Stdio::null());
    Ok((command, launch))
}

/// Launch the Minecraft client under HeadlessMC, with the driver spec as its only configuration.
///
/// stdout and stderr both land in `log`, which is the only record of what the client did — the
/// driver's status file says what it decided, not what Java printed on the way there.
pub(crate) fn launch(
    state_dir: &Path,
    x_display: &str,
    gl_threads: u32,
    jvm_threads: u32,
    gpu_device: Option<&str>,
    spec_path: &Path,
    log: &Path,
) -> Result<Proc> {
    let handle =
        std::fs::File::create(log).with_context(|| format!("creating {}", log.display()))?;
    let (mut command, launch) = launch_command(
        state_dir,
        x_display,
        gl_threads,
        jvm_threads,
        gpu_device,
        spec_path,
    )?;
    command.stderr(handle.try_clone()?).stdout(handle);
    tracing::info!(display = x_display, %launch, spec = %spec_path.display(), "launching render client");
    Proc::spawn("minecraft client", command)
}

/// How the client session ended.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Supervision {
    /// The watchdog deadline expired and the client was stopped mid-render.
    pub(crate) timed_out: bool,
    pub(crate) exit_code: Option<i32>,
}

/// Poll the driver's status file until the session ends, and stop the client when it does.
///
/// Success is decided from the status file, never from the client's exit code: HeadlessMC exits 0
/// on a client that rendered nothing, and non-zero on one whose render is already on disk.
///
/// `deadline` is the sum of the per-projection timeouts. It is a WALL-CLOCK budget; nothing here
/// measures CPU.
pub(crate) fn supervise(
    client: &mut Proc,
    status_path: &Path,
    deadline: Duration,
) -> Result<Supervision> {
    let started = Instant::now();
    let mut last_phase = None;
    loop {
        let exit_code = client.try_wait()?;
        let status = read_status(status_path);
        let phase = status.as_ref().map(|status| status.phase);
        if phase != last_phase {
            tracing::info!(
                ?phase,
                tick = status.as_ref().map(|status| status.tick),
                "driver phase"
            );
            last_phase = phase;
        }

        if matches!(phase, Some(DriverPhase::Done | DriverPhase::Failed)) && exit_code.is_none() {
            tracing::info!(
                ?phase,
                "driver reached a terminal phase; waiting for the client to exit"
            );
            let code = match client.wait_for(SHUTDOWN_GRACE)? {
                Some(code) => Some(code),
                None => client.stop()?,
            };
            return Ok(Supervision {
                timed_out: false,
                exit_code: code,
            });
        }
        if exit_code.is_some() {
            tracing::info!(?exit_code, "client process exited");
            return Ok(Supervision {
                timed_out: false,
                exit_code,
            });
        }
        if started.elapsed() > deadline {
            tracing::warn!(
                deadline_s = deadline.as_secs(),
                "render watchdog timeout; terminating client"
            );
            return Ok(Supervision {
                timed_out: true,
                exit_code: client.stop()?,
            });
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Read the driver's status file, tolerating the window where it is half-written.
///
/// The driver rewrites this file in place as it progresses, so a read can land mid-write and see
/// truncated JSON. That is not an error — the next poll five seconds later sees the whole thing.
pub(crate) fn read_status(path: &Path) -> Option<DriverStatus> {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str(&text) {
            Ok(status) => Some(status),
            Err(err) => {
                tracing::debug!(%err, "driver status file is not (yet) parseable");
                None
            }
        },
        Err(err) if err.kind() == ErrorKind::NotFound => None,
        Err(err) => {
            tracing::debug!(%err, "could not read driver status file");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{gl_version, launch_command};
    use std::ffi::OsStr;
    use std::path::Path;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvRestore(Option<std::ffi::OsString>);

    impl EnvRestore {
        fn hmc_jar(path: Option<&Path>) -> Self {
            let old = std::env::var_os("HMC_JAR");
            match path {
                Some(path) => unsafe { std::env::set_var("HMC_JAR", path) },
                None => unsafe { std::env::remove_var("HMC_JAR") },
            }
            Self(old)
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => unsafe { std::env::set_var("HMC_JAR", value) },
                None => unsafe { std::env::remove_var("HMC_JAR") },
            }
        }
    }

    #[test]
    fn gl_version_reads_what_mesa_actually_prints() {
        assert_eq!(gl_version("4.5 (Core Profile) Mesa 24.0.9"), Some((4, 5)));
        assert_eq!(gl_version("3.1 Mesa 20.0"), Some((3, 1)));
        assert_eq!(gl_version("OpenGL ES 3.2"), Some((3, 2)));
        assert_eq!(gl_version("unknown"), None);
    }

    #[test]
    fn hmc_runs_from_lane_state_with_an_absolute_jar() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("slot-3");
        std::fs::create_dir(&state).unwrap();
        let jar = dir.path().join("headlessmc.jar");
        std::fs::write(&jar, b"jar").unwrap();
        let _restore = EnvRestore::hmc_jar(Some(&jar));

        let (command, _) =
            launch_command(&state, ":102", 4, 8, None, Path::new("/work/spec.json")).unwrap();
        assert_eq!(command.get_current_dir(), Some(state.as_path()));
        let args: Vec<&OsStr> = command.get_args().collect();
        let jar_pos = args.iter().position(|arg| *arg == "-jar").unwrap();
        assert_eq!(args[jar_pos + 1], jar.as_os_str());
    }

    #[test]
    fn hmc_jar_must_be_an_absolute_regular_file() {
        let _guard = ENV_LOCK.lock().unwrap();
        for path in [
            Path::new("relative.jar"),
            Path::new("/missing/headlessmc.jar"),
        ] {
            let _restore = EnvRestore::hmc_jar(Some(path));
            assert!(
                launch_command(Path::new("/lane"), ":99", 1, 1, None, Path::new("/spec")).is_err()
            );
        }
        let dir = tempfile::tempdir().unwrap();
        let _restore = EnvRestore::hmc_jar(Some(dir.path()));
        assert!(launch_command(Path::new("/lane"), ":99", 1, 1, None, Path::new("/spec")).is_err());
    }
}
