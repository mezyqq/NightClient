//! AutoEat — eats from the hotbar when hunger drops to a threshold.
//!
//! Picks a food item in the hotbar (skipping food with bad effects and golden
//! apples, which are kept for emergencies), selects it, and holds the use key
//! until the player is full or the stack runs out; then puts the previously
//! selected slot back.

use crate::mapping::raw;
use crate::mapping::MappedObject;
use crate::module::player::hands;
use crate::module::{KeyboardKey, Module, ModuleCategory, ModuleData, ModuleId, ModuleSetting};
use crate::state::{mapping, minecraft};
use jni::objects::{JObject, JValue};
use jni::JNIEnv;
use std::sync::Mutex;

/// Food never eaten automatically: harmful effects, or too valuable.
const SKIPPED: [&str; 9] = [
    "minecraft:rotten_flesh",
    "minecraft:spider_eye",
    "minecraft:poisonous_potato",
    "minecraft:pufferfish",
    "minecraft:chicken",
    "minecraft:suspicious_stew",
    "minecraft:chorus_fruit",
    "minecraft:golden_apple",
    "minecraft:enchanted_golden_apple",
];
/// Ticks to wait for the eating animation to start before giving up.
const START_GRACE_TICKS: u32 = 6;

#[derive(Debug)]
struct Eating {
    previous_slot: i32,
    ticks: u32,
}

#[derive(Debug)]
pub struct AutoEatModule {
    pub module: ModuleData,
    eating: Mutex<Option<Eating>>,
}

impl AutoEatModule {
    pub fn new() -> Self {
        Self {
            module: ModuleData {
                id: ModuleId::AutoEat,
                description: "Eats from the hotbar when hungry".to_string(),
                category: ModuleCategory::Player,
                key_bind: KeyboardKey::KeyNone,
                enabled: false,
                settings: vec![ModuleSetting::Slider {
                    name: "Hunger".to_string(),
                    value: 14.0,
                    min: 1.0,
                    max: 19.0,
                }],
            },
            eating: Mutex::new(None),
        }
    }

    fn threshold(&self) -> i32 {
        self.module
            .get_setting("Hunger")
            .and_then(ModuleSetting::get_slider_value)
            .unwrap_or(14.0) as i32
    }

    /// Stops eating: releases the key and restores the hotbar slot.
    fn finish(&self) {
        let Some(eating) = self.eating.lock().unwrap().take() else {
            return;
        };
        let _ = hands::hold_use_key(false);
        if let Ok(Some(player)) = minecraft().player() {
            let _ = mapping().in_frame(|| {
                let mut env = mapping().get_env()?;
                let inventory = hands::inventory(&mut env, player.jni_ref().as_obj())?;
                hands::set_selected_slot(&mut env, &inventory, eating.previous_slot)
            });
        }
    }
}

impl Module for AutoEatModule {
    fn on_start(&self) -> anyhow::Result<()> {
        Ok(())
    }

    fn on_stop(&self) -> anyhow::Result<()> {
        self.finish();
        Ok(())
    }

    fn on_tick(&self) -> anyhow::Result<()> {
        let Some(player) = minecraft().player()? else {
            return Ok(());
        };
        let player_obj = player.jni_ref().as_obj();
        let threshold = self.threshold();

        let mut guard = self.eating.lock().unwrap();
        if let Some(eating) = guard.as_mut() {
            eating.ticks += 1;
            let (food, using) = mapping().in_frame(|| {
                let mut env = mapping().get_env()?;
                Ok((food_level(&mut env, player_obj)?, raw::call(&mut env, player_obj, "isUsingItem", "()Z", &[])?.z()?))
            })?;
            let started = using || eating.ticks < START_GRACE_TICKS;
            if food >= 20 || !started {
                drop(guard);
                self.finish();
            } else {
                // Keep the key held — opening a screen releases every key.
                hands::hold_use_key(true)?;
            }
            return Ok(());
        }

        if minecraft().current_screen().is_open() {
            return Ok(());
        }
        let picked = mapping().in_frame(|| {
            let mut env = mapping().get_env()?;
            if food_level(&mut env, player_obj)? > threshold {
                return Ok(None);
            }
            let inventory = hands::inventory(&mut env, player_obj)?;
            let current = hands::selected_slot(&mut env, &inventory)?;
            // Prefer the selected slot, then the rest of the hotbar.
            let order = std::iter::once(current).chain((0..9).filter(|slot| *slot != current));
            for slot in order {
                let stack = hands::stack_in(&mut env, &inventory, slot)?;
                if is_edible(&mut env, &stack)? {
                    hands::set_selected_slot(&mut env, &inventory, slot)?;
                    return Ok(Some(current));
                }
            }
            Ok(None)
        })?;
        if let Some(previous_slot) = picked {
            *guard = Some(Eating { previous_slot, ticks: 0 });
            hands::hold_use_key(true)?;
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

fn food_level(env: &mut JNIEnv, player: &JObject) -> anyhow::Result<i32> {
    let food = raw::call(env, player, "getFoodData", "()Lnet/minecraft/world/food/FoodData;", &[])?.l()?;
    Ok(raw::call(env, &food, "getFoodLevel", "()I", &[])?.i()?)
}

/// Whether `stack` is food worth eating: it carries the `FOOD` data
/// component (1.20.5+) and is not on the skip list.
fn is_edible(env: &mut JNIEnv, stack: &JObject) -> anyhow::Result<bool> {
    if raw::call(env, stack, "isEmpty", "()Z", &[])?.z()? {
        return Ok(false);
    }
    let components = mapping().resolve_class(env, "net/minecraft/core/component/DataComponents")?;
    let food_type = env
        .get_static_field(&components, "FOOD", "Lnet/minecraft/core/component/DataComponentType;")
        .inspect_err(|_| {
            let _ = env.exception_clear();
        })?
        .l()?;
    let food = raw::call(
        env,
        stack,
        "get",
        "(Lnet/minecraft/core/component/DataComponentType;)Ljava/lang/Object;",
        &[JValue::Object(&food_type)],
    )?
    .l()?;
    if food.is_null() {
        return Ok(false);
    }
    let id = hands::item_id(env, stack)?;
    Ok(!SKIPPED.contains(&id.as_str()))
}
