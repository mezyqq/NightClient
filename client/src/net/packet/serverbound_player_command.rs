//! `ServerboundPlayerCommandPacket` — sneak / sprint / sleep / horse-jump
//! state changes. Read-only: modules look at the action and may cancel the
//! packet (AntiHunger drops `START_SPRINTING`); it is never rebuilt.

use crate::mapping::raw;
use crate::mapping::MinecraftClassType;
use jni::objects::{GlobalRef, JObject};
use jni::JNIEnv;

/// Class of `ServerboundPlayerCommandPacket`.
pub const CLASS_TYPE: MinecraftClassType = MinecraftClassType::ServerboundPlayerCommandPacket;

/// A captured player-command packet.
#[derive(Debug, Clone)]
pub struct ServerboundPlayerCommandPacket {
    /// The action's enum name (`START_SPRINTING`, `PRESS_SHIFT_KEY`, …).
    pub action: String,
    /// The original packet, forwarded as is.
    pub jni_ref: GlobalRef,
}

impl ServerboundPlayerCommandPacket {
    pub fn read(env: &mut JNIEnv, packet: &JObject) -> anyhow::Result<Self> {
        let action = raw::call(
            env,
            packet,
            "getAction",
            "()Lnet/minecraft/network/protocol/game/ServerboundPlayerCommandPacket$Action;",
            &[],
        )?
        .l()?;
        let name = raw::call(env, &action, "name", "()Ljava/lang/String;", &[])?.l()?;
        Ok(Self {
            action: crate::state::mapping().get_string(name)?,
            jni_ref: env.new_global_ref(packet)?,
        })
    }
}

/// Compared by action only — the packet is never rebuilt.
impl PartialEq for ServerboundPlayerCommandPacket {
    fn eq(&self, other: &Self) -> bool {
        self.action == other.action
    }
}
