//! AirPlace — places blocks in mid-air, with nothing to build against.
//!
//! While the use key is held with a block in the main hand and the crosshair
//! hits nothing, the block is placed at the point `Range` blocks along the
//! look direction. The placement is an ordinary `useItemOn` aimed at the
//! target block itself (an air block is replaceable, so `BlockItem` places
//! into it), which a vanilla server accepts as long as the target is within
//! its interaction reach (~4.5 blocks + 1). Anti-cheats that check for a
//! supporting face reject it. A wireframe box marks the target; the ESP
//! overlay draws it through [`preview`].

use crate::mapping::block::Direction;
use crate::mapping::entity::player::LocalPlayer;
use crate::mapping::math::{BlockPos, Vec3};
use crate::mapping::raw;
use crate::mapping::MappedObject;
use crate::module::{KeyboardKey, Module, ModuleCategory, ModuleData, ModuleId, ModuleSetting};
use crate::state::{mapping, minecraft};
use jni::objects::{JObject, JValue};
use jni::JNIEnv;
use std::sync::Mutex;

/// The block to be placed and the preview colour, refreshed every tick.
static PREVIEW: Mutex<Option<([i32; 3], [f32; 4])>> = Mutex::new(None);
/// Game tick of the last placement.
static LAST_PLACED: Mutex<Option<u64>> = Mutex::new(None);

/// The target box to draw, if any — read by the ESP overlay each frame.
pub fn preview() -> Option<([i32; 3], [f32; 4])> {
    *PREVIEW.lock().unwrap_or_else(|p| p.into_inner())
}

fn set_preview(value: Option<([i32; 3], [f32; 4])>) {
    *PREVIEW.lock().unwrap_or_else(|p| p.into_inner()) = value;
}

/// Pure: the block `range` blocks from `eye` along the unit vector `look`.
pub fn air_target(eye: [f64; 3], look: [f64; 3], range: f64) -> [i32; 3] {
    [0, 1, 2].map(|i| (eye[i] + look[i] * range).floor() as i32)
}

/// Pure: whether a placement may happen at game tick `now` — every `delay`
/// ticks while the key is held (`delay` 0 = every tick).
pub fn due(now: u64, last: Option<u64>, delay: u64) -> bool {
    last.is_none_or(|last| now.saturating_sub(last) >= delay.max(1) || now < last)
}

#[derive(Debug)]
pub struct AirPlaceModule {
    pub module: ModuleData,
}

impl AirPlaceModule {
    pub fn new() -> Self {
        Self {
            module: ModuleData {
                id: ModuleId::AirPlace,
                description: "Places blocks in mid-air where you look".to_string(),
                category: ModuleCategory::World,
                key_bind: KeyboardKey::KeyNone,
                enabled: false,
                settings: vec![
                    ModuleSetting::Slider {
                        name: "Range".to_string(),
                        value: 4.0,
                        min: 1.0,
                        max: 5.5,
                    },
                    ModuleSetting::Slider {
                        name: "Delay (ticks)".to_string(),
                        value: 4.0,
                        min: 0.0,
                        max: 10.0,
                    },
                    ModuleSetting::Toggle {
                        name: "Show target".to_string(),
                        value: true,
                    },
                    ModuleSetting::Color {
                        name: "Target color".to_string(),
                        value: [0.3, 0.85, 1.0, 1.0],
                    },
                ],
            },
        }
    }

    fn slider(&self, name: &str, fallback: f32) -> f32 {
        self.module
            .get_setting(name)
            .and_then(ModuleSetting::get_slider_value)
            .unwrap_or(fallback)
    }
}

impl Module for AirPlaceModule {
    fn on_start(&self) -> anyhow::Result<()> {
        Ok(())
    }

    fn on_stop(&self) -> anyhow::Result<()> {
        set_preview(None);
        *LAST_PLACED.lock().unwrap_or_else(|p| p.into_inner()) = None;
        Ok(())
    }

    fn on_tick(&self) -> anyhow::Result<()> {
        let mc = minecraft();
        let (Some(player), Some(game_mode)) = (mc.player()?, mc.game_mode()?) else {
            set_preview(None);
            return Ok(());
        };
        if mc.current_screen().is_open() {
            set_preview(None);
            return Ok(());
        }
        let range = self.slider("Range", 4.0) as f64;
        let Some((target, use_down)) = find_target(&player, range)? else {
            set_preview(None);
            return Ok(());
        };

        let show = self
            .module
            .get_setting("Show target")
            .and_then(ModuleSetting::get_toggle_value)
            .unwrap_or(true);
        let color = match self.module.get_setting("Target color") {
            Some(ModuleSetting::Color { value, .. }) => *value,
            _ => [0.3, 0.85, 1.0, 1.0],
        };
        set_preview(show.then_some((target, color)));

        if !use_down {
            *LAST_PLACED.lock().unwrap_or_else(|p| p.into_inner()) = None;
            return Ok(());
        }
        let now = crate::graphic::hook::game_clock().0;
        let mut last = LAST_PLACED.lock().unwrap_or_else(|p| p.into_inner());
        if !due(now, *last, self.slider("Delay (ticks)", 4.0).round() as u64) {
            return Ok(());
        }
        *last = Some(now);
        drop(last);

        let [x, y, z] = target;
        let pos = BlockPos::new(x, y, z);
        let hit = Vec3::new(x as f64 + 0.5, y as f64 + 0.5, z as f64 + 0.5);
        game_mode.place_block_on(&player, &pos, Direction::Up, hit)?;
        swing(&player)
    }

    fn get_module_data(&self) -> &ModuleData {
        &self.module
    }

    fn get_module_data_mut(&mut self) -> &mut ModuleData {
        &mut self.module
    }
}

/// The air block to place into and whether the use key is down — `None`
/// when there is nothing to do: no block in the main hand, the crosshair on
/// a block or entity, or the target already filled.
fn find_target(player: &LocalPlayer, range: f64) -> anyhow::Result<Option<([i32; 3], bool)>> {
    let world = minecraft().world()?;
    mapping().in_frame(|| {
        let Some(world) = world else { return Ok(None) };
        let mut env = mapping().get_env()?;
        let p = player.jni_ref().as_obj();
        if !holds_block(&mut env, p)? || !crosshair_misses(&mut env)? {
            return Ok(None);
        }
        let eye = raw::call(&mut env, p, "getEyePosition", "()Lnet/minecraft/world/phys/Vec3;", &[])?.l()?;
        let eye = read_vec(&mut env, &eye)?;
        let look = raw::call(&mut env, p, "getViewVector", "(F)Lnet/minecraft/world/phys/Vec3;", &[JValue::Float(1.0)])?.l()?;
        let look = read_vec(&mut env, &look)?;
        let target = air_target(eye, look, range);
        if !replaceable(&mut env, world.jni_ref().as_obj(), target)? {
            return Ok(None);
        }
        Ok(Some((target, use_key_down(&mut env)?)))
    })
}

/// Whether the main hand holds a `BlockItem`.
fn holds_block(env: &mut JNIEnv, player: &JObject) -> anyhow::Result<bool> {
    let stack = raw::call(env, player, "getMainHandItem", &format!("(){}", crate::module::player::hands::STACK_SIG), &[])?.l()?;
    if raw::call(env, &stack, "isEmpty", "()Z", &[])?.z()? {
        return Ok(false);
    }
    let item = raw::call(env, &stack, "getItem", "()Lnet/minecraft/world/item/Item;", &[])?.l()?;
    let block_item = mapping().resolve_class(env, "net/minecraft/world/item/BlockItem")?;
    Ok(env.is_instance_of(&item, &block_item)?)
}

/// Whether the crosshair hits nothing (`HitResult.Type.MISS`).
fn crosshair_misses(env: &mut JNIEnv) -> anyhow::Result<bool> {
    let hit = raw::field(env, minecraft().jni_ref().as_obj(), "hitResult", "Lnet/minecraft/world/phys/HitResult;")?.l()?;
    if hit.is_null() {
        return Ok(true);
    }
    let kind = raw::call(env, &hit, "getType", "()Lnet/minecraft/world/phys/HitResult$Type;", &[])?.l()?;
    Ok(raw::to_string(env, &kind)? == "MISS")
}

/// Whether a block can be placed into `pos` (air, water, tall grass, …).
fn replaceable(env: &mut JNIEnv, level: &JObject, [x, y, z]: [i32; 3]) -> anyhow::Result<bool> {
    let pos = BlockPos::new(x, y, z).to_java(env)?;
    let state = raw::call(
        env,
        level,
        "getBlockState",
        "(Lnet/minecraft/core/BlockPos;)Lnet/minecraft/world/level/block/state/BlockState;",
        &[JValue::Object(&pos)],
    )?
    .l()?;
    if let Ok(value) = raw::call(env, &state, "canBeReplaced", "()Z", &[]) {
        return Ok(value.z()?);
    }
    Ok(raw::call(env, &state, "isAir", "()Z", &[])?.z()?)
}

/// Whether the use key (right mouse by default) is held.
fn use_key_down(env: &mut JNIEnv) -> anyhow::Result<bool> {
    let options = raw::field(env, minecraft().jni_ref().as_obj(), "options", "Lnet/minecraft/client/Options;")?.l()?;
    let key = raw::field(env, &options, "keyUse", "Lnet/minecraft/client/KeyMapping;")?.l()?;
    Ok(raw::call(env, &key, "isDown", "()Z", &[])?.z()?)
}

fn read_vec(env: &mut JNIEnv, vec: &JObject) -> anyhow::Result<[f64; 3]> {
    let mut out = [0.0; 3];
    for (slot, field) in out.iter_mut().zip(["x", "y", "z"]) {
        *slot = raw::field(env, vec, field, "D")?.d()?;
    }
    Ok(out)
}

/// Swings the main hand, as a normal placement does.
fn swing(player: &LocalPlayer) -> anyhow::Result<()> {
    mapping().in_frame(|| {
        let mut env = mapping().get_env()?;
        let hand_class = mapping().resolve_class(&mut env, "net/minecraft/world/InteractionHand")?;
        let hand = env
            .get_static_field(&hand_class, "MAIN_HAND", "Lnet/minecraft/world/InteractionHand;")?
            .l()?;
        raw::call(
            &mut env,
            player.jni_ref().as_obj(),
            "swing",
            "(Lnet/minecraft/world/InteractionHand;)V",
            &[JValue::Object(&hand)],
        )?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_target_is_range_blocks_along_the_look() {
        assert_eq!(air_target([0.5, 65.62, 0.5], [0.0, 0.0, 1.0], 4.0), [0, 65, 4]);
        assert_eq!(air_target([0.5, 65.62, 0.5], [0.0, -1.0, 0.0], 3.0), [0, 62, 0]);
        assert_eq!(air_target([0.5, 65.0, 0.5], [-1.0, 0.0, 0.0], 2.0), [-2, 65, 0]);
    }

    #[test]
    fn placements_wait_for_the_delay() {
        assert!(due(10, None, 4));
        assert!(!due(12, Some(10), 4));
        assert!(due(14, Some(10), 4));
        // Delay 0 still places at most once a tick.
        assert!(!due(10, Some(10), 0));
        assert!(due(11, Some(10), 0));
        // The clock restarted (re-inject): not stuck.
        assert!(due(3, Some(10), 4));
    }
}
