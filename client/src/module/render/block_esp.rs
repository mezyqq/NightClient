//! Block ESP — highlights chosen blocks (ores, spawners, …) with wireframe
//! boxes. The blocks are picked from a searchable list in the Block ESP tool
//! window and saved to `block_esp.json` beside the client config.
//!
//! # Scanning
//!
//! Probing every block through JNI would cost hundreds of thousands of calls
//! per pass, so the scan reads chunk storage wholesale instead: for each
//! non-empty `LevelChunkSection` it fetches the packed `long[]` of palette
//! indices with a single `BitStorage.getRaw()` call, decodes it in Rust, and
//! asks the palette only about the handful of distinct indices in use. A
//! section without a wanted block is ruled out after a few dozen calls.
//!
//! The scan runs on its own thread (started in `on_start`, joined in
//! `on_stop`) so it never costs frame time; [`crate::graphic::esp`] draws the
//! latest result every frame.

use crate::config;
use crate::mapping::client::world::LevelChunk;
use crate::mapping::entity::EntityRef;
use crate::mapping::raw;
use crate::mapping::MappedObject;
use crate::module::{KeyboardKey, Module, ModuleCategory, ModuleData, ModuleId, ModuleSetting};
use crate::state::{mapping, minecraft};
use jni::objects::{JLongArray, JObject, JObjectArray, JValue};
use jni::JNIEnv;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// File, in the config directory, holding the chosen block ids.
const SELECTION_FILE: &str = "block_esp.json";
/// Blocks highlighted until the user picks their own.
const DEFAULT_BLOCKS: [&str; 4] = [
    "minecraft:diamond_ore",
    "minecraft:deepslate_diamond_ore",
    "minecraft:ancient_debris",
    "minecraft:spawner",
];
/// Pause between two scan passes.
const SCAN_INTERVAL: Duration = Duration::from_millis(1000);
/// Hard cap on blocks collected per pass — picking `stone` must not eat memory.
const MAX_FOUND: usize = 20_000;
/// Blocks per chunk section (16³), in `(y << 8) | (z << 4) | x` order.
const SECTION_VOLUME: usize = 4096;

const SECTIONS_SIG: &str = "()[Lnet/minecraft/world/level/chunk/LevelChunkSection;";
const STATES_SIG: &str = "()Lnet/minecraft/world/level/chunk/PalettedContainer;";
const DATA_SIG: &str = "Lnet/minecraft/world/level/chunk/PalettedContainer$Data;";
const PALETTE_SIG: &str = "()Lnet/minecraft/world/level/chunk/Palette;";
const STORAGE_SIG: &str = "()Lnet/minecraft/util/BitStorage;";

/// One highlighted block.
#[derive(Debug, Clone)]
pub struct FoundBlock {
    pub pos: [i32; 3],
    /// The block id, e.g. `minecraft:diamond_ore`.
    pub id: Arc<str>,
}

/// Scan radius in chunks — mirrored from the module's slider each tick, so
/// the scanner thread never has to lock the module.
static RADIUS: AtomicI32 = AtomicI32::new(4);
/// The latest scan result, nearest block first.
static FOUND: Mutex<Vec<FoundBlock>> = Mutex::new(Vec::new());
/// The chosen block ids; `None` until first read from disk.
static SELECTED: Mutex<Option<BTreeSet<String>>> = Mutex::new(None);

/// Runs `f` on the latest scan result.
pub fn with_found<T>(f: impl FnOnce(&[FoundBlock]) -> T) -> T {
    let found = FOUND.lock().unwrap_or_else(|poison| poison.into_inner());
    f(&found)
}

/// The chosen block ids.
pub fn selected() -> BTreeSet<String> {
    let mut guard = SELECTED.lock().unwrap_or_else(|poison| poison.into_inner());
    guard.get_or_insert_with(load_selection).clone()
}

/// Adds `id` to the selection, or removes it if already chosen.
pub fn toggle_selected(id: &str) {
    edit_selection(|set| {
        if !set.remove(id) {
            set.insert(id.to_string());
        }
    });
}

/// Empties the selection.
pub fn clear_selected() {
    edit_selection(BTreeSet::clear);
}

fn edit_selection(f: impl FnOnce(&mut BTreeSet<String>)) {
    let mut guard = SELECTED.lock().unwrap_or_else(|poison| poison.into_inner());
    let set = guard.get_or_insert_with(load_selection);
    f(set);
    let json = serde_json::to_string_pretty(set).unwrap_or_else(|_| "[]".into());
    let path = config::base_dir().join(SELECTION_FILE);
    if let Err(error) = std::fs::write(&path, json) {
        log::warn!("Block ESP: could not write {}: {error}", path.display());
    }
}

fn load_selection() -> BTreeSet<String> {
    std::fs::read_to_string(config::base_dir().join(SELECTION_FILE))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| DEFAULT_BLOCKS.iter().map(|id| id.to_string()).collect())
}

/// The background scanner of a running module.
#[derive(Debug)]
struct Scanner {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

#[derive(Debug)]
pub struct BlockEspModule {
    pub module: ModuleData,
    scanner: Mutex<Option<Scanner>>,
}

impl BlockEspModule {
    pub fn new() -> Self {
        Self {
            module: ModuleData {
                id: ModuleId::BlockEsp,
                description: "Draws a 3D box around chosen blocks (ores, spawners, …)".to_string(),
                category: ModuleCategory::Render,
                key_bind: KeyboardKey::KeyNone,
                enabled: false,
                settings: vec![
                    ModuleSetting::Color {
                        name: "Color".to_string(),
                        value: [0.3, 0.85, 1.0, 1.0],
                    },
                    ModuleSetting::Slider {
                        name: "Range (chunks)".to_string(),
                        value: 4.0,
                        min: 1.0,
                        max: 12.0,
                    },
                    ModuleSetting::Slider {
                        name: "Max boxes".to_string(),
                        value: 300.0,
                        min: 50.0,
                        max: 2000.0,
                    },
                ],
            },
            scanner: Mutex::new(None),
        }
    }

    fn sync_radius(&self) {
        if let Some(value) = self
            .module
            .get_setting("Range (chunks)")
            .and_then(ModuleSetting::get_slider_value)
        {
            RADIUS.store(value.round() as i32, Ordering::Relaxed);
        }
    }
}

impl Module for BlockEspModule {
    fn on_start(&self) -> anyhow::Result<()> {
        self.sync_radius();
        let mut slot = self.scanner.lock().unwrap();
        if slot.is_some() {
            return Ok(());
        }
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("dark-block-esp".into())
            .spawn(move || scan_loop(&thread_stop))?;
        *slot = Some(Scanner { stop, thread });
        Ok(())
    }

    fn on_stop(&self) -> anyhow::Result<()> {
        // Joined, not detached: the library may be unloaded right after this
        // (hot-reload), and the thread must not outlive its code.
        if let Some(scanner) = self.scanner.lock().unwrap().take() {
            scanner.stop.store(true, Ordering::SeqCst);
            let _ = scanner.thread.join();
        }
        FOUND.lock().unwrap_or_else(|p| p.into_inner()).clear();
        Ok(())
    }

    fn on_tick(&self) -> anyhow::Result<()> {
        self.sync_radius();
        Ok(())
    }

    fn get_module_data(&self) -> &ModuleData {
        &self.module
    }

    fn get_module_data_mut(&mut self) -> &mut ModuleData {
        &mut self.module
    }
}

// --- scanner ---------------------------------------------------------------

fn scan_loop(stop: &AtomicBool) {
    let mut logged_error = false;
    while !stop.load(Ordering::SeqCst) {
        match scan_pass(stop) {
            Ok(Some(found)) => *FOUND.lock().unwrap_or_else(|p| p.into_inner()) = found,
            Ok(None) => {} // interrupted — keep the previous result
            Err(error) => {
                if !logged_error {
                    log::warn!("Block ESP: scan failed: {error}");
                    logged_error = true;
                }
            }
        }
        let resume = Instant::now() + SCAN_INTERVAL;
        while Instant::now() < resume && !stop.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// One pass over every chunk in range. `Ok(None)` when stopped midway.
fn scan_pass(stop: &AtomicBool) -> anyhow::Result<Option<Vec<FoundBlock>>> {
    let targets = selected();
    if targets.is_empty() {
        return Ok(Some(Vec::new()));
    }
    let mc = minecraft();
    let (Some(world), Some(player)) = (mc.world()?, mc.player()?) else {
        return Ok(Some(Vec::new()));
    };
    let pos = player.get_position()?;
    let (px, py, pz) = (pos.x(), pos.y(), pos.z());
    let pcx = (px / 16.0).floor() as i32;
    let pcz = (pz / 16.0).floor() as i32;
    let radius = RADIUS.load(Ordering::Relaxed).clamp(1, 32);

    let mut out = Vec::new();
    for cx in (pcx - radius)..=(pcx + radius) {
        for cz in (pcz - radius)..=(pcz + radius) {
            if stop.load(Ordering::SeqCst) {
                return Ok(None);
            }
            if out.len() >= MAX_FOUND {
                break;
            }
            if let Some(chunk) = world.get_chunk(cx, cz)? {
                scan_chunk(&chunk, cx, cz, &targets, &mut out)?;
            }
        }
    }

    let distance_sq = |block: &FoundBlock| {
        let dx = block.pos[0] as f64 + 0.5 - px;
        let dy = block.pos[1] as f64 + 0.5 - py;
        let dz = block.pos[2] as f64 + 0.5 - pz;
        dx * dx + dy * dy + dz * dz
    };
    out.sort_by(|a, b| distance_sq(a).total_cmp(&distance_sq(b)));
    Ok(Some(out))
}

/// The `LevelHeightAccessor` method giving the lowest section index:
/// `getMinSectionY` from 1.21.2 on, `getMinSection` before.
fn min_section_y(env: &mut JNIEnv, chunk: &JObject) -> anyhow::Result<i32> {
    static NAME: OnceLock<&'static str> = OnceLock::new();
    if let Some(name) = NAME.get() {
        return Ok(raw::call(env, chunk, name, "()I", &[])?.i()?);
    }
    for name in ["getMinSectionY", "getMinSection"] {
        if let Ok(value) = raw::call(env, chunk, name, "()I", &[]) {
            let _ = NAME.set(name);
            return Ok(value.i()?);
        }
    }
    Err(anyhow::anyhow!("no getMinSectionY/getMinSection on the chunk"))
}

fn scan_chunk(
    chunk: &LevelChunk,
    cx: i32,
    cz: i32,
    targets: &BTreeSet<String>,
    out: &mut Vec<FoundBlock>,
) -> anyhow::Result<()> {
    mapping().in_frame(|| {
        let mut env = mapping().get_env()?;
        let chunk = chunk.jni_ref().as_obj();
        let min_section = min_section_y(&mut env, chunk)?;
        let sections = JObjectArray::from(raw::call(&mut env, chunk, "getSections", SECTIONS_SIG, &[])?.l()?);
        let count = env.get_array_length(&sections)?;
        for index in 0..count {
            env.with_local_frame(16, |env| -> anyhow::Result<()> {
                let section = env.get_object_array_element(&sections, index)?;
                if section.is_null() {
                    return Ok(());
                }
                let origin = [cx * 16, (min_section + index) * 16, cz * 16];
                scan_section(env, &section, origin, targets, out)
            })?;
        }
        Ok(())
    })
}

fn scan_section(
    env: &mut JNIEnv,
    section: &JObject,
    origin: [i32; 3],
    targets: &BTreeSet<String>,
    out: &mut Vec<FoundBlock>,
) -> anyhow::Result<()> {
    if raw::call(env, section, "hasOnlyAir", "()Z", &[])?.z()? {
        return Ok(());
    }
    let states = raw::call(env, section, "getStates", STATES_SIG, &[])?.l()?;
    let data = raw::field(env, &states, "data", DATA_SIG)?.l()?;
    let palette = raw::call(env, &data, "palette", PALETTE_SIG, &[])?.l()?;
    let storage = raw::call(env, &data, "storage", STORAGE_SIG, &[])?.l()?;
    let bits = raw::call(env, &storage, "getBits", "()I", &[])?.i()? as u32;
    let packed = JLongArray::from(raw::call(env, &storage, "getRaw", "()[J", &[])?.l()?);
    let len = env.get_array_length(&packed)? as usize;
    let mut longs = vec![0i64; len];
    env.get_long_array_region(&packed, 0, &mut longs)?;
    let indices = unpack(&longs, bits);

    // Ask the palette only about the distinct indices actually in use.
    let used: HashSet<u32> = indices.iter().copied().collect();
    let mut wanted: HashMap<u32, Arc<str>> = HashMap::new();
    for id in used {
        let name = env.with_local_frame(4, |env| -> anyhow::Result<Option<String>> {
            let state = raw::call(env, &palette, "valueFor", "(I)Ljava/lang/Object;", &[JValue::Int(id as i32)])?.l()?;
            if state.is_null() {
                return Ok(None);
            }
            Ok(block_id(&raw::to_string(env, &state)?))
        });
        // A stale index can make `valueFor` throw; it simply matches nothing.
        if let Ok(Some(name)) = name {
            if targets.contains(&name) {
                wanted.insert(id, Arc::from(name));
            }
        }
    }
    if wanted.is_empty() {
        return Ok(());
    }

    for (i, id) in indices.iter().enumerate() {
        if out.len() >= MAX_FOUND {
            break;
        }
        if let Some(name) = wanted.get(id) {
            out.push(FoundBlock {
                pos: [
                    origin[0] + (i & 15) as i32,
                    origin[1] + (i >> 8) as i32,
                    origin[2] + ((i >> 4) & 15) as i32,
                ],
                id: Arc::clone(name),
            });
        }
    }
    Ok(())
}

/// Decodes a `SimpleBitStorage` array: values are packed `bits` wide, never
/// straddling two longs. `bits == 0` (a single-value palette) means every
/// entry is index 0.
fn unpack(longs: &[i64], bits: u32) -> Vec<u32> {
    if bits == 0 {
        return vec![0; SECTION_VOLUME];
    }
    let per_long = (64 / bits) as usize;
    let mask = if bits >= 32 { u32::MAX as u64 } else { (1u64 << bits) - 1 };
    (0..SECTION_VOLUME)
        .map(|i| {
            let word = longs.get(i / per_long).copied().unwrap_or(0) as u64;
            ((word >> ((i % per_long) as u32 * bits)) & mask) as u32
        })
        .collect()
}

/// The block id inside a `BlockState.toString()` — `Block{minecraft:furnace}[facing=north]`
/// gives `minecraft:furnace`.
fn block_id(state: &str) -> Option<String> {
    let start = state.find('{')? + 1;
    let end = start + state[start..].find('}')?;
    Some(state[start..end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_id_is_read_from_a_block_state() {
        assert_eq!(
            block_id("Block{minecraft:furnace}[facing=north,lit=false]").as_deref(),
            Some("minecraft:furnace")
        );
        assert_eq!(block_id("Block{minecraft:stone}").as_deref(), Some("minecraft:stone"));
        assert_eq!(block_id("garbage"), None);
    }

    #[test]
    fn unpack_reads_values_that_do_not_straddle_longs() {
        // 5 bits → 12 values per long; value i is i % 32.
        let bits = 5;
        let mut longs = vec![0i64; SECTION_VOLUME.div_ceil(12)];
        for i in 0..SECTION_VOLUME {
            let v = (i % 32) as u64;
            longs[i / 12] |= (v << ((i % 12) * 5)) as i64;
        }
        let values = unpack(&longs, bits);
        assert!(values.iter().enumerate().all(|(i, &v)| v == (i % 32) as u32));
    }

    #[test]
    fn unpack_of_zero_bits_is_all_index_zero() {
        assert!(unpack(&[], 0).iter().all(|&v| v == 0));
    }
}
