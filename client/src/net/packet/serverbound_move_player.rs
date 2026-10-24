//! Rust value-snapshot of `ServerboundMovePlayerPacket`.
//!
//! The packet is an abstract class with four subclasses — `Pos`, `PosRot`,
//! `Rot`, `StatusOnly` — chosen by which of position / rotation it carries.
//! `read` snapshots any of them; `to_java` rebuilds the matching subclass.

use crate::mapping::MinecraftClassType;
use crate::state::mapping;
use jni::objects::{JObject, JValue};
use jni::sys::jobject;
use jni::JNIEnv;
use std::sync::atomic::{AtomicBool, Ordering};

/// Class of the abstract `ServerboundMovePlayerPacket`.
pub const CLASS_TYPE: MinecraftClassType = MinecraftClassType::ServerboundMovePlayerPacket;

/// A snapshot of a `ServerboundMovePlayerPacket`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ServerboundMovePlayerPacket {
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub y_rot: f32,
    pub x_rot: f32,
    pub on_ground: bool,
    pub horizontal_collision: bool,
    pub has_position: bool,
    pub has_rotation: bool,
}

impl ServerboundMovePlayerPacket {
    /// Reads a `ServerboundMovePlayerPacket` JVM object into a snapshot.
    pub fn read(env: &mut JNIEnv, packet: &JObject) -> anyhow::Result<ServerboundMovePlayerPacket> {
        Ok(ServerboundMovePlayerPacket {
            x: env
                .call_method(packet, "getX", "(D)D", &[JValue::Double(0.0)])?
                .d()?,
            y: env
                .call_method(packet, "getY", "(D)D", &[JValue::Double(0.0)])?
                .d()?,
            z: env
                .call_method(packet, "getZ", "(D)D", &[JValue::Double(0.0)])?
                .d()?,
            y_rot: env
                .call_method(packet, "getYRot", "(F)F", &[JValue::Float(0.0)])?
                .f()?,
            x_rot: env
                .call_method(packet, "getXRot", "(F)F", &[JValue::Float(0.0)])?
                .f()?,
            on_ground: env.call_method(packet, "isOnGround", "()Z", &[])?.z()?,
            // `horizontalCollision` only exists from 1.21.2 on.
            horizontal_collision: match env.call_method(packet, "horizontalCollision", "()Z", &[]) {
                Ok(value) => value.z()?,
                Err(_) => {
                    env.exception_clear()?;
                    false
                }
            },
            has_position: env.call_method(packet, "hasPosition", "()Z", &[])?.z()?,
            has_rotation: env.call_method(packet, "hasRotation", "()Z", &[])?.z()?,
        })
    }

    /// Builds the matching `ServerboundMovePlayerPacket` subclass from this
    /// snapshot, returning the new JVM object.
    pub fn to_java(self, env: &mut JNIEnv) -> anyhow::Result<jobject> {
        let on_ground = JValue::Bool(self.on_ground as u8);
        let collision = JValue::Bool(self.horizontal_collision as u8);
        let (x, y, z) = (JValue::Double(self.x), JValue::Double(self.y), JValue::Double(self.z));
        let (y_rot, x_rot) = (JValue::Float(self.y_rot), JValue::Float(self.x_rot));

        // Each constructor takes a trailing `horizontalCollision` flag from
        // 1.21.2 on; older versions end at `onGround`.
        let (class_type, modern_sig, args): (_, _, Vec<JValue>) =
            match (self.has_position, self.has_rotation) {
                (true, true) => (
                    MinecraftClassType::ServerboundMovePlayerPacketPosRot,
                    "(DDDFFZZ)V",
                    vec![x, y, z, y_rot, x_rot, on_ground],
                ),
                (true, false) => (
                    MinecraftClassType::ServerboundMovePlayerPacketPos,
                    "(DDDZZ)V",
                    vec![x, y, z, on_ground],
                ),
                (false, true) => (
                    MinecraftClassType::ServerboundMovePlayerPacketRot,
                    "(FFZZ)V",
                    vec![y_rot, x_rot, on_ground],
                ),
                (false, false) => (
                    MinecraftClassType::ServerboundMovePlayerPacketStatusOnly,
                    "(ZZ)V",
                    vec![on_ground],
                ),
            };
        let class = mapping().resolve_class(env, class_type.get_name())?;

        if !LEGACY_CONSTRUCTORS.load(Ordering::Relaxed) {
            let mut modern_args = args.clone();
            modern_args.push(collision);
            match env.new_object(&class, modern_sig, &modern_args) {
                Ok(object) => return Ok(object.into_raw()),
                Err(_) => {
                    env.exception_clear()?;
                    LEGACY_CONSTRUCTORS.store(true, Ordering::Relaxed);
                }
            }
        }
        // `(…Z)V` — the modern descriptor minus its last `Z`.
        let legacy_sig = modern_sig.replacen("Z)V", ")V", 1);
        Ok(env.new_object(&class, &legacy_sig, &args)?.into_raw())
    }
}

/// Set once the 1.21.2+ constructors turn out to be missing, so later packets
/// go straight to the pre-1.21.2 ones.
static LEGACY_CONSTRUCTORS: AtomicBool = AtomicBool::new(false);
