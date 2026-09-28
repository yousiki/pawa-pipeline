//! Scenario scripts, one module per catalog entry.
//!
//! Many scripts refer to "the revisit problem": in generation-5 data, many training windows had
//! short one-way trajectories and camera turns that never came back, so the ground-truth frames
//! showed content the context frame could not have observed (zero overlap between first and last
//! frame; a teammate walking into frame from nowhere). Generation-6+ scripts counter it with
//! loop-shaped trajectories, camera turns that return, and compact staging.

use crate::*;

mod action_sampler;
mod arena_build_medley;
pub(super) mod audience_build;
pub(super) mod build_house;
mod build_tower;
mod calibration;
mod carousel_chase;
mod circuit_bearing_ladder;
mod circuit_bowtie_cross;
mod circuit_counter_pair_meet;
pub(super) mod circuit_cross_revisit;
mod circuit_dogleg_retrace;
mod circuit_double_loop;
mod circuit_foldback_route;
mod circuit_gate_weave;
mod circuit_landmark_chain;
mod circuit_lap_tally_loop;
mod circuit_petal_hub_return;
mod circuit_polygon_return;
mod circuit_shuttle_gate;
mod circuit_square_backsight;
mod circuit_switchback_loop;
mod collector;
mod column_relay_wall;
mod common;
mod compact_repair_shuttle;
mod converge_scatter_pulse;
mod copy_chain_extension;
mod counter_ring_flow;
mod cross_region_rendezvous;
mod crowded_interaction;
mod depart_and_revisit;
pub(super) mod duet_anchor_calibration;
mod duet_call_response;
mod duet_face_off_wall;
mod duet_ferry_marker;
mod duet_joint_compass;
pub(super) mod duet_layer_swap;
mod duet_screen_peek;
mod duet_shadow_lesson;
mod duet_span_closure;
mod duet_twin_sightline;
mod duetloop_body_shutter;
mod duetloop_branch_rejoin;
mod duetloop_cross_triangulation;
mod duetloop_endpoint_transfer;
mod eval_grounding_turn_back;
mod eval_memory_dwell;
mod eval_shared_unobserved;
mod figure_eight_weave;
mod follow_cam_chain;
pub(super) mod follow_leader;
mod formation_morph;
pub(super) mod formation_walk;
mod gap_jump;
pub(super) mod group_orbit;
mod gully_ring_traffic;
mod handoff_edit_crosscheck;
mod handoff_report_clover;
mod handoff_reveal_order;
mod heading_mine;
mod independent_build_structure;
mod independent_build_tower;
mod landmark_azimuth_height;
mod landmark_depth_order;
mod landmark_marker_pair;
mod landmark_tiered_landform;
mod landmark_tour;
mod landmark_triple_depth_relay;
mod loopbuild_axis_mirror;
mod loopbuild_beacon_anchor;
mod loopbuild_copy_template;
mod loopbuild_corner_retrace;
mod loopbuild_facing_pair;
mod loopbuild_occlusion_reveal;
mod loopbuild_raze_revisit;
mod loopbuild_riser_backcheck;
mod loopbuild_step_back_review;
mod loopbuild_tier_pullback;
mod mirror_dance;
mod mob_chase;
mod no_contact_pair_chase;
mod occl_archway_exit;
mod occl_boulder_slalom;
mod occl_canopy_shelter;
mod occl_corner_bypass;
mod occl_double_screen_zigzag;
mod occl_dropoff_climb;
mod occl_gate_corridor;
mod occl_grove_weave;
mod occl_peer_eclipse;
mod occl_ridge_window;
mod occl_terrain_return;
mod occl_tree_trunk_pass;
mod occl_twin_slit_transfer;
mod occl_wade_sightline;
mod occl_wall_peekaboo;
mod orbit_lane_swap;
mod pair_chase;
mod parallel_plot_remodel;
mod phase_shifted_ring_patrol;
mod pillar_rise_and_collapse;
mod plot_swap_raze;
mod pve;
mod pvp;
mod radial_trail_build;
mod reflection_build_pairs;
mod regional_capacity_traffic;
mod revisit_anchor_triangle;
mod revisit_breadcrumb_backtrack;
mod revisit_cache_probe;
mod revisit_departure_ladder;
mod revisit_growing_cairn;
mod revisit_multi_anchor_crosscheck;
mod revisit_mutated_landmark;
mod revisit_mutual_sentry;
mod revisit_occluded_return;
mod revisit_temporal_scale;
mod ridge_valley_circuit;
mod ring_mosaic_bloom;
mod ring_slot_exchange;
mod rosette_tower_ring;
mod row_replace_after_detour;
mod satellite_escort;
mod sentry_scan_rotation;
mod shaft_three_fill_one;
mod sightmark_horizon_pair;
mod sightmark_natural_triangle;
mod sightmark_waterbend_arc;
mod signal_post_rounds;
mod site_handoff_build;
mod skyline_build_yard;
mod sparse_checkpoint_return;
mod sparse_single_block_lesson;
mod sparse_single_placement;
mod spiral_march;
mod split_and_reunite;
mod stereo_antipodal_orbit;
mod stereo_arc_sweep;
mod stereo_baseline_ladder;
mod stereo_baseline_pulse;
mod stereo_convergence_fan;
mod stereo_covisibility_bloom;
mod stereo_crossing_exchange;
mod stereo_crossover_swap;
mod stereo_inline_depth_relay;
mod stereo_occlusion_complement;
mod stereo_opposed_landmark;
mod stereo_parallel_rail;
mod stereo_staggered_baseline;
mod stereo_stair_parallax;
mod straight_line_walk;
mod structure_metronome;
pub(super) mod terrain_bank_height;
mod terrain_boulder_backsight;
mod terrain_canyon_axis;
mod terrain_contour_traverse;
pub(super) mod terrain_crest_and_back;
mod terrain_gully_crossgaze;
mod terrain_hollow_mouth_cycle;
mod terrain_overhang_skylight;
mod terrain_saddle_pass;
mod terrain_shallow_wade_return;
pub(super) mod terrain_shore_flank_walk;
mod terrain_step_shuttle_pitch;
mod terrain_tree_gap;
mod torch_route_and_gate;
mod totem_orbit_film;
mod trail_write_and_erase;
mod v7;
mod vertical_relay_crew;
mod wade_column_detour;
mod walk_look;
mod walk_look_away;
mod wander_edit_trail;
mod watch_build_progress;
mod watch_flank_station;
mod watch_lead_swap;
mod watch_locked_dolly;
mod watch_loop_courier;
mod watch_marker_trail;
mod watch_mutual_arc;
mod wave_relay;
mod zigzag_lane_shuttle;

use common::*;

/// Hostile mob kinds the pve episode targets — the azalea `EntityKind` analogue of solaris
/// fighting.js's `HOSTILE_MOBS_SUMMON_IDS` ({zombie,skeleton,spider,husk}). These are exactly the
/// mobs the server coordinator is expected to summon near the bots (see the pve doc-comment). Kept
/// in sync with the coordinator: if it summons other hostiles, add their kinds here too.
const HOSTILE_KINDS: [EntityKind; 4] = [
    EntityKind::Zombie,
    EntityKind::Skeleton,
    EntityKind::Spider,
    EntityKind::Husk,
];

pub(crate) fn is_hostile_kind(kind: EntityKind) -> bool {
    HOSTILE_KINDS.contains(&kind)
}

pub(crate) async fn run_scenario(ctx: &mut Ctx) {
    // The recorder starts before scenario code runs. Establish the v6 context frame here, before
    // any scenario-specific movement or construction can change the view: the anchor is a shared,
    // observed region and is the common reference that makes later turn-away/return windows
    // interpretable. The scenario scripts also settle defensively before probing their own terrain;
    // repeating that cheap wait is preferable to letting a falling spawn make the first frame random.
    if pawa_schema::catalog::scenario(ctx.engine().name.as_str())
        .is_some_and(|info| info.generation == 6)
    {
        settle_on_ground(ctx).await;
        if let Some((x, y, z)) = ctx.position() {
            let (ax, az) = ctx.anchor();
            let opened = ctx.look_until_facing((ax, y + 1.4, az), 8.0, 5_000).await;
            ctx.record(
                "observe",
                &format!("vis-v6-opening-anchor-{}", ctx.index()),
                "Open the v6 episode on the shared anchor before its first loop.",
                json!({
                    "opening_confirmed": opened,
                    "anchor": {"x": r2(ax), "z": r2(az)},
                    "start": {"x": r2(x), "y": r2(y), "z": r2(z)},
                    "generation": 6,
                }),
            );
        }
    }
    if pawa_schema::catalog::scenario(ctx.engine().name.as_str())
        .is_some_and(|info| info.generation == 7)
    {
        settle_on_ground(ctx).await;
        if let Some((x, y, z)) = ctx.position() {
            let (ax, az) = ctx.anchor();
            let peer = (0..ctx.player_count())
                .filter(|&j| j != ctx.index())
                .filter_map(|j| ctx.peer_position(j).map(|p| (j, (p.0 - x).hypot(p.2 - z))))
                .min_by(|a, b| a.1.total_cmp(&b.1))
                .map(|(j, _)| j);
            let target = peer
                .and_then(|j| ctx.peer_position(j).map(head_of))
                .unwrap_or((ax, y + 1.4, az));
            let opened = ctx.look_until_facing(target, 8.0, 5_000).await;
            ctx.record(
                "observe",
                &format!("vis-v7-opening-anchor-{}", ctx.index()),
                "Open the v7 episode on the nearest teammate, or the shared anchor if unavailable.",
                json!({
                    "opening_confirmed": opened,
                    "peer": peer,
                    "anchor": {"x": r2(ax), "z": r2(az)},
                    "start": {"x": r2(x), "y": r2(y), "z": r2(z)},
                    "generation": 7,
                }),
            );
        }
    }
    if pawa_schema::catalog::scenario(ctx.engine().name.as_str())
        .is_some_and(|info| info.generation == 7)
    {
        v7::run(ctx).await;
        return;
    }
    match ctx.engine().name.as_str() {
        "follow_leader" => follow_leader::run(ctx).await,
        "formation_walk" => formation_walk::run(ctx).await,
        "group_orbit" => group_orbit::run(ctx).await,
        "audience_build" => audience_build::run(ctx).await,
        "pair_chase" => pair_chase::run(ctx).await,
        "mob_chase" => mob_chase::run(ctx).await,
        "straight_line_walk" => straight_line_walk::run(ctx).await,
        "gap_jump" => gap_jump::run(ctx).await,
        "collector" => collector::run(ctx).await,
        "build_house" => build_house::run(ctx).await,
        "walk_look" => walk_look::run(ctx, false).await,
        "walk_look_away" => walk_look_away::run(ctx).await,
        "heading_mine" => heading_mine::run(ctx).await,
        "build_tower" => build_tower::run(ctx, false).await,
        "independent_build_tower" => independent_build_tower::run(ctx).await,
        "independent_build_structure" => independent_build_structure::run(ctx).await,
        "pvp" => pvp::run(ctx).await,
        "pve" => pve::run(ctx).await,
        "calibration" => calibration::run(ctx).await,
        "cross_region_rendezvous" => cross_region_rendezvous::run(ctx, false).await,
        "rendezvous_after_edits" => cross_region_rendezvous::run(ctx, true).await,
        "split_and_reunite" => split_and_reunite::run(ctx).await,
        "depart_and_revisit" => depart_and_revisit::run(ctx).await,
        "crowded_interaction" => crowded_interaction::run(ctx).await,
        "regional_capacity_traffic" => regional_capacity_traffic::run(ctx).await,
        // Translated from the retired JSON DSL + the action sampler.
        "column_relay_wall" => column_relay_wall::run(ctx).await,
        "compact_repair_shuttle" => compact_repair_shuttle::run(ctx).await,
        "pillar_rise_and_collapse" => pillar_rise_and_collapse::run(ctx).await,
        "row_replace_after_detour" => row_replace_after_detour::run(ctx).await,
        "sparse_single_placement" => sparse_single_placement::run(ctx).await,
        "torch_route_and_gate" => torch_route_and_gate::run(ctx).await,
        "vertical_relay_crew" => vertical_relay_crew::run(ctx).await,
        "phase_shifted_ring_patrol" => phase_shifted_ring_patrol::run(ctx).await,
        "ring_slot_exchange" => ring_slot_exchange::run(ctx).await,
        "sparse_checkpoint_return" => sparse_checkpoint_return::run(ctx).await,
        "zigzag_lane_shuttle" => zigzag_lane_shuttle::run(ctx).await,
        "shaft_three_fill_one" => shaft_three_fill_one::run(ctx).await,
        "sparse_single_block_lesson" => sparse_single_block_lesson::run(ctx).await,
        "no_contact_pair_chase" => no_contact_pair_chase::run(ctx).await,
        "action_sampler" => action_sampler::run(ctx).await,
        "skyline_build_yard" => skyline_build_yard::run(ctx).await,
        "trail_write_and_erase" => trail_write_and_erase::run(ctx).await,
        "parallel_plot_remodel" => parallel_plot_remodel::run(ctx).await,
        "radial_trail_build" => radial_trail_build::run(ctx).await,
        "arena_build_medley" => arena_build_medley::run(ctx).await,
        "wander_edit_trail" => wander_edit_trail::run(ctx).await,
        "formation_morph" => formation_morph::run(ctx).await,
        "wave_relay" => wave_relay::run(ctx).await,
        "signal_post_rounds" => signal_post_rounds::run(ctx).await,
        "converge_scatter_pulse" => converge_scatter_pulse::run(ctx).await,
        // Non-build motion scenes.
        "carousel_chase" => carousel_chase::run(ctx).await,
        "ridge_valley_circuit" => ridge_valley_circuit::run(ctx).await,
        "spiral_march" => spiral_march::run(ctx).await,
        "mirror_dance" => mirror_dance::run(ctx).await,
        "site_handoff_build" => site_handoff_build::run(ctx).await,
        "figure_eight_weave" => figure_eight_weave::run(ctx).await,
        // World-edit rhythm scenes.
        "plot_swap_raze" => plot_swap_raze::run(ctx).await,
        "ring_mosaic_bloom" => ring_mosaic_bloom::run(ctx).await,
        "structure_metronome" => structure_metronome::run(ctx).await,
        "reflection_build_pairs" => reflection_build_pairs::run(ctx).await,
        "rosette_tower_ring" => rosette_tower_ring::run(ctx).await,
        "copy_chain_extension" => copy_chain_extension::run(ctx).await,
        // Camera-behavior scenes. All resolve peers through
        // shard-local indices (totem_orbit_film also consumes the in-process recent_edits
        // witness channel), hence single_process.
        "totem_orbit_film" => totem_orbit_film::run(ctx).await,
        "follow_cam_chain" => follow_cam_chain::run(ctx).await,
        "sentry_scan_rotation" => sentry_scan_rotation::run(ctx).await,
        // Rotation-family group motion.
        "counter_ring_flow" => counter_ring_flow::run(ctx).await,
        "orbit_lane_swap" => orbit_lane_swap::run(ctx).await,
        "satellite_escort" => satellite_escort::run(ctx).await,
        // Lateral terrain-crossing scenes.
        "gully_ring_traffic" => gully_ring_traffic::run(ctx).await,
        "eval_grounding_turn_back" => eval_grounding_turn_back::run(ctx).await,
        "eval_memory_dwell" => eval_memory_dwell::run(ctx).await,
        "eval_shared_unobserved" => eval_shared_unobserved::run(ctx).await,
        "wade_column_detour" => wade_column_detour::run(ctx).await,
        "circuit_bearing_ladder" => circuit_bearing_ladder::run(ctx).await,
        "circuit_counter_pair_meet" => circuit_counter_pair_meet::run(ctx).await,
        "circuit_dogleg_retrace" => circuit_dogleg_retrace::run(ctx).await,
        "circuit_lap_tally_loop" => circuit_lap_tally_loop::run(ctx).await,
        "circuit_petal_hub_return" => circuit_petal_hub_return::run(ctx).await,
        "circuit_shuttle_gate" => circuit_shuttle_gate::run(ctx).await,
        "circuit_square_backsight" => circuit_square_backsight::run(ctx).await,
        "duet_face_off_wall" => duet_face_off_wall::run(ctx).await,
        "duet_ferry_marker" => duet_ferry_marker::run(ctx).await,
        "duet_layer_swap" => duet_layer_swap::run(ctx).await,
        "duet_screen_peek" => duet_screen_peek::run(ctx).await,
        "duet_shadow_lesson" => duet_shadow_lesson::run(ctx).await,
        "duet_span_closure" => duet_span_closure::run(ctx).await,
        "duet_twin_sightline" => duet_twin_sightline::run(ctx).await,
        "loopbuild_axis_mirror" => loopbuild_axis_mirror::run(ctx).await,
        "loopbuild_beacon_anchor" => loopbuild_beacon_anchor::run(ctx).await,
        "loopbuild_copy_template" => loopbuild_copy_template::run(ctx).await,
        "loopbuild_facing_pair" => loopbuild_facing_pair::run(ctx).await,
        "loopbuild_raze_revisit" => loopbuild_raze_revisit::run(ctx).await,
        "loopbuild_step_back_review" => loopbuild_step_back_review::run(ctx).await,
        "loopbuild_tier_pullback" => loopbuild_tier_pullback::run(ctx).await,
        "occl_canopy_shelter" => occl_canopy_shelter::run(ctx).await,
        "occl_dropoff_climb" => occl_dropoff_climb::run(ctx).await,
        "occl_grove_weave" => occl_grove_weave::run(ctx).await,
        "occl_peer_eclipse" => occl_peer_eclipse::run(ctx).await,
        "occl_terrain_return" => occl_terrain_return::run(ctx).await,
        "occl_wade_sightline" => occl_wade_sightline::run(ctx).await,
        "occl_wall_peekaboo" => occl_wall_peekaboo::run(ctx).await,
        "revisit_anchor_triangle" => revisit_anchor_triangle::run(ctx).await,
        "revisit_breadcrumb_backtrack" => revisit_breadcrumb_backtrack::run(ctx).await,
        "revisit_cache_probe" => revisit_cache_probe::run(ctx).await,
        "revisit_departure_ladder" => revisit_departure_ladder::run(ctx).await,
        "revisit_growing_cairn" => revisit_growing_cairn::run(ctx).await,
        "revisit_mutated_landmark" => revisit_mutated_landmark::run(ctx).await,
        "revisit_mutual_sentry" => revisit_mutual_sentry::run(ctx).await,
        "stereo_antipodal_orbit" => stereo_antipodal_orbit::run(ctx).await,
        "stereo_arc_sweep" => stereo_arc_sweep::run(ctx).await,
        "stereo_baseline_ladder" => stereo_baseline_ladder::run(ctx).await,
        "stereo_convergence_fan" => stereo_convergence_fan::run(ctx).await,
        "stereo_crossover_swap" => stereo_crossover_swap::run(ctx).await,
        "stereo_parallel_rail" => stereo_parallel_rail::run(ctx).await,
        "terrain_contour_traverse" => terrain_contour_traverse::run(ctx).await,
        "terrain_crest_and_back" => terrain_crest_and_back::run(ctx).await,
        "terrain_gully_crossgaze" => terrain_gully_crossgaze::run(ctx).await,
        "terrain_hollow_mouth_cycle" => terrain_hollow_mouth_cycle::run(ctx).await,
        "terrain_shallow_wade_return" => terrain_shallow_wade_return::run(ctx).await,
        "terrain_shore_flank_walk" => terrain_shore_flank_walk::run(ctx).await,
        "terrain_step_shuttle_pitch" => terrain_step_shuttle_pitch::run(ctx).await,
        "watch_build_progress" => watch_build_progress::run(ctx).await,
        "watch_flank_station" => watch_flank_station::run(ctx).await,
        "watch_lead_swap" => watch_lead_swap::run(ctx).await,
        "watch_locked_dolly" => watch_locked_dolly::run(ctx).await,
        "watch_loop_courier" => watch_loop_courier::run(ctx).await,
        "watch_marker_trail" => watch_marker_trail::run(ctx).await,
        "watch_mutual_arc" => watch_mutual_arc::run(ctx).await,
        "stereo_inline_depth_relay" => stereo_inline_depth_relay::run(ctx).await,
        "circuit_polygon_return" => circuit_polygon_return::run(ctx).await,
        "occl_tree_trunk_pass" => occl_tree_trunk_pass::run(ctx).await,
        "stereo_baseline_pulse" => stereo_baseline_pulse::run(ctx).await,
        "stereo_crossing_exchange" => stereo_crossing_exchange::run(ctx).await,
        "stereo_opposed_landmark" => stereo_opposed_landmark::run(ctx).await,
        "terrain_boulder_backsight" => terrain_boulder_backsight::run(ctx).await,
        "circuit_cross_revisit" => circuit_cross_revisit::run(ctx).await,
        "circuit_double_loop" => circuit_double_loop::run(ctx).await,
        "circuit_foldback_route" => circuit_foldback_route::run(ctx).await,
        "circuit_gate_weave" => circuit_gate_weave::run(ctx).await,
        "duet_anchor_calibration" => duet_anchor_calibration::run(ctx).await,
        "duet_call_response" => duet_call_response::run(ctx).await,
        "loopbuild_corner_retrace" => loopbuild_corner_retrace::run(ctx).await,
        "loopbuild_occlusion_reveal" => loopbuild_occlusion_reveal::run(ctx).await,
        "loopbuild_riser_backcheck" => loopbuild_riser_backcheck::run(ctx).await,
        "occl_archway_exit" => occl_archway_exit::run(ctx).await,
        "occl_boulder_slalom" => occl_boulder_slalom::run(ctx).await,
        "occl_corner_bypass" => occl_corner_bypass::run(ctx).await,
        "occl_ridge_window" => occl_ridge_window::run(ctx).await,
        "stereo_covisibility_bloom" => stereo_covisibility_bloom::run(ctx).await,
        "stereo_occlusion_complement" => stereo_occlusion_complement::run(ctx).await,
        "stereo_stair_parallax" => stereo_stair_parallax::run(ctx).await,
        "terrain_bank_height" => terrain_bank_height::run(ctx).await,
        "terrain_canyon_axis" => terrain_canyon_axis::run(ctx).await,
        "terrain_saddle_pass" => terrain_saddle_pass::run(ctx).await,
        "terrain_tree_gap" => terrain_tree_gap::run(ctx).await,
        "circuit_bowtie_cross" => circuit_bowtie_cross::run(ctx).await,
        "circuit_landmark_chain" => circuit_landmark_chain::run(ctx).await,
        "circuit_switchback_loop" => circuit_switchback_loop::run(ctx).await,
        "duet_joint_compass" => duet_joint_compass::run(ctx).await,
        "occl_double_screen_zigzag" => occl_double_screen_zigzag::run(ctx).await,
        "occl_gate_corridor" => occl_gate_corridor::run(ctx).await,
        "occl_twin_slit_transfer" => occl_twin_slit_transfer::run(ctx).await,
        "revisit_multi_anchor_crosscheck" => revisit_multi_anchor_crosscheck::run(ctx).await,
        "revisit_occluded_return" => revisit_occluded_return::run(ctx).await,
        "revisit_temporal_scale" => revisit_temporal_scale::run(ctx).await,
        "stereo_staggered_baseline" => stereo_staggered_baseline::run(ctx).await,
        "terrain_overhang_skylight" => terrain_overhang_skylight::run(ctx).await,
        "duetloop_body_shutter" => duetloop_body_shutter::run(ctx).await,
        "duetloop_cross_triangulation" => duetloop_cross_triangulation::run(ctx).await,
        "handoff_report_clover" => handoff_report_clover::run(ctx).await,
        "handoff_edit_crosscheck" => handoff_edit_crosscheck::run(ctx).await,
        "landmark_tiered_landform" => landmark_tiered_landform::run(ctx).await,
        "duetloop_branch_rejoin" => duetloop_branch_rejoin::run(ctx).await,
        "duetloop_endpoint_transfer" => duetloop_endpoint_transfer::run(ctx).await,
        "handoff_reveal_order" => handoff_reveal_order::run(ctx).await,
        "landmark_depth_order" => landmark_depth_order::run(ctx).await,
        "landmark_marker_pair" => landmark_marker_pair::run(ctx).await,
        "landmark_azimuth_height" => landmark_azimuth_height::run(ctx).await,
        "sightmark_horizon_pair" => sightmark_horizon_pair::run(ctx).await,
        "sightmark_natural_triangle" => sightmark_natural_triangle::run(ctx).await,
        "sightmark_waterbend_arc" => sightmark_waterbend_arc::run(ctx).await,
        "landmark_triple_depth_relay" => landmark_triple_depth_relay::run(ctx).await,
        _ => landmark_tour::run(ctx).await,
    }
}

pub(crate) fn merged(base: &Value, extras: Value) -> Value {
    let mut out = base.as_object().cloned().unwrap_or_default();
    if let Value::Object(extra) = extras {
        for (k, v) in extra {
            out.insert(k, v);
        }
    }
    Value::Object(out)
}
