//! The target lists of the auras: player names for KillAura, entity type ids
//! (`minecraft:zombie`, …) for MobAura, and MobAura's "never attack" list. Edited in the auras' tool windows and
//! saved to `aura_targets.json` beside the client config.

use crate::config;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::sync::Mutex;

const FILE: &str = "aura_targets.json";

/// Which list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetList {
    /// Player names (KillAura).
    Players,
    /// Entity type ids (MobAura).
    Mobs,
    /// Entity type ids MobAura never attacks, whatever its filter.
    MobIgnore,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct Lists {
    #[serde(default)]
    players: BTreeSet<String>,
    #[serde(default)]
    mobs: BTreeSet<String>,
    #[serde(default)]
    mob_ignore: BTreeSet<String>,
}

impl Lists {
    fn get_mut(&mut self, kind: TargetList) -> &mut BTreeSet<String> {
        match kind {
            TargetList::Players => &mut self.players,
            TargetList::Mobs => &mut self.mobs,
            TargetList::MobIgnore => &mut self.mob_ignore,
        }
    }
}

/// `None` until first read from disk.
static LISTS: Mutex<Option<Lists>> = Mutex::new(None);

fn with_lists<T>(f: impl FnOnce(&mut Lists) -> T) -> T {
    let mut guard = LISTS.lock().unwrap_or_else(|poison| poison.into_inner());
    f(guard.get_or_insert_with(load))
}

fn load() -> Lists {
    std::fs::read_to_string(config::base_dir().join(FILE))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn save(lists: &Lists) {
    let json = serde_json::to_string_pretty(lists).unwrap_or_else(|_| "{}".into());
    let path = config::base_dir().join(FILE);
    if let Err(error) = std::fs::write(&path, json) {
        log::warn!("aura targets: could not write {}: {error}", path.display());
    }
}

/// A snapshot of one list.
pub fn list(kind: TargetList) -> BTreeSet<String> {
    with_lists(|lists| lists.get_mut(kind).clone())
}

/// Adds `entry` to the list, or removes it if already there.
pub fn toggle(kind: TargetList, entry: &str) {
    with_lists(|lists| {
        let set = lists.get_mut(kind);
        if !set.remove(entry) {
            set.insert(entry.to_string());
        }
        save(lists);
    });
}

/// Empties one list.
pub fn clear(kind: TargetList) {
    with_lists(|lists| {
        lists.get_mut(kind).clear();
        save(lists);
    });
}
