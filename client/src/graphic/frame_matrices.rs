//! The game's own camera for the frame just rendered — so the overlay lines
//! up with the world whatever changed the view.
//!
//! The ESP used to rebuild the projection from the camera's yaw / pitch and
//! a FOV read from the options. Anything else that shapes the view — zoom
//! mods (Zoom, Zoomify), Camera Overhaul's roll, view bobbing, the FOV
//! effects — was invisible to it, so boxes, name tags and lines drifted off
//! their targets. Instead, a NeoForge `RenderLevelStageEvent` listener (built
//! from method handles, no native code in the render path) copies the exact
//! projection and model-view matrices and the camera position the level was
//! drawn with into Java arrays; the overlay, drawn at the end of that same
//! frame, reads them back.
//!
//! The projection holder's last element is set to NaN after each read and
//! the event overwrites it, so a frame without a level render (or a loader
//! without the event) is detected and the ESP falls back to its own maths.

use crate::module::optimization::java::{self, EventListeners};
use crate::state::mapping;
use jni::objects::{GlobalRef, JDoubleArray, JFloatArray, JObject};
use jni::JNIEnv;
use std::sync::Mutex;

const EVENT: &str = "net/neoforged/neoforge/client/event/RenderLevelStageEvent";

/// What the level was rendered with.
#[derive(Debug, Clone, Copy)]
pub struct Captured {
    /// `projection × model-view`, column-major — maps a camera-relative
    /// world offset to clip space.
    pub matrix: [f64; 16],
    /// Camera position in the world.
    pub cam: [f64; 3],
}

struct Capture {
    listeners: EventListeners,
    projection: GlobalRef,
    model_view: GlobalRef,
    camera: GlobalRef,
}

static CAPTURE: Mutex<Option<Capture>> = Mutex::new(None);
/// Set when the listener could not be built — not retried.
static FAILED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Pure: `a × b` for column-major 4×4 matrices.
pub fn multiply(a: &[f64; 16], b: &[f64; 16]) -> [f64; 16] {
    let mut out = [0.0; 16];
    for col in 0..4 {
        for row in 0..4 {
            out[col * 4 + row] = (0..4).map(|k| a[k * 4 + row] * b[col * 4 + k]).sum();
        }
    }
    out
}

/// Pure: a camera-relative offset `r` through `m` to normalised device
/// coordinates, or `None` behind (or too close to) the camera.
pub fn to_ndc(m: &[f64; 16], r: [f64; 3]) -> Option<[f64; 2]> {
    let clip = |row: usize| m[row] * r[0] + m[4 + row] * r[1] + m[8 + row] * r[2] + m[12 + row];
    let w = clip(3);
    if w < 0.05 {
        return None;
    }
    Some([clip(0) / w, clip(1) / w])
}

/// The matrices of the frame just rendered, if the level was rendered.
pub fn take() -> Option<Captured> {
    if FAILED.load(std::sync::atomic::Ordering::Relaxed) {
        return None;
    }
    let mut guard = CAPTURE.lock().unwrap_or_else(|p| p.into_inner());
    if guard.is_none() {
        match create() {
            Ok(capture) => *guard = Some(capture),
            Err(error) => {
                log::warn!("ESP: cannot read the game's camera matrices, using the camera angles: {error}");
                FAILED.store(true, std::sync::atomic::Ordering::Relaxed);
                return None;
            }
        }
    }
    let capture = guard.as_mut()?;
    let result = mapping().in_frame(|| {
        let mut env = mapping().get_env()?;
        read(&mut env, capture)
    });
    match result {
        Ok(captured) => captured,
        Err(error) => {
            log::debug!("ESP: reading the camera matrices: {error}");
            None
        }
    }
}

fn read(env: &mut JNIEnv, capture: &Capture) -> anyhow::Result<Option<Captured>> {
    let projection: &JFloatArray = capture.projection.as_obj().into();
    let model_view: &JFloatArray = capture.model_view.as_obj().into();
    let camera: &JDoubleArray = capture.camera.as_obj().into();
    let mut p = [0f32; 16];
    let mut mv = [0f32; 16];
    let mut cam = [0f64; 3];
    env.get_float_array_region(projection, 0, &mut p)?;
    if p[15].is_nan() {
        return Ok(None); // no level render since the last read
    }
    env.get_float_array_region(model_view, 0, &mut mv)?;
    env.get_double_array_region(camera, 0, &mut cam)?;
    env.set_float_array_region(projection, 15, &[f32::NAN])?;

    let matrix = multiply(&p.map(f64::from), &mv.map(f64::from));
    if matrix.iter().any(|v| !v.is_finite()) || cam.iter().any(|v| !v.is_finite()) {
        return Ok(None);
    }
    Ok(Some(Captured { matrix, cam }))
}

fn create() -> anyhow::Result<Capture> {
    let (projection, model_view, camera) = mapping().in_frame(|| {
        let env = mapping().get_env()?;
        let projection = env.new_float_array(16)?;
        env.set_float_array_region(&projection, 15, &[f32::NAN])?;
        let model_view = env.new_float_array(16)?;
        let camera = env.new_double_array(3)?;
        Ok((
            env.new_global_ref(projection)?,
            env.new_global_ref(model_view)?,
            env.new_global_ref(camera)?,
        ))
    })?;
    let mut listeners = EventListeners::default();
    let (p, mv, c) = (projection.clone(), model_view.clone(), camera.clone());
    listeners.ensure("matrices", EVENT, "v1".to_string(), move |env, event| {
        build(env, event, p.as_obj(), mv.as_obj(), c.as_obj())
    });
    if !listeners.is_registered("matrices") {
        anyhow::bail!("the NeoForge render event listener could not be registered");
    }
    Ok(Capture { listeners, projection, model_view, camera })
}

/// `event -> { proj.get(P); mv.get(MV); C = camera position }`.
fn build<'l>(
    env: &mut JNIEnv<'l>,
    event: &JObject<'l>,
    projection: &JObject,
    model_view: &JObject,
    camera: &JObject,
) -> anyhow::Result<JObject<'l>> {
    let matrix_class = java::game_class(env, "org/joml/Matrix4f")?;
    let floats = env.call_method(projection, "getClass", "()Ljava/lang/Class;", &[])?.l()?;
    let get_projection = java::key_chain(env, &[(event, "getProjectionMatrix")])?;
    let get_model_view = java::key_chain(env, &[(event, "getModelViewMatrix")])?;
    let mut action = java::call_with(env, &matrix_class, "get", &floats, projection, &get_projection)?;
    let copy_model_view = java::call_with(env, &matrix_class, "get", &floats, model_view, &get_model_view)?;
    action = java::sequence(env, &action, &copy_model_view)?;

    let camera_class = java::game_class(env, "net/minecraft/client/Camera")?;
    let vec3_class = java::game_class(env, "net/minecraft/world/phys/Vec3")?;
    for (index, axis) in ["x", "y", "z"].into_iter().enumerate() {
        let coordinate = java::key_chain(env, &[(event, "getCamera"), (&camera_class, "getPosition"), (&vec3_class, axis)])?;
        let store = java::store_double(env, camera, index as i32, &coordinate)?;
        action = java::sequence(env, &action, &store)?;
    }
    java::proxy(env, "java/util/function/Consumer", &action)
}

/// Unregisters the listener — on unload.
pub fn cleanup() {
    if let Some(mut capture) = CAPTURE.lock().unwrap_or_else(|p| p.into_inner()).take() {
        capture.listeners.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDENTITY: [f64; 16] = [1., 0., 0., 0., 0., 1., 0., 0., 0., 0., 1., 0., 0., 0., 0., 1.];

    /// A perspective matrix like JOML's `perspective(fovY, aspect, near, far)`.
    fn perspective(fov_deg: f64, aspect: f64) -> [f64; 16] {
        let f = 1.0 / (fov_deg.to_radians() / 2.0).tan();
        let (near, far) = (0.05, 1000.0);
        let mut m = [0.0; 16];
        m[0] = f / aspect;
        m[5] = f;
        m[10] = (far + near) / (near - far);
        m[11] = -1.0;
        m[14] = 2.0 * far * near / (near - far);
        m
    }

    #[test]
    fn multiplying_by_identity_changes_nothing() {
        let p = perspective(70.0, 16.0 / 9.0);
        assert_eq!(multiply(&p, &IDENTITY), p);
        assert_eq!(multiply(&IDENTITY, &p), p);
    }

    #[test]
    fn a_point_straight_ahead_lands_in_the_centre() {
        // OpenGL looks down -Z.
        let p = perspective(70.0, 16.0 / 9.0);
        let [x, y] = to_ndc(&p, [0.0, 0.0, -10.0]).unwrap();
        assert!(x.abs() < 1e-9 && y.abs() < 1e-9);
        assert!(to_ndc(&p, [0.0, 0.0, 10.0]).is_none(), "behind the camera");
    }

    #[test]
    fn zooming_in_spreads_points_out() {
        let wide = to_ndc(&perspective(70.0, 1.0), [1.0, 0.0, -10.0]).unwrap()[0];
        let zoomed = to_ndc(&perspective(20.0, 1.0), [1.0, 0.0, -10.0]).unwrap()[0];
        assert!(zoomed > wide * 3.0);
    }
}
