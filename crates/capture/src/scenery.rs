//! Deterministic static scenery, built into the world over RCON at episode start.
//!
//! Vanilla silently drops an offline bot's chat-commands (`/setblock`, `/fill`, ...) because the bot
//! never completes the secure-chat acknowledgment chain, so every `bot.chat("/setblock …")` build
//! placed *nothing* in the world. Deterministic, pre-computable geometry is therefore built
//! server-side over RCON instead — reliably, on vanilla and Fabric.
//!
//! The coordinator calls [`build_commands`] once, AFTER the scene freeze and BEFORE `replay start`,
//! so the scenery is present in every recorded frame. The commands are pure data here, so the layout
//! is testable without a server.
//!
//! Anchoring: the build is **relative to an origin** the coordinator passes in (the swarm's spawn
//! position). A normal world's spawn is NOT at (0,0,0) — for seed 2026 it is ~(-275,64,-675) — so a
//! build at fixed world coordinates near the origin lands in UNLOADED chunks ("That position is not
//! loaded") and places nothing. Anchoring to the bots' position keeps the build inside loaded chunks
//! AND next to where they actually play; `forceload` guarantees the region is loaded regardless.
//! Geometry is fully deterministic given (seed, origin).
//!
//! Boundary: RCON places blocks INSTANTLY with no bot animation, so it is used only for
//! static scenery and deterministic site preparation. Incremental build scenarios still perform
//! their actual construction with survival placement after the arena is prepared.

use md5::{Digest, Md5};

/// A bright, unambiguous marker so the structure reads in the rendered video.
pub const MARKER: &str = "minecraft:glowstone";

/// Cosmetic palette the seed rotates through; geometry stays fully reproducible.
const MATERIALS: [&str; 4] = [
    "minecraft:stone_bricks",
    "minecraft:smooth_quartz",
    "minecraft:oak_planks",
    "minecraft:sandstone",
];

/// A block position in world coordinates, as the builders anchor against.
pub type Origin = (i64, i64, i64);

/// A deterministic non-negative integer from the (string) scenery seed.
///
/// Numeric strings are used directly; any other non-empty string is hashed (MD5) so a stable name
/// yields a stable course. Absent or blank is `0`.
pub fn resolve_seed(raw: Option<&str>) -> u64 {
    let text = raw.unwrap_or("").trim();
    if text.is_empty() {
        return 0;
    }
    if let Ok(number) = text.parse::<f64>() {
        return number.trunc().abs() as u64;
    }
    let digest = Md5::digest(text.as_bytes());
    u64::from_be_bytes(digest[..8].try_into().expect("8 bytes"))
}

/// A parkour course: 3x3 platforms march along +X with two-block jumpable gaps above a deep, wide
/// cleared trench, so pathfinding cannot satisfy platform targets by walking below and foliage
/// cannot occlude the takeoff, landing, or final look-back. The final platform widens forward and
/// sideways to 7x7 so multiple bots can separate. Glowstone marks both ends. Anchored three blocks
/// off `origin` and lifted one block; the seed varies platform count (5..7) and material only.
/// `forceload` covers the entire cleared review volume.
fn gap_jump(seed: u64, origin: Origin) -> Vec<String> {
    let (ox, oy, oz) = origin;
    let count = 5 + (seed % 3) as i64;
    let material = MATERIALS[(seed % MATERIALS.len() as u64) as usize];
    let span = 3;
    let pitch = span + 2;
    let y = oy + 1;
    let start_x = ox + 3;
    let (z0, z1) = (oz - 1, oz + 1);
    let (last_z0, last_z1) = (oz - 3, oz + 3);
    let last_x0 = start_x + (count - 1) * pitch;
    let clear_x1 = last_x0 + 6 + 4;
    let (clear_z0, clear_z1) = (oz - 10, oz + 10);
    let mut commands = vec![
        format!("forceload add {start_x} {clear_z0} {clear_x1} {clear_z1}"),
        format!(
            "fill {start_x} {} {clear_z0} {clear_x1} {} {clear_z1} minecraft:air replace",
            oy - 4,
            y + 12
        ),
    ];
    for index in 0..count {
        let x0 = start_x + index * pitch;
        let last = index == count - 1;
        let platform_x1 = x0 + if last { 6 } else { span - 1 };
        let (platform_z0, platform_z1) = if last { (last_z0, last_z1) } else { (z0, z1) };
        commands.push(format!(
            "fill {x0} {y} {platform_z0} {platform_x1} {y} {platform_z1} {material} replace"
        ));
    }
    commands.push(format!(
        "setblock {start_x} {} {oz} {MARKER} replace",
        y + 1
    ));
    commands.push(format!(
        "setblock {last_x0} {} {oz} {MARKER} replace",
        y + 1
    ));
    commands
}

/// The eight inset marker tiles every arena carries: corners plus edge midpoints.
fn arena_markers(origin: Origin, radius: i64) -> [(i64, i64); 8] {
    let (ox, _, oz) = origin;
    let (x0, x1) = (ox - radius, ox + radius);
    let (z0, z1) = (oz - radius, oz + radius);
    [
        (x0, z0),
        (ox, z0),
        (x1, z0),
        (x1, oz),
        (x1, z1),
        (ox, z1),
        (x0, z1),
        (x0, oz),
    ]
}

/// Clear a `radius`-square site down to a flat floor one block below spawn level.
///
/// The temporary top layer makes the following air fill a verifiable non-noop even in an already-open
/// biome (see [`replies_ok`]); it is removed by the air fill itself before recording starts.
fn cleared_site(origin: Origin, radius: i64) -> Vec<String> {
    let (ox, oy, oz) = origin;
    let (x0, x1) = (ox - radius, ox + radius);
    let (z0, z1) = (oz - radius, oz + radius);
    let clear_top = oy + 10;
    vec![
        format!("forceload add {x0} {z0} {x1} {z1}"),
        format!("fill {x0} {clear_top} {z0} {x1} {clear_top} {z1} minecraft:smooth_stone replace"),
        format!("fill {x0} {oy} {z0} {x1} {clear_top} {z1} minecraft:air replace"),
    ]
}

/// A centered 25x25 open arena for unobstructed PVP/PVE capture.
///
/// The floor is one block below the players' spawn level and the eleven blocks above it are cleared,
/// removing trunks, leaves, and uneven terrain without adding sight-blocking walls. `seed` is
/// accepted for the common builder interface; this arena intentionally has fixed geometry.
fn combat_arena(_seed: u64, origin: Origin) -> Vec<String> {
    open_arena(origin, 12)
}

/// A centered 41x41 level site for real survival block-placement scenarios.
///
/// Radius 20 covers tower sites up to radius 12 plus a five-block retreat. Embedded perimeter
/// lighting stays outside every possible build and player station.
fn build_arena(_seed: u64, origin: Origin) -> Vec<String> {
    open_arena(origin, 20)
}

fn open_arena(origin: Origin, radius: i64) -> Vec<String> {
    let (ox, oy, oz) = origin;
    let (x0, x1) = (ox - radius, ox + radius);
    let (z0, z1) = (oz - radius, oz + radius);
    let floor_y = oy - 1;
    let mut commands = cleared_site(origin, radius);
    commands.push(format!(
        "fill {x0} {floor_y} {z0} {x1} {floor_y} {z1} minecraft:smooth_stone replace"
    ));
    commands.extend(
        arena_markers(origin, radius)
            .into_iter()
            .map(|(x, z)| format!("setblock {x} {floor_y} {z} {MARKER} replace")),
    );
    commands
}

/// A centered open mining site with a solid underground slab and an exposed ore face.
///
/// The sixteen-block-deep smooth-stone foundation covers the collector's full vertical scan radius,
/// so natural underground veins cannot outrank the deterministic exposed ore face. It also
/// guarantees that heading-mining episodes can dig down twice and carve a supported horizontal
/// tunnel. The one-block-thick wall sits eight blocks north of spawn, making each removed ore a clear
/// hole from the arena floor. Geometry is fixed; `seed` is accepted for the common builder interface.
fn mining_arena(_seed: u64, origin: Origin) -> Vec<String> {
    let (ox, oy, oz) = origin;
    let radius = 20;
    let (x0, x1) = (ox - radius, ox + radius);
    let (z0, z1) = (oz - radius, oz + radius);
    let floor_y = oy - 1;
    let wall_z = oz + 8;
    let mut commands = cleared_site(origin, radius);
    commands.push(format!(
        "fill {x0} {} {z0} {x1} {floor_y} {z1} minecraft:smooth_stone replace",
        oy - 16
    ));
    commands.push(format!(
        "fill {} {oy} {wall_z} {} {} {wall_z} minecraft:stone_bricks replace",
        ox - 8,
        ox + 8,
        oy + 3
    ));
    let ores = [
        (ox - 7, oy, "minecraft:coal_ore"),
        (ox - 5, oy, "minecraft:copper_ore"),
        (ox - 3, oy, "minecraft:iron_ore"),
        (ox - 1, oy, "minecraft:gold_ore"),
        (ox + 1, oy, "minecraft:redstone_ore"),
        (ox + 3, oy, "minecraft:lapis_ore"),
        (ox + 5, oy, "minecraft:emerald_ore"),
        (ox + 7, oy, "minecraft:diamond_ore"),
        (ox - 7, oy + 2, "minecraft:deepslate_coal_ore"),
        (ox - 5, oy + 2, "minecraft:deepslate_copper_ore"),
        (ox - 3, oy + 2, "minecraft:deepslate_iron_ore"),
        (ox - 1, oy + 2, "minecraft:deepslate_gold_ore"),
        (ox + 1, oy + 2, "minecraft:deepslate_redstone_ore"),
        (ox + 3, oy + 2, "minecraft:deepslate_lapis_ore"),
        (ox + 5, oy + 2, "minecraft:deepslate_emerald_ore"),
        (ox + 7, oy + 2, "minecraft:deepslate_diamond_ore"),
    ];
    commands.extend(
        ores.into_iter()
            .map(|(x, y, block)| format!("setblock {x} {y} {wall_z} {block} replace")),
    );
    commands.extend(
        arena_markers(origin, radius)
            .into_iter()
            .map(|(x, z)| format!("setblock {x} {floor_y} {z} {MARKER} replace")),
    );
    commands
}

/// The ordered RCON commands that build scenery `name` anchored at `origin`.
///
/// `none` and any unknown name yield an empty list (the coordinator then builds nothing). The names
/// are [`pawa_schema::catalog::KNOWN_SCENERY`].
pub fn build_commands(name: &str, seed: u64, origin: Origin) -> Vec<String> {
    match name {
        "build_arena" => build_arena(seed, origin),
        "combat_arena" => combat_arena(seed, origin),
        "mining_arena" => mining_arena(seed, origin),
        "gap_jump" => gap_jump(seed, origin),
        _ => Vec::new(),
    }
}

/// Whether every block-PLACING command (`fill`/`setblock`) reported success in its RCON reply.
///
/// The server confirms a real placement with "Successfully filled N block(s)" (N > 0) / "Changed the
/// block at …"; an unloaded-chunk attempt replies "That position is not loaded" and places nothing —
/// so this rejects the silent no-op the bots' dropped chat-commands produced, without a
/// block read (vanilla `data get block` only works on block entities, not plain blocks like
/// glowstone). Setup commands (`forceload`) are not placements and are not checked. Server locale is
/// en_US.
pub fn replies_ok(commands: &[String], replies: &[String]) -> bool {
    let mut checked = 0;
    for (command, reply) in commands.iter().zip(replies) {
        let low = reply.to_lowercase();
        match command.split_whitespace().next().unwrap_or("") {
            "fill" => {
                checked += 1;
                if !low.contains("successfully filled") || filled_count(&low).unwrap_or(0) == 0 {
                    return false;
                }
            }
            "setblock" => {
                checked += 1;
                if !low.contains("changed the block") {
                    return false;
                }
            }
            _ => {}
        }
    }
    checked > 0
}

/// The `N` of the first `filled N` in an already-lowercased reply.
fn filled_count(reply: &str) -> Option<u64> {
    for (index, _) in reply.match_indices("filled ") {
        let digits: String = reply[index + "filled ".len()..]
            .trim_start()
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        if let Ok(count) = digits.parse() {
            return Some(count);
        }
    }
    None
}

// ---- player stations on the built geometry ---------------------------------------------

/// The exact entity coordinate at the centre of the containing block.
fn block_center(value: f64) -> f64 {
    value.floor() + 0.5
}

/// Keep sub-block precision while avoiding bare integer `tp` block-centre semantics.
fn explicit_entity_coordinate(value: f64) -> f64 {
    if value.fract() == 0.0 {
        value + 0.001
    } else {
        value
    }
}

/// `count` distinct positions centered on known support.
///
/// Minecraft's absolute `tp` coordinates are exact entity coordinates, not block indices. Every
/// deterministic station stays on a block centre so RCON never applies the command parser's
/// integer-coordinate centring (observed: `tp ... z=-86` settles at z=-85.5).
fn centered_grid(count: usize, radius: f64, y: f64, cx: f64, cz: f64) -> Vec<(f64, f64, f64)> {
    if count == 0 {
        return Vec::new();
    }
    if count == 1 {
        return vec![(block_center(cx), y, block_center(cz))];
    }
    let ring_radius = radius.min(f64::max(1.5, count as f64 * 1.5 / std::f64::consts::TAU));
    (0..count)
        .map(|index| {
            let angle = std::f64::consts::PI + index as f64 * std::f64::consts::TAU / count as f64;
            (
                block_center(cx + ring_radius * angle.cos()),
                y,
                block_center(cz + ring_radius * angle.sin()),
            )
        })
        .collect()
}

/// Pack players at distinct sub-block coordinates within one 3x3 platform's safe interior.
///
/// These deliberately remain precise sub-block entity coordinates: snapping the dense grid to block
/// centres would collapse distinct players or push edge cells beyond the platform's support.
fn platform_grid(count: usize, y: f64, cx: f64, cz: f64) -> Vec<(f64, f64, f64)> {
    if count == 0 {
        return Vec::new();
    }
    if count == 1 {
        return vec![(block_center(cx), y, block_center(cz))];
    }
    let columns = (count as f64).sqrt().ceil() as usize;
    let rows = count.div_ceil(columns);
    let x_step = 1.8 / (columns.saturating_sub(1)).max(1) as f64;
    let z_step = 1.8 / (rows.saturating_sub(1)).max(1) as f64;
    let x0 = cx - x_step * (columns - 1) as f64 / 2.0;
    let z0 = cz - z_step * (rows - 1) as f64 / 2.0;
    (0..count)
        .map(|index| {
            (
                explicit_entity_coordinate(x0 + (index % columns) as f64 * x_step),
                y,
                explicit_entity_coordinate(z0 + (index / columns) as f64 * z_step),
            )
        })
        .collect()
}

/// Deterministic, non-stacked standing positions supported by scenery `name`'s geometry.
///
/// Empty for `none` and unknown names — the coordinator then leaves the spread placement alone.
pub fn player_positions(
    name: &str,
    seed: u64,
    origin: Origin,
    player_count: usize,
) -> Vec<(f64, f64, f64)> {
    let (ox, oy, oz) = (origin.0 as f64, origin.1 as f64, origin.2 as f64);
    match name {
        "build_arena" => centered_grid(player_count, 17.0, oy, ox, oz),
        "combat_arena" => centered_grid(player_count, 9.0, oy, ox, oz),
        "mining_arena" => centered_grid(player_count, 17.0, oy, ox, oz - 5.0),
        "gap_jump" => {
            let platform_count = (5 + seed % 3) as usize;
            let mut positions = vec![None; player_count];
            for platform in 0..platform_count {
                let indices: Vec<usize> =
                    (platform..player_count).step_by(platform_count).collect();
                let local = platform_grid(
                    indices.len(),
                    oy + 2.0,
                    ox + 4.0 + platform as f64 * 5.0,
                    oz,
                );
                for (index, position) in indices.into_iter().zip(local) {
                    positions[index] = Some(position);
                }
            }
            positions.into_iter().flatten().collect()
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Values hand-checked at origin (-275, 64, -675) — the seed-2026 spawn the golden corpus was
    /// captured at.
    const SPAWN: Origin = (-275, 64, -675);

    #[test]
    fn gap_jump_builds_the_expected_course() {
        let commands = build_commands("gap_jump", 0, SPAWN);
        // 2 setup + 5 platforms (seed 0) + 2 markers.
        assert_eq!(commands.len(), 9);
        assert_eq!(commands[0], "forceload add -272 -685 -242 -665");
        assert_eq!(
            commands[1],
            "fill -272 60 -685 -242 77 -665 minecraft:air replace"
        );
        assert_eq!(
            commands[2],
            "fill -272 65 -676 -270 65 -674 minecraft:stone_bricks replace"
        );
        // The last platform widens to 7x7 and keeps the marker at its near edge.
        assert_eq!(
            commands[6],
            "fill -252 65 -678 -246 65 -672 minecraft:stone_bricks replace"
        );
        assert_eq!(
            commands[7],
            "setblock -272 66 -675 minecraft:glowstone replace"
        );
        assert_eq!(
            commands[8],
            "setblock -252 66 -675 minecraft:glowstone replace"
        );
        // The seed only varies platform count and material.
        assert_eq!(build_commands("gap_jump", 2, SPAWN).len(), 11);
        assert!(
            build_commands("gap_jump", 1, SPAWN)[2].ends_with("minecraft:smooth_quartz replace")
        );
    }

    #[test]
    fn arenas_clear_before_they_floor() {
        let commands = build_commands("combat_arena", 0, SPAWN);
        assert_eq!(commands.len(), 12);
        assert_eq!(commands[0], "forceload add -287 -687 -263 -663");
        assert_eq!(
            commands[1],
            "fill -287 74 -687 -263 74 -663 minecraft:smooth_stone replace"
        );
        assert_eq!(
            commands[2],
            "fill -287 64 -687 -263 74 -663 minecraft:air replace"
        );
        assert_eq!(
            commands[3],
            "fill -287 63 -687 -263 63 -663 minecraft:smooth_stone replace"
        );
        assert_eq!(
            commands[4],
            "setblock -287 63 -687 minecraft:glowstone replace"
        );
        // 41x41 site, same shape.
        assert_eq!(build_commands("build_arena", 0, SPAWN).len(), 12);
        assert!(build_commands("build_arena", 0, SPAWN)[0].starts_with("forceload add -295 -695"));
        // 4 setup + 16 ores + 8 markers.
        assert_eq!(build_commands("mining_arena", 0, SPAWN).len(), 29);
        assert_eq!(
            build_commands("mining_arena", 0, SPAWN)[5],
            "setblock -282 64 -667 minecraft:coal_ore replace"
        );
        assert!(build_commands("none", 0, SPAWN).is_empty());
        assert!(build_commands("no_such_scenery", 0, SPAWN).is_empty());
    }

    #[test]
    fn seeds_resolve_from_numbers_or_token_digests() {
        assert_eq!(resolve_seed(None), 0);
        assert_eq!(resolve_seed(Some("  ")), 0);
        assert_eq!(resolve_seed(Some("2026")), 2026);
        assert_eq!(resolve_seed(Some("-7.9")), 7);
        // md5("builder_and_witness")[:8], read as a big-endian integer.
        assert_eq!(
            resolve_seed(Some("builder_and_witness")),
            7_190_654_623_423_626_701
        );
        // Same token, same course.
        assert_eq!(
            resolve_seed(Some("builder_and_witness")),
            resolve_seed(Some("builder_and_witness"))
        );
    }

    #[test]
    fn placement_replies_must_show_real_blocks() {
        let commands = vec![
            "forceload add 0 0 1 1".to_string(),
            "fill 0 0 0 1 1 1 minecraft:air replace".to_string(),
            "setblock 0 0 0 minecraft:glowstone replace".to_string(),
        ];
        let ok = vec![
            "Added chunks".to_string(),
            "Successfully filled 8 blocks".to_string(),
            "Changed the block at 0, 0, 0".to_string(),
        ];
        assert!(replies_ok(&commands, &ok));
        // A fill that placed nothing is the silent no-op this exists to catch.
        let zero = vec![
            "Added chunks".to_string(),
            "Successfully filled 0 blocks".to_string(),
            "Changed the block at 0, 0, 0".to_string(),
        ];
        assert!(!replies_ok(&commands, &zero));
        let unloaded = vec![
            "Added chunks".to_string(),
            "That position is not loaded".to_string(),
            "Changed the block at 0, 0, 0".to_string(),
        ];
        assert!(!replies_ok(&commands, &unloaded));
        // Nothing but setup commands is not a verified build.
        assert!(!replies_ok(&commands[..1], &ok[..1]));
    }

    #[test]
    fn stations_are_distinct_and_on_support() {
        let ring = player_positions("combat_arena", 0, SPAWN, 4);
        assert_eq!(ring.len(), 4);
        assert_eq!(ring[0], (-276.5, 64.0, -674.5));
        for position in &ring {
            assert_eq!(position.0.fract().abs(), 0.5);
            assert_eq!(position.2.fract().abs(), 0.5);
        }
        // gap_jump spreads players across platforms, never stacking two on one coordinate.
        let course = player_positions("gap_jump", 0, SPAWN, 7);
        assert_eq!(course.len(), 7);
        let mut keys: Vec<String> = course.iter().map(|p| format!("{:?}", p)).collect();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), 7);
        assert!(player_positions("none", 0, SPAWN, 4).is_empty());
    }
}
