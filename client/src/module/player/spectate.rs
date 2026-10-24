//! Spectate — watch other players from a detached camera while your own body
//! keeps doing exactly what it was doing.
//!
//! * **Input freeze.** While watching, every keyboard and mouse event is kept
//!   from the game (see `graphic::input`). Minecraft's key bindings therefore
//!   stay in whatever state they were in — a held attack key keeps mining, a
//!   held W keeps running — until Spectate stops and the bindings are re-synced
//!   with the physical keyboard.
//! * **Camera.** The view is moved through the Free Camera API mod (see
//!   [`CameraOverride`]): the player stays the camera entity, so movement,
//!   mining and position packets carry on untouched. The perspective is
//!   switched to third person meanwhile, so no first-person hand floats in
//!   front of the view and your own body is visible.
//! * **Modes.** *Orbit* hovers around the watched player and follows them —
//!   mouse rotates, the wheel zooms, W / S switch to the next / previous
//!   player. *3D* is a free camera confined to a sphere around the watched
//!   player — WASD + Space / Shift fly, mouse looks, the wheel sets fly speed.
//!   J flips between the two; Esc stops spectating.
//!
//! Only players inside entity-tracking range can be watched — the server sends
//! nothing about anyone farther away.
//!
//! The session state lives in a static rather than on the module struct: the
//! GLFW input callbacks, the per-frame camera update, the HUD and the GUI
//! window all reach it, none of them through the module registry.

use crate::graphic::notification::{Notification, NotificationType};
use crate::mapping::compat::free_camera::{self, CameraOverride};
use crate::mapping::entity::player::Player;
use crate::mapping::entity::EntityRef;
use crate::mapping::MappedObject;
use crate::module::{KeyboardKey, Module, ModuleCategory, ModuleData, ModuleId, ModuleSetting};
use crate::state::{client, minecraft};
use jni::objects::GlobalRef;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Instant;

/// How the camera follows the watched player.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Hovers around the player at a mouse-chosen angle and wheel-chosen
    /// distance, following them.
    Orbit,
    /// Free flight within a sphere around the player.
    Free,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Orbit => "Auto",
            Mode::Free => "3D",
        }
    }
}

/// The camera override and what to restore when it ends. Present only while a
/// player is actually being watched.
#[derive(Debug)]
struct Session {
    camera: CameraOverride,
    previous_camera_type: Option<GlobalRef>,
}

/// Movement keys held for 3D flight.
#[derive(Debug, Default, Clone, Copy)]
struct Held {
    forward: bool,
    back: bool,
    left: bool,
    right: bool,
    up: bool,
    down: bool,
}

#[derive(Debug)]
struct State {
    /// Whether the module is running (between `on_start` and `on_stop`).
    running: bool,
    session: Option<Session>,
    /// Name of the watched player.
    target: Option<String>,
    /// Whether the watched player was found among tracked entities last frame.
    target_in_range: bool,
    mode: Mode,
    /// Orbit angles (degrees) and distance (blocks).
    yaw: f32,
    pitch: f32,
    distance: f32,
    /// Smoothed point the orbit camera circles — the player's upper body.
    focus: Option<[f64; 3]>,
    /// The camera position last placed — the 3D mode starts from it.
    camera_pos: [f64; 3],
    /// 3D-mode camera position and look.
    free_pos: [f64; 3],
    free_yaw: f32,
    free_pitch: f32,
    fly_speed: f32,
    held: Held,
    /// Input accumulated by the GLFW callbacks, drained each frame.
    mouse_dx: f64,
    mouse_dy: f64,
    scroll: f64,
    switch_steps: i32,
    last_cursor: Option<(f64, f64)>,
    last_frame: Option<Instant>,
    /// Settings, copied in from the module each tick.
    sensitivity: f32,
    free_radius: f32,
    /// GUI scale, for placing the HUD bar above the hotbar.
    gui_scale: f32,
}

impl State {
    const fn new() -> State {
        State {
            running: false,
            session: None,
            target: None,
            target_in_range: false,
            mode: Mode::Orbit,
            yaw: 0.0,
            pitch: 25.0,
            distance: 6.0,
            focus: None,
            camera_pos: [0.0; 3],
            free_pos: [0.0; 3],
            free_yaw: 0.0,
            free_pitch: 0.0,
            fly_speed: 10.0,
            held: Held {
                forward: false,
                back: false,
                left: false,
                right: false,
                up: false,
                down: false,
            },
            mouse_dx: 0.0,
            mouse_dy: 0.0,
            scroll: 0.0,
            switch_steps: 0,
            last_cursor: None,
            last_frame: None,
            sensitivity: 0.15,
            free_radius: 32.0,
            gui_scale: 2.0,
        }
    }
}

static STATE: Mutex<State> = Mutex::new(State::new());

/// Whether input is currently being withheld from the game — true exactly
/// while a player is being watched. Read by the GLFW callbacks.
static CAPTURING: AtomicBool = AtomicBool::new(false);

const MIN_DISTANCE: f32 = 1.5;
const MAX_DISTANCE: f32 = 48.0;

// --- Public API: input, GUI, HUD ----------------------------------------------

/// Whether Spectate is watching someone and so owns the keyboard and mouse.
pub fn capturing() -> bool {
    CAPTURING.load(Ordering::Relaxed)
}

/// Whether the module is running.
pub fn running() -> bool {
    STATE.lock().map(|state| state.running).unwrap_or(false)
}

/// The watched player's name.
pub fn target() -> Option<String> {
    STATE.lock().ok().and_then(|state| state.target.clone())
}

pub fn mode() -> Mode {
    STATE.lock().map(|state| state.mode).unwrap_or(Mode::Orbit)
}

/// Switches between Auto (orbit) and 3D (free) mode.
pub fn set_mode(mode: Mode) {
    if let Ok(mut state) = STATE.lock() {
        if state.mode == mode {
            return;
        }
        if mode == Mode::Free {
            // Take off from wherever the orbit camera is.
            state.free_pos = state.camera_pos;
            state.free_yaw = state.yaw;
            state.free_pitch = state.pitch;
        }
        state.mode = mode;
        state.held = Held::default();
    }
}

/// Starts watching `name`, enabling the module first if it is off.
pub fn watch(name: &str) {
    if let Ok(mut state) = STATE.lock() {
        if state.target.as_deref() != Some(name) {
            state.focus = None;
        }
        state.target = Some(name.to_string());
    }
    if running() {
        if let Err(error) = begin_session() {
            notify_error(&error);
        }
    } else {
        set_module_enabled(true);
    }
}

/// Stops spectating by disabling the module (as its GUI toggle would).
pub fn stop() {
    set_module_enabled(false);
}

/// What the HUD bar above the hotbar shows.
pub struct HudInfo {
    pub name: String,
    pub in_range: bool,
    pub mode: Mode,
    pub gui_scale: f32,
}

pub fn hud_info() -> Option<HudInfo> {
    let state = STATE.lock().ok()?;
    state.session.as_ref()?;
    Some(HudInfo {
        name: state.target.clone()?,
        in_range: state.target_in_range,
        mode: state.mode,
        gui_scale: state.gui_scale,
    })
}

/// A key event, while [`capturing`]. Called from the GLFW key callback.
pub fn on_key(key: i32, action: i32) {
    const RELEASE: i32 = 0;
    const PRESS: i32 = 1;
    let pressed = action != RELEASE;

    if action == PRESS && key == KeyboardKey::KeyEscape as i32 {
        stop();
        return;
    }
    if action == PRESS && key == KeyboardKey::KeyJ as i32 {
        let next = match mode() {
            Mode::Orbit => Mode::Free,
            Mode::Free => Mode::Orbit,
        };
        set_mode(next);
        return;
    }

    let Ok(mut state) = STATE.lock() else {
        return;
    };
    match state.mode {
        Mode::Orbit => {
            if action == PRESS {
                if key == KeyboardKey::KeyW as i32 {
                    state.switch_steps += 1;
                } else if key == KeyboardKey::KeyS as i32 {
                    state.switch_steps -= 1;
                }
            }
        }
        Mode::Free => {
            let held = &mut state.held;
            match key {
                k if k == KeyboardKey::KeyW as i32 => held.forward = pressed,
                k if k == KeyboardKey::KeyS as i32 => held.back = pressed,
                k if k == KeyboardKey::KeyA as i32 => held.left = pressed,
                k if k == KeyboardKey::KeyD as i32 => held.right = pressed,
                k if k == KeyboardKey::KeySpace as i32 => held.up = pressed,
                k if k == KeyboardKey::KeyLShift as i32 => held.down = pressed,
                _ => {}
            }
        }
    }
}

/// A cursor move, while [`capturing`]. Called from the GLFW cursor callback.
pub fn on_cursor(x: f64, y: f64) {
    let Ok(mut state) = STATE.lock() else {
        return;
    };
    if let Some((last_x, last_y)) = state.last_cursor {
        let (dx, dy) = (x - last_x, y - last_y);
        // A jump this large is the cursor being re-centred (GUI closed), not
        // a real mouse movement.
        if dx.abs() < 400.0 && dy.abs() < 400.0 {
            state.mouse_dx += dx;
            state.mouse_dy += dy;
        }
    }
    state.last_cursor = Some((x, y));
}

/// A scroll-wheel step, while [`capturing`]. Called from the GLFW callback.
pub fn on_scroll(delta: f64) {
    if let Ok(mut state) = STATE.lock() {
        state.scroll += delta;
    }
}

/// Forgets the last cursor position, so the next move is not read as a jump.
pub fn reset_cursor() {
    if let Ok(mut state) = STATE.lock() {
        state.last_cursor = None;
    }
}

/// Places the camera for this frame. Called once per frame from the frame
/// hook, on the game thread.
pub fn update() {
    if !capturing() {
        return;
    }
    if let Err(error) = update_inner() {
        log::warn!("Spectate: frame update failed: {error}");
    }
}

// --- Internals ----------------------------------------------------------------

fn update_inner() -> anyhow::Result<()> {
    let players = other_players()?;
    let gui_scale = minecraft().window.gui_scale().unwrap_or(2.0) as f32;

    let mut state = STATE
        .lock()
        .map_err(|_| anyhow::anyhow!("state poisoned"))?;
    let now = Instant::now();
    let dt = state
        .last_frame
        .map(|last| now.duration_since(last).as_secs_f32().min(0.1))
        .unwrap_or(0.0);
    state.last_frame = Some(now);
    state.gui_scale = gui_scale;

    // W / S in orbit mode: step through the watchable players by name.
    let steps = std::mem::take(&mut state.switch_steps);
    if steps != 0 && !players.is_empty() {
        let current = state
            .target
            .as_ref()
            .and_then(|name| players.iter().position(|(n, _)| n == name));
        let len = players.len() as i32;
        let index = match current {
            Some(i) => (i as i32 + steps).rem_euclid(len),
            None => 0,
        };
        state.target = Some(players[index as usize].0.clone());
        state.focus = None;
    }

    let target = state
        .target
        .as_ref()
        .and_then(|name| players.iter().find(|(n, _)| n == name))
        .map(|(_, player)| player);
    state.target_in_range = target.is_some();

    // The point the camera follows: the watched player's upper body, eased
    // toward each frame so the 20 Hz entity updates read smoothly.
    if let Some(player) = target {
        let feet = player.get_position()?;
        let height = player.bb_height().unwrap_or(1.8) as f64;
        let point = [feet.x(), feet.y() + height * 0.75, feet.z()];
        let blend = 1.0 - (-dt as f64 * 14.0).exp();
        state.focus = Some(match state.focus {
            Some(focus) if distance(focus, point) < 16.0 => lerp3(focus, point, blend),
            _ => point,
        });
    }
    let Some(focus) = state.focus else {
        return Ok(()); // never seen the target yet — leave the view alone
    };

    let (dx, dy) = (
        std::mem::take(&mut state.mouse_dx) as f32,
        std::mem::take(&mut state.mouse_dy) as f32,
    );
    let scroll = std::mem::take(&mut state.scroll) as f32;
    let sensitivity = state.sensitivity;

    let (pos, yaw, pitch) = match state.mode {
        Mode::Orbit => {
            state.yaw = wrap_degrees(state.yaw + dx * sensitivity);
            state.pitch = (state.pitch + dy * sensitivity).clamp(-89.0, 89.0);
            state.distance =
                (state.distance * 0.88_f32.powf(scroll)).clamp(MIN_DISTANCE, MAX_DISTANCE);
            let dir = forward(state.yaw, state.pitch);
            let d = state.distance as f64;
            let pos = [
                focus[0] - dir[0] * d,
                focus[1] - dir[1] * d,
                focus[2] - dir[2] * d,
            ];
            (pos, state.yaw, state.pitch)
        }
        Mode::Free => {
            state.free_yaw = wrap_degrees(state.free_yaw + dx * sensitivity);
            state.free_pitch = (state.free_pitch + dy * sensitivity).clamp(-90.0, 90.0);
            state.fly_speed = (state.fly_speed * 1.15_f32.powf(scroll)).clamp(1.0, 60.0);

            let held = state.held;
            let axis = |plus: bool, minus: bool| (plus as i32 - minus as i32) as f64;
            let yaw_rad = (state.free_yaw as f64).to_radians();
            let flat_forward = [-yaw_rad.sin(), 0.0, yaw_rad.cos()];
            let right = [-yaw_rad.cos(), 0.0, -yaw_rad.sin()];
            let f = axis(held.forward, held.back);
            let r = axis(held.right, held.left);
            let u = axis(held.up, held.down);
            let step = state.fly_speed as f64 * dt as f64;
            let mut pos = state.free_pos;
            for i in 0..3 {
                pos[i] += (flat_forward[i] * f + right[i] * r) * step;
            }
            pos[1] += u * step;

            // Keep the camera within the radius around the watched player.
            let radius = state.free_radius as f64;
            let offset = [pos[0] - focus[0], pos[1] - focus[1], pos[2] - focus[2]];
            let len = (offset[0].powi(2) + offset[1].powi(2) + offset[2].powi(2)).sqrt();
            if len > radius {
                let k = radius / len;
                pos = [
                    focus[0] + offset[0] * k,
                    focus[1] + offset[1] * k,
                    focus[2] + offset[2] * k,
                ];
            }
            state.free_pos = pos;
            (pos, state.free_yaw, state.free_pitch)
        }
    };

    state.camera_pos = pos;
    if let Some(session) = &state.session {
        session.camera.set(pos, yaw, pitch)?;
    }
    Ok(())
}

/// Every tracked player except yourself, with their names, sorted by name.
fn other_players() -> anyhow::Result<Vec<(String, Player)>> {
    let Some(world) = minecraft().world()? else {
        return Ok(Vec::new());
    };
    let me = minecraft().player()?;
    let mut players: Vec<(String, Player)> = world
        .players()?
        .into_iter()
        .filter(|player| !me.as_ref().is_some_and(|me| player.is_same(me)))
        .filter_map(|player| Some((player.get_name().ok()?.get_string().ok()?, player)))
        .collect();
    players.sort_by_key(|(name, _)| name.to_lowercase());
    Ok(players)
}

/// Names of the players that can be watched right now — those in range.
pub fn watchable_players() -> Vec<String> {
    other_players()
        .map(|players| players.into_iter().map(|(name, _)| name).collect())
        .unwrap_or_default()
}

/// Takes over the camera and the input, if not already. Needs a target.
fn begin_session() -> anyhow::Result<()> {
    let mut state = STATE
        .lock()
        .map_err(|_| anyhow::anyhow!("state poisoned"))?;
    if state.session.is_some() || state.target.is_none() {
        return Ok(());
    }
    if !free_camera::is_installed() {
        anyhow::bail!("the Free Camera API mod is not installed");
    }
    let camera = CameraOverride::acquire(true)?;
    let mc = minecraft();
    let previous_camera_type = mc.camera_type().ok();
    if let Ok(third_person) = mc.third_person_camera(false) {
        let _ = mc.set_camera_type(&third_person);
    }
    // Start the orbit behind the player's own facing, looking slightly down.
    if let Some(player) = mc.player()? {
        state.yaw = player.get_yaw().unwrap_or(0.0);
    }
    state.pitch = 25.0;
    state.mode = Mode::Orbit;
    state.held = Held::default();
    state.last_cursor = None;
    state.last_frame = None;
    state.mouse_dx = 0.0;
    state.mouse_dy = 0.0;
    state.scroll = 0.0;
    state.session = Some(Session {
        camera,
        previous_camera_type,
    });
    CAPTURING.store(true, Ordering::Relaxed);
    Ok(())
}

/// Hands the camera and the input back to the game.
fn end_session() {
    CAPTURING.store(false, Ordering::Relaxed);
    let Some(session) = STATE.lock().ok().and_then(|mut state| state.session.take()) else {
        return;
    };
    session.camera.release();
    let mc = minecraft();
    if let Some(kind) = &session.previous_camera_type {
        let _ = mc.set_camera_type(kind);
    }
    // Keys released while the game was not listening would otherwise stay
    // "down" — the player would keep running or mining forever.
    if let Err(error) = mc.resync_key_mappings() {
        log::warn!("Spectate: could not re-sync key bindings: {error}");
    }
    crate::graphic::input::recenter_cursor();
}

/// Enables / disables the module through the registry, exactly like a GUI
/// toggle, and persists the change.
fn set_module_enabled(enabled: bool) {
    let Some(handle) = client().modules.get(ModuleId::Spectate) else {
        return;
    };
    let Ok(mut module) = handle.lock() else {
        return;
    };
    if module.get_module_data().enabled == enabled {
        return;
    }
    module.get_module_data_mut().set_enabled(enabled);
    if client().modules.is_active() {
        let result = if enabled {
            module.on_start()
        } else {
            module.on_stop()
        };
        if let Err(error) = result {
            log::warn!("Spectate: toggle failed: {error}");
        }
    }
    drop(module);
    crate::config::save();
}

fn notify_error(error: &anyhow::Error) {
    Notification::send(NotificationType::Warning, "Spectate", &error.to_string());
}

/// Unit look vector for a Minecraft yaw / pitch (degrees).
fn forward(yaw: f32, pitch: f32) -> [f64; 3] {
    let (yaw, pitch) = ((yaw as f64).to_radians(), (pitch as f64).to_radians());
    [
        -yaw.sin() * pitch.cos(),
        -pitch.sin(),
        yaw.cos() * pitch.cos(),
    ]
}

fn wrap_degrees(angle: f32) -> f32 {
    (angle + 180.0).rem_euclid(360.0) - 180.0
}

fn lerp3(a: [f64; 3], b: [f64; 3], t: f64) -> [f64; 3] {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

fn distance(a: [f64; 3], b: [f64; 3]) -> f64 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

// --- The module -----------------------------------------------------------------

#[derive(Debug)]
pub struct SpectateModule {
    pub module: ModuleData,
}

impl SpectateModule {
    pub fn new() -> Self {
        Self {
            module: ModuleData {
                id: ModuleId::Spectate,
                description: "Watch other players while your body keeps its current action"
                    .to_string(),
                category: ModuleCategory::Player,
                key_bind: KeyboardKey::KeyNone,
                enabled: false,
                settings: vec![
                    ModuleSetting::Slider {
                        name: "Sensitivity".to_string(),
                        value: 0.15,
                        min: 0.03,
                        max: 0.6,
                    },
                    ModuleSetting::Slider {
                        name: "3D radius".to_string(),
                        value: 32.0,
                        min: 8.0,
                        max: 96.0,
                    },
                ],
            },
        }
    }

    /// Copies the setting values into the shared state.
    fn sync_settings(&self) {
        let slider = |name| {
            self.module
                .get_setting(name)
                .and_then(ModuleSetting::get_slider_value)
        };
        if let Ok(mut state) = STATE.lock() {
            if let Some(value) = slider("Sensitivity") {
                state.sensitivity = value;
            }
            if let Some(value) = slider("3D radius") {
                state.free_radius = value;
            }
        }
    }
}

impl Module for SpectateModule {
    fn on_start(&self) -> anyhow::Result<()> {
        self.sync_settings();
        if let Ok(mut state) = STATE.lock() {
            state.running = true;
        }
        if !free_camera::is_installed() {
            let error =
                anyhow::anyhow!("Install the Free Camera API mod (freecameraapi) to spectate");
            notify_error(&error);
            return Err(error);
        }
        // No one chosen (or they left): pick the first player in range.
        let current = target();
        let watchable = watchable_players();
        if current
            .as_ref()
            .is_none_or(|name| !watchable.contains(name))
        {
            match watchable.first() {
                Some(name) => {
                    if let Ok(mut state) = STATE.lock() {
                        state.target = Some(name.clone());
                        state.focus = None;
                    }
                }
                None if current.is_none() => {
                    Notification::send(
                        NotificationType::Info,
                        "Spectate",
                        "No players in range — pick one in the Spectate window",
                    );
                    return Ok(());
                }
                None => {}
            }
        }
        begin_session().inspect_err(notify_error)
    }

    fn on_stop(&self) -> anyhow::Result<()> {
        if let Ok(mut state) = STATE.lock() {
            state.running = false;
        }
        end_session();
        Ok(())
    }

    fn on_tick(&self) -> anyhow::Result<()> {
        self.sync_settings();
        Ok(())
    }

    fn get_module_data(&self) -> &ModuleData {
        &self.module
    }

    fn get_module_data_mut(&mut self) -> &mut ModuleData {
        &mut self.module
    }
}
