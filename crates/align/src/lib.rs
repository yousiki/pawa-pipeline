//! The align stage: joins one episode's rendered timelines to the world-age clock, assembles the
//! pre-joined `frames.jsonl` rows, and computes the QC evidence. Pure logic — no filesystem
//! layout knowledge; the `pawa` binary's units own paths and orchestration.

pub mod frames;
pub mod join;
pub mod observer;
pub mod qc;
pub mod v1compat;
