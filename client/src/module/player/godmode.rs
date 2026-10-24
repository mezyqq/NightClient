//! God Mode — makes the player invulnerable.
//!
//! Health is decided by the server, so this can only work where the client
//! *is* the server: singleplayer, or a world opened to LAN from this game. It
//! reaches the integrated server's own `ServerPlayer` and
//!
//! * sets its `invulnerable` ability (blocks mob, fall, fire, … damage), and
//! * tops up health, hunger and air every tick — which also covers the damage
//!   types that bypass invulnerability, such as falling into the void.
//!
//! On a remote server the module only shows a warning: no client can change
//! what the server thinks its health is.

use crate::graphic::notification::{Notification, NotificationType};
use crate::mapping::raw;
use crate::mapping::MappedObject;
use crate::module::{KeyboardKey, Module, ModuleCategory, ModuleData, ModuleId};
use crate::state::{mapping, minecraft};
use jni::objects::{GlobalRef, JObject, JValue};
use jni::JNIEnv;
use std::sync::Mutex;

const ABILITIES_SIG: &str = "Lnet/minecraft/world/entity/player/Abilities;";

#[derive(Debug)]
pub struct GodModeModule {
    pub module: ModuleData,
    /// The server player's `invulnerable` flag before God Mode turned it on —
    /// creative players already have it, and must keep it on disable.
    previous_invulnerable: Mutex<Option<bool>>,
}

impl GodModeModule {
    pub fn new() -> Self {
        Self {
            module: ModuleData {
                id: ModuleId::GodMode,
                description: "Invulnerability — singleplayer / LAN host only".to_string(),
                category: ModuleCategory::Player,
                key_bind: KeyboardKey::KeyNone,
                enabled: false,
                settings: vec![],
            },
            previous_invulnerable: Mutex::new(None),
        }
    }
}

impl Module for GodModeModule {
    fn on_start(&self) -> anyhow::Result<()> {
        let Some(server_player) = server_player()? else {
            Notification::send(
                NotificationType::Warning,
                "God Mode",
                "Works only in singleplayer or when hosting LAN",
            );
            return Ok(());
        };
        mapping().in_frame(|| {
            let mut env = mapping().get_env()?;
            let player = server_player.as_obj();
            let abilities = raw::field(&mut env, player, "abilities", ABILITIES_SIG)?.l()?;
            let was = raw::field(&mut env, &abilities, "invulnerable", "Z")?.z()?;
            *self.previous_invulnerable.lock().unwrap() = Some(was);
            apply(&mut env, player)
        })
    }

    fn on_stop(&self) -> anyhow::Result<()> {
        let Some(previous) = self.previous_invulnerable.lock().unwrap().take() else {
            return Ok(());
        };
        let Some(server_player) = server_player()? else {
            return Ok(());
        };
        mapping().in_frame(|| {
            let mut env = mapping().get_env()?;
            let player = server_player.as_obj();
            let abilities = raw::field(&mut env, player, "abilities", ABILITIES_SIG)?.l()?;
            raw::set_field(&mut env, &abilities, "invulnerable", "Z", JValue::Bool(previous as u8))?;
            raw::call(&mut env, player, "onUpdateAbilities", "()V", &[])?;
            Ok(())
        })
    }

    fn on_tick(&self) -> anyhow::Result<()> {
        let Some(server_player) = server_player()? else {
            return Ok(());
        };
        mapping().in_frame(|| {
            let mut env = mapping().get_env()?;
            apply(&mut env, server_player.as_obj())
        })
    }

    fn get_module_data(&self) -> &ModuleData {
        &self.module
    }

    fn get_module_data_mut(&mut self) -> &mut ModuleData {
        &mut self.module
    }
}

/// The integrated server's `ServerPlayer` for the local player, or `None`
/// when connected to a remote server.
pub fn server_player() -> anyhow::Result<Option<GlobalRef>> {
    let Some(local) = minecraft().player()? else {
        return Ok(None);
    };
    mapping().in_frame(|| {
        let mut env = mapping().get_env()?;
        let server = raw::call(
            &mut env,
            minecraft().jni_ref().as_obj(),
            "getSingleplayerServer",
            "()Lnet/minecraft/client/server/IntegratedServer;",
            &[],
        )?
        .l()?;
        if server.is_null() {
            return Ok(None);
        }
        let uuid = raw::call(&mut env, local.jni_ref().as_obj(), "getUUID", "()Ljava/util/UUID;", &[])?.l()?;
        let players = raw::call(
            &mut env,
            &server,
            "getPlayerList",
            "()Lnet/minecraft/server/players/PlayerList;",
            &[],
        )?
        .l()?;
        let player = raw::call(
            &mut env,
            &players,
            "getPlayer",
            "(Ljava/util/UUID;)Lnet/minecraft/server/level/ServerPlayer;",
            &[JValue::Object(&uuid)],
        )?
        .l()?;
        if player.is_null() {
            return Ok(None);
        }
        Ok(Some(env.new_global_ref(player)?))
    })
}

/// Makes `player` (a `ServerPlayer`) invulnerable and tops up its health,
/// hunger, air and fire. Writes only what changed, so the per-tick cost of an
/// already-healthy player is a handful of reads.
fn apply(env: &mut JNIEnv, player: &JObject) -> anyhow::Result<()> {
    let abilities = raw::field(env, player, "abilities", ABILITIES_SIG)?.l()?;
    if !raw::field(env, &abilities, "invulnerable", "Z")?.z()? {
        raw::set_field(env, &abilities, "invulnerable", "Z", JValue::Bool(1))?;
        raw::call(env, player, "onUpdateAbilities", "()V", &[])?;
    }

    let health = raw::call(env, player, "getHealth", "()F", &[])?.f()?;
    let max_health = raw::call(env, player, "getMaxHealth", "()F", &[])?.f()?;
    if health < max_health {
        raw::call(env, player, "setHealth", "(F)V", &[JValue::Float(max_health)])?;
    }

    let food = raw::call(env, player, "getFoodData", "()Lnet/minecraft/world/food/FoodData;", &[])?.l()?;
    if raw::call(env, &food, "getFoodLevel", "()I", &[])?.i()? < 20 {
        raw::call(env, &food, "setFoodLevel", "(I)V", &[JValue::Int(20)])?;
        raw::call(env, &food, "setSaturation", "(F)V", &[JValue::Float(5.0)])?;
    }

    let air = raw::call(env, player, "getAirSupply", "()I", &[])?.i()?;
    let max_air = raw::call(env, player, "getMaxAirSupply", "()I", &[])?.i()?;
    if air < max_air {
        raw::call(env, player, "setAirSupply", "(I)V", &[JValue::Int(max_air)])?;
    }

    if raw::call(env, player, "getRemainingFireTicks", "()I", &[])?.i()? > 0 {
        raw::call(env, player, "clearFire", "()V", &[])?;
    }
    Ok(())
}
