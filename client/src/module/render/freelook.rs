//! Freelook — look around your character without turning it. The mouse
//! orbits a third-person camera around the player while the body keeps its
//! facing, so you can run one way and watch another. The camera distance is
//! the "Distance" setting.
//!
//! Built on the Free Camera API mod, like Spectate: the camera is placed
//! every frame relative to the player, who stays the camera entity. Mouse movement is kept
//! from the game while active (see `graphic::input`); keys are not, so you
//! keep walking, jumping and mining as usual.

use crate::graphic::notification::{Notification, NotificationType};
use crate::mapping::compat::free_camera::{self, CameraOverride};
use crate::mapping::entity::EntityRef;
use crate::mapping::raw;
use crate::mapping::MappedObject;
use crate::module::registry::request_disable;
use crate::module::{KeyboardKey, Module, ModuleCategory, ModuleData, ModuleId, ModuleSetting};
use crate::state::{mapping, minecraft};
use jni::objects::GlobalRef;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

const MIN_DISTANCE: f32 = 1.5;
const MAX_DISTANCE: f32 = 12.0;

#[derive(Debug)]
struct Session {
    camera: CameraOverride,
    previous_camera_type: Option<GlobalRef>,
    yaw: f32,
    pitch: f32,
    distance: f32,
    /// Degrees per pixel of mouse movement — the game's own sensitivity.
    sensitivity: f32,
    last_cursor: Option<(f64, f64)>,
    mouse_dx: f64,
    mouse_dy: f64,
}

static SESSION: Mutex<Option<Session>> = Mutex::new(None);
static CAPTURING: AtomicBool = AtomicBool::new(false);

/// Whether the mouse currently steers the freelook camera.
pub fn capturing() -> bool {
    CAPTURING.load(Ordering::Relaxed)
}

/// A cursor move while [`capturing`]. Called from the GLFW cursor callback.
pub fn on_cursor(x: f64, y: f64) {
    let mut guard = SESSION.lock().unwrap_or_else(|p| p.into_inner());
    let Some(session) = guard.as_mut() else {
        return;
    };
    if let Some((last_x, last_y)) = session.last_cursor {
        let (dx, dy) = (x - last_x, y - last_y);
        // A big jump is the cursor being re-centred, not a real movement.
        if dx.abs() < 400.0 && dy.abs() < 400.0 {
            session.mouse_dx += dx;
            session.mouse_dy += dy;
        }
    }
    session.last_cursor = Some((x, y));
}

/// Forgets the last cursor position, so the next move is not read as a jump.
pub fn reset_cursor() {
    if let Some(session) = SESSION.lock().unwrap_or_else(|p| p.into_inner()).as_mut() {
        session.last_cursor = None;
    }
}

/// Places the camera for the next frame. Called every frame from the hook.
pub fn update() {
    if !capturing() {
        return;
    }
    if let Err(error) = update_inner() {
        log::debug!("Freelook: {error}");
    }
}

fn update_inner() -> anyhow::Result<()> {
    let Some(player) = minecraft().player()? else {
        return Ok(());
    };
    let mut guard = SESSION.lock().unwrap_or_else(|p| p.into_inner());
    let Some(session) = guard.as_mut() else {
        return Ok(());
    };
    let (dx, dy) = (std::mem::take(&mut session.mouse_dx), std::mem::take(&mut session.mouse_dy));
    session.yaw = (session.yaw + dx as f32 * session.sensitivity + 180.0).rem_euclid(360.0) - 180.0;
    session.pitch = (session.pitch + dy as f32 * session.sensitivity).clamp(-89.0, 89.0);

    let body_yaw = player.get_yaw()?;
    let eye_height = eye_height(&player).unwrap_or(1.62);
    let (offset, yaw) = relative_orbit(session.yaw, session.pitch, session.distance, body_yaw, eye_height);
    session.camera.set(offset, yaw, session.pitch)
}

/// The orbit camera in the Free Camera API's relative mode: the offset from
/// the player's feet in the body's frame (the mod rotates it back by the body
/// yaw and adds the feet interpolated at render time, so the camera never lags
/// the player by a frame), and the yaw relative to the body.
fn relative_orbit(yaw: f32, pitch: f32, distance: f32, body_yaw: f32, eye_height: f64) -> ([f64; 3], f32) {
    let (y, p) = ((yaw as f64).to_radians(), (pitch as f64).to_radians());
    let d = distance as f64;
    let world = [y.sin() * p.cos() * d, eye_height + p.sin() * d, -y.cos() * p.cos() * d];
    // The mod applies JOML `rotateY(-body_yaw)`; this is its inverse.
    let b = ((body_yaw % 360.0) as f64).to_radians();
    let local = [
        world[0] * b.cos() + world[2] * b.sin(),
        world[1],
        -world[0] * b.sin() + world[2] * b.cos(),
    ];
    (local, yaw - body_yaw)
}

fn eye_height(player: &crate::mapping::entity::player::LocalPlayer) -> anyhow::Result<f64> {
    mapping().in_frame(|| {
        let mut env = mapping().get_env()?;
        Ok(raw::call(&mut env, player.jni_ref().as_obj(), "getEyeHeight", "()F", &[])?.f()? as f64)
    })
}

/// Degrees of camera turn per pixel, from the game's mouse sensitivity —
/// the same formula `MouseHandler.turnPlayer` uses.
fn sensitivity() -> f32 {
    let option = minecraft()
        .options()
        .and_then(|options| options.sensitivity())
        .and_then(|value| value.get_double())
        .unwrap_or(0.5);
    let f = option * 0.6 + 0.2;
    (f * f * f * 8.0 * 0.15) as f32
}

#[derive(Debug)]
pub struct FreelookModule {
    pub module: ModuleData,
}

impl FreelookModule {
    pub fn new() -> Self {
        Self {
            module: ModuleData {
                id: ModuleId::Freelook,
                description: "Look around without turning your character".to_string(),
                category: ModuleCategory::Render,
                key_bind: KeyboardKey::KeyNone,
                enabled: false,
                settings: vec![ModuleSetting::Slider {
                    name: "Distance".to_string(),
                    value: 4.0,
                    min: MIN_DISTANCE,
                    max: MAX_DISTANCE,
                }],
            },
        }
    }

    fn begin(&self) -> anyhow::Result<()> {
        if crate::module::player::spectate::capturing() {
            anyhow::bail!("Spectate is using the camera");
        }
        if !free_camera::is_installed() {
            anyhow::bail!("Needs the Free Camera API mod");
        }
        let Some(player) = minecraft().player()? else {
            return Ok(());
        };
        let camera = CameraOverride::acquire(false)?;
        let mc = minecraft();
        let previous_camera_type = mc.camera_type().ok();
        // Front, not back: third-person mods (Better Third Person) turn the
        // body toward the walking direction in the back view, never mirrored.
        if let Ok(third_person) = mc.third_person_camera(true) {
            let _ = mc.set_camera_type(&third_person);
        }
        let distance = self
            .module
            .get_setting("Distance")
            .and_then(ModuleSetting::get_slider_value)
            .unwrap_or(4.0);
        *SESSION.lock().unwrap_or_else(|p| p.into_inner()) = Some(Session {
            camera,
            previous_camera_type,
            yaw: player.get_yaw()?,
            pitch: player.get_pitch()?,
            distance,
            sensitivity: sensitivity(),
            last_cursor: None,
            mouse_dx: 0.0,
            mouse_dy: 0.0,
        });
        CAPTURING.store(true, Ordering::Relaxed);
        Ok(())
    }
}

impl Module for FreelookModule {
    fn on_start(&self) -> anyhow::Result<()> {
        if let Err(error) = self.begin() {
            Notification::send(NotificationType::Warning, "Freelook", &error.to_string());
            request_disable(ModuleId::Freelook);
        }
        Ok(())
    }

    fn on_stop(&self) -> anyhow::Result<()> {
        CAPTURING.store(false, Ordering::Relaxed);
        let Some(session) = SESSION.lock().unwrap_or_else(|p| p.into_inner()).take() else {
            return Ok(());
        };
        session.camera.release();
        if let Some(kind) = &session.previous_camera_type {
            let _ = minecraft().set_camera_type(kind);
        }
        crate::graphic::input::recenter_cursor();
        Ok(())
    }

    fn on_tick(&self) -> anyhow::Result<()> {
        // Follow the distance slider live.
        if let Some(distance) = self
            .module
            .get_setting("Distance")
            .and_then(ModuleSetting::get_slider_value)
        {
            if let Some(session) = SESSION.lock().unwrap_or_else(|p| p.into_inner()).as_mut() {
                session.distance = distance.clamp(MIN_DISTANCE, MAX_DISTANCE);
            }
        }
        Ok(())
    }

    fn get_module_data(&self) -> &ModuleData {
        &self.module
    }

    fn get_module_data_mut(&mut self) -> &mut ModuleData {
        &mut self.module
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// JOML `Vector3d.rotateY`, as the Free Camera API applies it.
    fn rotate_y(v: [f64; 3], angle: f64) -> [f64; 3] {
        [v[0] * angle.cos() + v[2] * angle.sin(), v[1], -v[0] * angle.sin() + v[2] * angle.cos()]
    }

    #[test]
    fn relative_orbit_lands_behind_the_look_direction() {
        for body_yaw in [0.0f32, 37.0, -170.0, 400.0] {
            let (local, rel_yaw) = relative_orbit(90.0, 30.0, 4.0, body_yaw, 1.62);
            let world = rotate_y(local, -((body_yaw % 360.0) as f64).to_radians());
            // Yaw 90 looks toward -X, pitch 30 looks down: the camera sits at +X, above.
            let (c, s) = (30f64.to_radians().cos() * 4.0, 30f64.to_radians().sin() * 4.0);
            for (got, want) in world.iter().zip([c, 1.62 + s, 0.0]) {
                assert!((got - want).abs() < 1e-4, "{body_yaw}: {world:?}");
            }
            assert_eq!(rel_yaw + body_yaw, 90.0);
        }
    }
}
