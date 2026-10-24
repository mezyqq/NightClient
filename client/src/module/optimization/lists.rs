//! The user-chosen lists of the optimization modules — extra particle types
//! to hide and entity types to hide — saved to `optimization.json` beside the
//! client config.

use crate::config;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::sync::Mutex;

const FILE: &str = "optimization.json";

/// Which list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum List {
    /// Particle type ids (`minecraft:rain`, …).
    Particles,
    /// Entity type ids (`minecraft:armor_stand`, …).
    Entities,
    /// Item ids shown by ItemESP (`minecraft:diamond`, …).
    Items,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct Lists {
    #[serde(default)]
    particles: BTreeSet<String>,
    #[serde(default)]
    entities: BTreeSet<String>,
    #[serde(default)]
    items: BTreeSet<String>,
}

impl Lists {
    fn get_mut(&mut self, list: List) -> &mut BTreeSet<String> {
        match list {
            List::Particles => &mut self.particles,
            List::Entities => &mut self.entities,
            List::Items => &mut self.items,
        }
    }
}

static LISTS: Mutex<Option<Lists>> = Mutex::new(None);

fn with_lists<T>(f: impl FnOnce(&mut Lists) -> T) -> T {
    let mut guard = LISTS.lock().unwrap_or_else(|p| p.into_inner());
    f(guard.get_or_insert_with(|| {
        std::fs::read_to_string(config::base_dir().join(FILE))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }))
}

fn save(lists: &Lists) {
    let path = config::base_dir().join(FILE);
    let json = serde_json::to_string_pretty(lists).unwrap_or_else(|_| "{}".into());
    if let Err(error) = std::fs::write(&path, json) {
        log::warn!("Optimization: could not write {}: {error}", path.display());
    }
}

pub fn get(list: List) -> BTreeSet<String> {
    with_lists(|lists| lists.get_mut(list).clone())
}

/// Adds `entry` to the list, or removes it if present.
pub fn toggle(list: List, entry: &str) {
    with_lists(|lists| {
        let set = lists.get_mut(list);
        if !set.remove(entry) {
            set.insert(entry.to_string());
        }
        save(lists);
    });
}

pub fn clear(list: List) {
    with_lists(|lists| {
        lists.get_mut(list).clear();
        save(lists);
    });
}
