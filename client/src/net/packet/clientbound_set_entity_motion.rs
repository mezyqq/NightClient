//! Rust value-snapshot of `ClientboundSetEntityMotionPacket` — the server
//! telling the client an entity's velocity changed (knockback, explosions, …).
//!
//! A `record (int id, Vec3 movement)`, so `read` / `to_java` are direct.

use crate::mapping::MinecraftClassType;
use crate::state::mapping;
use jni::objects::{JObject, JValue};
use jni::sys::jobject;
use jni::JNIEnv;

/// Class of `ClientboundSetEntityMotionPacket`.
pub const CLASS_TYPE: MinecraftClassType = MinecraftClassType::ClientboundSetEntityMotionPacket;

/// A snapshot of a `ClientboundSetEntityMotionPacket`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClientboundSetEntityMotionPacket {
    /// Network id of the entity whose motion is being set.
    pub entity_id: i32,
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

impl ClientboundSetEntityMotionPacket {
    /// Reads a `ClientboundSetEntityMotionPacket` JVM object into a snapshot.
    pub fn read(
        env: &mut JNIEnv,
        packet: &JObject,
    ) -> anyhow::Result<ClientboundSetEntityMotionPacket> {
        // 1.21.5+ is a record (`id()`, `movement()`); before it the packet has
        // `getId()` and `getXa/Ya/Za()` (already divided by 8000).
        if let Ok(id) = env.call_method(packet, "id", "()I", &[]) {
            let entity_id = id.i()?;
            let movement = env
                .call_method(packet, "movement", "()Lnet/minecraft/world/phys/Vec3;", &[])?
                .l()?;
            return Ok(ClientboundSetEntityMotionPacket {
                entity_id,
                x: env.get_field(&movement, "x", "D")?.d()?,
                y: env.get_field(&movement, "y", "D")?.d()?,
                z: env.get_field(&movement, "z", "D")?.d()?,
            });
        }
        env.exception_clear()?;
        Ok(ClientboundSetEntityMotionPacket {
            entity_id: env.call_method(packet, "getId", "()I", &[])?.i()?,
            x: env.call_method(packet, "getXa", "()D", &[])?.d()?,
            y: env.call_method(packet, "getYa", "()D", &[])?.d()?,
            z: env.call_method(packet, "getZa", "()D", &[])?.d()?,
        })
    }

    /// Builds the `ClientboundSetEntityMotionPacket` JVM object from this
    /// snapshot.
    pub fn to_java(self, env: &mut JNIEnv) -> anyhow::Result<jobject> {
        let vec3_class = mapping().resolve_class(env, MinecraftClassType::Vec3.get_name())?;
        let movement = env.new_object(
            &vec3_class,
            "(DDD)V",
            &[
                JValue::Double(self.x),
                JValue::Double(self.y),
                JValue::Double(self.z),
            ],
        )?;

        let packet_class = mapping().resolve_class(env, CLASS_TYPE.get_name())?;
        let packet = env.new_object(
            &packet_class,
            "(ILnet/minecraft/world/phys/Vec3;)V",
            &[JValue::Int(self.entity_id), JValue::Object(&movement)],
        )?;
        Ok(packet.into_raw())
    }
}
