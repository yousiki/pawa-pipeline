//! The run report: the status tree, summarized.

use std::path::Path;

use anyhow::Result;
use pawa_schema::manifest::RunManifest;
use pawa_schema::status::{RunReport, UnitKind, UnitOutcome};
use pawa_schema::time::EpochMs;

use pawa::status;

/// Build a report for a phase-filtered plan. Split capture/render deployments use the same
/// manifest but intentionally complete different subsets; judging a capture-only tree against
/// missing render statuses would report every not-yet-run render as a false failure.
pub fn build_with_plan(
    manifest: &RunManifest,
    out_root: &Path,
    plan: &pawa::plan::Plan,
) -> Result<RunReport> {
    let statuses: std::collections::HashMap<String, _> = status::scan(out_root)?
        .into_iter()
        .map(|(path, status)| (path.display().to_string(), status))
        .collect();
    // Judge against the PLAN, not against whatever status files happen to exist: a unit that
    // crashed (or never ran because the scheduler died) leaves no file, and an empty status tree
    // must read as "everything missing", never as "nothing failed".
    let mut units = Vec::new();
    let mut missing = 0usize;
    for task in &plan.tasks {
        let rel = pawa::plan::status_rel_path(task);
        match statuses.get(&out_root.join(&rel).display().to_string()) {
            Some(status) => units.push(status.clone()),
            None => missing += 1,
        }
    }
    let mut succeeded = 0;
    let mut skipped = 0;
    let mut failed = missing as u32;
    let mut complete_episodes = Vec::new();
    for unit in &units {
        match unit.outcome {
            UnitOutcome::Complete => {
                succeeded += 1;
                if unit.unit == UnitKind::Finalize {
                    complete_episodes.push(unit.episode_id.clone());
                }
            }
            UnitOutcome::SkippedNoConnect => skipped += 1,
            UnitOutcome::Failed { .. } => failed += 1,
        }
    }
    complete_episodes.sort();
    let started = units
        .iter()
        .map(|u| u.started_epoch_ms)
        .min()
        .unwrap_or(EpochMs(0));
    let finished = units
        .iter()
        .map(|u| u.finished_epoch_ms)
        .max()
        .unwrap_or(EpochMs(0));
    let rounds = units.iter().map(|u| u.attempt).max().unwrap_or(1);
    Ok(RunReport {
        run_id: manifest.run_id.clone(),
        started_epoch_ms: started,
        finished_epoch_ms: finished,
        rounds,
        units,
        succeeded_units: succeeded,
        skipped_units: skipped,
        failed_units: failed,
        complete_episodes,
    })
}
