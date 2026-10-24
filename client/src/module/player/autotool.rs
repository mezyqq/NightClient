//! AutoTool — while digging, switches to the hotbar item that breaks the
//! targeted block fastest, and optionally switches back when digging stops.

use crate::mapping::raw;
use crate::mapping::MappedObject;
use crate::module::player::hands;
use crate::module::{KeyboardKey, Module, ModuleCategory, ModuleData, ModuleId, ModuleSetting};
use crate::state::{mapping, minecraft};
use jni::objects::{JObject, JValue};
use jni::JNIEnv;
use std::sync::Mutex;

const STATE_SIG: &str = "Lnet/minecraft/world/level/block/state/BlockState;";

#[derive(Debug)]
pub struct AutoToolModule {
    pub module: ModuleData,
    /// The slot selected before the first switch, while switched.
    original_slot: Mutex<Option<i32>>,
}

impl AutoToolModule {
    pub fn new() -> Self {
        Self {
            module: ModuleData {
                id: ModuleId::AutoTool,
                description: "Picks the best hotbar tool while digging".to_string(),
                category: ModuleCategory::Player,
                key_bind: KeyboardKey::KeyNone,
                enabled: false,
                settings: vec![ModuleSetting::Toggle {
                    name: "Switch back".to_string(),
                    value: true,
                }],
            },
            original_slot: Mutex::new(None),
        }
    }

    fn switch_back(&self) -> bool {
        self.module
            .get_setting("Switch back")
            .and_then(ModuleSetting::get_toggle_value)
            .unwrap_or(true)
    }
}

impl Module for AutoToolModule {
    fn on_start(&self) -> anyhow::Result<()> {
        Ok(())
    }

    fn on_stop(&self) -> anyhow::Result<()> {
        *self.original_slot.lock().unwrap() = None;
        Ok(())
    }

    fn on_tick(&self) -> anyhow::Result<()> {
        let mc = minecraft();
        let (Some(player), Some(world), Some(game_mode)) = (mc.player()?, mc.world()?, mc.game_mode()?) else {
            return Ok(());
        };
        let mut original = self.original_slot.lock().unwrap();
        mapping().in_frame(|| {
            let mut env = mapping().get_env()?;
            let inventory = hands::inventory(&mut env, player.jni_ref().as_obj())?;
            let destroying = raw::call(&mut env, game_mode.jni_ref().as_obj(), "isDestroying", "()Z", &[])?.z()?;

            if !destroying {
                if let Some(slot) = original.take() {
                    if self.switch_back() {
                        hands::set_selected_slot(&mut env, &inventory, slot)?;
                    }
                }
                return Ok(());
            }

            let Some(state) = targeted_block_state(&mut env, world.jni_ref().as_obj())? else {
                return Ok(());
            };
            let current = hands::selected_slot(&mut env, &inventory)?;
            let mut best = (current, speed(&mut env, &inventory, current, &state)?);
            for slot in 0..9 {
                let value = speed(&mut env, &inventory, slot, &state)?;
                if value > best.1 + 1e-3 {
                    best = (slot, value);
                }
            }
            if best.0 != current {
                original.get_or_insert(current);
                hands::set_selected_slot(&mut env, &inventory, best.0)?;
            }
            Ok(())
        })
    }

    fn get_module_data(&self) -> &ModuleData {
        &self.module
    }

    fn get_module_data_mut(&mut self) -> &mut ModuleData {
        &mut self.module
    }
}

/// The state of the block under the crosshair, if the crosshair is on one.
fn targeted_block_state<'local>(
    env: &mut JNIEnv<'local>,
    level: &JObject,
) -> anyhow::Result<Option<JObject<'local>>> {
    let hit = raw::field(
        env,
        minecraft().jni_ref().as_obj(),
        "hitResult",
        "Lnet/minecraft/world/phys/HitResult;",
    )?
    .l()?;
    if hit.is_null() {
        return Ok(None);
    }
    let kind = raw::call(env, &hit, "getType", "()Lnet/minecraft/world/phys/HitResult$Type;", &[])?.l()?;
    if raw::to_string(env, &kind)? != "BLOCK" {
        return Ok(None);
    }
    let pos = raw::call(env, &hit, "getBlockPos", "()Lnet/minecraft/core/BlockPos;", &[])?.l()?;
    let state = raw::call(
        env,
        level,
        "getBlockState",
        &format!("(Lnet/minecraft/core/BlockPos;){STATE_SIG}"),
        &[JValue::Object(&pos)],
    )?
    .l()?;
    Ok(Some(state))
}

/// How fast the item in hotbar `slot` breaks `state`.
fn speed(env: &mut JNIEnv, inventory: &JObject, slot: i32, state: &JObject) -> anyhow::Result<f32> {
    env.with_local_frame(4, |env| -> anyhow::Result<f32> {
        let stack = hands::stack_in(env, inventory, slot)?;
        Ok(raw::call(env, &stack, "getDestroySpeed", &format!("({STATE_SIG})F"), &[JValue::Object(state)])?.f()?)
    })
}
