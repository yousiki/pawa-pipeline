//! The on-disk status tree: atomic writes, resume scans, run reports.
//!
//! Every unit writes exactly one `UnitStatus` file as its last act, atomically
//! (`NamedTempFile::persist` in the same directory, so the rename never crosses filesystems).
//! `scan` reads the whole tree once; resume (`plan --resume`) and the run report both derive from
//! it — there is no in-memory driver state to lose.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Context;
use pawa_schema::status::UnitStatus;

/// Atomically write `status` at `path` (parents created).
pub fn write_status(path: &Path, status: &UnitStatus) -> anyhow::Result<()> {
    let dir = path.parent().context("status path has no parent")?;
    fs::create_dir_all(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    serde_json::to_writer_pretty(&mut tmp, status)?;
    tmp.write_all(b"\n")?;
    tmp.persist(path)
        .with_context(|| format!("persist {}", path.display()))?;
    Ok(())
}

pub fn read_status(path: &Path) -> anyhow::Result<UnitStatus> {
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))
}

/// Every status file under `<out>/status/`, with its path. One directory walk — callers filter.
pub fn scan(out_root: &Path) -> anyhow::Result<Vec<(PathBuf, UnitStatus)>> {
    let status_root = out_root.join("status");
    let mut found = Vec::new();
    let Ok(episodes) = fs::read_dir(&status_root) else {
        return Ok(found); // No tree yet — a fresh run.
    };
    for episode in episodes {
        let episode = episode?;
        if !episode.file_type()?.is_dir() {
            continue;
        }
        for entry in fs::read_dir(episode.path())? {
            let path = entry?.path();
            if path.extension().is_some_and(|ext| ext == "json") {
                match read_status(&path) {
                    Ok(status) => found.push((path, status)),
                    // A torn or foreign file is not fatal to a scan; resume simply re-runs the
                    // unit, which is the safe direction.
                    Err(_) => continue,
                }
            }
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pawa_schema::status::{UnitKind, UnitOutcome, UnitStatus};
    use pawa_schema::time::EpochMs;

    #[test]
    fn round_trip_and_scan() {
        let dir = tempfile::tempdir().unwrap();
        let status = UnitStatus {
            unit: UnitKind::RenderPov,
            episode_id: "ep-a".into(),
            target: Some("p00".into()),
            outcome: UnitOutcome::SkippedNoConnect,
            attempt: 1,
            started_epoch_ms: EpochMs(1),
            finished_epoch_ms: EpochMs(2),
            error: None,
        };
        let path = dir.path().join("status/ep-a/p00.render.json");
        write_status(&path, &status).unwrap();
        assert_eq!(read_status(&path).unwrap(), status);
        let scanned = scan(dir.path()).unwrap();
        assert_eq!(scanned.len(), 1);
        assert_eq!(scanned[0].1, status);
    }
}
