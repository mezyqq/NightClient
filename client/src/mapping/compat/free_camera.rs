//! Bridge to the Free Camera API mod (`freecameraapi`).
//!
//! The mod applies registered camera modifiers right after `Camera.setup` in
//! `GameRenderer.renderLevel`. A modifier in *global* mode overrides the
//! camera's world position and rotation outright, while the player stays the
//! camera entity — so movement, mining and position packets keep working
//! exactly as before. Spectate is built on it.

use crate::state::mapping;
use jni::objects::{GlobalRef, JObject, JValue};

const MANAGER: &str = "cn/anecansaitin/freecameraapi/CameraModifierManager";
const MODIFIER_SIG: &str = "()Lcn/anecansaitin/freecameraapi/ICameraModifier;";
/// Modifier id, shown in the mod's own modifier screen.
const MODIFIER_ID: &str = "darkclient";

/// Whether the Free Camera API mod is loaded in the game.
pub fn is_installed() -> bool {
    super::class_present(MANAGER)
}

/// A camera modifier owned by DarkClient.
#[derive(Debug)]
pub struct CameraOverride {
    modifier: GlobalRef,
}

impl CameraOverride {
    /// Fetches (or creates) the high-priority DarkClient modifier and switches
    /// it on, no lerp (the caller smooths per frame itself). `global`: absolute
    /// position and rotation; otherwise relative — the position is an offset
    /// from the player's feet in the body's frame (the mod interpolates the
    /// feet at render time) and the yaw is added to the body's.
    pub fn acquire(global: bool) -> anyhow::Result<CameraOverride> {
        mapping().in_frame(|| {
            let mut env = mapping().get_env()?;
            let manager = mapping().resolve_class(&mut env, MANAGER)?;
            let id: JObject = env.new_string(MODIFIER_ID)?.into();
            let modifier = env
                .call_static_method(
                    manager,
                    "createModifier",
                    "(Ljava/lang/String;Z)Lcn/anecansaitin/freecameraapi/ICameraModifier;",
                    &[JValue::Object(&id), JValue::Bool(1)],
                )
                .map_err(|_| clear("createModifier"))?
                .l()?;
            let mode = if global { "enableGlobalMode" } else { "disableGlobalMode" };
            for method in ["enable", "enablePos", "enableRotation", mode, "disableLerp"] {
                env.call_method(&modifier, method, MODIFIER_SIG, &[])
                    .map_err(|_| clear(method))?;
            }
            Ok(CameraOverride {
                modifier: env.new_global_ref(modifier)?,
            })
        })
    }

    /// Places the camera at `pos`, looking along `yaw` / `pitch` (degrees,
    /// Minecraft convention). Takes effect on the next rendered frame.
    pub fn set(&self, pos: [f64; 3], yaw: f32, pitch: f32) -> anyhow::Result<()> {
        mapping().in_frame(|| {
            let mut env = mapping().get_env()?;
            env.call_method(
                self.modifier.as_obj(),
                "setPos",
                "(DDD)Lcn/anecansaitin/freecameraapi/ICameraModifier;",
                &[
                    JValue::Double(pos[0]),
                    JValue::Double(pos[1]),
                    JValue::Double(pos[2]),
                ],
            )
            .map_err(|_| clear("setPos"))?;
            // The modifier's rotation vector is (pitch, yaw, roll).
            env.call_method(
                self.modifier.as_obj(),
                "setRotationYXZ",
                "(FFF)Lcn/anecansaitin/freecameraapi/ICameraModifier;",
                &[JValue::Float(pitch), JValue::Float(yaw), JValue::Float(0.0)],
            )
            .map_err(|_| clear("setRotationYXZ"))?;
            Ok(())
        })
    }

    /// Switches the modifier off; the camera snaps back to the player.
    pub fn release(&self) {
        let _ = mapping().in_frame(|| {
            let mut env = mapping().get_env()?;
            env.call_method(self.modifier.as_obj(), "disableAll", MODIFIER_SIG, &[])
                .map_err(|_| clear("disableAll"))?;
            Ok(())
        });
    }
}

/// Clears the pending Java exception after a failed call and describes it.
fn clear(method: &str) -> anyhow::Error {
    if let Ok(env) = mapping().get_env() {
        let _ = env.exception_clear();
    }
    anyhow::anyhow!("Free Camera API: {method} failed")
}
