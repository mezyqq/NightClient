//! FarmHelper — replants what you harvest. Break a crop and the matching
//! seed from the hotbar goes straight back onto the farmland.
//!
//! Crops break instantly, so instead of watching the dig, the module
//! remembers crops recently under the crosshair; when one of them turns to
//! air with its soil still there, it is replanted.
//!
//! The crosshair is read **every frame** ([`frame`]), not every tick: when a
//! row is swept fast (held button, FastPlace), a crop is often targeted and
//! broken within the same tick, and a per-tick look would only ever see the
//! air left behind. All replants of a tick share one hotbar switch per seed
//! and a single switch back.

use crate::mapping::block::Direction;
use crate::mapping::client::world::World;
use crate::mapping::entity::player::LocalPlayer;
use crate::mapping::entity::EntityRef;
use crate::mapping::math::{BlockPos, Vec3};
use crate::mapping::raw;
use crate::mapping::MappedObject;
use crate::module::player::hands;
use crate::module::{KeyboardKey, Module, ModuleCategory, ModuleData, ModuleId, ModuleSetting};
use crate::state::{mapping, minecraft};
use jni::objects::JValue;
use jni::JNIEnv;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Crop block → (seed item, soil block).
const CROPS: [(&str, &str, &str); 6] = [
    ("minecraft:wheat", "minecraft:wheat_seeds", "minecraft:farmland"),
    ("minecraft:carrots", "minecraft:carrot", "minecraft:farmland"),
    ("minecraft:potatoes", "minecraft:potato", "minecraft:farmland"),
    ("minecraft:beetroots", "minecraft:beetroot_seeds", "minecraft:farmland"),
    ("minecraft:torchflower_crop", "minecraft:torchflower_seeds", "minecraft:farmland"),
    ("minecraft:nether_wart", "minecraft:nether_wart", "minecraft:soul_sand"),
];
/// How long a crop seen under the crosshair stays watched.
const WATCH_FOR: Duration = Duration::from_millis(1500);
/// Farthest replant, in blocks from the eyes.
const REACH: f64 = 4.5;

#[derive(Debug, Clone)]
struct Watched {
    pos: [i32; 3],
    seed: &'static str,
    soil: &'static str,
    seen: Instant,
}

/// Crops recently under the crosshair, filled by [`frame`].
struct Tracker {
    /// The block the crosshair was on last frame, so each block is looked up
    /// once, not every frame.
    last: Option<[i32; 3]>,
    watched: Vec<Watched>,
}

static TRACKER: Mutex<Tracker> = Mutex::new(Tracker { last: None, watched: Vec::new() });
/// Whether the module is running — read by the frame hook.
static RUNNING: AtomicBool = AtomicBool::new(false);

/// Pure: whether `actual` is the soil `wanted` — any farmland (Farmer's
/// Delight rich soil farmland too) counts as farmland.
pub fn soil_fits(wanted: &str, actual: &str) -> bool {
    if wanted.ends_with(":farmland") {
        actual.ends_with("farmland")
    } else {
        actual == wanted
    }
}

/// Pure: whether a block id is air of any kind.
pub fn is_air(id: &str) -> bool {
    id.ends_with(":air") || id.ends_with("_air")
}

#[derive(Debug)]
pub struct FarmHelperModule {
    pub module: ModuleData,
}

impl FarmHelperModule {
    pub fn new() -> Self {
        Self {
            module: ModuleData {
                id: ModuleId::FarmHelper,
                description: "Replants the seed when you harvest a crop".to_string(),
                category: ModuleCategory::World,
                key_bind: KeyboardKey::KeyNone,
                enabled: false,
                settings: vec![ModuleSetting::Toggle {
                    name: "Switch back".to_string(),
                    value: true,
                }],
            },
        }
    }
}

impl Module for FarmHelperModule {
    fn on_start(&self) -> anyhow::Result<()> {
        RUNNING.store(true, Ordering::Relaxed);
        Ok(())
    }

    fn on_stop(&self) -> anyhow::Result<()> {
        RUNNING.store(false, Ordering::Relaxed);
        let mut tracker = TRACKER.lock().unwrap_or_else(|p| p.into_inner());
        tracker.watched.clear();
        tracker.last = None;
        Ok(())
    }

    fn on_tick(&self) -> anyhow::Result<()> {
        let mc = minecraft();
        let (Some(player), Some(world), Some(game_mode)) = (mc.player()?, mc.world()?, mc.game_mode()?) else {
            return Ok(());
        };
        let switch_back = self
            .module
            .get_setting("Switch back")
            .and_then(ModuleSetting::get_toggle_value)
            .unwrap_or(true);

        let watched = {
            let mut tracker = TRACKER.lock().unwrap_or_else(|p| p.into_inner());
            tracker.watched.retain(|w| w.seen.elapsed() < WATCH_FOR);
            tracker.watched.clone()
        };
        if watched.is_empty() {
            return Ok(());
        }

        let eye = player.get_eye_position()?;
        let mut due = Vec::new();
        for entry in watched {
            let [x, y, z] = entry.pos;
            if !is_air(&block_id(&world, entry.pos)?) {
                continue;
            }
            let distance = ((x as f64 + 0.5 - eye.x()).powi(2)
                + (y as f64 - eye.y()).powi(2)
                + (z as f64 + 0.5 - eye.z()).powi(2))
            .sqrt();
            if distance <= REACH && soil_fits(entry.soil, &block_id(&world, [x, y - 1, z])?) {
                due.push(entry);
            }
        }
        // Broken spots are done with, replanted or not.
        {
            let mut tracker = TRACKER.lock().unwrap_or_else(|p| p.into_inner());
            tracker.watched.retain(|w| !due.iter().any(|d| d.pos == w.pos));
        }
        if !due.is_empty() {
            if let Err(error) = replant_all(&player, &game_mode, &due, switch_back) {
                log::debug!("FarmHelper: {error}");
            }
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

/// Per frame, from the frame hook: remembers the crop under the crosshair.
pub fn frame() {
    if !RUNNING.load(Ordering::Relaxed) {
        return;
    }
    let result = (|| -> anyhow::Result<()> {
        let Some(pos) = looked_at_block()? else {
            TRACKER.lock().unwrap_or_else(|p| p.into_inner()).last = None;
            return Ok(());
        };
        let mut tracker = TRACKER.lock().unwrap_or_else(|p| p.into_inner());
        if tracker.last == Some(pos) {
            // Still on it — keep it fresh.
            if let Some(entry) = tracker.watched.iter_mut().find(|w| w.pos == pos) {
                entry.seen = Instant::now();
            }
            return Ok(());
        }
        tracker.last = Some(pos);
        drop(tracker);
        let Some(world) = minecraft().world()? else {
            return Ok(());
        };
        let id = block_id(&world, pos)?;
        if let Some((_, seed, soil)) = CROPS.iter().find(|(crop, _, _)| *crop == id) {
            let mut tracker = TRACKER.lock().unwrap_or_else(|p| p.into_inner());
            tracker.watched.retain(|w| w.pos != pos);
            tracker.watched.push(Watched { pos, seed, soil, seen: Instant::now() });
        }
        Ok(())
    })();
    if let Err(error) = result {
        log::debug!("FarmHelper: {error}");
    }
}

/// Replants every spot in `due`: one hotbar switch per seed kind, one
/// switch back at the end.
fn replant_all(
    player: &LocalPlayer,
    game_mode: &crate::mapping::client::gamemode::MultiPlayerGameMode,
    due: &[Watched],
    switch_back: bool,
) -> anyhow::Result<()> {
    let previous = mapping().in_frame(|| {
        let mut env = mapping().get_env()?;
        let inventory = hands::inventory(&mut env, player.jni_ref().as_obj())?;
        hands::selected_slot(&mut env, &inventory)
    })?;
    let mut selected = previous;
    let mut seeds: Vec<&str> = due.iter().map(|w| w.seed).collect();
    seeds.sort_unstable();
    seeds.dedup();
    for seed in seeds {
        let Some(slot) = select_seed(player, seed, selected)? else {
            continue; // none in the hotbar
        };
        selected = slot;
        for entry in due.iter().filter(|w| w.seed == seed) {
            let [x, y, z] = entry.pos;
            let soil = BlockPos::new(x, y - 1, z);
            let hit = Vec3::new(x as f64 + 0.5, y as f64, z as f64 + 0.5);
            if let Err(error) = game_mode.place_block_on(player, &soil, Direction::Up, hit) {
                log::debug!("FarmHelper: replant at {x} {y} {z}: {error}");
            }
        }
    }
    if switch_back && selected != previous {
        mapping().in_frame(|| {
            let mut env = mapping().get_env()?;
            let inventory = hands::inventory(&mut env, player.jni_ref().as_obj())?;
            hands::set_selected_slot(&mut env, &inventory, previous)
        })?;
    }
    Ok(())
}

/// Selects the hotbar slot holding `seed` (the selected one first) and
/// returns it, or `None` without changing anything.
fn select_seed(player: &LocalPlayer, seed: &str, selected: i32) -> anyhow::Result<Option<i32>> {
    mapping().in_frame(|| {
        let mut env = mapping().get_env()?;
        let inventory = hands::inventory(&mut env, player.jni_ref().as_obj())?;
        for slot in std::iter::once(selected).chain((0..9).filter(|s| *s != selected)) {
            let matches = env.with_local_frame(4, |env| -> anyhow::Result<bool> {
                let stack = hands::stack_in(env, &inventory, slot)?;
                Ok(!raw::call(env, &stack, "isEmpty", "()Z", &[])?.z()? && hands::item_id(env, &stack)? == seed)
            })?;
            if matches {
                if slot != selected {
                    hands::set_selected_slot(&mut env, &inventory, slot)?;
                }
                return Ok(Some(slot));
            }
        }
        Ok(None)
    })
}

/// The id of the block at `pos`, e.g. `minecraft:wheat`.
fn block_id(world: &World, [x, y, z]: [i32; 3]) -> anyhow::Result<String> {
    mapping().in_frame(|| {
        let mut env = mapping().get_env()?;
        let pos = BlockPos::new(x, y, z).to_java(&mut env)?;
        let state = raw::call(
            &mut env,
            world.jni_ref().as_obj(),
            "getBlockState",
            "(Lnet/minecraft/core/BlockPos;)Lnet/minecraft/world/level/block/state/BlockState;",
            &[JValue::Object(&pos)],
        )?
        .l()?;
        let text = raw::to_string(&mut env, &state)?;
        Ok(state_block_id(&text).unwrap_or(text))
    })
}

/// `Block{minecraft:wheat}[age=7]` → `minecraft:wheat`.
fn state_block_id(state: &str) -> Option<String> {
    let start = state.find('{')? + 1;
    let end = start + state[start..].find('}')?;
    Some(state[start..end].to_string())
}

/// The block under the crosshair, if any.
fn looked_at_block() -> anyhow::Result<Option<[i32; 3]>> {
    mapping().in_frame(|| {
        let mut env = mapping().get_env()?;
        looked_at(&mut env)
    })
}

fn looked_at(env: &mut JNIEnv) -> anyhow::Result<Option<[i32; 3]>> {
    let hit = raw::field(env, minecraft().jni_ref().as_obj(), "hitResult", "Lnet/minecraft/world/phys/HitResult;")?.l()?;
    if hit.is_null() {
        return Ok(None);
    }
    let kind = raw::call(env, &hit, "getType", "()Lnet/minecraft/world/phys/HitResult$Type;", &[])?.l()?;
    if raw::to_string(env, &kind)? != "BLOCK" {
        return Ok(None);
    }
    let pos = raw::call(env, &hit, "getBlockPos", "()Lnet/minecraft/core/BlockPos;", &[])?.l()?;
    let mut xyz = [0; 3];
    for (slot, name) in xyz.iter_mut().zip(["getX", "getY", "getZ"]) {
        *slot = raw::call(env, &pos, name, "()I", &[])?.i()?;
    }
    Ok(Some(xyz))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_id_is_read_from_a_state() {
        assert_eq!(state_block_id("Block{minecraft:wheat}[age=7]").as_deref(), Some("minecraft:wheat"));
        assert_eq!(state_block_id("Block{minecraft:air}").as_deref(), Some("minecraft:air"));
    }

    #[test]
    fn any_farmland_is_farmland() {
        assert!(soil_fits("minecraft:farmland", "minecraft:farmland"));
        assert!(soil_fits("minecraft:farmland", "farmersdelight:rich_soil_farmland"));
        assert!(!soil_fits("minecraft:farmland", "minecraft:dirt"));
        assert!(soil_fits("minecraft:soul_sand", "minecraft:soul_sand"));
        assert!(!soil_fits("minecraft:soul_sand", "minecraft:farmland"));
    }

    #[test]
    fn air_kinds_are_air() {
        assert!(is_air("minecraft:air"));
        assert!(is_air("minecraft:cave_air"));
        assert!(!is_air("minecraft:wheat"));
    }
}
