//! `pawa` — the one binary: manifest validation, planning, the schedulable units, reporting.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use pawa_schema::manifest::RunManifest;
use pawa_schema::status::UnitKind;

use pawa::plan;
use pawa::status;
use pawa::units::{self, EpisodePaths};

mod doctor;
mod report;

mod warmup;

#[derive(Parser)]
#[command(
    name = "pawa",
    about = "PAWA: panoramic multiplayer Minecraft data pipeline"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
enum PlanStage {
    #[default]
    All,
    /// Emit only capture units. Used by the CPU-only capture hosts.
    Capture,
    /// Emit render, finalize and HUD units, with capture dependencies treated as satisfied by the
    /// synced capture tree. Used by the GPU render hosts.
    Render,
}

#[derive(Subcommand)]
enum Command {
    /// Validate a run manifest and print its resolved plan summary.
    Manifest {
        #[arg(long)]
        manifest: PathBuf,
    },
    /// Emit the HyperQueue Job Definition File for a run.
    Plan {
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        out: PathBuf,
        /// argv[0] for every task command (defaults to this binary's own path).
        #[arg(long)]
        pawa_bin: Option<String>,
        /// Skip units whose status tree already records success. This is also the resubmission
        /// round: everything not yet succeeded — failed units AND their never-ran dependents —
        /// is emitted, with satisfied deps pruned. (A failed-only filter was tried first and
        /// starved downstream finalizes that had no status at all.)
        #[arg(long)]
        resume: bool,
        /// Limit the emitted DAG to one deployment phase. `capture` is CPU-only; `render` assumes
        /// the capture work/status tree has already been synced into `--out`.
        #[arg(long, value_enum, default_value_t)]
        stage: PlanStage,
    },
    /// Run one schedulable unit (invoked by HyperQueue tasks).
    Unit {
        #[command(subcommand)]
        unit: Unit,
    },
    /// Reconstruct the run report from the status tree.
    RunReport {
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        out: PathBuf,
        /// Report only one split deployment phase; defaults to the complete DAG.
        #[arg(long, value_enum, default_value_t)]
        stage: PlanStage,
    },
    /// Overlay the action HUD onto a POV video (review instrument).
    Hud {
        #[arg(long)]
        video: PathBuf,
        #[arg(long)]
        frames: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
    /// Preflight the pod: toolchain, GL, encoders, mounts.
    Doctor,
    /// Build-time: download Minecraft + Fabric and pre-fetch assets into the game dir.
    WarmupRender,
    /// Build-time: bake the offline Fabric server pack.
    WarmupServer {
        #[arg(long)]
        pack_out: PathBuf,
    },
    /// Export every contract as JSON Schema.
    SchemaExport {
        #[arg(long)]
        out: PathBuf,
    },
    /// Run the finalize-stage action QC over a capture dir (scenario dev-loop instrument).
    QcActions {
        #[arg(long)]
        capture_dir: PathBuf,
        /// Override the scenario read from the dir's capture.json (tiers the thresholds).
        #[arg(long)]
        scenario: Option<String>,
    },
}

#[derive(Parser)]
struct UnitArgs {
    #[arg(long)]
    manifest: PathBuf,
    #[arg(long)]
    episode_id: String,
    #[arg(long)]
    out: PathBuf,
}

#[derive(Subcommand)]
enum Unit {
    Capture {
        #[command(flatten)]
        args: UnitArgs,
    },
    RenderPov {
        #[command(flatten)]
        args: UnitArgs,
        #[arg(long)]
        pov: String,
    },
    RenderObserver {
        #[command(flatten)]
        args: UnitArgs,
        #[arg(long)]
        observer: String,
    },
    Finalize {
        #[command(flatten)]
        args: UnitArgs,
    },
    Hud {
        #[command(flatten)]
        args: UnitArgs,
    },
}

fn load_manifest(path: &Path) -> Result<RunManifest> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read manifest {}", path.display()))?;
    let manifest: RunManifest = serde_json::from_str(&text)
        .with_context(|| format!("parse manifest {}", path.display()))?;
    let problems = manifest.validate();
    if !problems.is_empty() {
        bail!("manifest invalid:\n  {}", problems.join("\n  "));
    }
    Ok(manifest)
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_target(false)
        .init();
    match run() {
        Ok(code) => code,
        Err(error) => {
            tracing::error!("{error:#}");
            // Top-level errors outside a unit body are contract-shaped: bad CLI input, unreadable
            // manifest. Units map their own codes.
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<ExitCode> {
    match Cli::parse().command {
        Command::Manifest { manifest } => {
            let parsed = load_manifest(&manifest)?;
            let plan = plan::plan(&parsed);
            println!(
                "{}",
                serde_json::json!({
                    "run_id": parsed.run_id,
                    "episodes": parsed.episodes.len(),
                    "tasks": plan.tasks.len(),
                    "capture_cpus": parsed.capture_cpus,
                    "threads_per_render": parsed.render.threads_per_render,
                })
            );
            Ok(ExitCode::SUCCESS)
        }
        Command::Plan {
            manifest,
            out,
            pawa_bin,
            resume,
            stage,
        } => {
            let parsed = load_manifest(&manifest)?;
            let full = plan::plan(&parsed);
            let phased = filter_stage(&full, stage, &out)?;
            let filtered = filter_plan(&phased, &out, resume)?;
            let bin = pawa_bin.unwrap_or_else(|| {
                std::env::current_exe()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| "pawa".into())
            });
            let jdf = plan::to_jdf(
                &parsed,
                &filtered,
                &bin,
                &manifest.display().to_string(),
                &out.display().to_string(),
            );
            let dir = out.join("runs");
            std::fs::create_dir_all(&dir)?;
            let suffix = match stage {
                PlanStage::All => "",
                PlanStage::Capture => ".capture",
                PlanStage::Render => ".render",
            };
            let path = dir.join(format!("{}{suffix}.jdf.toml", parsed.run_id));
            std::fs::write(&path, &jdf)?;
            println!("{}", path.display());
            println!("tasks: {}", filtered.tasks.len());
            Ok(ExitCode::SUCCESS)
        }
        Command::Unit { unit } => {
            let code = match unit {
                Unit::Capture { args } => {
                    let manifest = load_manifest(&args.manifest)?;
                    let paths = EpisodePaths {
                        out_root: args.out,
                        episode_id: args.episode_id,
                    };
                    units::run_unit(UnitKind::Capture, &paths, None, "capture.json", || {
                        units::unit_capture(&manifest, &paths)
                    })
                }
                Unit::RenderPov { args, pov } => {
                    let manifest = load_manifest(&args.manifest)?;
                    let paths = EpisodePaths {
                        out_root: args.out,
                        episode_id: args.episode_id,
                    };
                    units::run_unit(
                        UnitKind::RenderPov,
                        &paths,
                        Some(&pov),
                        &format!("{pov}.render.json"),
                        || units::unit_render_pov(&manifest, &paths, &pov),
                    )
                }
                Unit::RenderObserver { args, observer } => {
                    let manifest = load_manifest(&args.manifest)?;
                    let paths = EpisodePaths {
                        out_root: args.out,
                        episode_id: args.episode_id,
                    };
                    units::run_unit(
                        UnitKind::RenderObserver,
                        &paths,
                        Some(&observer),
                        &format!("{observer}.render.json"),
                        || units::unit_render_observer(&manifest, &paths, &observer),
                    )
                }
                Unit::Finalize { args } => {
                    let manifest = load_manifest(&args.manifest)?;
                    let paths = EpisodePaths {
                        out_root: args.out,
                        episode_id: args.episode_id,
                    };
                    units::run_unit(UnitKind::Finalize, &paths, None, "finalize.json", || {
                        units::unit_finalize(&manifest, &paths)
                    })
                }
                Unit::Hud { args } => {
                    load_manifest(&args.manifest)?;
                    let paths = EpisodePaths {
                        out_root: args.out,
                        episode_id: args.episode_id,
                    };
                    units::run_unit(UnitKind::Hud, &paths, None, "hud.json", || {
                        units::unit_hud(&paths)
                    })
                }
            };
            Ok(ExitCode::from(u8::try_from(code).unwrap_or(1)))
        }
        Command::RunReport {
            manifest,
            out,
            stage,
        } => {
            let parsed = load_manifest(&manifest)?;
            let full = plan::plan(&parsed);
            let phased = filter_stage(&full, stage, &out)?;
            let report = report::build_with_plan(&parsed, &out, &phased)?;
            let path = out.join("runs").join(format!("{}.json", parsed.run_id));
            std::fs::create_dir_all(path.parent().unwrap())?;
            std::fs::write(&path, serde_json::to_string_pretty(&report)? + "\n")?;
            println!(
                "{}",
                serde_json::to_string(&serde_json::json!({
                    "report": path.display().to_string(),
                    "succeeded": report.succeeded_units,
                    "skipped": report.skipped_units,
                    "failed": report.failed_units,
                    "complete_episodes": report.complete_episodes,
                }))?
            );
            Ok(if report.failed_units == 0 {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(3)
            })
        }
        Command::Hud { video, frames, out } => {
            let stats = pawa_hud::render_hud(&pawa_hud::HudJob {
                video: &video,
                frames: &frames,
                out: &out,
            })?;
            println!("{stats:?}");
            Ok(ExitCode::SUCCESS)
        }
        Command::Doctor => {
            let report = doctor::run()?;
            println!("{report}");
            Ok(ExitCode::SUCCESS)
        }
        Command::WarmupRender => {
            warmup::render_warmup()?;
            Ok(ExitCode::SUCCESS)
        }
        Command::WarmupServer { pack_out } => {
            pawa_capture::warmup::build_server_pack(&pack_out)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::SchemaExport { out } => {
            pawa::schema_export(&out)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::QcActions {
            capture_dir,
            scenario,
        } => Ok(if units::qc_actions(&capture_dir, scenario.as_deref())? {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(3)
        }),
    }
}

/// Keep one deployment phase and prune dependencies on tasks in the other phase. The filesystem
/// status/work tree is the cross-phase boundary: after capture output is synced to the render host, a
/// render unit consumes those files directly and must not wait on a capture task id that does not
/// exist in the local HQ submission.
fn filter_stage(full: &plan::Plan, stage: PlanStage, out: &Path) -> Result<plan::Plan> {
    use pawa_schema::status::UnitOutcome;

    let eligible_render_episodes: std::collections::HashSet<&str> =
        if matches!(stage, PlanStage::Render) {
            let settled: std::collections::HashMap<String, UnitOutcome> = status::scan(out)?
                .into_iter()
                .map(|(path, status)| (path.display().to_string(), status.outcome))
                .collect();
            full.tasks
                .iter()
                .filter(|task| matches!(task.unit, plan::PlannedUnit::Capture))
                .filter(|task| {
                    let path = out.join(plan::status_rel_path(task)).display().to_string();
                    matches!(settled.get(&path), Some(UnitOutcome::Complete))
                })
                .map(|task| task.episode_id.as_str())
                .collect()
        } else {
            std::collections::HashSet::new()
        };

    let keep_unit = |task: &plan::PlannedTask| match stage {
        PlanStage::All => true,
        PlanStage::Capture => matches!(task.unit, plan::PlannedUnit::Capture),
        // Render can be started incrementally while capture hosts are still capturing, but only for episodes
        // whose capture status was actually synced. Pruning capture deps without this gate used to
        // emit every render immediately, turning not-yet-downloaded captures into contract failures.
        PlanStage::Render => {
            !matches!(task.unit, plan::PlannedUnit::Capture)
                && eligible_render_episodes.contains(task.episode_id.as_str())
        }
    };
    let kept: std::collections::HashSet<u32> = full
        .tasks
        .iter()
        .filter(|task| keep_unit(task))
        .map(|task| task.id)
        .collect();
    Ok(plan::Plan {
        tasks: full
            .tasks
            .iter()
            .filter(|task| kept.contains(&task.id))
            .map(|task| plan::PlannedTask {
                id: task.id,
                episode_id: task.episode_id.clone(),
                unit: task.unit.clone(),
                deps: task
                    .deps
                    .iter()
                    .copied()
                    .filter(|dep| kept.contains(dep))
                    .collect(),
            })
            .collect(),
    })
}

/// Drop tasks the status tree already settled: everything else (failures and never-ran
/// dependents alike) is kept for the (re)submission.
fn filter_plan(full: &plan::Plan, out: &Path, resume: bool) -> Result<plan::Plan> {
    use pawa_schema::status::UnitOutcome;
    if !resume {
        return Ok(plan::Plan {
            tasks: full.tasks.clone(),
        });
    }
    let settled: std::collections::HashMap<String, UnitOutcome> = status::scan(out)?
        .into_iter()
        .map(|(path, status)| (path.display().to_string(), status.outcome))
        .collect();
    let keep = |task: &plan::PlannedTask| {
        use pawa_schema::status::FailureClass;
        let rel = plan::status_rel_path(task);
        let outcome = settled.get(&out.join(&rel).display().to_string());
        !matches!(
            outcome,
            Some(
                UnitOutcome::Complete
                    | UnitOutcome::SkippedNoConnect
                    // A contract failure is a broken input, not a flaky stage — the taxonomy says
                    // it is never retried, so a resume round must not resubmit it either.
                    | UnitOutcome::Failed {
                        class: FailureClass::Contract
                    }
            )
        )
    };
    let kept: std::collections::HashSet<u32> = full
        .tasks
        .iter()
        .filter(|t| keep(t))
        .map(|t| t.id)
        .collect();
    let tasks = full
        .tasks
        .iter()
        .filter(|t| kept.contains(&t.id))
        .map(|t| plan::PlannedTask {
            id: t.id,
            episode_id: t.episode_id.clone(),
            unit: t.unit.clone(),
            // Deps on settled tasks are satisfied by their status files; HQ must not wait on
            // task ids that are not in this submission.
            deps: t
                .deps
                .iter()
                .copied()
                .filter(|d| kept.contains(d))
                .collect(),
        })
        .collect();
    Ok(plan::Plan { tasks })
}
