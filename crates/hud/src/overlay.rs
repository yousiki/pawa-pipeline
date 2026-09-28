//! The HUD panel itself: one [`FrameRow`] in, one SVG document out.
//!
//! The panel is drawn twice per frame, as two layers over one shared geometry:
//!
//! * [`static_layer`] — the panel background and everything driven by the *action state*: the WASD
//!   keycaps, the modifier keys, the discrete-action badges, the hotbar and held item. Two frames
//!   that hold the same keys produce byte-identical SVG, which is what makes the caller's pixmap
//!   cache worth having (a bot holds `forward` for hundreds of consecutive frames).
//! * [`dynamic_layer`] — everything that changes every single frame: the camera-motion indicator and
//!   the status line. Kept out of the static layer on purpose; folding a monotonically increasing
//!   frame index into a cache key is the same as having no cache.
//!
//! Both layers are the same [`PANEL_W`] × [`PANEL_H`] canvas and are composited in order, so the
//! dynamic layer draws over the static panel background it does not itself paint.
//!
//! Coordinates are in the panel's own logical units throughout. The caller picks the pixel size and
//! resvg scales; nothing here knows the video resolution.

use std::fmt::Write as _;

use pawa_schema::dataset::{FrameRow, SolarisAction};

/// Logical width of the panel canvas. The pixel width the caller rasterizes at is independent.
pub const PANEL_W: f32 = 360.0;
/// Logical height of the panel canvas.
pub const PANEL_H: f32 = 216.0;

/// Accent for a row that carries an action.
const ACCENT: &str = "#60a5fa";
/// Accent for a row whose `action` is `None`. Every lit element goes dead with it, so a coverage
/// hole reads as a hole at a glance rather than as an all-keys-released frame.
const ACCENT_DEAD: &str = "#6b7280";
/// Amber, used only for the `NO ACTION` tag. Nothing else on the panel is this colour.
const WARN: &str = "#f59e0b";

const PANEL_FILL: &str = "#070a12";
const KEY_FILL: &str = "#1f2330";
const KEY_STROKE: &str = "#5f6776";
const KEY_TEXT: &str = "#dae2ec";
/// Text colour on a *lit* key — dark, because the key itself is now bright.
const KEY_TEXT_ON: &str = "#0a1018";
const MUTED: &str = "#b0bac8";

/// Font stack. resvg resolves these against the system font database; the last entry is the generic
/// family, so a machine with neither DejaVu nor Liberation still renders text rather than nothing.
const FONT: &str = "DejaVu Sans,Liberation Sans,Arial,sans-serif";

/// Fallback full-scale deflection for the camera indicator, degrees per frame, used when a
/// `frames.jsonl` contains no camera motion at all to calibrate against.
///
/// 3.6 deg/frame is 72 deg/s of yaw at 20 FPS — the capture runtime's default `yaw_rate_deg_s`.
/// The indicator is normalised by the *largest step actually present in this POV* (see
/// [`camera_full_scale`]) precisely because that default is configurable: a fixed pixels-per-degree
/// gain made a full-rate turn deflect a quarter of the radius and read as "the dot never moves".
pub const DEFAULT_CAM_FULL_SCALE_DEG: f64 = 3.6;

/// Per-frame camera deltas for display, in notchian DEGREES — the v3 row stores
/// `action.camera` as mineflayer radians (sign-flipped), and the dial's viewer thinks in the
/// game's own convention.
fn camera_deg(action: &SolarisAction) -> (f64, f64) {
    (
        -action.camera[0].to_degrees(),
        -action.camera[1].to_degrees(),
    )
}

/// The largest per-frame camera step in this POV, in degrees — the deflection that should land the
/// indicator dot exactly on its reference ring.
///
/// This is the instrument's calibration knob, and it has to be measured rather than assumed: the
/// turn rate is a runtime setting, and a HUD normalised to the wrong one is legible but lying about
/// magnitude. Falls back to [`DEFAULT_CAM_FULL_SCALE_DEG`] when the POV never turned.
pub fn camera_full_scale(rows: &[FrameRow]) -> f64 {
    let max = rows
        .iter()
        .filter_map(|row| row.action.as_ref())
        .map(|a| {
            let (d_yaw, d_pitch) = camera_deg(a);
            d_yaw.abs().max(d_pitch.abs())
        })
        .fold(0.0f64, f64::max);
    if max > 1e-3 {
        max
    } else {
        DEFAULT_CAM_FULL_SCALE_DEG
    }
}

/// Escape the five XML metacharacters. `held_item` is an open vocabulary that reaches us from the
/// game; a name containing `&` must not be able to produce a document that fails to parse.
fn esc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

/// A text run. `anchor` is an SVG `text-anchor` value; `y` is the baseline, not the top, because
/// baselines are the only vertical reference resvg and every other SVG renderer agree on.
#[allow(clippy::too_many_arguments)] // flat SVG primitive; a param struct would be pure ceremony
fn text(
    out: &mut String,
    x: f32,
    y: f32,
    size: f32,
    bold: bool,
    fill: &str,
    anchor: &str,
    s: &str,
) {
    let weight = if bold { "bold" } else { "normal" };
    let _ = write!(
        out,
        r#"<text x="{x}" y="{y}" font-family="{FONT}" font-size="{size}" font-weight="{weight}" fill="{fill}" text-anchor="{anchor}">{}</text>"#,
        esc(s)
    );
}

/// One keycap: a rounded rect plus a centred label, lit or unlit. The single primitive behind the
/// WASD cluster, the modifier row and the action badges — they differ only in size and label.
#[allow(clippy::too_many_arguments)] // flat SVG primitive; a param struct would be pure ceremony
fn keycap(out: &mut String, x: f32, y: f32, w: f32, h: f32, label: &str, on: bool, accent: &str) {
    let (fill, stroke, ink) = if on {
        (accent, "#ffffff", KEY_TEXT_ON)
    } else {
        (KEY_FILL, KEY_STROKE, KEY_TEXT)
    };
    let opacity = if on { 0.92 } else { 0.82 };
    let _ = write!(
        out,
        r#"<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="{rx}" fill="{fill}" fill-opacity="{opacity}" stroke="{stroke}" stroke-width="1.5"/>"#,
        rx = (h / 6.0).max(3.0)
    );
    // 0.35em below centre puts the cap height of a typical sans face optically in the middle.
    let size = (h * 0.42).clamp(8.0, 16.0);
    text(
        out,
        x + w / 2.0,
        y + h / 2.0 + size * 0.35,
        size,
        true,
        ink,
        "middle",
        label,
    );
}

// ---- layout ----
//
// Every magic number the two layers share lives here, so the dynamic layer can draw into the space
// the static layer leaves for it without either one guessing.

const PAD: f32 = 12.0;
const KEY: f32 = 28.0;
const GAP: f32 = 4.0;
/// Top-left of the WASD cluster (the `W` row).
const WASD_X: f32 = PAD;
const WASD_Y: f32 = 28.0;
/// Left edge of the right-hand column: badges above, modifiers below.
const COL2_X: f32 = 114.0;
const BADGE_H: f32 = 26.0;
const HOTBAR_Y: f32 = 96.0;
const HOTBAR_SLOT: f32 = 16.0;
const HOTBAR_GAP: f32 = 3.0;
/// Centre of the camera-motion dial, and its outer radius.
const DIAL_CX: f32 = 298.0;
const DIAL_CY: f32 = 152.0;
const DIAL_R: f32 = 34.0;
/// Baseline of the bottom status line.
const STATUS_Y: f32 = 206.0;

/// The state-driven half of the panel, including its background. Reads only the row's action
/// state (`action`, `inventory.quickBarSlot`, `held_item`), never a per-frame counter — that is
/// what makes the caller's cache work.
///
/// The returned string is used verbatim as the caller's cache key: two frames whose action state
/// matches produce the same bytes and therefore the same pixmap. That makes the key impossible to
/// drift out of sync with the drawing, which a hand-written key struct would eventually do.
pub fn static_layer(row: &FrameRow) -> String {
    let action = row.action.as_ref();
    let accent = if action.is_some() {
        ACCENT
    } else {
        ACCENT_DEAD
    };
    let mut out = String::with_capacity(3072);
    let _ = write!(
        out,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{PANEL_W}" height="{PANEL_H}" viewBox="0 0 {PANEL_W} {PANEL_H}">"#
    );
    let _ = write!(
        out,
        r#"<rect x="1" y="1" width="{w}" height="{h}" rx="12" fill="{PANEL_FILL}" fill-opacity="0.72" stroke="{accent}" stroke-width="2" stroke-opacity="0.85"/>"#,
        w = PANEL_W - 2.0,
        h = PANEL_H - 2.0
    );

    match action {
        Some(_) => text(&mut out, PAD, 19.0, 11.0, true, accent, "start", "ACTION"),
        // The whole point of the instrument: say where action coverage stops, loudly, on the frame
        // itself. A greyed panel alone could be misread as "all keys released".
        None => text(&mut out, PAD, 19.0, 11.0, true, WARN, "start", "NO ACTION"),
    }

    let keys = action.map(|a| {
        (
            a.forward, a.back, a.left, a.right, a.jump, a.sprint, a.sneak,
        )
    });
    let (forward, back, left, right, jump, sprint, sneak) =
        keys.unwrap_or((false, false, false, false, false, false, false));
    // Cluster: W on top, A S D beneath.
    keycap(
        &mut out,
        WASD_X + KEY + GAP,
        WASD_Y,
        KEY,
        KEY,
        "W",
        forward,
        accent,
    );
    let row2 = WASD_Y + KEY + GAP;
    keycap(&mut out, WASD_X, row2, KEY, KEY, "A", left, accent);
    keycap(
        &mut out,
        WASD_X + KEY + GAP,
        row2,
        KEY,
        KEY,
        "S",
        back,
        accent,
    );
    keycap(
        &mut out,
        WASD_X + 2.0 * (KEY + GAP),
        row2,
        KEY,
        KEY,
        "D",
        right,
        accent,
    );

    // Discrete-action badges, top of the right column.
    let badges = [
        ("ATTACK", action.is_some_and(|a| a.attack)),
        ("USE", action.is_some_and(|a| a.use_item)),
        ("MINE", action.is_some_and(|a| a.mine)),
        ("PLACE", action.is_some_and(|a| a.place_block)),
    ];
    for (i, (label, on)) in badges.iter().enumerate() {
        let x = COL2_X + i as f32 * 59.0;
        keycap(&mut out, x, WASD_Y, 54.0, BADGE_H, label, *on, accent);
    }

    // Modifier keys, beneath the badges and level with the A/S/D row.
    let mods = [
        ("SPACE", jump, 62.0),
        ("SPRINT", sprint, 60.0),
        ("SNEAK", sneak, 56.0),
    ];
    let mut x = COL2_X;
    for (label, on, w) in mods {
        keycap(&mut out, x, row2, w, BADGE_H, label, on, accent);
        x += w + 5.0;
    }

    // Hotbar: nine slots, the selected one lit. The v3 row keeps the slot STATE in
    // `inventory.quickBarSlot` (the `action["hotbar.N"]` keys are switch edges, useless for
    // display); a row without an inventory lights nothing rather than inventing a fact.
    let slot = row
        .inventory
        .as_ref()
        .map(|inventory| inventory.quick_bar_slot);
    for i in 0..9u8 {
        let sx = PAD + f32::from(i) * (HOTBAR_SLOT + HOTBAR_GAP);
        let on = slot == Some(i);
        let (fill, stroke, ink) = if on {
            (accent, "#ffffff", KEY_TEXT_ON)
        } else {
            (KEY_FILL, KEY_STROKE, KEY_TEXT)
        };
        let _ = write!(
            out,
            r#"<rect x="{sx}" y="{HOTBAR_Y}" width="{HOTBAR_SLOT}" height="{HOTBAR_SLOT}" rx="3" fill="{fill}" fill-opacity="0.8" stroke="{stroke}" stroke-width="1"/>"#
        );
        text(
            &mut out,
            sx + HOTBAR_SLOT / 2.0,
            HOTBAR_Y + HOTBAR_SLOT / 2.0 + 3.0,
            8.0,
            true,
            ink,
            "middle",
            &(i + 1).to_string(),
        );
    }
    let held = row
        .held_item
        .as_deref()
        // `minecraft:` prefixes every vanilla id and costs a third of the line for no information.
        .map(|item| item.strip_prefix("minecraft:").unwrap_or(item))
        .map(|item| item.chars().take(26).collect::<String>())
        .unwrap_or_else(|| "-".into());
    text(
        &mut out,
        PAD,
        124.0,
        10.0,
        false,
        MUTED,
        "start",
        &format!("held: {held}"),
    );

    out.push_str("</svg>");
    out
}

/// The per-frame half: the camera dial and the status line. Transparent everywhere else, so it
/// composites straight over the static panel.
///
/// `full_scale_deg` is the deflection that reaches the reference ring; see [`camera_full_scale`].
pub fn dynamic_layer(row: &FrameRow, full_scale_deg: f64) -> String {
    let accent = if row.action.is_some() {
        ACCENT
    } else {
        ACCENT_DEAD
    };
    let (d_yaw, d_pitch) = row.action.as_ref().map(camera_deg).unwrap_or((0.0, 0.0));

    let mut out = String::with_capacity(1024);
    let _ = write!(
        out,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{PANEL_W}" height="{PANEL_H}" viewBox="0 0 {PANEL_W} {PANEL_H}">"#
    );

    // Dial: outer rim, crosshair, and the reference ring one full-scale step out.
    let ring = DIAL_R - 5.0;
    let _ = write!(
        out,
        r#"<circle cx="{DIAL_CX}" cy="{DIAL_CY}" r="{DIAL_R}" fill="none" stroke="{accent}" stroke-width="2" stroke-opacity="0.85"/>
<circle cx="{DIAL_CX}" cy="{DIAL_CY}" r="{ring}" fill="none" stroke="{MUTED}" stroke-width="1" stroke-opacity="0.35"/>
<path d="M{l} {DIAL_CY} H{r} M{DIAL_CX} {t} V{b}" stroke="{MUTED}" stroke-width="1" stroke-opacity="0.5"/>"#,
        l = DIAL_CX - DIAL_R + 6.0,
        r = DIAL_CX + DIAL_R - 6.0,
        t = DIAL_CY - DIAL_R + 6.0,
        b = DIAL_CY + DIAL_R - 6.0,
    );
    let gain = ring as f64 / full_scale_deg.max(1e-6);
    let dx = (d_yaw * gain).clamp(-ring as f64, ring as f64) as f32;
    let dy = (d_pitch * gain).clamp(-ring as f64, ring as f64) as f32;
    let _ = write!(
        out,
        r#"<path d="M{DIAL_CX} {DIAL_CY} L{x} {y}" stroke="{accent}" stroke-width="3" stroke-linecap="round"/>
<circle cx="{x}" cy="{y}" r="3.5" fill="{accent}"/>"#,
        x = DIAL_CX + dx,
        y = DIAL_CY + dy,
    );

    text(
        &mut out,
        PAD,
        146.0,
        10.0,
        false,
        KEY_TEXT,
        "start",
        &format!("cam dyaw {d_yaw:+.2}deg"),
    );
    text(
        &mut out,
        PAD,
        160.0,
        10.0,
        false,
        KEY_TEXT,
        "start",
        &format!("    dpitch {d_pitch:+.2}deg"),
    );
    text(
        &mut out,
        PAD,
        174.0,
        9.0,
        false,
        MUTED,
        "start",
        &format!("ring = {full_scale_deg:.2}deg/frame"),
    );

    // Status line: the two clocks that make this video checkable against the dataset, plus the
    // player's own position when the row carries one.
    let mut status = format!(
        "frame {} | world {}",
        row.frame_count,
        row.world_age_ticks.get()
    );
    if let (Some(x), Some(y), Some(z)) = (row.x, row.y, row.z) {
        let _ = write!(status, " | pos {x:.1},{y:.1},{z:.1}");
    }
    text(
        &mut out, PAD, STATUS_Y, 10.0, false, MUTED, "start", &status,
    );

    out.push_str("</svg>");
    out
}
