//! ItemESP — boxes and labels on items lying on the ground.
//!
//! Item entities are collected ten times a second (their item id, stack size
//! and name); positions are eased between collections, so falling or pushed
//! items move smoothly. "Only listed items" limits it to the ids picked in
//! the Item ESP window (stored in `optimization.json`, list `items`). The
//! drawing goes through the ESP's projection — see [`crate::graphic::esp`].

use crate::mapping::raw;
use crate::mapping::MappedObject;
use crate::module::optimization::lists::{self, List};
use crate::module::player::hands;
use crate::module::{KeyboardKey, Module, ModuleCategory, ModuleData, ModuleId, ModuleSetting};
use crate::state::{client, mapping, minecraft};
use egui::{Align2, Color32, FontId, Painter, Pos2, Stroke, Vec2};
use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const GATHER_EVERY: Duration = Duration::from_millis(100);
/// Half the size of an item's box.
const HALF: f64 = 0.18;

#[derive(Debug, Clone)]
struct Item {
    id: i32,
    prev: [f64; 3],
    pos: [f64; 3],
    label: String,
}

struct State {
    items: Vec<Item>,
    prev_gather: Option<Instant>,
    last_gather: Option<Instant>,
}

static STATE: Mutex<State> = Mutex::new(State { items: Vec::new(), prev_gather: None, last_gather: None });

#[derive(Debug, Clone)]
struct Config {
    color: Color32,
    range: f64,
    labels: bool,
    only_listed: bool,
    listed: BTreeSet<String>,
}

#[derive(Debug)]
pub struct ItemEspModule {
    pub module: ModuleData,
}

impl ItemEspModule {
    pub fn new() -> Self {
        Self {
            module: ModuleData {
                id: ModuleId::ItemEsp,
                description: "Highlights items lying on the ground".to_string(),
                category: ModuleCategory::Render,
                key_bind: KeyboardKey::KeyNone,
                enabled: false,
                settings: vec![
                    ModuleSetting::Color {
                        name: "Item color".to_string(),
                        value: [1.0, 0.85, 0.3, 1.0],
                    },
                    ModuleSetting::Slider {
                        name: "Range".to_string(),
                        value: 48.0,
                        min: 8.0,
                        max: 128.0,
                    },
                    ModuleSetting::Toggle {
                        name: "Labels".to_string(),
                        value: true,
                    },
                    ModuleSetting::Toggle {
                        name: "Only listed items".to_string(),
                        value: false,
                    },
                ],
            },
        }
    }
}

impl Module for ItemEspModule {
    fn on_start(&self) -> anyhow::Result<()> {
        Ok(())
    }

    fn on_stop(&self) -> anyhow::Result<()> {
        STATE.lock().unwrap_or_else(|p| p.into_inner()).items.clear();
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

fn config() -> Option<Config> {
    if !client().modules.is_active() {
        return None;
    }
    let handle = client().modules.get(ModuleId::ItemEsp)?;
    let module = handle.lock().ok()?;
    let data = module.get_module_data();
    if !data.enabled {
        return None;
    }
    let toggle = |name: &str, fallback: bool| data.get_setting(name).and_then(ModuleSetting::get_toggle_value).unwrap_or(fallback);
    let [r, g, b, a] = match data.get_setting("Item color") {
        Some(ModuleSetting::Color { value, .. }) => *value,
        _ => [1.0, 0.85, 0.3, 1.0],
    };
    let only_listed = toggle("Only listed items", false);
    Some(Config {
        color: Color32::from_rgba_unmultiplied((r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8, (a * 255.0).max(60.0) as u8),
        range: data.get_setting("Range").and_then(ModuleSetting::get_slider_value).unwrap_or(48.0) as f64,
        labels: toggle("Labels", true),
        only_listed,
        listed: if only_listed { lists::get(List::Items) } else { BTreeSet::new() },
    })
}

/// Whether ItemESP has anything to do this frame.
pub fn active() -> bool {
    config().is_some()
}

/// Pure: `name ×count`, or just the name for a single item.
pub fn item_label(name: &str, count: i32) -> String {
    if count > 1 {
        format!("{name} ×{count}")
    } else {
        name.to_string()
    }
}

fn gather(cfg: &Config, previous: &[Item]) -> anyhow::Result<Vec<Item>> {
    let mc = minecraft();
    let (Some(world), Some(player)) = (mc.world()?, mc.player()?) else {
        return Ok(Vec::new());
    };
    let prev: HashMap<i32, [f64; 3]> = previous.iter().map(|i| (i.id, i.pos)).collect();
    let me = crate::mapping::entity::EntityRef::get_position(&player)?;
    let range_sq = cfg.range * cfg.range;
    let entities = world.get_entities()?;
    mapping().in_frame(|| {
        let mut env = mapping().get_env()?;
        let item_class = mapping().resolve_class(&mut env, "net/minecraft/world/entity/item/ItemEntity")?;
        let mut out = Vec::new();
        for entity in &entities {
            let obj = entity.jni_ref().as_obj();
            if !env.is_instance_of(obj, &item_class)? {
                continue;
            }
            let found = env.with_local_frame(8, |env| -> anyhow::Result<Option<Item>> {
                let x = raw::call(env, obj, "getX", "()D", &[])?.d()?;
                let y = raw::call(env, obj, "getY", "()D", &[])?.d()?;
                let z = raw::call(env, obj, "getZ", "()D", &[])?.d()?;
                let (dx, dy, dz) = (x - me.x(), y - me.y(), z - me.z());
                if dx * dx + dy * dy + dz * dz > range_sq {
                    return Ok(None);
                }
                let stack = raw::call(env, obj, "getItem", &format!("(){}", hands::STACK_SIG), &[])?.l()?;
                if cfg.only_listed && !cfg.listed.contains(&hands::item_id(env, &stack)?) {
                    return Ok(None);
                }
                let count = raw::call(env, &stack, "getCount", "()I", &[])?.i()?;
                let name = if cfg.labels {
                    let component = raw::call(env, &stack, "getHoverName", "()Lnet/minecraft/network/chat/Component;", &[])?.l()?;
                    let text = raw::call(env, &component, "getString", "()Ljava/lang/String;", &[])?.l()?;
                    mapping().get_string(text)?
                } else {
                    String::new()
                };
                let id = raw::call(env, obj, "getId", "()I", &[])?.i()?;
                let pos = [x, y, z];
                Ok(Some(Item { id, prev: prev.get(&id).copied().unwrap_or(pos), pos, label: item_label(&name, count) }))
            });
            match found {
                Ok(Some(item)) => out.push(item),
                Ok(None) => {}
                Err(error) => log::debug!("ItemESP: {error}"),
            }
        }
        Ok(out)
    })
}

/// Draws the items through `project` (world point → screen), gathering
/// first when due.
pub fn draw(painter: &Painter, project: &dyn Fn([f64; 3]) -> Option<Pos2>) {
    let Some(cfg) = config() else { return };
    let mut state = STATE.lock().unwrap_or_else(|p| p.into_inner());
    let now = Instant::now();
    if state.last_gather.is_none_or(|at| now - at >= GATHER_EVERY) {
        let previous = std::mem::take(&mut state.items);
        state.items = gather(&cfg, &previous).unwrap_or_else(|error| {
            log::debug!("ItemESP: {error}");
            Vec::new()
        });
        state.prev_gather = state.last_gather;
        state.last_gather = Some(now);
    }
    // Ease from the previous collection to the latest.
    let t = match (state.prev_gather, state.last_gather) {
        (Some(prev), Some(last)) if last > prev => ((now - last).as_secs_f64() / (last - prev).as_secs_f64()).clamp(0.0, 1.0),
        _ => 1.0,
    };
    let stroke = Stroke::new(1.4_f32, cfg.color);
    for item in &state.items {
        let p: [f64; 3] = std::array::from_fn(|i| item.prev[i] + (item.pos[i] - item.prev[i]) * t);
        let corners: Vec<Option<Pos2>> = (0..8)
            .map(|c| {
                let x = if c & 1 == 0 { -HALF } else { HALF };
                let y = if c & 2 == 0 { 0.0 } else { HALF * 2.0 };
                let z = if c & 4 == 0 { -HALF } else { HALF };
                project([p[0] + x, p[1] + y, p[2] + z])
            })
            .collect();
        // The 12 edges of the box: corners differing in exactly one bit.
        for a in 0..8usize {
            for bit in [1, 2, 4] {
                let b = a | bit;
                if b != a {
                    if let (Some(pa), Some(pb)) = (corners[a], corners[b]) {
                        painter.line_segment([pa, pb], stroke);
                    }
                }
            }
        }
        if cfg.labels && !item.label.is_empty() {
            if let Some(top) = project([p[0], p[1] + HALF * 2.0 + 0.15, p[2]]) {
                let font = FontId::proportional(11.0);
                painter.text(top + Vec2::new(0.8, 0.8), Align2::CENTER_BOTTOM, &item.label, font.clone(), Color32::from_black_alpha(210));
                painter.text(top, Align2::CENTER_BOTTOM, &item.label, font, cfg.color);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stacks_show_their_count() {
        assert_eq!(item_label("Diamond", 1), "Diamond");
        assert_eq!(item_label("Diamond", 12), "Diamond ×12");
    }
}
