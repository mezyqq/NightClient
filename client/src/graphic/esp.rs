//! ESP overlay: 3D wireframe boxes for players, mobs, containers and chosen
//! blocks (the Block ESP scan itself runs in [`block_esp`]).
//!
//! # Performance model
//!
//! The expensive part of an ESP is the JNI traffic — reading every entity's
//! position, type and stats. Doing that per frame is what tanks the FPS, so it
//! is **decoupled from the frame rate**:
//!
//! * [`gather`] (all the JNI work) runs at most ~20 Hz, throttled by wall time.
//! * Every frame only reads the camera (~6 JNI calls) and does pure-CPU
//!   projection + egui drawing.
//! * Positions are interpolated between the last two gathers, so boxes move
//!   smoothly even though the data behind them updates at 20 Hz.
//! * Every JNI scope is wrapped in a local-reference frame: without this the
//!   JVM local-ref table grows unbounded, which is itself a slow FPS killer.
//!
//! The chest scan is heavier (it walks loaded chunks) so it runs even rarer,
//! every [`CHEST_SCAN_INTERVAL`].

use crate::mapping::client::camera::Camera;
use crate::mapping::client::world::World;
use crate::mapping::entity::mob::Mob;
use crate::mapping::entity::player::Player;
use crate::mapping::entity::{Entity, EntityRef, LivingEntityRef};
use crate::mapping::math::Vec3;
use crate::mapping::MappedObject;
use crate::module::misc::friends;
use crate::module::render::block_esp::{self, FoundBlock};
use crate::module::render::trajectories::{self, Path};
use crate::graphic::frame_matrices;
use crate::module::{ModuleId, ModuleSetting};
use crate::state::{client, minecraft};
use egui::{
    pos2, vec2, Align2, Color32, Context, FontId, Id, LayerId, Order, Painter, Pos2, Rect,
    Rounding, Stroke,
};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Minimum wall-time between two entity gathers (≈15 Hz). Interpolation keeps
/// the boxes smooth between gathers, so a low rate costs nothing visually.
const GATHER_INTERVAL: Duration = Duration::from_millis(66);
/// Wall-time between two chest scans — chests do not move, so this is rare.
const CHEST_SCAN_INTERVAL: Duration = Duration::from_secs(2);
/// Half-extent, in chunks, of the area scanned for containers.
const CHEST_CHUNK_RADIUS: i32 = 8;
/// Box edge thickness, in points.
const LINE_WIDTH: f32 = 1.6;

// --- math ------------------------------------------------------------------

/// A plain 3D vector — kept separate from egui/JNI types so the projection
/// math stays allocation- and dependency-free.
#[derive(Debug, Clone, Copy)]
struct V3 {
    x: f64,
    y: f64,
    z: f64,
}

impl V3 {
    fn sub(self, o: V3) -> V3 {
        V3 {
            x: self.x - o.x,
            y: self.y - o.y,
            z: self.z - o.z,
        }
    }

    fn dot(self, o: V3) -> f64 {
        self.x * o.x + self.y * o.y + self.z * o.z
    }

    fn cross(self, o: V3) -> V3 {
        V3 {
            x: self.y * o.z - self.z * o.y,
            y: self.z * o.x - self.x * o.z,
            z: self.x * o.y - self.y * o.x,
        }
    }

    fn lerp(self, o: V3, t: f64) -> V3 {
        V3 {
            x: self.x + (o.x - self.x) * t,
            y: self.y + (o.y - self.y) * t,
            z: self.z + (o.z - self.z) * t,
        }
    }

    fn length(self) -> f64 {
        self.dot(self).sqrt()
    }
}

/// Converts a JNI-snapshot [`Vec3`] into the projection-math vector.
impl From<Vec3> for V3 {
    fn from(v: Vec3) -> V3 {
        V3 {
            x: v.x(),
            y: v.y(),
            z: v.z(),
        }
    }
}

/// A camera, reduced to exactly what world→screen projection needs.
struct View {
    cam: V3,
    fwd: V3,
    right: V3,
    up: V3,
    tan_x: f64,
    tan_y: f64,
    w: f32,
    h: f32,
    /// The game's own `projection × model-view` for this frame, when
    /// captured — see [`frame_matrices`]. Preferred: it includes zoom mods,
    /// camera roll and view bobbing, which the angles above know nothing of.
    matrix: Option<[f64; 16]>,
}

impl View {
    /// Projects a world point to logical screen coordinates, or `None` if it
    /// lies behind the camera (where a perspective divide is meaningless).
    fn project(&self, p: V3) -> Option<Pos2> {
        let r = p.sub(self.cam);
        if let Some(matrix) = &self.matrix {
            let [x, y] = frame_matrices::to_ndc(matrix, [r.x, r.y, r.z])?;
            let sx = (x * 0.5 + 0.5) * self.w as f64;
            let sy = (0.5 - y * 0.5) * self.h as f64;
            return Some(pos2(sx as f32, sy as f32));
        }
        let depth = r.dot(self.fwd);
        if depth < 0.05 {
            return None;
        }
        let ndc_x = (r.dot(self.right) / depth) / self.tan_x;
        let ndc_y = (r.dot(self.up) / depth) / self.tan_y;
        let sx = (ndc_x * 0.5 + 0.5) * self.w as f64;
        let sy = (0.5 - ndc_y * 0.5) * self.h as f64;
        Some(pos2(sx as f32, sy as f32))
    }
}

/// Builds a [`View`] from Minecraft's camera state.
fn build_view(cam: V3, yaw_deg: f32, pitch_deg: f32, fov_deg: f64, w: f32, h: f32) -> View {
    let yaw = (yaw_deg as f64).to_radians();
    let pitch = (pitch_deg as f64).to_radians();
    let (sy, cy) = yaw.sin_cos();
    let (sp, cp) = pitch.sin_cos();

    // Minecraft yaw: 0° faces +Z, increasing clockwise. Pitch: positive looks
    // down. The forward vector follows directly from those conventions.
    let fwd = V3 {
        x: -sy * cp,
        y: -sp,
        z: cy * cp,
    };
    let right = V3 {
        x: -cy,
        y: 0.0,
        z: -sy,
    };
    let up = right.cross(fwd);

    let tan_y = (fov_deg.to_radians() * 0.5).tan();
    let tan_x = tan_y * (w / h) as f64;

    View {
        cam,
        fwd,
        right,
        up,
        tan_x,
        tan_y,
        w,
        h,
        matrix: None,
    }
}

// --- gathered snapshot -----------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetKind {
    Player,
    Mob,
}

/// One living target, as captured by the last [`gather`].
#[derive(Debug, Clone)]
struct EntityTarget {
    id: i32,
    kind: TargetKind,
    /// Position at the previous gather — the start of the interpolation.
    prev: V3,
    /// Position at the latest gather — the end of the interpolation.
    pos: V3,
    width: f64,
    height: f64,
    name: String,
    health: f32,
    max_health: f32,
    /// A player on the Friends list — drawn in the friend color.
    friend: bool,
}

/// One container block, by its lower-corner block coordinates.
#[derive(Debug, Clone, Copy)]
struct ChestTarget {
    pos: V3,
}

/// Cross-frame ESP state: the cached camera handle and the latest snapshot.
struct EspState {
    camera: Option<Camera>,
    entities: Vec<EntityTarget>,
    chests: Vec<ChestTarget>,
    prev_gather: Option<Instant>,
    last_gather: Option<Instant>,
    last_chest_scan: Option<Instant>,
    /// Field of view currently used for projection — eased toward `target_fov`
    /// each frame so flying / sprinting transitions do not snap the boxes.
    fov: f64,
    /// Field of view the game is heading to, refreshed each gather.
    target_fov: f64,
    /// Set once camera resolution has failed, so the failure is logged once.
    camera_logged: bool,
}

impl EspState {
    fn new() -> Self {
        EspState {
            camera: None,
            entities: Vec::new(),
            chests: Vec::new(),
            prev_gather: None,
            last_gather: None,
            last_chest_scan: None,
            fov: 70.0,
            target_fov: 70.0,
            camera_logged: false,
        }
    }
}

fn state() -> &'static Mutex<EspState> {
    static STATE: OnceLock<Mutex<EspState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(EspState::new()))
}

// --- module configuration --------------------------------------------------

/// Per-category drawing options, snapshotted from the modules once per frame.
struct EntityCfg {
    enabled: bool,
    color: Color32,
    show_name: bool,
    show_distance: bool,
    show_health: bool,
    /// Maximum distance, in blocks, an entity is drawn (and processed) at.
    range: f32,
}

struct ChestCfg {
    enabled: bool,
    color: Color32,
    show_distance: bool,
}

/// Tracer-line options, snapshotted from the Tracers module once per frame.
struct TracerCfg {
    enabled: bool,
    color: Color32,
    players: bool,
    mobs: bool,
    /// Maximum distance, in blocks, a tracer is drawn at.
    range: f32,
}

/// Name-tag options, snapshotted from the Nametags module once per frame.
struct NametagCfg {
    enabled: bool,
    color: Color32,
    show_health: bool,
    show_distance: bool,
    /// Maximum distance, in blocks, a name tag is drawn at.
    range: f32,
}

/// Block ESP options, snapshotted from the Block ESP module once per frame.
struct BlockCfg {
    enabled: bool,
    color: Color32,
    /// How many of the nearest found blocks are drawn.
    max_boxes: usize,
}

/// Trajectories options, snapshotted from the module once per frame.
struct TrajectoryCfg {
    enabled: bool,
    color: Color32,
    options: trajectories::Options,
}

struct EspConfig {
    trajectory: TrajectoryCfg,
    player: EntityCfg,
    mob: EntityCfg,
    chest: ChestCfg,
    block: BlockCfg,
    tracer: TracerCfg,
    nametag: NametagCfg,
}

impl EspConfig {
    fn any_enabled(&self) -> bool {
        self.player.enabled
            || self.mob.enabled
            || self.chest.enabled
            || self.block.enabled
            || self.trajectory.enabled
            || self.tracer.enabled
            || self.nametag.enabled
    }

    /// Whether players must be gathered — wanted by Player ESP, by tracers, or
    /// by name tags.
    fn want_players(&self) -> bool {
        self.player.enabled || (self.tracer.enabled && self.tracer.players) || self.nametag.enabled
    }

    /// Whether mobs must be gathered — wanted by Mob ESP or by tracers.
    fn want_mobs(&self) -> bool {
        self.mob.enabled || (self.tracer.enabled && self.tracer.mobs)
    }
}

fn color_setting(data: &crate::module::ModuleData, name: &str, fallback: Color32) -> Color32 {
    match data.get_setting(name) {
        Some(ModuleSetting::Color { value, .. }) => Color32::from_rgba_unmultiplied(
            (value[0] * 255.0) as u8,
            (value[1] * 255.0) as u8,
            (value[2] * 255.0) as u8,
            (value[3] * 255.0).max(40.0) as u8,
        ),
        _ => fallback,
    }
}

fn toggle_setting(data: &crate::module::ModuleData, name: &str, fallback: bool) -> bool {
    data.get_setting(name)
        .and_then(ModuleSetting::get_toggle_value)
        .unwrap_or(fallback)
}

fn slider_setting(data: &crate::module::ModuleData, name: &str, fallback: f32) -> f32 {
    data.get_setting(name)
        .and_then(ModuleSetting::get_slider_value)
        .unwrap_or(fallback)
}

/// Reads the three ESP modules' state with a single lock pass.
fn read_config() -> EspConfig {
    let disabled_entity = || EntityCfg {
        enabled: false,
        color: Color32::WHITE,
        show_name: false,
        show_distance: false,
        show_health: false,
        range: 0.0,
    };
    let mut cfg = EspConfig {
        trajectory: TrajectoryCfg {
            enabled: false,
            color: Color32::WHITE,
            options: trajectories::Options {
                pearls: false,
                throwables: false,
                potions: false,
                bows: false,
                tridents: false,
            },
        },
        player: disabled_entity(),
        mob: disabled_entity(),
        chest: ChestCfg {
            enabled: false,
            color: Color32::WHITE,
            show_distance: false,
        },
        block: BlockCfg {
            enabled: false,
            color: Color32::WHITE,
            max_boxes: 0,
        },
        tracer: TracerCfg {
            enabled: false,
            color: Color32::WHITE,
            players: false,
            mobs: false,
            range: 0.0,
        },
        nametag: NametagCfg {
            enabled: false,
            color: Color32::WHITE,
            show_health: false,
            show_distance: false,
            range: 0.0,
        },
    };

    let modules = &client().modules;

    if let Some(arc) = modules.get(ModuleId::PlayerEsp) {
        if let Ok(module) = arc.lock() {
            let data = module.get_module_data();
            cfg.player = EntityCfg {
                enabled: data.enabled,
                color: color_setting(data, "Color", Color32::from_rgb(255, 70, 70)),
                show_name: toggle_setting(data, "Name", true),
                show_distance: toggle_setting(data, "Distance", true),
                show_health: toggle_setting(data, "Health", true),
                range: slider_setting(data, "Range", 64.0),
            };
        }
    }
    if let Some(arc) = modules.get(ModuleId::MobEsp) {
        if let Ok(module) = arc.lock() {
            let data = module.get_module_data();
            cfg.mob = EntityCfg {
                enabled: data.enabled,
                color: color_setting(data, "Color", Color32::from_rgb(255, 215, 50)),
                show_name: toggle_setting(data, "Name", true),
                show_distance: toggle_setting(data, "Distance", true),
                show_health: toggle_setting(data, "Health", true),
                range: slider_setting(data, "Range", 64.0),
            };
        }
    }
    if let Some(arc) = modules.get(ModuleId::ChestEsp) {
        if let Ok(module) = arc.lock() {
            let data = module.get_module_data();
            cfg.chest = ChestCfg {
                enabled: data.enabled,
                color: color_setting(data, "Color", Color32::from_rgb(255, 140, 30)),
                show_distance: toggle_setting(data, "Distance", true),
            };
        }
    }
    if let Some(arc) = modules.get(ModuleId::Trajectories) {
        if let Ok(module) = arc.lock() {
            let data = module.get_module_data();
            cfg.trajectory = TrajectoryCfg {
                enabled: data.enabled,
                color: color_setting(data, "Color", Color32::from_rgb(190, 90, 255)),
                options: trajectories::Options::from_module(data),
            };
        }
    }
    if let Some(arc) = modules.get(ModuleId::BlockEsp) {
        if let Ok(module) = arc.lock() {
            let data = module.get_module_data();
            cfg.block = BlockCfg {
                enabled: data.enabled,
                color: color_setting(data, "Color", Color32::from_rgb(76, 217, 255)),
                max_boxes: slider_setting(data, "Max boxes", 300.0) as usize,
            };
        }
    }
    if let Some(arc) = modules.get(ModuleId::Tracers) {
        if let Ok(module) = arc.lock() {
            let data = module.get_module_data();
            cfg.tracer = TracerCfg {
                enabled: data.enabled,
                color: color_setting(data, "Color", Color32::from_rgb(100, 200, 255)),
                players: toggle_setting(data, "Players", true),
                mobs: toggle_setting(data, "Mobs", false),
                range: slider_setting(data, "Range", 96.0),
            };
        }
    }
    if let Some(arc) = modules.get(ModuleId::Nametags) {
        if let Ok(module) = arc.lock() {
            let data = module.get_module_data();
            cfg.nametag = NametagCfg {
                enabled: data.enabled,
                color: color_setting(data, "Color", Color32::WHITE),
                show_health: toggle_setting(data, "Health", true),
                show_distance: toggle_setting(data, "Distance", false),
                range: slider_setting(data, "Range", 96.0),
            };
        }
    }

    cfg
}

// --- public entry point ----------------------------------------------------

/// Draws the whole ESP overlay for the current frame.
///
/// Called once per frame from the overlay renderer; cheap when every ESP
/// module is disabled.
pub fn draw(ctx: &Context) {
    let cfg = read_config();
    let mut state = state().lock().unwrap();

    let air_target = crate::module::world::airplace::preview();
    let extras = crate::module::render::item_esp::active() || crate::module::render::hit_effects::wants_world();
    if !cfg.any_enabled() && air_target.is_none() && !extras {
        state.entities.clear();
        state.chests.clear();
        return;
    }

    let now = Instant::now();
    if state.last_gather.is_none_or(|t| now - t >= GATHER_INTERVAL) {
        gather(&mut state, &cfg, now);
    }

    let view = match read_view(&mut state, ctx) {
        Some(view) => view,
        None => return,
    };
    let t = interp_factor(&state, now);

    let painter = ctx.layer_painter(LayerId::new(Order::Background, Id::new("esp_overlay")));

    for entity in &state.entities {
        draw_entity(&painter, &view, entity, t, &cfg);
        if cfg.tracer.enabled {
            draw_tracer(&painter, &view, entity, t, &cfg.tracer);
        }
        if cfg.nametag.enabled {
            draw_nametag(&painter, &view, entity, t, &cfg.nametag);
        }
    }
    for chest in &state.chests {
        draw_chest(&painter, &view, chest, &cfg);
    }
    if cfg.trajectory.enabled {
        if let Some(path) = trajectories::current(cfg.trajectory.options) {
            draw_trajectory(&painter, &view, &path, cfg.trajectory.color);
        }
    }
    if let Some(([x, y, z], [r, g, b, a])) = air_target {
        let min = V3 { x: x as f64, y: y as f64, z: z as f64 };
        let max = V3 { x: min.x + 1.0, y: min.y + 1.0, z: min.z + 1.0 };
        let color = Color32::from_rgba_unmultiplied(
            (r * 255.0) as u8,
            (g * 255.0) as u8,
            (b * 255.0) as u8,
            (a * 255.0) as u8,
        );
        draw_wire_box(&painter, &view, &box_corners(min, max), color);
    }
    // ItemESP and damage numbers draw through the same projection.
    let project = |p: [f64; 3]| view.project(V3 { x: p[0], y: p[1], z: p[2] });
    crate::module::render::item_esp::draw(&painter, &project);
    crate::module::render::hit_effects::draw_world(&painter, &project);
    if cfg.block.enabled {
        block_esp::with_found(|blocks| {
            for block in blocks.iter().take(cfg.block.max_boxes) {
                draw_block(&painter, &view, block, &cfg.block);
            }
        });
    }
}

/// Fraction of the way from the previous gather to the latest one.
fn interp_factor(state: &EspState, now: Instant) -> f64 {
    match (state.prev_gather, state.last_gather) {
        (Some(prev), Some(last)) => {
            let span = (last - prev).as_secs_f64();
            if span <= 1e-4 {
                1.0
            } else {
                ((now - last).as_secs_f64() / span).clamp(0.0, 1.0)
            }
        }
        _ => 1.0,
    }
}

// --- camera ----------------------------------------------------------------

/// Resolves the current camera into a [`View`], caching the [`Camera`] handle.
fn read_view(state: &mut EspState, ctx: &Context) -> Option<View> {
    if state.camera.is_none() {
        match minecraft()
            .game_renderer()
            .and_then(|gr| gr.get_main_camera())
        {
            Ok(camera) => state.camera = Some(camera),
            Err(e) => {
                log::debug!("ESP: camera unavailable: {e}");
                return None;
            }
        }
    }

    let camera = state.camera.clone()?;
    let rect = ctx.screen_rect();
    if rect.width() < 1.0 || rect.height() < 1.0 {
        return None;
    }

    let read = (|| -> anyhow::Result<(V3, f32, f32)> {
        Ok((camera.position()?.into(), camera.yaw()?, camera.pitch()?))
    })();

    let (cam_pos, yaw, pitch) = match read {
        Ok(values) => values,
        Err(error) => {
            if !state.camera_logged {
                state.camera_logged = true;
                log::warn!("ESP: camera read failed: {error}");
            }
            return None;
        }
    };

    let fov_deg = resolve_fov(&camera, state, ctx);

    let mut view = build_view(cam_pos, yaw, pitch, fov_deg, rect.width(), rect.height());
    // The exact camera the level was just drawn with, when available.
    if let Some(captured) = frame_matrices::take() {
        view.matrix = Some(captured.matrix);
        view.cam = V3 {
            x: captured.cam[0],
            y: captured.cam[1],
            z: captured.cam[2],
        };
    }
    Some(view)
}

/// Per-frame FOV resolution.
///
/// Preferred path: read `Camera.fov` directly — the game writes the actually
/// rendered FOV there each frame, already folding in the player modifier, the
/// `fovEffectScale` blend, and the water / lava / dying tweaks. The field
/// only exists on newer builds (it is missing from the obfuscated 1.21.x
/// mappings), so we fall back to recomputing the modifier from options on
/// older versions and ease the result so transitions stay smooth.
fn resolve_fov(camera: &Camera, state: &mut EspState, ctx: &Context) -> f64 {
    if let Ok(fov) = camera.fov() {
        if fov.is_finite() && fov > 0.0 {
            // The cached easing state is no longer authoritative — re-seed it
            // so an unexpected fall back later does not snap from a stale fov.
            let snapped = (fov as f64).clamp(1.0, 179.0);
            state.fov = snapped;
            state.target_fov = snapped;
            return snapped;
        }
    }

    state.target_fov = formula_fov();
    let dt = ctx.input(|input| input.stable_dt).clamp(0.0, 0.1) as f64;
    let blend = 1.0 - 0.5_f64.powf(dt / 0.05);
    state.fov += (state.target_fov - state.fov) * blend;
    state.fov
}

/// Fallback FOV computation, used when [`Camera::fov`] is not exposed by the
/// running build (notably the obfuscated 1.21.x line).
///
/// Mirrors what `Player.getFieldOfViewModifier` does:
/// `lerp(fovEffectScale, 1.0, raw_modifier)`. Reading the blend factor is
/// what fixes ESP misalignment with the "FOV Effects" slider set below 1 —
/// the previous code always applied the full modifier and so drifted off
/// entities whenever the player had the effect dialled down.
fn formula_fov() -> f64 {
    let base = match read_option_fov() {
        Ok(fov) if fov.is_finite() && (1.0..=179.0).contains(&fov) => fov,
        _ => 70.0,
    };
    let raw = fov_modifier_raw();
    let scale = read_fov_effect_scale().unwrap_or(1.0).clamp(0.0, 1.0);
    let effective = 1.0 + (raw - 1.0) * scale;
    (base * effective).clamp(1.0, 179.0)
}

/// The raw FOV multiplier Minecraft would apply, before the `fovEffectScale`
/// blend: ×1.1 while flying, ≈×1.15 while sprinting — the constants from
/// `Player.getFieldOfViewModifier` for a player at vanilla movement speed.
/// (The exact formula also factors in attribute-modified walking speed; the
/// approximation only drifts under Speed/Slowness effects, where the FOV
/// modifier is small to begin with.)
fn fov_modifier_raw() -> f64 {
    let player = match minecraft().player() {
        Ok(Some(player)) => player,
        _ => return 1.0,
    };

    let mut modifier = 1.0;
    if player.abilities.is_flying().unwrap_or(false) {
        modifier *= 1.1;
    }
    if player.is_sprinting().unwrap_or(false) {
        modifier *= 1.15;
    }
    modifier
}

/// Reads the raw FOV slider value from the game options.
fn read_option_fov() -> anyhow::Result<f64> {
    Ok(minecraft().options()?.fov()?.get_int()? as f64)
}

/// Reads the "FOV Effects" accessibility slider (0.0..=1.0).
fn read_fov_effect_scale() -> anyhow::Result<f64> {
    minecraft().options()?.fov_effect_scale()?.get_double()
}

// --- gather ----------------------------------------------------------------

/// Refreshes the snapshot: entities every call, chests on their own schedule.
fn gather(state: &mut EspState, cfg: &EspConfig, now: Instant) {
    state.prev_gather = state.last_gather;
    state.last_gather = Some(now);

    let want_players = cfg.want_players();
    let want_mobs = cfg.want_mobs();
    if want_players || want_mobs {
        let mut range = 0.0_f32;
        if cfg.player.enabled {
            range = range.max(cfg.player.range);
        }
        if cfg.mob.enabled {
            range = range.max(cfg.mob.range);
        }
        if cfg.tracer.enabled {
            range = range.max(cfg.tracer.range);
        }
        if cfg.nametag.enabled {
            range = range.max(cfg.nametag.range);
        }
        let range_sq = (range as f64) * (range as f64);

        let previous = std::mem::take(&mut state.entities);
        match gather_entities(&previous, range_sq, want_players, want_mobs) {
            Ok(list) => state.entities = list,
            Err(e) => log::debug!("ESP: entity gather failed: {e}"),
        }
    } else {
        state.entities.clear();
    }

    if cfg.chest.enabled {
        let due = state
            .last_chest_scan
            .is_none_or(|t| now - t >= CHEST_SCAN_INTERVAL);
        if due {
            state.last_chest_scan = Some(now);
            match gather_chests() {
                Ok(list) => state.chests = list,
                Err(e) => log::debug!("ESP: chest scan failed: {e}"),
            }
        }
    } else {
        state.chests.clear();
        state.last_chest_scan = None;
    }
}

/// Walks `Level.entitiesForRendering()` once, classifying players and mobs.
fn gather_entities(
    previous: &[EntityTarget],
    range_sq: f64,
    want_player: bool,
    want_mob: bool,
) -> anyhow::Result<Vec<EntityTarget>> {
    let mc = minecraft();
    let (Some(world), Some(player)) = (mc.world()?, mc.player()?) else {
        return Ok(Vec::new());
    };

    let local_id = player.id()?;
    let player_pos: V3 = player.get_position()?.into();

    // Carry positions forward so the new snapshot can interpolate from them.
    let prev_pos: HashMap<i32, V3> = previous.iter().map(|e| (e.id, e.pos)).collect();

    let mut out: Vec<EntityTarget> = Vec::new();
    for entity in world.get_entities()? {
        if let Some(target) = process_entity(
            &entity,
            local_id,
            player_pos,
            range_sq,
            want_player,
            want_mob,
            &prev_pos,
        ) {
            out.push(target);
        }
    }

    Ok(out)
}

/// Turns one [`Entity`] into an [`EntityTarget`], or `None` if it is not a
/// wanted target. Errors are swallowed per-field so one bad entity cannot
/// abort the whole gather.
#[allow(clippy::too_many_arguments)]
fn process_entity(
    entity: &Entity,
    local_id: i32,
    player_pos: V3,
    range_sq: f64,
    want_player: bool,
    want_mob: bool,
    prev_pos: &HashMap<i32, V3>,
) -> Option<EntityTarget> {
    // Cheap distance gate first — a far entity then costs just this one JNI
    // call. Skipped entirely if `distanceToSqr` is not exposed by this build.
    if let Ok(dist_sq) = entity.distance_to_sqr(player_pos.x, player_pos.y, player_pos.z) {
        if dist_sq > range_sq {
            return None;
        }
    }

    let kind = if want_player && entity.instance_of::<Player>() {
        TargetKind::Player
    } else if want_mob && entity.instance_of::<Mob>() {
        TargetKind::Mob
    } else {
        return None;
    };

    let id = entity.id().ok()?;
    if id == local_id {
        return None;
    }

    let pos: V3 = entity.get_position().ok()?.into();
    let width = entity.bb_width().ok()? as f64;
    let height = entity.bb_height().ok()? as f64;

    let name = read_name(entity);
    let (health, max_health) = read_health(entity).unwrap_or((0.0, 0.0));

    Some(EntityTarget {
        id,
        kind,
        prev: prev_pos.get(&id).copied().unwrap_or(pos),
        pos,
        width,
        height,
        friend: kind == TargetKind::Player && friends::is_friend(&name),
        name,
        health,
        max_health,
    })
}

/// Reads an entity's display name, truncated to a sane label length.
fn read_name(entity: &Entity) -> String {
    let name = entity
        .get_name()
        .and_then(|component| component.get_string())
        .unwrap_or_default();
    if name.chars().count() > 24 {
        name.chars().take(24).collect()
    } else {
        name
    }
}

/// Reads `(health, maxHealth)`, or `None` if the entity is not living.
fn read_health(entity: &Entity) -> Option<(f32, f32)> {
    let living = entity.as_living()?;
    Some((living.get_health().ok()?, living.get_max_health().ok()?))
}

/// Scans loaded chunks around the player for container block entities.
fn gather_chests() -> anyhow::Result<Vec<ChestTarget>> {
    let mc = minecraft();
    let (Some(world), Some(player)) = (mc.world()?, mc.player()?) else {
        return Ok(Vec::new());
    };

    let player_pos: V3 = player.get_position()?.into();
    let pcx = (player_pos.x / 16.0).floor() as i32;
    let pcz = (player_pos.z / 16.0).floor() as i32;

    let mut out: Vec<ChestTarget> = Vec::new();
    for cx in (pcx - CHEST_CHUNK_RADIUS)..=(pcx + CHEST_CHUNK_RADIUS) {
        for cz in (pcz - CHEST_CHUNK_RADIUS)..=(pcz + CHEST_CHUNK_RADIUS) {
            scan_chunk(&world, cx, cz, &mut out)?;
        }
    }

    Ok(out)
}

/// Adds every container block entity of one chunk to `out`.
fn scan_chunk(world: &World, cx: i32, cz: i32, out: &mut Vec<ChestTarget>) -> anyhow::Result<()> {
    let Some(chunk) = world.get_chunk(cx, cz)? else {
        return Ok(());
    };

    for block_entity in chunk.get_block_entities()? {
        if !block_entity.is_container() {
            continue;
        }
        if let Ok(pos) = block_entity.get_block_pos() {
            out.push(ChestTarget {
                pos: V3 {
                    x: pos.x() as f64,
                    y: pos.y() as f64,
                    z: pos.z() as f64,
                },
            });
        }
    }
    Ok(())
}

// --- drawing ---------------------------------------------------------------

/// The 12 edges of a box, as index pairs into an 8-corner array.
const EDGES: [(usize, usize); 12] = [
    (0, 1),
    (1, 2),
    (2, 3),
    (3, 0), // bottom
    (4, 5),
    (5, 6),
    (6, 7),
    (7, 4), // top
    (0, 4),
    (1, 5),
    (2, 6),
    (3, 7), // verticals
];

/// The 8 corners of an axis-aligned box `[min, max]`.
fn box_corners(min: V3, max: V3) -> [V3; 8] {
    [
        V3 {
            x: min.x,
            y: min.y,
            z: min.z,
        },
        V3 {
            x: max.x,
            y: min.y,
            z: min.z,
        },
        V3 {
            x: max.x,
            y: min.y,
            z: max.z,
        },
        V3 {
            x: min.x,
            y: min.y,
            z: max.z,
        },
        V3 {
            x: min.x,
            y: max.y,
            z: min.z,
        },
        V3 {
            x: max.x,
            y: max.y,
            z: min.z,
        },
        V3 {
            x: max.x,
            y: max.y,
            z: max.z,
        },
        V3 {
            x: min.x,
            y: max.y,
            z: max.z,
        },
    ]
}

/// Draws a wireframe box and returns its 2D screen bounds (for label
/// placement), or `None` if no corner is in front of the camera.
fn draw_wire_box(
    painter: &Painter,
    view: &View,
    corners: &[V3; 8],
    color: Color32,
) -> Option<Rect> {
    let projected: [Option<Pos2>; 8] = std::array::from_fn(|i| view.project(corners[i]));
    let stroke = Stroke::new(LINE_WIDTH, color);

    for &(a, b) in &EDGES {
        if let (Some(pa), Some(pb)) = (projected[a], projected[b]) {
            painter.line_segment([pa, pb], stroke);
        }
    }

    let mut bounds: Option<Rect> = None;
    for point in projected.into_iter().flatten() {
        bounds = Some(match bounds {
            Some(rect) => rect.union(Rect::from_min_max(point, point)),
            None => Rect::from_min_max(point, point),
        });
    }
    bounds
}

fn draw_entity(painter: &Painter, view: &View, entity: &EntityTarget, t: f64, cfg: &EspConfig) {
    let icfg = match entity.kind {
        TargetKind::Player => &cfg.player,
        TargetKind::Mob => &cfg.mob,
    };
    // The entity may have been gathered only for the tracers — skip the box if
    // the matching ESP category is itself disabled.
    if !icfg.enabled {
        return;
    }

    // Friends get their own color while the Friends module is on.
    let friend_color = if entity.friend {
        friends::esp_color().map(|c| {
            Color32::from_rgba_unmultiplied(
                (c[0] * 255.0) as u8,
                (c[1] * 255.0) as u8,
                (c[2] * 255.0) as u8,
                (c[3] * 255.0).max(40.0) as u8,
            )
        })
    } else {
        None
    };
    let color = friend_color.unwrap_or(icfg.color);

    let feet = entity.prev.lerp(entity.pos, t);
    let half = entity.width * 0.5;
    let corners = box_corners(
        V3 {
            x: feet.x - half,
            y: feet.y,
            z: feet.z - half,
        },
        V3 {
            x: feet.x + half,
            y: feet.y + entity.height,
            z: feet.z + half,
        },
    );

    let rect = match draw_wire_box(painter, view, &corners, color) {
        Some(rect) => rect,
        None => return,
    };

    if icfg.show_health && entity.max_health > 0.0 {
        draw_health_bar(painter, rect, entity.health / entity.max_health);
    }
    if icfg.show_name && !entity.name.is_empty() {
        draw_label(
            painter,
            pos2(rect.center().x, rect.top() - 7.0),
            Align2::CENTER_BOTTOM,
            &entity.name,
            color,
        );
    }
    if icfg.show_distance {
        let distance = feet.sub(view.cam).length();
        draw_label(
            painter,
            pos2(rect.center().x, rect.bottom() + 3.0),
            Align2::CENTER_TOP,
            &format!("{distance:.0}m"),
            Color32::from_rgb(214, 216, 224),
        );
    }
}

/// Draws a line from the bottom-centre of the screen to one entity. Skips the
/// entity if its kind is not wanted, it lies past the tracer range, or its
/// centre is behind the camera.
fn draw_tracer(painter: &Painter, view: &View, entity: &EntityTarget, t: f64, cfg: &TracerCfg) {
    let want = match entity.kind {
        TargetKind::Player => cfg.players,
        TargetKind::Mob => cfg.mobs,
    };
    if !want {
        return;
    }

    let feet = entity.prev.lerp(entity.pos, t);
    let center = V3 {
        x: feet.x,
        y: feet.y + entity.height * 0.5,
        z: feet.z,
    };
    if center.sub(view.cam).length() > cfg.range as f64 {
        return;
    }

    let Some(target) = view.project(center) else {
        return;
    };
    let origin = pos2(view.w * 0.5, view.h);
    painter.line_segment([origin, target], Stroke::new(LINE_WIDTH, cfg.color));
}

/// Draws a floating name tag above one player — a background panel with the
/// player's name, optionally their health and distance. Skips non-players,
/// nameless entities, those past the range, or any behind the camera.
fn draw_nametag(painter: &Painter, view: &View, entity: &EntityTarget, t: f64, cfg: &NametagCfg) {
    if entity.kind != TargetKind::Player || entity.name.is_empty() {
        return;
    }

    let feet = entity.prev.lerp(entity.pos, t);
    let head = V3 {
        x: feet.x,
        y: feet.y + entity.height + 0.45,
        z: feet.z,
    };
    if head.sub(view.cam).length() > cfg.range as f64 {
        return;
    }
    let Some(anchor) = view.project(head) else {
        return;
    };

    let mut label = entity.name.clone();
    if cfg.show_health && entity.max_health > 0.0 {
        label.push_str(&format!("  {:.0}\u{2764}", entity.health.max(0.0)));
    }
    if cfg.show_distance {
        label.push_str(&format!("  {:.0}m", feet.sub(view.cam).length()));
    }

    let galley = painter.layout_no_wrap(label, FontId::proportional(12.0), cfg.color);
    let pad = vec2(5.0, 2.5);
    let size = galley.size() + pad * 2.0;
    // The panel sits centred on, and just above, the anchor point.
    let rect = Rect::from_min_size(pos2(anchor.x - size.x * 0.5, anchor.y - size.y), size);
    painter.rect_filled(rect, Rounding::same(3.0), Color32::from_black_alpha(180));
    painter.galley(rect.min + pad, galley, cfg.color);
}

fn draw_chest(painter: &Painter, view: &View, chest: &ChestTarget, cfg: &EspConfig) {
    let corners = box_corners(
        chest.pos,
        V3 {
            x: chest.pos.x + 1.0,
            y: chest.pos.y + 1.0,
            z: chest.pos.z + 1.0,
        },
    );
    let rect = match draw_wire_box(painter, view, &corners, cfg.chest.color) {
        Some(rect) => rect,
        None => return,
    };

    if cfg.chest.show_distance {
        let center = V3 {
            x: chest.pos.x + 0.5,
            y: chest.pos.y + 0.5,
            z: chest.pos.z + 0.5,
        };
        let distance = center.sub(view.cam).length();
        draw_label(
            painter,
            pos2(rect.center().x, rect.bottom() + 3.0),
            Align2::CENTER_TOP,
            &format!("{distance:.0}m"),
            cfg.chest.color,
        );
    }
}

fn draw_trajectory(painter: &Painter, view: &View, path: &Path, color: Color32) {
    let stroke = Stroke::new(LINE_WIDTH, color);
    let projected: Vec<Option<Pos2>> = path
        .points
        .iter()
        .map(|p| view.project(V3 { x: p[0], y: p[1], z: p[2] }))
        .collect();
    let on_screen = Rect::from_min_size(Pos2::ZERO, vec2(view.w, view.h));
    let visible = projected.iter().flatten().filter(|p| on_screen.contains(**p)).count();
    trajectories::report("draw", format!(
        "{} of the path on screen",
        if visible == 0 { "none" } else { "part" },
    ));
    for pair in projected.windows(2) {
        if let [Some(a), Some(b)] = pair {
            painter.line_segment([*a, *b], stroke);
        }
    }
    // A small box marks the landing point.
    if let (true, Some(end)) = (path.hit, path.points.last()) {
        let r = 0.25;
        let min = V3 { x: end[0] - r, y: end[1] - r, z: end[2] - r };
        let max = V3 { x: end[0] + r, y: end[1] + r, z: end[2] + r };
        draw_wire_box(painter, view, &box_corners(min, max), color);
    }
}

fn draw_block(painter: &Painter, view: &View, block: &FoundBlock, cfg: &BlockCfg) {
    let min = V3 {
        x: block.pos[0] as f64,
        y: block.pos[1] as f64,
        z: block.pos[2] as f64,
    };
    let max = V3 {
        x: min.x + 1.0,
        y: min.y + 1.0,
        z: min.z + 1.0,
    };
    draw_wire_box(painter, view, &box_corners(min, max), cfg.color);
}

/// A thin health bar straddling the top edge of `rect`, red→yellow→green.
fn draw_health_bar(painter: &Painter, rect: Rect, fraction: f32) {
    let fraction = fraction.clamp(0.0, 1.0);
    let top = rect.top() - 4.0;
    let bottom = rect.top() - 1.5;
    let background = Rect::from_min_max(pos2(rect.left(), top), pos2(rect.right(), bottom));
    painter.rect_filled(background, Rounding::ZERO, Color32::from_black_alpha(190));

    let fill_width = rect.width() * fraction;
    let fill = Rect::from_min_max(
        pos2(rect.left(), top),
        pos2(rect.left() + fill_width, bottom),
    );
    painter.rect_filled(fill, Rounding::ZERO, health_color(fraction));
}

/// Interpolates red → yellow → green across `0..=1`.
fn health_color(fraction: f32) -> Color32 {
    let (r, g) = if fraction < 0.5 {
        (255.0, 255.0 * (fraction * 2.0))
    } else {
        (255.0 * (1.0 - (fraction - 0.5) * 2.0), 255.0)
    };
    Color32::from_rgb(r as u8, g as u8, 60)
}

/// Draws text with a 1px drop shadow so it stays readable over any backdrop.
fn draw_label(painter: &Painter, pos: Pos2, anchor: Align2, text: &str, color: Color32) {
    let font = FontId::proportional(11.0);
    painter.text(
        pos + vec2(0.8, 0.8),
        anchor,
        text,
        font.clone(),
        Color32::from_black_alpha(210),
    );
    painter.text(pos, anchor, text, font, color);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v3(x: f64, y: f64, z: f64) -> V3 {
        V3 { x, y, z }
    }

    #[test]
    fn v3_length_and_dot() {
        let a = v3(3.0, 4.0, 0.0);
        assert_eq!(a.length(), 5.0);
        assert_eq!(a.dot(a), 25.0);
    }

    #[test]
    fn v3_cross_of_x_and_y_is_z() {
        let z = v3(1.0, 0.0, 0.0).cross(v3(0.0, 1.0, 0.0));
        assert!(z.sub(v3(0.0, 0.0, 1.0)).length() < 1e-9);
    }

    #[test]
    fn v3_lerp_finds_the_midpoint() {
        let m = v3(0.0, 0.0, 0.0).lerp(v3(10.0, 20.0, -4.0), 0.5);
        assert_eq!((m.x, m.y, m.z), (5.0, 10.0, -2.0));
    }

    #[test]
    fn a_point_dead_ahead_projects_to_the_screen_centre() {
        // Camera at the origin, yaw/pitch 0 -> looking toward +Z.
        let view = build_view(v3(0.0, 0.0, 0.0), 0.0, 0.0, 70.0, 1920.0, 1080.0);
        let projected = view
            .project(v3(0.0, 0.0, 10.0))
            .expect("a point straight ahead must project");
        assert!((projected.x - 960.0).abs() < 1.0);
        assert!((projected.y - 540.0).abs() < 1.0);
    }

    #[test]
    fn a_point_behind_the_camera_does_not_project() {
        let view = build_view(v3(0.0, 0.0, 0.0), 0.0, 0.0, 70.0, 1920.0, 1080.0);
        assert!(view.project(v3(0.0, 0.0, -10.0)).is_none());
    }

    #[test]
    fn box_corners_span_min_to_max() {
        let corners = box_corners(v3(0.0, 0.0, 0.0), v3(1.0, 2.0, 3.0));
        assert_eq!(corners.len(), 8);
        assert_eq!((corners[0].x, corners[0].y, corners[0].z), (0.0, 0.0, 0.0));
        assert_eq!((corners[6].x, corners[6].y, corners[6].z), (1.0, 2.0, 3.0));
    }
}
