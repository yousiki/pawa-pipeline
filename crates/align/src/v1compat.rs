//! v1-format adapters — TEST AND SAMPLE SCAFFOLDING ONLY.
//!
//! The golden fixtures (`tests/fixtures/builder_and_witness`) are in the earlier v1 on-disk
//! format; these adapters lift them into the v2 types so the golden regression tests and the
//! review-sample tooling can replay them. They go away together with the fixtures once v2
//! fixtures replace them. Nothing in the production pipeline may import this.

#![doc(hidden)]

use std::collections::BTreeMap;

use pawa_schema::capture::{ActionEvent, ActionFrame, ActionPayload};
use pawa_schema::dataset::MovementKeys;
use pawa_schema::time::{ClientTick, PlayerTimeMs, ServerTick, WorldAgeTick};
use serde_json::Value;

pub fn opt_i64(value: &Value, key: &str) -> Option<i64> {
    value.get(key).and_then(Value::as_i64)
}

/// v1 event row → v2 [`ActionEvent`].
pub fn adapt_event(row: &Value) -> ActionEvent {
    let payload = row.get("payload").cloned().unwrap_or(Value::Null);
    let description = payload
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let mut extra = BTreeMap::new();
    if let Value::Object(map) = payload {
        for (key, value) in map {
            if key != "description" {
                extra.insert(key, value);
            }
        }
    }
    ActionEvent {
        seq: row["seq"].as_u64().unwrap(),
        action_id: row["action_id"].as_str().unwrap().to_string(),
        action_type: row["action_type"].as_str().unwrap().to_string(),
        kind: row["type"].as_str().unwrap().to_string(),
        client_tick: ClientTick(row["client_tick"].as_i64().unwrap()),
        server_tick: ServerTick(row["server_tick"].as_i64().unwrap()),
        world_age_tick: opt_i64(row, "world_age_tick").map(WorldAgeTick),
        player_time_ms: PlayerTimeMs(row["player_time_ms"].as_i64().unwrap()),
        visible_event_id: row
            .get("visible_event_id")
            .and_then(Value::as_str)
            .map(str::to_string),
        payload: ActionPayload { description, extra },
    }
}

/// v1 dense action row → v2 [`ActionFrame`].
pub fn adapt_action(row: &Value) -> ActionFrame {
    let keys = &row["keys"];
    ActionFrame {
        client_tick: ClientTick(row["client_tick"].as_i64().unwrap()),
        server_tick: ServerTick(row["server_tick"].as_i64().unwrap()),
        world_age_tick: opt_i64(row, "world_age_tick").map(WorldAgeTick),
        player_time_ms: PlayerTimeMs(row["player_time_ms"].as_i64().unwrap()),
        x: row["x"].as_f64().unwrap(),
        y: row["y"].as_f64().unwrap(),
        z: row["z"].as_f64().unwrap(),
        yaw: row["yaw"].as_f64().unwrap(),
        pitch: row["pitch"].as_f64().unwrap(),
        d_yaw: row["d_yaw"].as_f64().unwrap(),
        d_pitch: row["d_pitch"].as_f64().unwrap(),
        keys: MovementKeys {
            forward: keys["forward"].as_bool().unwrap(),
            back: keys["back"].as_bool().unwrap(),
            left: keys["left"].as_bool().unwrap(),
            right: keys["right"].as_bool().unwrap(),
            jump: keys["jump"].as_bool().unwrap(),
            sprint: keys["sprint"].as_bool().unwrap(),
            sneak: keys["sneak"].as_bool().unwrap(),
        },
        attack: row["attack"].as_bool().unwrap(),
        use_item: row["use"].as_bool().unwrap(),
        place_block: row["place_block"].as_bool().unwrap(),
        mine: row["mine"].as_bool().unwrap(),
        hotbar_slot: row
            .get("hotbar_slot")
            .and_then(Value::as_u64)
            .map(|v| v as u8),
        held_item: row
            .get("held_item")
            .and_then(Value::as_str)
            .map(str::to_string),
        // v1 recorded neither; the golden replay does not exercise them.
        is_in_water: false,
        inventory: None,
    }
}
