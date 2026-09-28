//! Golden replay: a reference episode (`tests/fixtures/builder_and_witness`, recorded in the
//! earlier v1 on-disk format) is fed through the join, and the conclusions must match the
//! reference `p*-alignment.json` stored alongside it. The fixtures are v1-format files;
//! the adapters below translate them to the v2 types in memory — they are test scaffolding, not
//! product code.

use std::path::{Path, PathBuf};

use pawa_align::frames::{EpisodeRowCtx, assemble_frames, frame_time_ms};
use pawa_align::join::{Anchors, metadata_join_with_phase};
use pawa_align::v1compat::{adapt_action, adapt_event, opt_i64};
use pawa_schema::capture::{ActionEvent, ActionFrame};
use pawa_schema::dataset::AnchorMode;
use pawa_schema::driver::{PoseDoc, PoseKeyframe};
use pawa_schema::time::{EpochMs, WorldAgeTick};
use serde_json::Value;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/builder_and_witness")
        .join(name)
}

fn read_json(name: &str) -> Value {
    serde_json::from_str(&std::fs::read_to_string(fixture(name)).unwrap()).unwrap()
}

fn read_jsonl(name: &str) -> Vec<Value> {
    std::fs::read_to_string(fixture(name))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

struct GoldenPov {
    events: Vec<ActionEvent>,
    anchors: Anchors,
    frame_count: u64,
    expected: Value,
}

fn load_pov(pov: &str) -> GoldenPov {
    let summary = read_json("capture-summary.json");
    let meta = read_json(&format!("{pov}-meta.json"));
    let username = meta["username"].as_str().unwrap();
    let stop = summary["recording_stop_gametime_by_player"]
        .get(username)
        .and_then(Value::as_i64)
        .or_else(|| opt_i64(&summary, "recording_stop_gametime"));
    GoldenPov {
        events: read_jsonl(&format!("{pov}-events.jsonl"))
            .iter()
            .map(adapt_event)
            .collect(),
        anchors: Anchors {
            go_world_age_tick: opt_i64(&meta, "go_world_age").map(WorldAgeTick),
            recording_start_world_age_tick: opt_i64(&summary, "recording_start_gametime")
                .map(WorldAgeTick),
            recording_stop_world_age_tick: stop.map(WorldAgeTick),
        },
        frame_count: read_jsonl(&format!("{pov}-frames.jsonl")).len() as u64,
        expected: read_json(&format!("{pov}-alignment.json")),
    }
}

#[test]
fn join_reproduces_the_golden_alignment() {
    for pov in ["p00", "p01"] {
        let golden = load_pov(pov);
        let join = metadata_join_with_phase(
            &golden.events,
            golden.frame_count,
            20,
            20,
            6,
            golden.anchors,
            0, // v1 had no display-phase compensation; the golden comparison must not either.
        )
        .unwrap();
        let expected = &golden.expected["metadata_join"];

        assert_eq!(
            join.delta, expected["delta_world_age_minus_replay_tick"],
            "{pov}: delta"
        );
        let anchor = match expected["anchor_mode"].as_str().unwrap() {
            "stop_anchored_frames" => AnchorMode::StopAnchoredFrames,
            "recording_start_gametime" => AnchorMode::RecordingStartWorldAge,
            "go_world_age" => AnchorMode::GoWorldAge,
            other => panic!("unknown anchor {other}"),
        };
        assert_eq!(join.qc.anchor, anchor, "{pov}: anchor");
        assert_eq!(
            join.qc.actions_matched,
            expected["actions_matched"].as_u64().unwrap(),
            "{pov}"
        );
        assert_eq!(
            join.qc.actions_unavailable,
            expected["actions_unavailable"].as_u64().unwrap(),
            "{pov}"
        );
        assert_eq!(
            join.qc.dropped_trailing_count,
            expected["dropped_trailing_count"].as_u64().unwrap(),
            "{pov}"
        );
        if golden.expected["status"] == "pass" {
            assert!(join.failures.is_empty(), "{pov}: {:?}", join.failures);
        }
    }
}

/// Pin the whole QC block for p00 so an unnoticed semantic change shows up as a snapshot diff.
#[test]
fn p00_alignment_qc_snapshot() {
    let golden = load_pov("p00");
    let join = metadata_join_with_phase(
        &golden.events,
        golden.frame_count,
        20,
        20,
        6,
        golden.anchors,
        0,
    )
    .unwrap();
    insta::assert_json_snapshot!(join.qc);
}

/// Frame assembly self-consistency against the golden timeline: feeding the v1 rows' own cameras
/// back as a pose trace must reproduce them 1:1, and the world-age arithmetic must land every v1
/// row at `replay_tick + delta`. Field names are v3 (solaris vocabulary): `relativeTimeMs` is the
/// old `replay_time_ms` grid, `worldAgeTicks` the old `world_age_tick`, `camera_pose` the old
/// `camera`.
#[test]
fn frame_assembly_reproduces_the_golden_timeline() {
    let golden = load_pov("p00");
    let join = metadata_join_with_phase(
        &golden.events,
        golden.frame_count,
        20,
        20,
        6,
        golden.anchors,
        0,
    )
    .unwrap();
    let v1_rows = read_jsonl("p00-frames.jsonl");
    let poses = PoseDoc {
        source: None,
        keyframes: v1_rows
            .iter()
            .filter_map(|row| {
                let camera = row.get("camera")?;
                Some(PoseKeyframe {
                    time_ms: row["replay_time_ms"].as_i64().unwrap(),
                    x: camera["x"].as_f64().unwrap(),
                    y: camera["y"].as_f64().unwrap(),
                    z: camera["z"].as_f64().unwrap(),
                    yaw: camera["yaw"].as_f64().unwrap(),
                    pitch: camera["pitch"].as_f64().unwrap(),
                    roll: camera.get("roll").and_then(Value::as_f64).unwrap_or(0.0),
                })
            })
            .collect(),
    };
    let actions: Vec<ActionFrame> = read_jsonl("p00-actions.jsonl")
        .iter()
        .map(adapt_action)
        .collect();

    let ctx = EpisodeRowCtx {
        fps: 20,
        tick_rate_hz: 20,
        started_epoch_ms: EpochMs(1_755_600_000_000),
        episode_seq: 0,
        seed: 2026,
    };
    let outcome = assemble_frames(golden.frame_count, &ctx, &poses, &actions, join.delta, 70.0);
    assert_eq!(outcome.rows.len(), v1_rows.len());
    assert_eq!(outcome.coverage.unposed_interior, 0);
    assert!(outcome.action_rows_matched > 0);

    for (v3, v1) in outcome.rows.iter().zip(&v1_rows) {
        assert_eq!(
            v3.relative_time_ms.get(),
            v1["replay_time_ms"].as_i64().unwrap()
        );
        assert_eq!(
            v3.world_age_ticks.get(),
            v1["replay_tick"].as_i64().unwrap() + join.delta
        );
        // The synthetic wall clock is base + grid time, by construction.
        assert_eq!(
            v3.render_time.get(),
            ctx.started_epoch_ms.get() + v3.relative_time_ms.get()
        );
        match (v3.camera_pose.as_ref(), v1.get("camera")) {
            (Some(camera), Some(v1_camera)) => {
                assert_eq!(camera.x, v1_camera["x"].as_f64().unwrap());
                assert_eq!(camera.yaw, v1_camera["yaw"].as_f64().unwrap());
            }
            (None, None) => {}
            (v3_camera, v1_camera) => panic!(
                "frame {}: pose presence diverged (v3 {:?} vs v1 {:?})",
                v3.frame_count,
                v3_camera.is_some(),
                v1_camera.is_some()
            ),
        }
        // Convention spot check: the v3 body yaw must be the mineflayer image of the capture
        // radians the adapter fed in (v1 actions are notchian radians).
        if let (Some(yaw_mf), Some(_)) = (v3.yaw, v3.action.as_ref()) {
            assert!((0.0..std::f64::consts::TAU).contains(&yaw_mf));
        }
    }
    // The grid itself is the one the v1 pipeline wrote.
    for row in &v1_rows[..20] {
        assert_eq!(
            frame_time_ms(row["frame_index"].as_u64().unwrap(), 20),
            row["replay_time_ms"].as_i64().unwrap()
        );
    }
}
