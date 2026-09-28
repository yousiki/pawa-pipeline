//! The static task graph: manifest in, HyperQueue Job Definition File out.
//!
//! The whole DAG is computable up front because everything it depends on is a pure function of
//! the manifest: the POV list comes from `bot_count`, the render
//! sample from a seeded rank over the episode id, observers from the render config. The one
//! runtime unknown — a bot that never connected — is handled INSIDE the render unit (it exits 0
//! with a `skipped_no_connect` status), so the graph never changes shape.
//!
//! Task IDs are assigned densely per episode: capture, then each render, then finalize with
//! `deps` on every render of that episode. Renders depend only on their episode's capture; two
//! episodes never wait on each other.

use std::collections::BTreeMap;

use pawa_schema::catalog::pov_name;
use pawa_schema::manifest::RunManifest;
use serde::Serialize;
use sha2::{Digest, Sha256};

/// One planned unit, in the vocabulary the `unit` subcommands accept.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PlannedTask {
    pub id: u32,
    pub episode_id: String,
    pub unit: PlannedUnit,
    pub deps: Vec<u32>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "target")]
pub enum PlannedUnit {
    Capture,
    RenderPov(String),
    RenderObserver(String),
    Finalize,
    Hud,
}

/// Deterministic render sample: every POV ranked by `sha256(episode_id ":" pov)`, lowest `k`
/// digests win. The choice is a pure, auditable function of the episode id, reproducible from any
/// language with a SHA-256.
pub fn sample_povs(episode_id: &str, bot_count: u32, sample: Option<u32>) -> Vec<String> {
    let all: Vec<String> = (0..bot_count as usize)
        .map(|i| pov_name(i, bot_count as usize))
        .collect();
    let Some(k) = sample else { return all };
    if k as usize >= all.len() {
        return all;
    }
    let mut ranked: Vec<(Vec<u8>, String)> = all
        .into_iter()
        .map(|pov| {
            let digest = Sha256::digest(format!("{episode_id}:{pov}").as_bytes());
            (digest.to_vec(), pov)
        })
        .collect();
    ranked.sort();
    let mut chosen: Vec<String> = ranked
        .into_iter()
        .take(k as usize)
        .map(|(_, pov)| pov)
        .collect();
    chosen.sort();
    chosen
}

/// The full plan for a run.
pub struct Plan {
    pub tasks: Vec<PlannedTask>,
}

pub fn plan(manifest: &RunManifest) -> Plan {
    let mut tasks = Vec::new();
    let mut next_id = 0u32;
    for episode in &manifest.episodes {
        let capture_id = next_id;
        next_id += 1;
        tasks.push(PlannedTask {
            id: capture_id,
            episode_id: episode.episode_id.clone(),
            unit: PlannedUnit::Capture,
            deps: Vec::new(),
        });
        let mut render_ids = Vec::new();
        for pov in sample_povs(
            &episode.episode_id,
            episode.player_count,
            manifest.render.pov_sample,
        ) {
            tasks.push(PlannedTask {
                id: next_id,
                episode_id: episode.episode_id.clone(),
                unit: PlannedUnit::RenderPov(pov),
                deps: vec![capture_id],
            });
            render_ids.push(next_id);
            next_id += 1;
        }
        // Observers wait for every player render of the episode: planning the camera needs the
        // join delta and all players' tracks, both of which presume the POV renders exist.
        let pov_render_ids = render_ids.clone();
        for observer in &manifest.render.observers {
            let mut deps = vec![capture_id];
            deps.extend(&pov_render_ids);
            tasks.push(PlannedTask {
                id: next_id,
                episode_id: episode.episode_id.clone(),
                unit: PlannedUnit::RenderObserver(observer.name.clone()),
                deps,
            });
            render_ids.push(next_id);
            next_id += 1;
        }
        let finalize_id = next_id;
        tasks.push(PlannedTask {
            id: finalize_id,
            episode_id: episode.episode_id.clone(),
            unit: PlannedUnit::Finalize,
            deps: render_ids,
        });
        next_id += 1;
        if manifest.render.hud {
            tasks.push(PlannedTask {
                id: next_id,
                episode_id: episode.episode_id.clone(),
                unit: PlannedUnit::Hud,
                deps: vec![finalize_id],
            });
            next_id += 1;
        }
    }
    Plan { tasks }
}

/// Where a unit's status file lives, relative to the out root. The absence of this file after the
/// task ran is a hard error (HQ deps guarantee "ran"), so finalize can distinguish
/// "skipped by design" (a status file saying so) from "vanished" structurally.
pub fn status_rel_path(task: &PlannedTask) -> String {
    match &task.unit {
        PlannedUnit::Capture => format!("status/{}/capture.json", task.episode_id),
        PlannedUnit::RenderPov(pov) => format!("status/{}/{pov}.render.json", task.episode_id),
        PlannedUnit::RenderObserver(name) => {
            format!("status/{}/{name}.render.json", task.episode_id)
        }
        PlannedUnit::Finalize => format!("status/{}/finalize.json", task.episode_id),
        PlannedUnit::Hud => format!("status/{}/hud.json", task.episode_id),
    }
}

/// Render the plan as a HyperQueue Job Definition File.
///
/// `pawa_bin` is the argv head for every task (the binary re-invokes itself as
/// `pawa unit <kind> ...`). Retry policy note: HQ retries worker LOSS natively; an exit-3
/// stage failure is resubmitted by the head script as a filtered JDF (`--only-failed` round),
/// so `crash_limit` here covers crashes only.
pub fn to_jdf(
    manifest: &RunManifest,
    plan: &Plan,
    pawa_bin: &str,
    manifest_path: &str,
    out_root: &str,
) -> String {
    #[derive(Serialize)]
    struct Jdf {
        name: String,
        #[serde(rename = "task")]
        tasks: Vec<JdfTask>,
    }
    #[derive(Serialize)]
    struct JdfTask {
        id: u32,
        command: Vec<String>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        deps: Vec<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        time_limit: Option<String>,
        priority: i32,
        stdout: String,
        stderr: String,
        #[serde(rename = "request")]
        requests: Vec<JdfRequest>,
    }
    #[derive(Serialize)]
    struct JdfRequest {
        resources: BTreeMap<String, u64>,
    }

    let render_passes = if manifest.render.depth { 2 } else { 1 };
    let render_time_limit: u64 = manifest
        .render
        .timeout_sec
        .values()
        .map(|&secs| u64::from(secs))
        .sum::<u64>()
        .saturating_mul(render_passes);
    // Depth-first over episodes: every task of an earlier episode outranks every task of a later
    // one, so a worker with free CPUs finishes started episodes (render → finalize → hud) before
    // opening new captures — finished datasets appear continuously instead of after a global
    // stage-by-stage sweep. Dependencies still gate readiness; priority only orders the ready.
    let episode_rank: BTreeMap<&str, i32> = manifest
        .episodes
        .iter()
        .enumerate()
        .map(|(index, episode)| (episode.episode_id.as_str(), -(index as i32)))
        .collect();
    let tasks = plan
        .tasks
        .iter()
        .map(|task| {
            let (mut argv, resources, time_limit) = match &task.unit {
                PlannedUnit::Capture => (
                    vec!["unit".into(), "capture".into()],
                    BTreeMap::from([
                        ("cpus".into(), u64::from(manifest.capture_cpus)),
                        ("captureslot".into(), 1),
                    ]),
                    // Capture bounds itself via its own timeouts; give HQ a generous outer lid.
                    Some(format!(
                        "{}s",
                        manifest
                            .episodes
                            .iter()
                            .find(|e| e.episode_id == task.episode_id)
                            .map(|e| (e.episode_duration_sec as u64).saturating_mul(4) + 1800)
                            .unwrap_or(7200)
                    )),
                ),
                PlannedUnit::RenderPov(pov) => (
                    vec![
                        "unit".into(),
                        "render-pov".into(),
                        "--pov".into(),
                        pov.clone(),
                    ],
                    BTreeMap::from([
                        ("cpus".into(), u64::from(manifest.render.threads_per_render)),
                        ("renderslot".into(), 1),
                    ]),
                    Some(format!("{render_time_limit}s")),
                ),
                PlannedUnit::RenderObserver(name) => (
                    vec![
                        "unit".into(),
                        "render-observer".into(),
                        "--observer".into(),
                        name.clone(),
                    ],
                    BTreeMap::from([
                        ("cpus".into(), u64::from(manifest.render.threads_per_render)),
                        ("renderslot".into(), 1),
                    ]),
                    Some(format!("{render_time_limit}s")),
                ),
                PlannedUnit::Finalize => (
                    vec!["unit".into(), "finalize".into()],
                    BTreeMap::from([("cpus".into(), 2u64), ("cpustage".into(), 1u64)]),
                    Some("1800s".into()),
                ),
                PlannedUnit::Hud => (
                    vec!["unit".into(), "hud".into()],
                    BTreeMap::from([("cpus".into(), 4u64), ("cpustage".into(), 1u64)]),
                    Some("3600s".into()),
                ),
            };
            let mut command = vec![pawa_bin.to_string()];
            command.append(&mut argv);
            command.extend([
                "--manifest".into(),
                manifest_path.to_string(),
                "--episode-id".into(),
                task.episode_id.clone(),
                "--out".into(),
                out_root.to_string(),
            ]);
            JdfTask {
                id: task.id,
                command,
                deps: task.deps.clone(),
                time_limit,
                priority: episode_rank
                    .get(task.episode_id.as_str())
                    .copied()
                    .unwrap_or(i32::MIN),
                stdout: format!("{out_root}/logs/{}-%{{TASK_ID}}.out", manifest.run_id),
                stderr: format!("{out_root}/logs/{}-%{{TASK_ID}}.err", manifest.run_id),
                requests: vec![JdfRequest { resources }],
            }
        })
        .collect();

    toml::to_string_pretty(&Jdf {
        name: manifest.run_id.clone(),
        tasks,
    })
    .expect("JDF serialization is infallible for these types")
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;

    #[derive(Deserialize)]
    struct ParsedJdf {
        task: Vec<ParsedTask>,
    }

    #[derive(Deserialize)]
    struct ParsedTask {
        command: Vec<String>,
        request: Vec<ParsedRequest>,
    }

    #[derive(Deserialize)]
    struct ParsedRequest {
        resources: BTreeMap<String, u64>,
    }

    fn manifest(json: &str) -> RunManifest {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn dag_shape_and_determinism() {
        let m = manifest(
            r#"{"run_id": "t", "episodes": [
                {"episode_id": "ep-a", "player_count": 4, "shards": 1,
                 "scenario": {"name": "group_orbit"}},
                {"episode_id": "ep-b", "player_count": 2, "shards": 1,
                 "scenario": {"name": "follow_leader"}}
            ],
            "render": {"pov_sample": 2,
                       "observers": [{"name": "obs00", "mode": "crowd_static"}]}}"#,
        );
        assert!(m.validate().is_empty());
        let plan_a = plan(&m);
        let plan_b = plan(&m);
        assert_eq!(
            plan_a.tasks, plan_b.tasks,
            "plan must be a pure function of the manifest"
        );

        // ep-a: capture + 2 sampled povs + 1 observer + finalize = 5; ep-b: capture + 2 + 1 + 1.
        assert_eq!(plan_a.tasks.len(), 5 + 5);
        let finalize = plan_a
            .tasks
            .iter()
            .find(|t| t.unit == PlannedUnit::Finalize)
            .unwrap();
        assert_eq!(
            finalize.deps.len(),
            3,
            "finalize waits on every render of its episode"
        );

        let sampled = sample_povs("ep-a", 4, Some(2));
        assert_eq!(sampled.len(), 2);
        assert_eq!(sampled, sample_povs("ep-a", 4, Some(2)));
        assert_ne!(
            sample_povs("ep-a", 4, Some(2)),
            sample_povs("ep-c", 4, Some(2))
        );

        let jdf = to_jdf(&m, &plan_a, "pawa", "/mnt/m.json", "/mnt/out");
        let parsed: ParsedJdf = toml::from_str(&jdf).unwrap();
        let resources = |unit: &str| {
            &parsed
                .task
                .iter()
                .find(|task| task.command.get(2).is_some_and(|arg| arg == unit))
                .unwrap()
                .request[0]
                .resources
        };
        assert_eq!(
            resources("capture"),
            &BTreeMap::from([
                ("captureslot".into(), 1),
                ("cpus".into(), u64::from(m.capture_cpus)),
            ])
        );
        assert_eq!(
            resources("render-pov"),
            &BTreeMap::from([
                ("cpus".into(), u64::from(m.render.threads_per_render)),
                ("renderslot".into(), 1),
            ])
        );
        assert_eq!(
            resources("render-observer"),
            &BTreeMap::from([
                ("cpus".into(), u64::from(m.render.threads_per_render)),
                ("renderslot".into(), 1),
            ])
        );
        assert_eq!(
            resources("finalize"),
            &BTreeMap::from([("cpus".into(), 2), ("cpustage".into(), 1)])
        );
    }

    #[test]
    fn hud_uses_cpu_stage_resource() {
        let m = manifest(
            r#"{"run_id":"t","episodes":[{"episode_id":"ep","player_count":1,
                "shards":1,"scenario":{"name":"landmark_tour"}}],"render":{"hud":true}}"#,
        );
        assert!(m.validate().is_empty());
        let jdf = to_jdf(&m, &plan(&m), "pawa", "/mnt/m.json", "/mnt/out");
        let parsed: ParsedJdf = toml::from_str(&jdf).unwrap();
        let hud = parsed
            .task
            .iter()
            .find(|task| task.command.get(2).is_some_and(|arg| arg == "hud"))
            .unwrap();
        assert_eq!(
            hud.request[0].resources,
            BTreeMap::from([("cpus".into(), 4), ("cpustage".into(), 1)])
        );
    }
}
