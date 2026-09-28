use crate::*;
use azalea::{Vec3, interact::BlockStatePredictionHandler};
use azalea_protocol::packets::game::{
    s_interact::InteractionHand,
    s_use_item_on::{BlockHit, ServerboundUseItemOn},
};

fn adjacent_block(reference: BlockPos, face: Direction) -> BlockPos {
    let normal = face.normal();
    BlockPos::new(
        reference.x + normal.x,
        reference.y + normal.y,
        reference.z + normal.z,
    )
}

fn face_hit(reference: BlockPos, face: Direction) -> BlockHit {
    let centre = reference.center();
    let normal = face.normal();
    BlockHit {
        block_pos: reference,
        direction: face,
        location: Vec3 {
            x: centre.x + f64::from(normal.x) * 0.5,
            y: centre.y + f64::from(normal.y) * 0.5,
            z: centre.z + f64::from(normal.z) * 0.5,
        },
        inside: false,
        world_border: false,
    }
}

fn canonical_registry_name(name: &str) -> &str {
    name.strip_prefix("minecraft:").unwrap_or(name)
}

fn block_id_matches(actual: Option<&str>, expected: &str) -> bool {
    actual == Some(canonical_registry_name(expected))
}

fn is_natural_surface_support(id: &str) -> bool {
    matches!(
        canonical_registry_name(id),
        "stone"
            | "granite"
            | "diorite"
            | "andesite"
            | "deepslate"
            | "dirt"
            | "grass_block"
            | "coarse_dirt"
            | "podzol"
            | "rooted_dirt"
            | "mud"
            | "clay"
            | "sand"
            | "red_sand"
            | "gravel"
            | "snow"
            | "snow_block"
            | "ice"
            | "packed_ice"
            | "blue_ice"
            | "sandstone"
            | "red_sandstone"
            | "calcite"
            | "tuff"
            | "dripstone_block"
            | "moss_block"
            | "mycelium"
            | "terracotta"
            | "netherrack"
            | "end_stone"
    ) || canonical_registry_name(id).ends_with("_terracotta")
}

#[cfg(test)]
mod surface_tests {
    use super::{hold_drop_exceeded, is_natural_surface_support};

    #[test]
    fn surface_filter_keeps_terrain_and_rejects_scenario_materials() {
        assert!(is_natural_surface_support("minecraft:grass_block"));
        assert!(is_natural_surface_support("stone"));
        assert!(!is_natural_surface_support("minecraft:stone_bricks"));
        assert!(!is_natural_surface_support("minecraft:oak_planks"));
    }

    #[test]
    fn v6_open_loop_stops_after_a_three_block_drop() {
        assert!(!hold_drop_exceeded(true, Some(70.0), Some(67.0)));
        assert!(hold_drop_exceeded(true, Some(70.0), Some(66.9)));
        assert!(!hold_drop_exceeded(false, Some(70.0), Some(60.0)));
    }
}

// ---------------------------------------------------------------------------------------------
// Scenario context: the bot's validated primitives exposed to the scenario scripts
// (grounded pathfinding, settled looks, sweeps, dense recording, peer geometry).
// ---------------------------------------------------------------------------------------------

pub(crate) const TYPE_LOOK: &str = "look";
pub(crate) const TYPE_MOVE: &str = "move";
pub(crate) const TYPE_PLACE: &str = "place_block";
pub(crate) const TYPE_ATTACK: &str = "attack";
pub(crate) const TYPE_MINE: &str = "mine";

/// Valuable ore block registry ids the collector seeks — solaris collector-episode.js VALUABLE_ORES
/// (stone + deepslate variants of every overworld ore). Matched against `Ctx::block_id_at`.
const VALUABLE_ORES: [&str; 16] = [
    "diamond_ore",
    "deepslate_diamond_ore",
    "emerald_ore",
    "deepslate_emerald_ore",
    "gold_ore",
    "deepslate_gold_ore",
    "iron_ore",
    "deepslate_iron_ore",
    "lapis_ore",
    "deepslate_lapis_ore",
    "redstone_ore",
    "deepslate_redstone_ore",
    "coal_ore",
    "deepslate_coal_ore",
    "copper_ore",
    "deepslate_copper_ore",
];

fn canonical_type(action_type: &str) -> String {
    match action_type {
        "look" => "control.look".to_string(),
        "move" => "control.move".to_string(),
        "place_block" => "world.place_block".to_string(),
        "attack" => "control.attack".to_string(),
        "mine" => "world.mine".to_string(),
        other => format!("action.{other}"),
    }
}

/// How close the view must be to the placement target before the interaction packet is sent.
/// 5 deg is well inside a block's angular size at interaction reach (a 1-block face at 4 blocks
/// subtends ~14 deg), so the target is unambiguously on screen, and it is wide enough that the
/// quantized turn (see `quantize_step`) lands inside it rather than dithering across it.
const PLACE_FACING_TOLERANCE_DEG: f64 = 5.0;

/// How long [`Ctx::hold_visual`] lets a movement key run before it checks that the body actually
/// went somewhere, and how far it must have gone. A walking bot covers ~5 blocks in this window and
/// even a jump-climb clears more than a third of one, so only genuine obstacle contact trips it.
const HOLD_STALL_WINDOW: Duration = Duration::from_millis(1_200);
const HOLD_STALL_TRAVEL: f64 = 0.35;
const HOLD_MAX_V6_DROP: f64 = 3.0;

fn hold_drop_exceeded(generation_six: bool, start_y: Option<f64>, now_y: Option<f64>) -> bool {
    generation_six
        && start_y
            .zip(now_y)
            .is_some_and(|(start, now)| now < start - HOLD_MAX_V6_DROP)
}

/// What [`Ctx::move_to_watching`] keeps the camera on while the body walks.
#[derive(Clone, Copy)]
enum WatchTarget {
    Peer(usize),
    Point((f64, f64, f64)),
}

pub(crate) struct Ctx {
    pub(crate) bot: Client,
    pub(crate) inner: Arc<BotInner>,
    pub(crate) shared: Shared,
    pub(crate) rng: Mulberry32,
    // World anchor (the swarm's spawn) the origin-relative scenario plan is shifted onto. See
    // read_anchor(): vanilla spawns bots far from (0,0), so plan goals must follow the spawn.
    pub(crate) anchor: (f64, f64),
    // Witnessing state (see scenarios::common::witness_peer_edits): how far into the swarm's
    // shared `recent_edits` log this bot has read, and when it last turned to witness one.
    pub(crate) witness_cursor: usize,
    pub(crate) last_witness_ms: i64,
}

impl Ctx {
    pub(crate) fn cfg(&self) -> &Cfg {
        &self.shared.inner.cfg
    }

    /// The world anchor; origin-relative plan coordinates are shifted by this to reach the swarm.
    pub(crate) fn anchor(&self) -> (f64, f64) {
        self.anchor
    }

    pub(crate) fn engine(&self) -> &ScenarioEngine {
        &self.shared.inner.engine
    }

    pub(crate) fn index(&self) -> usize {
        self.inner.index
    }

    /// Players hosted by THIS process (the local shard); the global swarm size across all shards
    /// is `cfg().total_bot_count`.
    pub(crate) fn player_count(&self) -> usize {
        self.cfg().bot_count
    }

    pub(crate) fn on_ground(&self) -> bool {
        self.bot
            .component::<azalea_entity::Physics>()
            .is_ok_and(|physics| physics.on_ground())
    }

    pub(crate) fn rng_next(&mut self) -> f64 {
        self.rng.next()
    }

    pub(crate) fn elapsed_ms(&self) -> i64 {
        self.inner.elapsed_ms()
    }

    pub(crate) fn target_met(&self) -> bool {
        let duration = self.cfg().episode_duration_ms;
        duration <= 0 || self.elapsed_ms() >= duration
    }

    pub(crate) fn position(&self) -> Option<(f64, f64, f64)> {
        let p = self.bot.position().ok()?;
        Some((p.x, p.y, p.z))
    }

    pub(crate) fn peer_position(&self, j: usize) -> Option<(f64, f64, f64)> {
        self.shared
            .inner
            .states
            .get(j)
            .and_then(|s| *s.live_pos.lock())
    }

    pub(crate) fn peer_username(&self, j: usize) -> Option<String> {
        self.shared.inner.states.get(j).map(|s| s.username.clone())
    }

    pub(crate) fn group_center(&self) -> Option<(f64, f64, f64)> {
        let ps: Vec<(f64, f64, f64)> = self
            .shared
            .inner
            .states
            .iter()
            .filter_map(|s| *s.live_pos.lock())
            .collect();
        if ps.is_empty() {
            return None;
        }
        let n = ps.len() as f64;
        let sum = ps
            .iter()
            .fold((0.0, 0.0, 0.0), |a, p| (a.0 + p.0, a.1 + p.1, a.2 + p.2));
        Some((sum.0 / n, sum.1 / n, sum.2 / n))
    }

    /// Attack a peer bot by its username. Resolves the peer's ECS entity via its
    /// (offline) UUID and swings at it with the real azalea `Client::attack` if the entity
    /// is tracked/in range; returns whether a real swing was issued. The caller always sets
    /// the `attack` pending flag + records the action event regardless, so the action stream
    /// reflects the intent even when the entity is not currently tracked (out of view range /
    /// not yet replicated). A recorded `attack` is therefore an input, not a confirmed hit.
    pub(crate) fn attack_peer(&self, peer_username: &str) -> bool {
        if let Ok(Some(uuid)) = self.bot.player_uuid_by_username(peer_username)
            && let Some(entity) = self.bot.entity_by_uuid(uuid)
        {
            entity.attack();
            return true;
        }
        false
    }

    /// Swing at an arbitrary ECS entity handle (a peer player OR a hostile mob) with the real azalea
    /// `Client::attack` — the same melee swing `attack_peer` issues, generalized for the combat
    /// episodes. Caller sets the `attack` pending flag + records the event; this only sends the swing.
    pub(crate) fn attack_entity(&self, entity: Entity) {
        self.bot.attack(entity);
    }

    /// Live world position of an arbitrary entity (peer or mob), or `None` if it has despawned /
    /// lost its `Position` (azalea returns the component only while the entity is tracked). Used as
    /// both the aim target AND the despawn/"is it still alive" probe in the combat loops.
    pub(crate) fn entity_position(&self, entity: Entity) -> Option<(f64, f64, f64)> {
        self.bot
            .get_entity_component::<Position>(entity)
            .map(|p| (p.x, p.y, p.z))
    }

    /// Horizontal distance from this bot to `entity`, or `+inf` if the entity is gone (so callers
    /// treat a despawned target as "out of range").
    pub(crate) fn distance_to_entity(&self, entity: Entity) -> f64 {
        match (self.position(), self.entity_position(entity)) {
            (Some(s), Some(e)) => ((e.0 - s.0).powi(2) + (e.2 - s.2).powi(2)).sqrt(),
            _ => f64::INFINITY,
        }
    }

    /// Find the nearest tracked HOSTILE mob within `max_distance` (2D) of this bot via azalea's ECS
    /// entity query (`Client::nearest_entity_by`): it sorts every entity in our instance by distance
    /// and we keep the closest whose [`EntityKind`] is in `HOSTILE_KINDS`. Returns the lightweight
    /// [`Entity`] handle (read its live pose with [`Ctx::entity_position`]). Players are naturally
    /// excluded (their kind is `EntityKind::Player`, never hostile). `None` when no hostile is in
    /// range — the pve loop then idles/looks and still terminates. Ported from solaris
    /// fighting.js:getNearestHostile (its `nearestEntity(isHostileMobFilter(...))`).
    pub(crate) fn nearest_hostile(&self, max_distance: f64) -> Option<Entity> {
        let own = self.position()?;
        let max_sq = max_distance.max(0.0).powi(2);
        // nearest_entity_by already returns the closest match; we re-check the 2D range here because
        // the query sorts by full 3D distance over the whole instance (no built-in radius cutoff).
        let entity = self
            .bot
            .nearest_entity_id_by::<&EntityKindComponent, ()>(|kind: &EntityKindComponent| {
                is_hostile_kind(kind.0)
            })
            .ok()
            .flatten()?;
        let p = self.entity_position(entity)?;
        let d_sq = (p.0 - own.0).powi(2) + (p.2 - own.2).powi(2);
        if d_sq <= max_sq { Some(entity) } else { None }
    }

    pub(crate) fn set_pending(&self, key: &str) {
        let mut pending = self.inner.pending.lock();
        match key {
            "attack" => pending.attack = true,
            "use" => pending.use_item = true,
            "place_block" => pending.place_block = true,
            "mine" => pending.mine = true,
            _ => {}
        }
    }

    // -----------------------------------------------------------------------------------------
    // Real in-game world-action primitives (azalea 0.16 async Client API). These replace the
    // /setblock-chat fakes: the bot uses its OWN inventory + interactions, so the placement is a
    // genuine survival action the world model can learn. Materials are assumed already in the
    // inventory (the coordinator `give`s each bot a kit at episode setup — see attack_peer for the
    // existing "real world-action" style this mirrors). Ported from solaris-engine's
    // controller/primitives/{building,digging,items}.js.
    // -----------------------------------------------------------------------------------------

    /// Read the bot's live inventory component (the same source control.rs's per-frame capture
    /// reads via `Inventory::held_item`/`selected_hotbar_slot`).
    pub(crate) fn inventory(&self) -> Option<Inventory> {
        self.bot.component::<Inventory>().ok().map(|i| i.clone())
    }

    /// Select the requested hotbar item and wait until Azalea has applied the ECS event and sent
    /// the carried-item change on a game tick. Returns true only when the live inventory agrees.
    pub(crate) async fn equip(&self, item_name: &str) -> bool {
        let want = canonical_registry_name(item_name).to_ascii_lowercase();
        let Some(inv) = self.inventory() else {
            return false;
        };
        let menu = inv.menu();
        let slots = menu.slots();
        let hotbar = menu.hotbar_slots_range();
        let slot = hotbar.enumerate().find_map(|(hotbar_index, slot_index)| {
            let stack = slots.get(slot_index)?;
            (!stack.is_empty() && crate::item_name(stack).as_deref() == Some(want.as_str()))
                .then_some(hotbar_index as u8)
        });
        let Some(slot) = slot else {
            return false;
        };
        self.bot.set_selected_hotbar_slot(slot);
        self.bot.wait_updates(1).await;
        let selected = self
            .inventory()
            .map(|inventory| {
                inventory.selected_hotbar_slot == slot
                    && crate::item_name(inventory.held_item()).as_deref() == Some(want.as_str())
            })
            .unwrap_or(false);
        if !selected {
            return false;
        }
        self.bot.wait_ticks(1).await;
        true
    }

    /// Port of solaris building.js:findBestPlaceReference. Score every solid neighbour of `target`
    /// (the 6 cardinals) and return the best `(reference_block_pos, face, face_center)` to click,
    /// where `face` is the [`Direction`] normal pointing from the neighbour toward `target` so a
    /// placement against it lands the new block at `target`. Faces are SCORED (not just first-hit)
    /// like solaris `scoreFace`: how squarely we already face the normal, a top-face bonus, a
    /// bottom-face penalty, and a closer-is-better bonus — so a reachable, well-oriented side face
    /// can outrank a far/awkward top face. Air / unloaded neighbours and out-of-reach references
    /// are rejected. `face_center` is the exact world point to aim at (so the live
    /// `HitResultComponent` raycast resolves to this reference with this real face — see
    /// `place_block`). Returns the highest-scoring candidate, or `None` if no face is buildable.
    pub(crate) fn find_place_reference(
        &self,
        target: BlockPos,
    ) -> Option<(BlockPos, Direction, (f64, f64, f64))> {
        // (offset from target to the neighbour we click, face normal from neighbour -> target).
        // Ordered like solaris' CARDINALS (top first, bottom last) — only the tie-break order now;
        // the actual choice is by score.
        const CARDINALS: [((i32, i32, i32), Direction); 6] = [
            ((0, -1, 0), Direction::Up),    // neighbour below; place on its TOP face
            ((-1, 0, 0), Direction::East),  // neighbour west;  face points +X
            ((1, 0, 0), Direction::West),   // neighbour east;  face points -X
            ((0, 0, -1), Direction::South), // neighbour north; face points +Z
            ((0, 0, 1), Direction::North),  // neighbour south; face points -Z
            ((0, 1, 0), Direction::Down),   // neighbour above; place on its BOTTOM face
        ];
        let reach = self.cfg().place_reach.max(3.0);
        let eye = self.bot.eye_position().ok()?;
        let eye = (eye.x, eye.y, eye.z);
        // Current view direction (unit), from our authoritative quantized camera angles — used to
        // bias toward faces we already face (solaris scoreFace dot-product term).
        let yaw = (*self.inner.cam_yaw.lock()).to_radians();
        let pitch = (*self.inner.cam_pitch.lock()).to_radians();
        let view = (
            -yaw.sin() * pitch.cos(),
            -pitch.sin(),
            -yaw.cos() * pitch.cos(),
        );
        let world = self.bot.world().ok()?;
        let world = world.read();
        #[allow(clippy::type_complexity)] // (score, pos, face, cursor) scratch, local to this loop
        let mut best: Option<(f64, BlockPos, Direction, (f64, f64, f64))> = None;
        for ((dx, dy, dz), face) in CARDINALS {
            let reference = BlockPos::new(target.x + dx, target.y + dy, target.z + dz);
            // Only click full solid blocks. Unloaded and air neighbours are never references, and
            // rejecting partial collision shapes avoids sending a geometrically valid face that is
            // not actually present on the reference block.
            let Some(state) = world.get_block_state(reference) else {
                continue;
            };
            if state.is_air() || !state.is_collision_shape_full() {
                continue;
            }
            // The face normal points from the reference toward `target` (= -offset). Keep target,
            // face, and packet hit geometry tied to the same exact relationship.
            debug_assert_eq!(adjacent_block(reference, face), target);
            let hit = face_hit(reference, face);
            let face_center = (hit.location.x, hit.location.y, hit.location.z);
            let normal = face.normal();
            let (nx, ny, nz) = (
                f64::from(normal.x),
                f64::from(normal.y),
                f64::from(normal.z),
            );
            // Reject faces we can't physically reach (vanilla block reach ~4.5; we gate a bit
            // tighter via cfg).
            let dist = ((face_center.0 - eye.0).powi(2)
                + (face_center.1 - eye.1).powi(2)
                + (face_center.2 - eye.2).powi(2))
            .sqrt();
            if dist > reach {
                continue;
            }
            // --- solaris scoreFace ---
            let mut score = 50.0_f64;
            let dot = view.0 * nx + view.1 * ny + view.2 * nz;
            score += if dot > 0.0 { dot * 30.0 } else { dot * 20.0 };
            if ny == 0.0 {
                score += 10.0; // horizontal faces: easy to see/reach
            }
            if ny > 0.0 {
                score += 15.0; // top face: most natural placement
            }
            if ny < 0.0 {
                score -= 10.0; // bottom face: hardest
            }
            score += (1.0 - (dist / reach).min(1.0)) * 10.0; // closer is better
            if best.as_ref().map(|b| score > b.0).unwrap_or(true) {
                best = Some((score, reference, face, face_center));
            }
        }
        best.map(|(_, r, f, c)| (r, f, c))
    }

    /// Place the expected block at an air target using the currently-held item. The target must be
    /// loaded air before interaction, and success requires the canonical world block id to equal
    /// `expected_block`; unrelated terrain or a wrong held item can never become a false positive.
    pub(crate) async fn place_block(&self, target: BlockPos, expected_block: &str) -> bool {
        let expected = canonical_registry_name(expected_block);
        if !self.target_is_loaded_air(target) {
            return false;
        }

        for _ in 0..3 {
            let Some((reference, face, face_center)) = self.find_place_reference(target) else {
                return false;
            };
            // Refuse to place while facing elsewhere. The server would accept it — `use_item_on` is
            // judged on the packet's hit position — so nothing downstream could tell the footage
            // apart from a correct placement, and the action stream would label a frame `place` in
            // which the target is off screen. A skipped cell is recoverable; mislabelled training
            // footage is not.
            if !self
                .look_until_facing(
                    face_center,
                    PLACE_FACING_TOLERANCE_DEG,
                    self.place_facing_max_wait_ms(),
                )
                .await
            {
                return false;
            }
            self.send_use_item_on(face_hit(reference, face));
            self.set_pending("place_block");
            if self.wait_for_block_id(target, expected).await {
                self.broadcast_edit(target);
                return true;
            }
            if !self.target_is_loaded_air(target) {
                return false;
            }
        }
        false
    }

    /// True when the exact top-face hit used by `place_ground_torch` is within interaction reach.
    /// Generic placement reach is insufficient here: a nearby wall face can be reachable while
    /// the fixed floor-top packet hit is still too far away.
    pub(crate) fn ground_torch_face_in_reach(&self, target: BlockPos) -> bool {
        let hit = face_hit(target.down(1), Direction::Up);
        let Ok(eye) = self.bot.eye_position() else {
            return false;
        };
        let reach = self.cfg().place_reach.max(3.0);
        ((hit.location.x - eye.x).powi(2)
            + (hit.location.y - eye.y).powi(2)
            + (hit.location.z - eye.z).powi(2))
        .sqrt()
            <= reach
    }

    /// Place the currently-held torch on the top face of the full solid block below `target`.
    /// Uses the same explicit real use-item-on packet as `place_block`, with a fixed `Up` face,
    /// and reports success only when the loaded target world state is actually a torch.
    pub(crate) async fn place_ground_torch(&self, target: BlockPos) -> bool {
        match self.block_id_at(target).as_deref() {
            // Do not report a pre-existing torch as this bot's successful placement. The scenario
            // can try another trailing cell, and `placed:true` then always follows our interaction.
            Some("torch") => return false,
            Some("air" | "cave_air") => {}
            _ => return false,
        }
        let reference = target.down(1);
        {
            let Ok(world) = self.bot.world() else {
                return false;
            };
            let world = world.read();
            let Some(state) = world.get_block_state(reference) else {
                return false;
            };
            if state.is_air() || !state.is_collision_shape_full() {
                return false;
            }
        }
        if !self.ground_torch_face_in_reach(target) {
            return false;
        }
        let hit = face_hit(reference, Direction::Up);
        debug_assert_eq!(adjacent_block(reference, Direction::Up), target);
        if !self
            .look_until_facing(
                (hit.location.x, hit.location.y, hit.location.z),
                PLACE_FACING_TOLERANCE_DEG,
                self.place_facing_max_wait_ms(),
            )
            .await
        {
            return false;
        }
        self.send_use_item_on(hit);
        self.set_pending("place_block");
        let ok = self.wait_for_block_id(target, "torch").await;
        if ok {
            self.broadcast_edit(target);
        }
        ok
    }

    fn send_use_item_on(&self, block_hit: BlockHit) {
        let seq = self
            .bot
            .query_self::<&mut BlockStatePredictionHandler, _>(|mut h| h.start_predicting())
            .unwrap_or_default();
        self.bot.write_packet(ServerboundUseItemOn {
            hand: InteractionHand::MainHand,
            block_hit,
            seq,
        });
    }

    fn target_is_loaded_air(&self, target: BlockPos) -> bool {
        let Ok(world) = self.bot.world() else {
            return false;
        };
        let world = world.read();
        matches!(world.get_block_state(target), Some(state) if state.is_air())
    }

    async fn wait_for_block_id(&self, target: BlockPos, expected: &str) -> bool {
        let deadline =
            Instant::now() + Duration::from_millis(self.cfg().operation_pause_ms.max(350));
        loop {
            if self.block_id_at(target).as_deref() == Some(expected) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            sleep_ms(50).await;
        }
    }

    /// True when at least one real solid neighbour face can be reached and clicked for `target`.
    /// Keep navigation's readiness predicate identical to `place_block`; centre-distance margins
    /// otherwise allow an immediate placement attempt that has no reachable packet reference.
    pub(crate) fn target_in_reach(&self, target: BlockPos) -> bool {
        self.find_place_reference(target).is_some()
    }

    /// Build a list of `(world_block, material)` placements with REAL survival placement — the
    /// shared workhorse for the build-family scenarios (port of solaris building.js placeMultiple +
    /// buildPhase). For each block, in a buildable order (bottom-up, near-to-far — solaris
    /// sortByBuildability's intent so the bot never depends on an unplaced neighbour): equip the
    /// material, step closer if the block is out of reach (solaris moveToPlacementPosition — our
    /// "scaffold": pathfinder walks/jumps adjacent, since the kit has no dedicated scaffold blocks),
    /// look at the best face, and `place_block`. Records one real `place_block` action per success
    /// (with `record_extra`'s scenario-specific fields). Honors `target_met` and episode-stop.
    /// Returns `(placed, attempted)`.
    pub(crate) async fn build_blocks<F>(
        &mut self,
        blocks: &[((i64, i64, i64), String)],
        mut record_extra: F,
    ) -> (u64, u64)
    where
        F: FnMut(usize, BlockPos, &str) -> (String, String, Value),
    {
        // Buildable order: ascending Y first (so a layer's supports exist), then nearest-first
        // within a layer. Mirrors solaris sortByBuildability's bottom-up, near-to-far ordering.
        let origin = self.position().unwrap_or((0.0, 64.0, 0.0));
        let mut order: Vec<usize> = (0..blocks.len()).collect();
        order.sort_by(|&a, &b| {
            let ((ax, ay, az), _) = &blocks[a];
            let ((bx, by, bz), _) = &blocks[b];
            ay.cmp(by).then_with(|| {
                let da = (*ax as f64 - origin.0).powi(2) + (*az as f64 - origin.2).powi(2);
                let db = (*bx as f64 - origin.0).powi(2) + (*bz as f64 - origin.2).powi(2);
                da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
            })
        });
        let hold = self.cfg().action_hold_ms.max(400);
        let mut placed: u64 = 0;
        let mut attempted: u64 = 0;
        let mut dead_targets = 0u32;
        for &i in &order {
            if self.target_met() {
                break;
            }
            let ((bx, by, bz), material) = &blocks[i];
            let target = BlockPos::new(*bx as i32, *by as i32, *bz as i32);
            // Navigate first: Azalea pathfinding may select a mining tool while moving.
            // Step closer if out of reach — our scaffold-up analogue (solaris walks/jumps to an
            // optimal placement position; here the pathfinder picks the route, with parkour for
            // small hops when enabled). Aim for ~2 blocks out from the target footprint.
            if !self.target_in_reach(target) {
                let cfg = self.cfg();
                self.move_to(
                    *bx as f64,
                    *bz as f64,
                    cfg.move_timeout_ms.min(8000),
                    (cfg.place_reach - 1.0).max(1.5),
                )
                .await;
            }
            // Synchronize the material immediately before the real interaction.
            if !self.equip(material).await {
                continue;
            }
            attempted += 1;
            let ok = self.place_block(target, material).await;
            if ok {
                placed += 1;
            }
            // A failure on a cell that is not even air means the pass is mis-sited (buried in
            // terrain, or overlapping an earlier structure): place_block bailed before sending a
            // packet, so retrying the remaining cells changes nothing — it only grinds out a
            // statue at ~4 s per cell. Three in a row abandons the pass.
            if ok || self.target_is_loaded_air(target) {
                dead_targets = 0;
            } else {
                dead_targets += 1;
            }
            let (vis_id, desc, extras) = record_extra(i, target, material);
            self.record(
                TYPE_PLACE,
                &vis_id,
                &desc,
                merged(
                    &extras,
                    json!({"placed": ok, "block": material,
                        "block_position": {"x": target.x, "y": target.y, "z": target.z}}),
                ),
            );
            if dead_targets >= 3 {
                break;
            }
            sleep_ms(hold).await;
        }
        (placed, attempted)
    }

    /// Dig (mine) the block at `target` — port of solaris digging.js:digBlock + digWithTimeout.
    /// Looks at the block centre via the quantized controller, then awaits azalea `Client::mine`
    /// (which blocks until the `Mining` component clears, i.e. the break finished) under a hard
    /// `dig_timeout_ms` cap so an unreachable/stuck break can never hang the episode (solaris wraps
    /// `bot.dig` in `digWithTimeout` for exactly this). The `mine` pending pulse covers the
    /// initiating tick; the capture system reads azalea's `Mining` component for the sustained
    /// in-progress ticks. Returns whether the target is air afterwards (success), when verifiable.
    pub(crate) async fn dig(&self, target: BlockPos) -> bool {
        // Skip air up front (digBlock's early-out), so we never record a no-op mine.
        {
            let Ok(world) = self.bot.world() else {
                return false;
            };
            let world = world.read();
            if world
                .get_block_state(target)
                .map(|s| s.is_air())
                .unwrap_or(false)
            {
                return true;
            }
        }
        let centre = target.center();
        self.look_default((centre.x, centre.y, centre.z)).await;
        self.set_pending("mine");
        // Cap the wait: `Client::mine` resolves when the `Mining` component clears (break done /
        // instamined / target swapped). A genuinely unreachable block would otherwise leave us
        // awaiting the tick broadcaster forever; timing out drops that future (the component clears
        // on its own next tick) so the scenario keeps progressing — solaris digWithTimeout parity.
        let cap = Duration::from_millis(self.cfg().dig_timeout_ms.max(1000));
        let _ = tokio::time::timeout(cap, self.bot.mine(target)).await;
        let Ok(world) = self.bot.world() else {
            return true;
        };
        let verified_air = {
            let world = world.read();
            world.get_block_state(target).map(|s| s.is_air())
        };
        // Broadcast only a VERIFIED break; the unverifiable fallback below reports success to the
        // caller but is not evidence another bot should turn around for.
        if verified_air == Some(true) {
            self.broadcast_edit(target);
        }
        verified_air.unwrap_or(true)
    }

    /// Registry string id of the block at `pos` (e.g. "stone", "diamond_ore", "deepslate_iron_ore"),
    /// or `None` if the chunk/block is not loaded. Uses the same lossless
    /// `BlockState -> Box<dyn BlockTrait>` conversion azalea's own Debug path uses, then `id()` —
    /// the canonical Minecraft string id, so ore-name matching is exact. The block-side analogue of
    /// `item_name()` for held items.
    pub(crate) fn block_id_at(&self, pos: BlockPos) -> Option<String> {
        let world = self.bot.world().ok()?;
        let world = world.read();
        let state = world.get_block_state(pos)?;
        Some(state.to_trait().id().to_string())
    }

    /// Port of solaris digging.js:findVisibleOres. Scan the loaded world in a cube of radius
    /// `radius` (Chebyshev) around the bot's feet for blocks whose registry id is a known ore
    /// (`VALUABLE_ORES`), returning their positions sorted nearest-first and capped to `max`. azalea
    /// has no `bot.findBlocks`, so we iterate the cube and query `block_id_at` per cell (the loaded
    /// chunk store makes this cheap for the small radii solaris uses, ~16). The `canSeeBlock`
    /// line-of-sight filter is dropped: we pathfind adjacent and dig regardless, and a raycast per
    /// candidate would be both costly and brittle — distance-gating is the faithful, robust subset.
    pub(crate) fn find_visible_ores(&self, radius: i32, max: usize) -> Vec<BlockPos> {
        let Some(p) = self.position() else {
            return Vec::new();
        };
        let (cx, cy, cz) = (p.0.floor() as i32, p.1.floor() as i32, p.2.floor() as i32);
        let radius = radius.clamp(1, 24);
        // Hold the world read-lock once for the whole cube scan (cheaper than re-locking per cell).
        let Ok(world) = self.bot.world() else {
            return Vec::new();
        };
        let world = world.read();
        let mut found: Vec<(BlockPos, i64)> = Vec::new();
        for dx in -radius..=radius {
            for dy in -radius..=radius {
                for dz in -radius..=radius {
                    let pos = BlockPos::new(cx + dx, cy + dy, cz + dz);
                    let Some(state) = world.get_block_state(pos) else {
                        continue;
                    };
                    if state.is_air() {
                        continue;
                    }
                    let id = state.to_trait().id();
                    if VALUABLE_ORES.contains(&id) {
                        // Squared distance (integer) — order only, no sqrt needed.
                        let d2 = (dx as i64).pow(2) + (dy as i64).pow(2) + (dz as i64).pow(2);
                        found.push((pos, d2));
                    }
                }
            }
        }
        found.sort_by_key(|&(_, d2)| d2);
        found.truncate(max);
        found.into_iter().map(|(pos, _)| pos).collect()
    }

    /// Hold / release the scenario-commanded jump (OR-ed into the controller's wire jump). See
    /// `BotInner::jump_request`.
    pub(crate) fn set_jump(&self, on: bool) {
        self.inner
            .jump_request
            .store(on, std::sync::atomic::Ordering::SeqCst);
    }

    /// Port of solaris building.js:buildTowerUnderneath — the classic pillar-jump. The caller
    /// synchronizes the held material first; every layer is counted only when the exact expected
    /// block id appears in the world.
    pub(crate) async fn build_tower_underneath(&self, height: i64, expected_block: &str) -> i64 {
        let expected_block = canonical_registry_name(expected_block);
        let expected = height.max(0);
        let Some(start) = self.position() else {
            return 0;
        };
        let column_x = start.0.floor() as i32;
        let column_z = start.2.floor() as i32;
        let start_y = start.1.floor() as i32;

        // Keep a single, stable column for the whole operation. Recomputing `feet` while a jump is
        // in flight can advance the target early and repeatedly click the wrong reference.
        let yaw = *self.inner.cam_yaw.lock();
        *self.inner.camera_intent.lock() = CameraIntent::LookAt { yaw, pitch: 89.0 };
        self.inner
            .camera_busy
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.wait_camera(self.cfg().look_settle_ms.max(1200)).await;

        let mut placed_layers = 0_i64;
        for layer in 0..expected {
            if self.target_met() {
                break;
            }
            let target = BlockPos::new(column_x, start_y + layer as i32, column_z);
            let reference = target.down(1);

            // A delayed packet from an earlier attempt may already have placed the expected block.
            // Any other occupied block is not progress and terminates this column honestly.
            let existing = self.block_id_at(target);
            let mut placed = block_id_matches(existing.as_deref(), expected_block);
            if !placed && !matches!(existing.as_deref(), Some("air" | "cave_air")) {
                break;
            }

            // Each layer gets a bounded number of complete jump attempts. Between attempts release
            // jump and wait to stand back on the current top, rather than interacting while grounded
            // or carrying stale jump state into the next layer.
            for attempt in 0..3 {
                if placed || self.target_met() {
                    break;
                }
                self.set_jump(false);
                sleep_ms(self.cfg().action_hold_ms.max(300)).await;

                // Do not jump unless the intended reference really exists. This also prevents a
                // failed lower layer from being reported as progress at a higher coordinate.
                let reference_ready = {
                    let Ok(world) = self.bot.world() else {
                        break;
                    };
                    let world = world.read();
                    world
                        .get_block_state(reference)
                        .map(|state| !state.is_air())
                        .unwrap_or(false)
                };
                if !reference_ready {
                    break;
                }

                let settled_y = self
                    .position()
                    .map(|p| p.1)
                    .unwrap_or(start.1 + layer as f64);
                self.set_jump(true);

                // Wait for visible lift-off before interacting. Position deltas are more reliable
                // here than a guessed fixed sleep because controller and server ticks can drift.
                let lift_deadline = Instant::now() + Duration::from_millis(900);
                let mut airborne = false;
                while Instant::now() < lift_deadline {
                    sleep_ms(25).await;
                    if self
                        .position()
                        .map(|p| p.1 > settled_y + 0.12)
                        .unwrap_or(false)
                    {
                        airborne = true;
                        break;
                    }
                }
                if !airborne {
                    self.set_jump(false);
                    continue;
                }

                // Keep the same target/reference throughout this jump. A few tick-spaced clicks
                // cover packet timing, but stop immediately after the real block appears.
                for _ in 0..8 {
                    self.bot.block_interact(reference);
                    self.set_pending("place_block");
                    sleep_ms(45).await;
                    placed = block_id_matches(self.block_id_at(target).as_deref(), expected_block);
                    if placed {
                        break;
                    }
                }
                self.set_jump(false);
                sleep_ms(self.cfg().action_hold_ms.max(350) + attempt * 100).await;
            }

            self.set_jump(false);
            if !placed {
                break;
            }
            placed_layers += 1;
            self.broadcast_edit(target);
            self.record(
                TYPE_PLACE,
                &format!("vis-pillar-{}-{placed_layers}", self.index()),
                &format!("Pillar-jump placed real layer {placed_layers}/{expected} underfoot."),
                json!({
                    "expected": expected,
                    "placed": placed_layers,
                    "height": placed_layers,
                    "layer": placed_layers,
                    "block_position": {"x": target.x, "y": target.y, "z": target.z},
                    "method": "pillar_jump",
                }),
            );
        }
        self.set_jump(false);
        placed_layers
    }

    /// Publish a verified world edit to the swarm's shared log, so other bots can witness it.
    /// Called by the world-action primitives themselves — a scenario never has to remember to.
    fn broadcast_edit(&self, pos: BlockPos) {
        self.shared
            .inner
            .engine
            .shared
            .recent_edits
            .lock()
            .push(RecentEdit {
                by: self.index(),
                x: pos.x as i64,
                y: pos.y as i64,
                z: pos.z as i64,
                at: Instant::now(),
            });
    }

    /// The newest unwitnessed PEER edit at least `min_age` old and within `max_distance` blocks,
    /// advancing this bot's cursor past everything that old (so a burst of placements is
    /// witnessed once, at its latest cell, not once per block). `None` when there is nothing
    /// ripe — edits younger than `min_age` stay unread for a later call.
    pub(crate) fn take_witnessable_edit(
        &mut self,
        min_age: Duration,
        max_distance: f64,
    ) -> Option<RecentEdit> {
        let own = self.position()?;
        let edits = self.shared.inner.engine.shared.recent_edits.lock();
        // The log is append-ordered by time, so the ripe edits are a prefix of the unread tail.
        let ripe_end = edits[self.witness_cursor..]
            .iter()
            .take_while(|edit| edit.at.elapsed() >= min_age)
            .count()
            + self.witness_cursor;
        let found = edits[self.witness_cursor..ripe_end]
            .iter()
            .rev()
            .find(|edit| {
                edit.by != self.index()
                    && (edit.x as f64 - own.0).hypot(edit.z as f64 - own.2) <= max_distance
            })
            .copied();
        self.witness_cursor = ripe_end;
        found
    }

    /// Record a sparse semantic action event (canonical Mineflayer-parity shape); extras
    /// are merged into the payload next to `description`.
    pub(crate) fn record(
        &self,
        action_type: &str,
        visible_event_id: &str,
        description: &str,
        extras: Value,
    ) {
        record_event(
            &self.inner,
            self.cfg(),
            action_type,
            visible_event_id,
            description,
            extras,
        );
    }

    pub(crate) async fn pause(&self) {
        sleep_ms(self.cfg().operation_pause_ms).await;
    }

    pub(crate) fn requires_exterior_surface(&self) -> bool {
        let name = self.engine().name.as_str();
        pawa_schema::catalog::scenario(name).is_some_and(|info| info.generation == 6)
            && !matches!(
                name,
                "occl_canopy_shelter" | "terrain_hollow_mouth_cycle" | "terrain_overhang_skylight"
            )
    }

    fn current_position_has_sky(&self) -> bool {
        const WORLD_TOP: i32 = 320;
        let (Ok(position), Ok(world)) = (self.bot.position(), self.bot.world()) else {
            return false;
        };
        let world = world.read();
        let x = position.x.floor() as i32;
        let z = position.z.floor() as i32;
        let head = position.y.floor() as i32 + 2;
        (head..WORLD_TOP).all(|y| {
            world
                .get_block_state(BlockPos::new(x, y, z))
                .is_some_and(|state| state.is_air())
        })
    }

    /// Resolve a generation-6 XZ goal to an open natural surface and retain its feet Y. XZ-only
    /// goals can land inside a cave even when the same column has an exposed roof; an exact block
    /// goal is required so the planner cannot terminate at the lower cave floor.
    fn generation_six_surface_goal(&self, desired: (f64, f64)) -> Option<(f64, f64, Option<i32>)> {
        if !self.requires_exterior_surface() {
            return Some((desired.0, desired.1, None));
        }
        const OFFSETS: [(i32, i32); 13] = [
            (0, 0),
            (1, 0),
            (-1, 0),
            (0, 1),
            (0, -1),
            (1, 1),
            (1, -1),
            (-1, 1),
            (-1, -1),
            (2, 0),
            (-2, 0),
            (0, 2),
            (0, -2),
        ];
        const WORLD_MIN: i32 = -64;
        const WORLD_TOP: i32 = 320;
        let world = match self.bot.world() {
            Ok(world) => world,
            Err(_) => return None,
        };
        let world = world.read();
        let bx = desired.0.floor() as i32;
        let bz = desired.1.floor() as i32;
        // Scan the practical world range, not just the current cave height. The first terrain support
        // with an all-air column to the world top is exterior sky, not a cave floor or roof top.
        for (dx, dz) in OFFSETS {
            for y in (WORLD_MIN..WORLD_TOP).rev() {
                let support = BlockPos::new(bx + dx, y, bz + dz);
                let Some(state) = world.get_block_state(support) else {
                    continue;
                };
                let id = state.to_trait().id();
                if state.is_air()
                    || !state.is_collision_shape_full()
                    || !is_natural_surface_support(id)
                {
                    continue;
                }
                if ((y + 1)..WORLD_TOP).all(|open_y| {
                    world
                        .get_block_state(BlockPos::new(support.x, open_y, support.z))
                        .is_some_and(|state| state.is_air())
                }) {
                    return Some((support.x as f64 + 0.5, support.z as f64 + 0.5, Some(y + 1)));
                }
            }
        }
        // Never send the original XZ back to XZGoal: that is the cave-floor failure this guard fixes.
        None
    }

    /// Walk toward (x, z). In `planned` nav (default) OUR nav loop plans the route (nav.rs:
    /// azalea's public `calculate_path` with basic-only successors, mining disabled; parkour
    /// successors when `allow_parkour`) and the ECS controller follows the corridor loosely.
    /// azalea's own goto/executor are never engaged. In `straight` nav we head straight at
    /// (x, z). The camera is set to face travel (Hold) for the move so sprint can engage; a
    /// later `look`/`sweep` overrides it. Best-effort: returns whether it arrived within
    /// `radius` — an unreachable goal (nav gives up after repeated stalled/empty replans)
    /// returns false quickly instead of burning the timeout.
    pub(crate) async fn move_to(&self, x: f64, z: f64, timeout_ms: u64, radius: f64) -> bool {
        self.move_with_bounds(x, z, timeout_ms, radius, None).await
    }

    pub(crate) async fn move_with_bounds(
        &self,
        x: f64,
        z: f64,
        timeout_ms: u64,
        radius: f64,
        bounds: Option<pawa_schema::regions::Bounds>,
    ) -> bool {
        // Data collection needs "near the destination", never an exact spot — a generous floor
        // keeps the controller from precision-maneuvering (short corrective key presses).
        let radius = radius.max(1.5);
        let start = self.bot.position().ok();
        let Some((x, z, target_y)) = self.generation_six_surface_goal((x, z)) else {
            // No proven exterior candidate: do not hand the original cave XZ to the planner.
            return false;
        };
        *self.inner.camera_intent.lock() = CameraIntent::Hold;
        let planned = bounds.is_some() || self.cfg().nav_mode != "straight";
        if planned {
            self.inner.nav.lock().set_goal(nav::NavGoal {
                bounds,
                x,
                z,
                radius,
                allow_parkour: self.cfg().allow_parkour,
                target_y,
            });
            *self.inner.loco_intent.lock() = LocoIntent::Planned { x, z, radius };
        } else {
            *self.inner.loco_intent.lock() = LocoIntent::Straight { x, z, radius };
        }
        let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(1));
        let no_progress = self.cfg().move_no_progress_ms;
        let mut arrived = false;
        let mut anchor = self.bot.position().unwrap_or_default();
        let mut last_progress = Instant::now();
        loop {
            sleep_ms(100).await;
            let Ok(p) = self.bot.position() else {
                break;
            };
            if ((p.x - x).powi(2) + (p.z - z).powi(2)).sqrt() <= radius
                && target_y.is_none_or(|y| (p.y - y as f64).abs() <= 1.0)
            {
                arrived = true;
                break;
            }
            // The nav loop declares a goal unreachable after repeated stalled/empty replans —
            // return immediately so the scenario moves on instead of dead-staring.
            if planned && self.inner.nav.lock().unreachable {
                break;
            }
            // Outer no-progress safety (straight nav has no planner to give up for us; for
            // planned nav this is a backstop behind nav's own give-up). The 1.5-block bar is
            // well under real navigation speed but above crowd-shove displacement.
            if ((p.x - anchor.x).powi(2) + (p.z - anchor.z).powi(2)).sqrt() >= 1.5 {
                anchor = p;
                last_progress = Instant::now();
            } else if no_progress > 0
                && last_progress.elapsed() >= Duration::from_millis(no_progress)
            {
                break;
            }
            if Instant::now() >= deadline {
                break;
            }
        }
        *self.inner.loco_intent.lock() = LocoIntent::Idle;
        if planned {
            self.inner.nav.lock().clear_goal();
        }
        if let Some(bounds) = bounds {
            self.unpin_in_region(arrived, start, bounds).await;
        } else {
            self.unpin_after_move(arrived, start).await;
        }
        self.pause().await;
        arrived
    }

    /// A short physical recovery for a planner stranded on a partial frontier. Both claims
    /// remain held until this returns. Unlike the generic escape, the heading and every body
    /// sample are constrained to the reserved rectangle, with room for braking at its edge.
    async fn unpin_in_region(
        &self,
        arrived: bool,
        start: Option<Vec3>,
        bounds: pawa_schema::regions::Bounds,
    ) {
        let Some(p) = self.position() else { return };
        let moved = start.is_none_or(|s| (p.0 - s.x).hypot(p.2 - s.z) >= 1.0);
        if arrived || moved {
            self.inner.move_stalls.store(0, Ordering::SeqCst);
            return;
        }
        if self.inner.move_stalls.fetch_add(1, Ordering::SeqCst) + 1 < 2 {
            return;
        }
        self.inner.move_stalls.store(0, Ordering::SeqCst);
        let phase = self.elapsed_ms() as f64 * 0.0021 + self.index() as f64 * 2.4;
        let Some((x, z)) = bounds.recovery_target(p.0, p.2, phase) else {
            return;
        };
        if !self.look_until_facing((x, p.1 + 1.62, z), 8.0, 5_000).await {
            return;
        }
        *self.inner.camera_intent.lock() = CameraIntent::Hold;
        *self.inner.loco_intent.lock() = LocoIntent::Hold {
            dir: WalkDirection::Forward,
            sprint: false,
        };
        self.set_jump(true);
        let begin = Instant::now();
        while begin.elapsed() < Duration::from_millis(800) && !self.target_met() {
            sleep_ms(50).await;
            if begin.elapsed() >= Duration::from_millis(150) {
                self.set_jump(false);
            }
            let Some(now) = self.position() else { break };
            if (now.0 - p.0).hypot(now.2 - p.2) >= 2.5
                || !bounds.contains(now.0 - 0.6, now.2 - 0.6)
                || !bounds.contains(now.0 + 0.6, now.2 + 0.6)
                || now.1 < p.1 - 1.0
            {
                break;
            }
        }
        *self.inner.loco_intent.lock() = LocoIntent::Idle;
        self.set_jump(false);
    }

    /// Escape hatch under EVERY scenario's movement: after two consecutive `move_to*` calls that
    /// failed without displacing the body, the planner is answering from a spot it cannot solve
    /// (a spawn inside foliage, the floor of a hole) and no fresh goal will help until the body
    /// has moved. Same blind-hop recipe as `common::shake_loose`, but here so a scenario that
    /// only ever calls `move_to` gets it too — a formation_walk bot re-issued failing slot goals
    /// from one unwalkable spawn cell for a full 120 s episode without pressing a single key.
    /// The heading spreads across retries by wall clock and bot index; `hold_visual`'s own stall
    /// guard bounds a hop that goes nowhere to ~1.2 s.
    async fn unpin_after_move(&self, arrived: bool, start: Option<Vec3>) {
        let moved = match (start, self.bot.position().ok()) {
            (Some(a), Some(b)) => {
                ((b.x - a.x).powi(2) + (b.y - a.y).powi(2) + (b.z - a.z).powi(2)).sqrt() >= 1.0
            }
            _ => true,
        };
        if arrived || moved {
            self.inner.move_stalls.store(0, Ordering::SeqCst);
            return;
        }
        if self.inner.move_stalls.fetch_add(1, Ordering::SeqCst) + 1 < 2 {
            return;
        }
        self.inner.move_stalls.store(0, Ordering::SeqCst);
        if self.requires_exterior_surface() {
            return;
        }
        let heading = (self.elapsed_ms() as f64 * 0.0021 + self.inner.index as f64 * 2.4)
            .rem_euclid(std::f64::consts::TAU);
        if let Some(p) = self.position() {
            let settle = self.cfg().look_settle_ms.max(400);
            self.look(
                (
                    p.0 - heading.sin() * 24.0,
                    p.1 + 1.62,
                    p.2 + heading.cos() * 24.0,
                ),
                settle,
            )
            .await;
        }
        self.hold_visual(WalkDirection::Forward, false, 1_600, 500)
            .await;
    }

    /// Walk toward `(x, z)` while continuously refreshing the camera toward a live peer.
    /// Social scenarios use this when the movement direction is orthogonal to the relationship
    /// they need the recording to show (orbit and formation motion).
    pub(crate) async fn move_to_watching_peer(
        &self,
        x: f64,
        z: f64,
        peer: usize,
        timeout_ms: u64,
        radius: f64,
    ) -> bool {
        self.move_to_watching(x, z, WatchTarget::Peer(peer), timeout_ms, radius)
            .await
    }

    /// Walk toward `(x, z)` with the camera pinned on a fixed world point instead of the path —
    /// the world-edit analogue of [`Self::move_to_watching_peer`]: a builder backing away from its
    /// tower keeps the full-height artifact in frame while the body does the traveling.
    pub(crate) async fn move_to_watching_point(
        &self,
        x: f64,
        z: f64,
        focus: (f64, f64, f64),
        timeout_ms: u64,
        radius: f64,
    ) -> bool {
        self.move_to_watching(x, z, WatchTarget::Point(focus), timeout_ms, radius)
            .await
    }

    async fn move_to_watching(
        &self,
        x: f64,
        z: f64,
        watch: WatchTarget,
        timeout_ms: u64,
        radius: f64,
    ) -> bool {
        let radius = radius.max(1.5);
        let start = self.bot.position().ok();
        let Some((x, z, target_y)) = self.generation_six_surface_goal((x, z)) else {
            // No proven exterior candidate: do not hand the original cave XZ to the planner.
            return false;
        };
        *self.inner.camera_intent.lock() = CameraIntent::Hold;
        let planned = self.cfg().nav_mode != "straight";
        if planned {
            self.inner.nav.lock().set_goal(nav::NavGoal {
                bounds: None,
                x,
                z,
                radius,
                allow_parkour: self.cfg().allow_parkour,
                target_y,
            });
            *self.inner.loco_intent.lock() = LocoIntent::Planned { x, z, radius };
        } else {
            *self.inner.loco_intent.lock() = LocoIntent::Straight { x, z, radius };
        }
        let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(1));
        let no_progress = self.cfg().move_no_progress_ms;
        let mut arrived = false;
        let mut anchor = self.bot.position().unwrap_or_default();
        let mut last_progress = Instant::now();
        loop {
            if let (Some(focus), Ok(eye)) = (self.watch_focus(watch), self.bot.eye_position()) {
                let (yaw, pitch) = look_angles((eye.x, eye.y, eye.z), focus);
                *self.inner.camera_intent.lock() = CameraIntent::LookAt { yaw, pitch };
            }
            sleep_ms(100).await;
            let Ok(p) = self.bot.position() else {
                break;
            };
            if ((p.x - x).powi(2) + (p.z - z).powi(2)).sqrt() <= radius
                && target_y.is_none_or(|y| (p.y - y as f64).abs() <= 1.0)
            {
                arrived = true;
                break;
            }
            if planned && self.inner.nav.lock().unreachable {
                break;
            }
            if ((p.x - anchor.x).powi(2) + (p.z - anchor.z).powi(2)).sqrt() >= 1.5 {
                anchor = p;
                last_progress = Instant::now();
            } else if no_progress > 0
                && last_progress.elapsed() >= Duration::from_millis(no_progress)
            {
                break;
            }
            if Instant::now() >= deadline {
                break;
            }
        }
        *self.inner.loco_intent.lock() = LocoIntent::Idle;
        if planned {
            self.inner.nav.lock().clear_goal();
        }
        if let (Some(focus), Ok(eye)) = (self.watch_focus(watch), self.bot.eye_position()) {
            let (yaw, pitch) = look_angles((eye.x, eye.y, eye.z), focus);
            *self.inner.camera_intent.lock() = CameraIntent::LookAt { yaw, pitch };
        } else {
            *self.inner.camera_intent.lock() = CameraIntent::Hold;
        }
        self.unpin_after_move(arrived, start).await;
        self.pause().await;
        arrived
    }

    /// Resolve what [`Self::move_to_watching`] should keep the camera on right now: a live peer's
    /// head (which moves between refreshes) or a fixed world point.
    fn watch_focus(&self, watch: WatchTarget) -> Option<(f64, f64, f64)> {
        match watch {
            WatchTarget::Peer(j) => self.peer_position(j).map(|p| (p.0, p.1 + 1.4, p.2)),
            WatchTarget::Point(p) => Some(p),
        }
    }

    pub(crate) async fn move_default(&self, x: f64, z: f64) -> bool {
        let cfg = self.cfg();
        self.move_to(x, z, cfg.move_timeout_ms, cfg.goal_radius)
            .await
    }

    /// Fixed-rate quantized look toward a world position: set the camera intent, then wait until
    /// the controller settles within ½ quantum. No wall-clock interpolation — the angular speed is
    /// the configured fixed rate; `settle_ms` only bounds the max wait.
    pub(crate) async fn look(&self, target: (f64, f64, f64), settle_ms: u64) {
        let Ok(eye) = self.bot.eye_position() else {
            return;
        };
        let (yaw, pitch) = look_angles((eye.x, eye.y, eye.z), target);
        *self.inner.camera_intent.lock() = CameraIntent::LookAt { yaw, pitch };
        self.inner.camera_busy.store(true, Ordering::SeqCst);
        self.wait_camera(settle_ms).await;
    }

    pub(crate) async fn look_default(&self, target: (f64, f64, f64)) {
        self.look(target, self.cfg().look_settle_ms).await;
    }

    /// Wall-clock ceiling for `look_until_facing` during a placement: twice the time a 180 deg
    /// reversal takes at the configured yaw rate.
    ///
    /// DERIVED, not chosen. The first version of this was a flat 2 500 ms, which at the default
    /// 72 deg/s is exactly the time a 180 deg turn needs and therefore leaves no margin for the
    /// 50 ms poll granularity or for the quantized final step — a legitimate wide turn would have
    /// been abandoned as a failure. Deriving it also keeps it correct if `yaw_rate_deg_s` is
    /// changed, which a constant would not.
    fn place_facing_max_wait_ms(&self) -> u64 {
        let rate = self.cfg().yaw_rate_deg_s.max(1.0);
        ((180.0 / rate) * 2.0 * 1000.0) as u64
    }

    /// Turn toward `target` and report whether the view ACTUALLY arrived, within `tolerance_deg`.
    ///
    /// `look` waits on a deadline, not on arrival: `wait_camera` clamps its wait to at least 1000 ms
    /// and the camera turns at [`control::DEFAULT_YAW_RATE_DEG_S`] (72 deg/s), so a turn wider than
    /// ~72 deg times out and the caller proceeds anyway. For a *placement* that is a defect the
    /// server cannot catch: `use_item_on` is judged on the hit position in the packet, not on where
    /// the player is facing, so the block lands correctly while the footage shows the bot placing it
    /// over its shoulder. Measured on `construction/audience_ring_build` p00 before this existed:
    /// of 33 placements the horizontal angle between look direction and target block ran 10.5 deg
    /// median, 79.2 deg at p90, 137.0 deg worst, with 8 beyond 45 deg and 2 beyond 90 deg — the owner
    /// spotted it in the video before any instrument did.
    ///
    /// Polls the authoritative `cam_yaw` rather than sleeping longer, so a wide turn costs the time
    /// it needs and a narrow one costs nothing extra.
    pub(crate) async fn look_until_facing(
        &self,
        target: (f64, f64, f64),
        tolerance_deg: f64,
        max_wait_ms: u64,
    ) -> bool {
        self.look(target, self.cfg().look_settle_ms).await;
        let deadline = Instant::now() + Duration::from_millis(max_wait_ms);
        loop {
            // Recomputed each pass: the bot may drift on its leash while turning, which moves the
            // required angle. Comparing against a target captured once would converge on a stale one.
            let Ok(eye) = self.bot.eye_position() else {
                return false;
            };
            let (want_yaw, _) = look_angles((eye.x, eye.y, eye.z), target);
            let have_yaw = *self.inner.cam_yaw.lock();
            if ((want_yaw - have_yaw + 180.0).rem_euclid(360.0) - 180.0).abs() <= tolerance_deg {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            sleep_ms(50).await;
        }
    }

    /// Constant-rate triangle sweep around the current heading for ~`duration_ms`.
    pub(crate) async fn sweep(&self, duration_ms: u64) {
        let cfg = self.cfg();
        // Authoritative heading is our cam_yaw, not the raw LookDirection (which can momentarily
        // hold a live-pathfinder look-at snap between controller writes).
        let base = *self.inner.cam_yaw.lock();
        let ticks = (duration_ms / 50).max(1);
        *self.inner.camera_intent.lock() = CameraIntent::Sweep {
            base_yaw: base,
            amp_deg: cfg.look_sweep_yaw_rad.to_degrees(),
            ticks,
            dir: 1.0,
        };
        self.inner.camera_busy.store(true, Ordering::SeqCst);
        self.wait_camera(duration_ms + 1000).await;
    }

    pub(crate) async fn sweep_default(&self) {
        self.sweep(self.cfg().look_sweep_duration_ms).await;
    }

    /// Hold Forward as ONE continuous press and release it the instant the feet leave the ground —
    /// the walk-off-an-edge primitive. Pressing through the fall (a fixed-length hold) air-strafes
    /// the body several blocks past the edge it stepped off, and splitting the approach into short
    /// pulses trips the key-chatter QC gate; both were measured on `build_tower` seed 2026. Returns
    /// whether the fall actually started within `max_ms`.
    pub(crate) async fn walk_until_airborne(&self, max_ms: u64) -> bool {
        let (fy, fp) = (*self.inner.cam_yaw.lock(), *self.inner.cam_pitch.lock());
        *self.inner.camera_intent.lock() = CameraIntent::LookAt { yaw: fy, pitch: fp };
        *self.inner.loco_intent.lock() = LocoIntent::Hold {
            dir: WalkDirection::Forward,
            sprint: false,
        };
        let end = Instant::now() + Duration::from_millis(max_ms.max(1));
        let mut airborne = false;
        while Instant::now() < end && !self.target_met() {
            sleep_ms(25).await;
            if !self.on_ground() {
                airborne = true;
                break;
            }
        }
        *self.inner.loco_intent.lock() = LocoIntent::Idle;
        airborne
    }

    /// CALIBRATION: hold a specific VISUAL movement key for `duration_ms` with the camera FROZEN at
    /// its current angle (so the key's translation is unambiguous on screen), pulsing JUMP for a few
    /// ticks every `jump_period_ms` (0 = never). `sprint` requests sprint (forward-effective only).
    /// `visual` is the direction a viewer should SEE; on azalea 0.16 the wire name IS the visual
    /// direction (see `control::STRAFE_SIGN`), so it is sent as-is.
    /// Returns whether the hold ran to term; `false` means it was cut short because the body had
    /// stopped translating (see [`HOLD_STALL_WINDOW`]).
    pub(crate) async fn hold_visual(
        &self,
        visual: WalkDirection,
        sprint: bool,
        duration_ms: u64,
        jump_period_ms: u64,
    ) -> bool {
        let (fy, fp) = (*self.inner.cam_yaw.lock(), *self.inner.cam_pitch.lock());
        *self.inner.camera_intent.lock() = CameraIntent::LookAt { yaw: fy, pitch: fp };
        *self.inner.loco_intent.lock() = LocoIntent::Hold {
            dir: visual,
            sprint,
        };
        let end = Instant::now() + Duration::from_millis(duration_ms.max(1));
        let period = Duration::from_millis(jump_period_ms.max(1));
        let mut next_jump = Instant::now() + period;
        let generation_six = self.requires_exterior_surface();
        let start_y = self.bot.position().ok().map(|p| p.y);
        let mut mark = (Instant::now(), self.bot.position().ok());
        let mut completed = true;
        while Instant::now() < end && !self.target_met() {
            sleep_ms(50).await;
            if jump_period_ms > 0 && Instant::now() >= next_jump {
                self.set_jump(true);
                sleep_ms(150).await;
                self.set_jump(false);
                next_jump = Instant::now() + period;
            }
            let now = self.bot.position().ok();
            // V6 uses open-loop holds for short visual dollies, not cave entry. Stop before a downhill
            // key press carries the bot from an exposed station into an unobserved cave floor.
            if hold_drop_exceeded(generation_six, start_y, now.map(|p| p.y))
                || generation_six && !self.current_position_has_sky()
            {
                completed = false;
                break;
            }
            // An open-loop key hold walks into whatever is ahead and keeps pressing: a trunk, a
            // wall, the lip of a ledge. `move_to` has a no-progress guard for exactly this; without
            // one here the video is a bot leaning on a tree, jump-pulsing, for the rest of the hold.
            if mark.0.elapsed() >= HOLD_STALL_WINDOW {
                if let (Some(a), Some(b)) = (mark.1, now) {
                    let travelled = ((b.x - a.x).powi(2) + (b.z - a.z).powi(2)).sqrt();
                    if travelled < HOLD_STALL_TRAVEL {
                        completed = false;
                        break;
                    }
                }
                mark = (Instant::now(), now);
            }
        }
        *self.inner.loco_intent.lock() = LocoIntent::Idle;
        self.set_jump(false);
        completed
    }

    /// CALIBRATION: stand still and drag the view by (`dyaw`, `dpitch`) degrees from the current
    /// angle at the fixed quantized rate — isolating the camera-motion action for inspection.
    pub(crate) async fn drag_view(&self, dyaw: f64, dpitch: f64, settle_ms: u64) {
        *self.inner.loco_intent.lock() = LocoIntent::Idle;
        let yaw = *self.inner.cam_yaw.lock() + dyaw;
        let pitch = (*self.inner.cam_pitch.lock() + dpitch).clamp(-90.0, 90.0);
        *self.inner.camera_intent.lock() = CameraIntent::LookAt { yaw, pitch };
        self.inner.camera_busy.store(true, Ordering::SeqCst);
        self.wait_camera(settle_ms).await;
    }

    /// CALIBRATION: stand still with the camera frozen for `ms` — a clean gap between segments.
    pub(crate) async fn still(&self, ms: u64) {
        *self.inner.loco_intent.lock() = LocoIntent::Idle;
        let (fy, fp) = (*self.inner.cam_yaw.lock(), *self.inner.cam_pitch.lock());
        *self.inner.camera_intent.lock() = CameraIntent::LookAt { yaw: fy, pitch: fp };
        self.set_jump(false);
        sleep_ms(ms).await;
    }

    /// Poll until the camera controller reports idle (settled within ½ quantum, or sweep done), or
    /// `max_wait_ms` (a safety bound — a fixed-rate ≤180° turn takes a few seconds).
    pub(crate) async fn wait_camera(&self, max_wait_ms: u64) {
        let cap = max_wait_ms.clamp(1000, 10000);
        let deadline = Instant::now() + Duration::from_millis(cap);
        loop {
            sleep_ms(50).await;
            if !self.inner.camera_busy.load(Ordering::SeqCst) || Instant::now() >= deadline {
                break;
            }
        }
    }
}

#[cfg(test)]
mod placement_tests {
    use super::{adjacent_block, block_id_matches, canonical_registry_name, face_hit};
    use azalea::{BlockPos, core::direction::Direction};

    #[test]
    fn reference_face_and_hit_location_describe_the_same_target() {
        let reference = BlockPos::new(10, 64, -4);
        for (face, target, location) in [
            (Direction::Up, BlockPos::new(10, 65, -4), (10.5, 65.0, -3.5)),
            (
                Direction::Down,
                BlockPos::new(10, 63, -4),
                (10.5, 64.0, -3.5),
            ),
            (
                Direction::North,
                BlockPos::new(10, 64, -5),
                (10.5, 64.5, -4.0),
            ),
            (
                Direction::South,
                BlockPos::new(10, 64, -3),
                (10.5, 64.5, -3.0),
            ),
            (
                Direction::West,
                BlockPos::new(9, 64, -4),
                (10.0, 64.5, -3.5),
            ),
            (
                Direction::East,
                BlockPos::new(11, 64, -4),
                (11.0, 64.5, -3.5),
            ),
        ] {
            let hit = face_hit(reference, face);
            assert_eq!(adjacent_block(reference, face), target);
            assert_eq!(hit.block_pos, reference);
            assert_eq!(hit.direction, face);
            assert_eq!((hit.location.x, hit.location.y, hit.location.z), location);
        }
    }

    #[test]
    fn placement_success_requires_the_exact_expected_block_id() {
        assert_eq!(canonical_registry_name("minecraft:bricks"), "bricks");
        assert!(block_id_matches(Some("bricks"), "minecraft:bricks"));
        assert!(!block_id_matches(Some("dirt"), "minecraft:bricks"));
        assert!(!block_id_matches(Some("air"), "minecraft:bricks"));
        assert!(!block_id_matches(None, "minecraft:bricks"));
    }
}

fn record_event(
    inner: &Arc<BotInner>,
    cfg: &Cfg,
    action_type: &str,
    visible_event_id: &str,
    description: &str,
    extras: Value,
) {
    let seq = inner.seq.fetch_add(1, Ordering::SeqCst);
    let mut extra = pawa_schema::JsonMap::new();
    if let Value::Object(values) = extras {
        extra.extend(values);
    }
    let _ = cfg;
    let event = ActionEvent {
        seq,
        action_id: format!("act-{}-{seq:04}", inner.index),
        action_type: action_type.to_string(),
        kind: canonical_type(action_type),
        client_tick: ClientTick(inner.client_ticks.load(Ordering::SeqCst)),
        server_tick: ServerTick(inner.offset_tick()),
        world_age_tick: inner.world_age_tick().map(WorldAgeTick),
        player_time_ms: PlayerTimeMs(inner.elapsed_ms().max(0)),
        visible_event_id: Some(visible_event_id.to_string()),
        payload: pawa_schema::capture::ActionPayload {
            description: description.to_string(),
            extra,
        },
    };
    inner.events.lock().push(event);
}

#[cfg(test)]
mod facing_tests {
    /// The bound must exceed the time a 180 deg reversal actually takes, or a legitimate wide turn
    /// is abandoned as a failure. The first version was a flat 2500 ms, which at the default rate is
    /// exactly 180 deg with zero margin for the 50 ms poll or the quantized final step.
    #[test]
    fn the_facing_wait_bound_covers_a_full_reversal_with_margin() {
        for rate in [36.0_f64, 72.0, 144.0] {
            let bound_ms = ((180.0 / rate) * 2.0 * 1000.0) as u64;
            let reversal_ms = (180.0 / rate * 1000.0) as u64;
            assert!(
                bound_ms >= reversal_ms + 1000,
                "rate {rate}: bound {bound_ms}ms leaves under 1s of margin over a {reversal_ms}ms reversal"
            );
        }
    }

    /// 5 deg has to be inside a block face's angular size at interaction reach, or the tolerance can
    /// never be met and every placement is abandoned.
    #[test]
    fn the_facing_tolerance_is_inside_a_block_face_at_reach() {
        let subtended_deg = (1.0_f64 / 4.0).atan().to_degrees() * 2.0;
        assert!(
            super::PLACE_FACING_TOLERANCE_DEG < subtended_deg / 2.0,
            "tolerance {} deg is not comfortably inside a face subtending {subtended_deg:.1} deg",
            super::PLACE_FACING_TOLERANCE_DEG
        );
    }
}
