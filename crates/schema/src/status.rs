//! Unit status files and the run report — the orchestration layer's on-disk state.
//!
//! Every scheduled unit (capture, one render, finalize) writes exactly one status file under
//! `status/<episode_id>/`, atomically, as its last act. The HyperQueue task graph guarantees a
//! unit RAN before its dependents start, so at finalize time a MISSING status file is a hard error,
//! never "not yet" — that structural guarantee is what lets resume (`plan --resume`) and the run
//! report both be derived from this tree alone, with no in-memory driver state to lose.

use serde::{Deserialize, Serialize};

use crate::time::EpochMs;

/// One unit's outcome, e.g. `status/<ep>/capture.json` or `status/<ep>/p00.render.json`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UnitStatus {
    pub unit: UnitKind,
    pub episode_id: String,
    /// POV name for render units; absent for capture/finalize.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    pub outcome: UnitOutcome,
    /// 1-based scheduling attempt that produced this file.
    pub attempt: u32,
    pub started_epoch_ms: EpochMs,
    pub finished_epoch_ms: EpochMs,
    /// Failure detail when `outcome` is `failed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum UnitKind {
    Capture,
    RenderPov,
    RenderObserver,
    Finalize,
    Hud,
}

/// Outcome vocabulary, aligned with the process exit-code taxonomy: `complete` and `skipped` exit
/// 0 (a skip must not break the static task graph), `failed` carries which class of exit it was.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum UnitOutcome {
    Complete,
    /// The unit had nothing to do by design (e.g. rendering a POV whose bot never connected).
    SkippedNoConnect,
    Failed {
        class: FailureClass,
    },
}

/// Why a unit failed, mapped one-to-one onto process exit codes. Only these two: a unit that
/// crashes or is OOM-killed never reaches the writer, so no status file can record it — that case
/// is HyperQueue's worker-loss handling, not an entry in this taxonomy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    /// Exit 2 — bad config or contract violation. Never retried.
    Contract,
    /// Exit 3 — the stage itself failed (server never ready, render timed out). Retryable.
    Stage,
}

/// `runs/<run_id>.json` — the whole run's outcome, reconstructed from the status tree plus
/// HyperQueue's job info after the last retry round.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunReport {
    pub run_id: String,
    pub started_epoch_ms: EpochMs,
    pub finished_epoch_ms: EpochMs,
    /// Retry rounds executed (1 = no resubmission was needed).
    pub rounds: u32,
    pub units: Vec<UnitStatus>,
    pub succeeded_units: u32,
    pub skipped_units: u32,
    pub failed_units: u32,
    /// Episodes whose finalize completed — the run's actual yield.
    pub complete_episodes: Vec<String>,
}
