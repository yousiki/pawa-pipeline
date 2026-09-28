//! The `pawa` binary's library half: planning, status bookkeeping, unit bodies. The stage
//! implementations live in their own crates (`pawa-capture`, `pawa-render`,
//! `pawa-align`, `pawa-hud`); the binary wires them behind `pawa unit <kind>`.

use std::path::Path;

pub mod plan;
pub mod status;
pub mod units;

/// Write every contract's JSON Schema under `out` (the `schema/` tree of a run's out root).
pub fn schema_export(out: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(out)?;
    macro_rules! export {
        ($name:literal, $ty:ty) => {
            let schema = schemars::schema_for!($ty);
            std::fs::write(
                out.join(concat!($name, ".schema.json")),
                serde_json::to_string_pretty(&schema)? + "\n",
            )?;
        };
    }
    export!("episode", pawa_schema::dataset::EpisodeManifest);
    export!("frame-row", pawa_schema::dataset::FrameRow);
    export!("event-row", pawa_schema::dataset::EventRow);
    export!("episode-info", pawa_schema::dataset::SolarisEpisodeInfo);
    export!("qc", pawa_schema::dataset::QcReport);
    export!("run-manifest", pawa_schema::manifest::RunManifest);
    export!("driver-spec", pawa_schema::driver::DriverSpec);
    export!("driver-status", pawa_schema::driver::DriverStatus);
    export!("pose-doc", pawa_schema::driver::PoseDoc);
    export!("unit-status", pawa_schema::status::UnitStatus);
    export!("run-report", pawa_schema::status::RunReport);
    export!("capture-record", pawa_schema::capture::CaptureRecord);
    export!("player-run-config", pawa_schema::PlayerRunConfig);
    Ok(())
}
