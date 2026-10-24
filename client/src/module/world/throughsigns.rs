//! Open Through Signs — right-clicking a sign or an item frame on a chest
//! opens the chest instead of editing the sign / turning the framed item.
//! Wall signs open the block they hang on, standing signs the block below,
//! item frames the block they are fixed to (any side, top included). Hold
//! Shift for the normal sign / frame behaviour.
//!
//! Hooked into the right mouse button (see `graphic::input`): when the
//! crosshair is on such a sign or frame, the click is used on the container
//! directly and never reaches the game.

use crate::mapping::block::Direction;
use crate::mapping::entity::{EntityRef, LivingEntityRef};
use crate::mapping::math::{BlockPos, Vec3};
use crate::mapping::raw;
use crate::mapping::MappedObject;
use crate::module::{KeyboardKey, Module, ModuleCategory, ModuleData, ModuleId};
use crate::state::{client, mapping, minecraft};
use jni::objects::{JObject, JValue};
use jni::JNIEnv;

#[derive(Debug)]
pub struct ThroughSignsModule {
    pub module: ModuleData,
}

impl ThroughSignsModule {
    pub fn new() -> Self {
        Self {
            module: ModuleData {
                id: ModuleId::ThroughSigns,
                description: "Right-click a sign or item frame on a chest to open it".to_string(),
                category: ModuleCategory::World,
                key_bind: KeyboardKey::KeyNone,
                enabled: false,
                settings: vec![],
            },
        }
    }
}

impl Module for ThroughSignsModule {
    fn on_start(&self) -> anyhow::Result<()> {
        Ok(())
    }

    fn on_stop(&self) -> anyhow::Result<()> {
        Ok(())
    }

    fn on_tick(&self) -> anyhow::Result<()> {
        Ok(())
    }

    fn get_module_data(&self) -> &ModuleData {
        &self.module
    }

    fn get_module_data_mut(&mut self) -> &mut ModuleData {
        &mut self.module
    }
}

fn enabled() -> bool {
    client()
        .modules
        .get(ModuleId::ThroughSigns)
        .and_then(|handle| handle.lock().ok().map(|m| m.get_module_data().enabled))
        .unwrap_or(false)
}

/// Right mouse button pressed in the world. Opens the container behind the
/// targeted sign and returns `true` when it did — the click must then not
/// reach the game.
pub fn intercept_use() -> bool {
    if !enabled() || !client().modules.is_active() || !minecraft().current_screen_is_null() {
        return false;
    }
    match try_open() {
        Ok(opened) => opened,
        Err(error) => {
            log::debug!("ThroughSigns: {error}");
            false
        }
    }
}

fn try_open() -> anyhow::Result<bool> {
    let mc = minecraft();
    let (Some(player), Some(world), Some(game_mode)) = (mc.player()?, mc.world()?, mc.game_mode()?) else {
        return Ok(false);
    };
    if player.is_shift_key_down()? {
        return Ok(false); // Shift: the sign / frame as usual
    }
    let target = mapping().in_frame(|| {
        let mut env = mapping().get_env()?;
        let level = world.jni_ref().as_obj();
        let Some((container, face)) = covered_block(&mut env, level)? else {
            return Ok(None);
        };
        Ok(is_container(&mut env, level, container)?.then_some((container, face)))
    })?;
    let Some(([x, y, z], face)) = target else {
        return Ok(false);
    };

    // Use the container's face the sign / frame sits on, at its centre.
    let (dx, dy, dz) = face.offset();
    let hit = Vec3::new(
        x as f64 + 0.5 + dx as f64 * 0.5,
        y as f64 + 0.5 + dy as f64 * 0.5,
        z as f64 + 0.5 + dz as f64 * 0.5,
    );
    game_mode.place_block_on(&player, &BlockPos::new(x, y, z), face, hit)?;
    player.swing()?;
    Ok(true)
}

/// For a sign's block-state text, the block it is attached to and the face
/// of that block it covers: a wall sign hangs on the block behind it, a
/// standing sign stands on the block below. `None` for anything else.
fn attached_block(state: &str, [x, y, z]: [i32; 3]) -> Option<([i32; 3], Direction)> {
    let id = state.split(['{', '}']).nth(1)?;
    if id.ends_with("_wall_sign") && !id.ends_with("_wall_hanging_sign") {
        // `facing` is the way the sign looks; it is attached behind.
        let facing = state.split("facing=").nth(1)?.split([',', ']']).next()?;
        let face = match facing {
            "north" => Direction::North,
            "south" => Direction::South,
            "west" => Direction::West,
            "east" => Direction::East,
            _ => return None,
        };
        let (dx, dy, dz) = face.opposite().offset();
        return Some(([x + dx, y + dy, z + dz], face));
    }
    if id.ends_with("_sign") && !id.contains("hanging") {
        return Some(([x, y - 1, z], Direction::Up));
    }
    None
}

/// The block hidden behind what the crosshair is on — a sign (block hit) or
/// an item frame (entity hit) — and the face of it that is covered.
fn covered_block(env: &mut JNIEnv, level: &JObject) -> anyhow::Result<Option<([i32; 3], Direction)>> {
    let hit = raw::field(env, minecraft().jni_ref().as_obj(), "hitResult", "Lnet/minecraft/world/phys/HitResult;")?.l()?;
    if hit.is_null() {
        return Ok(None);
    }
    let kind = raw::call(env, &hit, "getType", "()Lnet/minecraft/world/phys/HitResult$Type;", &[])?.l()?;
    match raw::to_string(env, &kind)?.as_str() {
        "BLOCK" => {
            let pos = raw::call(env, &hit, "getBlockPos", "()Lnet/minecraft/core/BlockPos;", &[])?.l()?;
            let sign = read_pos(env, &pos)?;
            let state = block_state_text(env, level, sign)?;
            Ok(attached_block(&state, sign))
        }
        "ENTITY" => {
            let entity = raw::call(env, &hit, "getEntity", "()Lnet/minecraft/world/entity/Entity;", &[])?.l()?;
            // Glow item frames are an `ItemFrame` subclass.
            let frame_class = mapping().resolve_class(env, "net/minecraft/world/entity/decoration/ItemFrame")?;
            if !env.is_instance_of(&entity, &frame_class)? {
                return Ok(None);
            }
            let pos = raw::call(env, &entity, "getPos", "()Lnet/minecraft/core/BlockPos;", &[])?.l()?;
            let frame = read_pos(env, &pos)?;
            let facing = raw::call(env, &entity, "getDirection", "()Lnet/minecraft/core/Direction;", &[])?.l()?;
            let facing = raw::to_string(env, &facing)?;
            Ok(frame_attachment(frame, &facing))
        }
        _ => Ok(None),
    }
}

fn read_pos(env: &mut JNIEnv, pos: &JObject) -> anyhow::Result<[i32; 3]> {
    let mut xyz = [0; 3];
    for (slot, name) in xyz.iter_mut().zip(["getX", "getY", "getZ"]) {
        *slot = raw::call(env, pos, name, "()I", &[])?.i()?;
    }
    Ok(xyz)
}

/// An item frame at `pos` looking `facing` (`north`, `up`, …) is fixed to
/// the block behind it, covering that block's `facing` face.
fn frame_attachment([x, y, z]: [i32; 3], facing: &str) -> Option<([i32; 3], Direction)> {
    let face = match facing {
        "down" => Direction::Down,
        "up" => Direction::Up,
        "north" => Direction::North,
        "south" => Direction::South,
        "west" => Direction::West,
        "east" => Direction::East,
        _ => return None,
    };
    let (dx, dy, dz) = face.opposite().offset();
    Some(([x + dx, y + dy, z + dz], face))
}

fn block_state_text(env: &mut JNIEnv, level: &JObject, [x, y, z]: [i32; 3]) -> anyhow::Result<String> {
    let pos = BlockPos::new(x, y, z).to_java(env)?;
    let state = raw::call(
        env,
        level,
        "getBlockState",
        "(Lnet/minecraft/core/BlockPos;)Lnet/minecraft/world/level/block/state/BlockState;",
        &[JValue::Object(&pos)],
    )?
    .l()?;
    raw::to_string(env, &state)
}

/// Whether the block at `pos` holds items: a `Container` block entity
/// (chest, barrel, shulker box, hopper, …) or an ender chest.
fn is_container(env: &mut JNIEnv, level: &JObject, [x, y, z]: [i32; 3]) -> anyhow::Result<bool> {
    let pos = BlockPos::new(x, y, z).to_java(env)?;
    let entity = raw::call(
        env,
        level,
        "getBlockEntity",
        "(Lnet/minecraft/core/BlockPos;)Lnet/minecraft/world/level/block/entity/BlockEntity;",
        &[JValue::Object(&pos)],
    )?
    .l()?;
    if entity.is_null() {
        return Ok(false);
    }
    for class in [
        "net/minecraft/world/Container",
        "net/minecraft/world/level/block/entity/EnderChestBlockEntity",
    ] {
        if let Ok(class) = mapping().resolve_class(env, class) {
            if env.is_instance_of(&entity, &class)? {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wall_sign_hangs_on_the_block_behind_it() {
        let state = "Block{minecraft:oak_wall_sign}[facing=north,waterlogged=false]";
        let (pos, face) = attached_block(state, [10, 64, 10]).unwrap();
        // Facing north (−z): the chest is one block south.
        assert_eq!(pos, [10, 64, 11]);
        assert_eq!(face, Direction::North);
    }

    #[test]
    fn a_standing_sign_stands_on_the_block_below() {
        let state = "Block{minecraft:spruce_sign}[rotation=4,waterlogged=false]";
        assert_eq!(attached_block(state, [0, 70, 0]), Some(([0, 69, 0], Direction::Up)));
    }

    #[test]
    fn an_item_frame_covers_the_block_behind_it() {
        // On the south side of a chest, looking south.
        assert_eq!(frame_attachment([5, 64, 6], "south"), Some(([5, 64, 5], Direction::South)));
        // Lying on top of a chest, looking up.
        assert_eq!(frame_attachment([5, 65, 5], "up"), Some(([5, 64, 5], Direction::Up)));
        assert_eq!(frame_attachment([0, 0, 0], "sideways"), None);
    }

    #[test]
    fn other_blocks_are_ignored() {
        assert_eq!(attached_block("Block{minecraft:chest}[facing=north]", [0, 0, 0]), None);
        assert_eq!(
            attached_block("Block{minecraft:oak_wall_hanging_sign}[facing=north]", [0, 0, 0]),
            None
        );
    }
}
