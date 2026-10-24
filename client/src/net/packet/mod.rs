//! Rust value-snapshots of the Minecraft packets DarkClient reads or rewrites.
//!
//! Each packet is read once into a plain Rust struct (`read`), wrapped in the
//! [`Packet`] enum, modified freely by the modules' `handle_packet`, then — if
//! changed — rebuilt into a JVM object (`to_java`). A module may also drop a
//! packet outright by returning [`PacketAction::Cancel`]. All JNI uses explicit
//! descriptors and the [`MinecraftClassType`] class table — no reflection, no
//! overload resolution, no string literals.
//!
//! Minecraft packet classes are strictly directional — `Serverbound*` are
//! outbound, `Clientbound*` inbound — so every [`Packet`] variant has a fixed
//! direction, and the dispatch only probes the variants of the right one
//! ([`Packet::from_outbound`] / [`Packet::from_inbound`]).

pub mod clientbound_player_info_update;
pub mod clientbound_set_entity_motion;
pub mod serverbound_move_player;
pub mod serverbound_player_command;

use crate::mapping::MinecraftClassType;
use crate::state::mapping;
use clientbound_player_info_update::ClientboundPlayerInfoUpdatePacket;
use clientbound_set_entity_motion::ClientboundSetEntityMotionPacket;
use jni::objects::JObject;
use jni::sys::jobject;
use jni::JNIEnv;
use serverbound_move_player::ServerboundMovePlayerPacket;
use serverbound_player_command::ServerboundPlayerCommandPacket;

/// A packet passing through the connection, in a form modules can `match` on.
/// Variant names mirror the Minecraft packet classes.
#[derive(Debug, Clone, PartialEq)]
pub enum Packet {
    /// Outbound `ServerboundMovePlayerPacket`.
    ServerboundMovePlayer(ServerboundMovePlayerPacket),
    /// Inbound `ClientboundSetEntityMotionPacket`.
    ClientboundSetEntityMotion(ClientboundSetEntityMotionPacket),
    /// Inbound `ClientboundPlayerInfoUpdatePacket` — held by reference so a
    /// module (Freecam) can queue and replay it unchanged.
    ClientboundPlayerInfoUpdate(ClientboundPlayerInfoUpdatePacket),
    /// Outbound `ServerboundPlayerCommandPacket` — read-only, may be cancelled.
    ServerboundPlayerCommand(ServerboundPlayerCommandPacket),
}

/// What should happen to a packet after the modules have seen it. Returned by
/// `Module::handle_packet`; the default is [`PacketAction::Forward`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PacketAction {
    /// Forward the packet — possibly after a module mutated it in place.
    #[default]
    Forward,
    /// Drop the packet entirely: it is never sent (outbound) nor delivered
    /// (inbound). One module asking to cancel is enough — the rest are skipped.
    Cancel,
}

impl Packet {
    /// Builds a `Packet` from an outbound (`Serverbound`) JVM packet, or `None`
    /// when it is not a type any module handles.
    pub fn from_outbound(env: &mut JNIEnv, packet: &JObject) -> anyhow::Result<Option<Packet>> {
        if is_instance(env, packet, serverbound_move_player::CLASS_TYPE)? {
            return Ok(Some(Packet::ServerboundMovePlayer(
                ServerboundMovePlayerPacket::read(env, packet)?,
            )));
        }
        if is_instance(env, packet, serverbound_player_command::CLASS_TYPE)? {
            return Ok(Some(Packet::ServerboundPlayerCommand(
                ServerboundPlayerCommandPacket::read(env, packet)?,
            )));
        }
        Ok(None)
    }

    /// Builds a `Packet` from an inbound (`Clientbound`) JVM packet, or `None`
    /// when it is not a type any module handles.
    pub fn from_inbound(env: &mut JNIEnv, packet: &JObject) -> anyhow::Result<Option<Packet>> {
        if is_instance(env, packet, clientbound_set_entity_motion::CLASS_TYPE)? {
            return Ok(Some(Packet::ClientboundSetEntityMotion(
                ClientboundSetEntityMotionPacket::read(env, packet)?,
            )));
        }
        if is_instance(env, packet, clientbound_player_info_update::CLASS_TYPE)? {
            return Ok(Some(Packet::ClientboundPlayerInfoUpdate(
                ClientboundPlayerInfoUpdatePacket {
                    jni_ref: env.new_global_ref(packet)?,
                },
            )));
        }
        Ok(None)
    }

    /// Rebuilds the JVM packet object from this (possibly modified) snapshot.
    pub fn to_java(self, env: &mut JNIEnv) -> anyhow::Result<jobject> {
        match self {
            Packet::ServerboundMovePlayer(packet) => packet.to_java(env),
            Packet::ClientboundSetEntityMotion(packet) => packet.to_java(env),
            // Pass-through: never mutated, so dispatch never asks to rebuild.
            // Return the original ref's raw handle for completeness.
            Packet::ClientboundPlayerInfoUpdate(packet) => {
                Ok(env.new_local_ref(packet.jni_ref.as_obj())?.into_raw())
            }
            Packet::ServerboundPlayerCommand(packet) => {
                Ok(env.new_local_ref(packet.jni_ref.as_obj())?.into_raw())
            }
        }
    }
}

/// Whether `object` is an instance of the given mapped class.
fn is_instance(
    env: &mut JNIEnv,
    object: &JObject,
    class: MinecraftClassType,
) -> anyhow::Result<bool> {
    let jclass = mapping().resolve_class(env, class.get_name())?;
    Ok(env.is_instance_of(object, &jclass)?)
}
