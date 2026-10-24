//! AutoSort — sorts chests and the inventory by your own rules.
//!
//! # Rules (`autosort.json` beside the client config)
//!
//! * **Chests** — each bound to its block position and dimension, with an
//!   ordered list of entries. An entry is an item pattern (`minecraft:diamond`,
//!   `*_ore`, `oak_*` — `*` matches anything, a missing namespace means
//!   `minecraft:`) plus two numbers: how many of it to *keep* in the inventory
//!   when depositing, and how many to *take* out of the chest. Per chest,
//!   three switchable actions run in this order:
//!   1. **Deposit** — matching items move from the inventory into the chest
//!      (beyond the keep amounts; the hotbar only with "Include hotbar");
//!   2. **Take** — items with a take amount are refilled from the chest;
//!   3. **Arrange** — the chest's stacks are merged, then laid out in entry
//!      order (unlisted items after, alphabetically).
//! * **Inventory** — an item pattern per hotbar slot, and an order for the
//!   main inventory.
//! * **Other chests** — an order for chests with no rule of their own.
//!
//! # Running
//!
//! A sort starts when a chest (or the inventory screen) opens, when set to,
//! or from the "Sort now" button. It is a series of ordinary inventory clicks,
//! a few per tick ("Clicks/tick"), replanned from the live menu after every
//! step, and stops as soon as the screen closes.

use crate::config;
use crate::graphic::notification::{Notification, NotificationType};
use crate::mapping::entity::player::LocalPlayer;
use crate::mapping::raw;
use crate::mapping::MappedObject;
use crate::module::player::hands;
use crate::module::{KeyboardKey, Module, ModuleCategory, ModuleData, ModuleId, ModuleSetting};
use crate::state::{mapping, minecraft};
use jni::objects::{GlobalRef, JObject, JValue};
use jni::JNIEnv;
use serde::{Deserialize, Serialize};
use std::collections::{HashSet, VecDeque};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

const FILE: &str = "autosort.json";
/// Slots of the player's inventory at the end of every container menu.
const PLAYER_SLOTS: usize = 36;
/// Give up on a sort that takes more steps than this — something keeps
/// undoing it.
const MAX_STEPS: u32 = 3000;
/// Plans tried per tick at most (a plan may come back empty).
const MAX_PLANS_PER_TICK: u32 = 8;

// --- rules ----------------------------------------------------------------------

/// One line of a rule: an item pattern and its amounts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub pattern: String,
    /// Deposit leaves at least this many in the inventory.
    #[serde(default)]
    pub keep: u32,
    /// Take refills the inventory up to this many (0 = never take).
    #[serde(default)]
    pub take: u32,
}

impl Entry {
    pub fn new(pattern: &str) -> Entry {
        Entry {
            pattern: normalize_pattern(pattern),
            keep: 0,
            take: 0,
        }
    }
}

/// A chest bound to a position, with what goes in it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChestRule {
    pub name: String,
    pub dimension: String,
    pub pos: [i32; 3],
    pub entries: Vec<Entry>,
    pub deposit: bool,
    pub take: bool,
    pub arrange: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InventoryRule {
    /// Item pattern per hotbar slot; empty = leave that slot alone.
    pub hotbar: [String; 9],
    pub order: Vec<Entry>,
    /// Sort when the inventory screen opens.
    pub on_open: bool,
}

impl Default for InventoryRule {
    fn default() -> Self {
        InventoryRule {
            hotbar: Default::default(),
            order: Vec::new(),
            on_open: false,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OtherChests {
    pub arrange: bool,
    pub order: Vec<Entry>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Rules {
    #[serde(default)]
    pub chests: Vec<ChestRule>,
    #[serde(default)]
    pub inventory: InventoryRule,
    #[serde(default)]
    pub other: OtherChests,
}

static RULES: Mutex<Option<Rules>> = Mutex::new(None);

fn load_rules() -> Rules {
    std::fs::read_to_string(config::base_dir().join(FILE))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// A snapshot of the rules.
pub fn rules() -> Rules {
    RULES
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get_or_insert_with(load_rules)
        .clone()
}

/// Edits the rules and saves them if anything changed.
pub fn edit_rules(f: impl FnOnce(&mut Rules)) {
    let mut guard = RULES.lock().unwrap_or_else(|p| p.into_inner());
    let rules = guard.get_or_insert_with(load_rules);
    let before = rules.clone();
    f(rules);
    if *rules != before {
        let json = serde_json::to_string_pretty(rules).unwrap_or_else(|_| "{}".into());
        let path = config::base_dir().join(FILE);
        if let Err(error) = std::fs::write(&path, json) {
            log::warn!("AutoSort: could not write {}: {error}", path.display());
        }
    }
}

/// `diamond` → `minecraft:diamond`; patterns with a namespace or starting
/// with `*` pass through. Lower-cased, trimmed.
pub fn normalize_pattern(pattern: &str) -> String {
    let pattern = pattern.trim().to_lowercase();
    if pattern.contains(':') || pattern.starts_with('*') {
        pattern
    } else {
        format!("minecraft:{pattern}")
    }
}

/// Glob match where `*` matches any run of characters.
pub fn glob(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == text;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !text.starts_with(first) || !text.ends_with(last) || text.len() < first.len() + last.len() {
        return false;
    }
    let mut rest = &text[first.len()..text.len() - last.len()];
    for middle in &parts[1..parts.len() - 1] {
        match rest.find(middle) {
            Some(at) => rest = &rest[at + middle.len()..],
            None => return false,
        }
    }
    true
}

/// Index of the first entry matching `id`.
pub fn rank(entries: &[Entry], id: &str) -> Option<usize> {
    entries.iter().position(|entry| glob(&entry.pattern, id))
}

/// Pattern groups offered as one-click presets in the window.
pub const PRESETS: &[(&str, &[&str])] = &[
    ("Ores", &["*_ore", "minecraft:ancient_debris", "minecraft:raw_*"]),
    (
        "Ingots & gems",
        &[
            "*_ingot",
            "*_nugget",
            "minecraft:diamond",
            "minecraft:emerald",
            "minecraft:lapis_lazuli",
            "minecraft:redstone",
            "minecraft:coal",
            "minecraft:quartz",
            "minecraft:amethyst_shard",
        ],
    ),
    ("Wood", &["*_log", "*_wood", "*_planks", "minecraft:stick"]),
    (
        "Stone",
        &[
            "minecraft:cobblestone",
            "minecraft:stone",
            "minecraft:cobbled_deepslate",
            "minecraft:deepslate",
            "minecraft:andesite",
            "minecraft:diorite",
            "minecraft:granite",
            "minecraft:tuff",
            "minecraft:netherrack",
        ],
    ),
    ("Dirt & sand", &["minecraft:dirt", "minecraft:gravel", "minecraft:sand", "minecraft:red_sand"]),
    (
        "Mob drops",
        &[
            "minecraft:rotten_flesh",
            "minecraft:bone",
            "minecraft:string",
            "minecraft:spider_eye",
            "minecraft:gunpowder",
            "minecraft:ender_pearl",
            "minecraft:slime_ball",
            "minecraft:arrow",
        ],
    ),
    (
        "Food",
        &["minecraft:cooked_*", "minecraft:bread", "*_apple", "minecraft:carrot", "*potato", "minecraft:melon_slice"],
    ),
    ("Tools", &["*_pickaxe", "*_axe", "*_shovel", "*_hoe", "minecraft:shears", "minecraft:flint_and_steel"]),
    ("Weapons & armor", &["*_sword", "minecraft:bow", "minecraft:crossbow", "*_helmet", "*_chestplate", "*_leggings", "*_boots", "minecraft:shield"]),
];

// --- the planner (pure) ----------------------------------------------------------

/// One menu slot as the planner sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct Slot {
    pub id: String,
    pub count: i32,
    pub max: i32,
    /// Stacks in the same group can merge (same item, same components).
    pub group: usize,
}

/// A menu click.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Click {
    /// Left click: pick up / put down / swap / merge.
    Pickup(i32),
    /// Shift click: move to the other side of the menu.
    QuickMove(i32),
    /// Number-key swap of a slot with hotbar slot `1`.
    Swap(i32, i32),
}

/// Merges two partial stacks of the same item within `region`.
pub fn plan_merge(view: &[Option<Slot>], region: Range<usize>) -> Option<Vec<Click>> {
    for i in region.clone() {
        let Some(a) = &view[i] else { continue };
        if a.count >= a.max {
            continue;
        }
        for j in (i + 1)..region.end {
            if let Some(b) = &view[j] {
                if b.group == a.group {
                    // Pick up the later stack, drop it onto the earlier one
                    // (which takes what fits), put any rest back.
                    return Some(vec![Click::Pickup(j as i32), Click::Pickup(i as i32), Click::Pickup(j as i32)]);
                }
            }
        }
    }
    None
}

/// The next swap putting `region` into order: listed items first in entry
/// order, then the rest alphabetically, empty slots last.
pub fn plan_order(view: &[Option<Slot>], region: Range<usize>, entries: &[Entry]) -> Option<Vec<Click>> {
    let key = |slot: &Option<Slot>| {
        slot.as_ref()
            .map(|s| (rank(entries, &s.id).unwrap_or(usize::MAX), s.id.clone()))
    };
    let current: Vec<Option<(usize, String)>> = region.clone().map(|i| key(&view[i])).collect();
    let mut desired: Vec<(usize, String)> = current.iter().flatten().cloned().collect();
    desired.sort();

    for (k, want) in desired.iter().enumerate() {
        if current[k].as_ref() == Some(want) {
            continue;
        }
        let q = (k + 1..current.len()).find(|&q| current[q].as_ref() == Some(want))?;
        let (p, q) = ((region.start + k) as i32, (region.start + q) as i32);
        return Some(if current[k].is_some() {
            vec![Click::Pickup(q), Click::Pickup(p), Click::Pickup(q)]
        } else {
            vec![Click::Pickup(q), Click::Pickup(p)]
        });
    }
    None
}

/// The next deposit: a stack in `player` matching an entry, beyond its keep
/// amount. Slots in `skip` were already tried.
pub fn plan_deposit(
    view: &[Option<Slot>],
    player: Range<usize>,
    entries: &[Entry],
    skip: &HashSet<i32>,
) -> Option<Click> {
    for i in player.clone() {
        let Some(slot) = &view[i] else { continue };
        if skip.contains(&(i as i32)) {
            continue;
        }
        let Some(entry) = rank(entries, &slot.id).map(|r| &entries[r]) else {
            continue;
        };
        let total: i32 = player
            .clone()
            .filter_map(|j| view[j].as_ref())
            .filter(|other| other.id == slot.id)
            .map(|other| other.count)
            .sum();
        if total - slot.count >= entry.keep as i32 {
            return Some(Click::QuickMove(i as i32));
        }
    }
    None
}

/// The next take: a chest stack for an entry the inventory holds fewer of
/// than its take amount.
pub fn plan_take(
    view: &[Option<Slot>],
    chest: Range<usize>,
    player: Range<usize>,
    entries: &[Entry],
    skip: &HashSet<i32>,
) -> Option<Click> {
    for entry in entries.iter().filter(|entry| entry.take > 0) {
        let have: i32 = player
            .clone()
            .filter_map(|j| view[j].as_ref())
            .filter(|s| glob(&entry.pattern, &s.id))
            .map(|s| s.count)
            .sum();
        if have >= entry.take as i32 {
            continue;
        }
        let source = chest.clone().find(|&i| {
            !skip.contains(&(i as i32)) && view[i].as_ref().is_some_and(|s| glob(&entry.pattern, &s.id))
        });
        if let Some(i) = source {
            return Some(Click::QuickMove(i as i32));
        }
    }
    None
}

/// The next hotbar fix in the player's own inventory menu (main 9-35,
/// hotbar 36-44): swaps an item matching slot `h`'s pattern into it.
pub fn plan_hotbar(view: &[Option<Slot>], hotbar: &[String; 9], done: &HashSet<i32>) -> Option<Click> {
    let assigned = |h: usize, slot: &Option<Slot>| {
        slot.as_ref()
            .is_some_and(|s| !hotbar[h].is_empty() && glob(&normalize_pattern(&hotbar[h]), &s.id))
    };
    for h in 0..9 {
        if hotbar[h].trim().is_empty() || done.contains(&(h as i32)) || assigned(h, &view[36 + h]) {
            continue;
        }
        let pattern = normalize_pattern(&hotbar[h]);
        // From the main inventory, or a hotbar slot not holding its own pick.
        let source = (9..36)
            .chain((36..45).filter(|&s| s != 36 + h && !assigned(s - 36, &view[s])))
            .find(|&s| view[s].as_ref().is_some_and(|slot| glob(&pattern, &slot.id)));
        if let Some(s) = source {
            return Some(Click::Swap(s as i32, h as i32));
        }
    }
    None
}

// --- the module ------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Deposit,
    Take,
    Hotbar,
    Merge,
    Order,
}

#[derive(Debug)]
struct Job {
    container_id: i32,
    /// Index into `Rules::chests`; `None` = other chest / inventory.
    rule: Option<usize>,
    inventory: bool,
    phases: VecDeque<Phase>,
    pending: VecDeque<Click>,
    /// Slots already deposited / taken from (or hotbar slots fixed).
    tried: HashSet<i32>,
    steps: u32,
}

#[derive(Debug, Default)]
struct Tracker {
    last_container: i32,
    inventory_open: bool,
    /// Chest position (and dimension) the open container was opened at.
    opened_at: Option<(String, [i32; 3])>,
    job: Option<Job>,
    status: String,
}

static TRACKER: Mutex<Option<Tracker>> = Mutex::new(None);
static SORT_NOW: AtomicBool = AtomicBool::new(false);

/// Asks for a sort of whatever is open, on the next tick.
pub fn sort_now() {
    SORT_NOW.store(true, Ordering::Relaxed);
}

/// A line describing what AutoSort is doing / last did.
pub fn status() -> String {
    TRACKER
        .lock()
        .ok()
        .and_then(|t| t.as_ref().map(|t| t.status.clone()))
        .unwrap_or_default()
}

/// The block the player looks at, with the dimension — for binding a chest.
pub fn looked_at_block() -> Option<(String, [i32; 3])> {
    let opened = TRACKER
        .lock()
        .ok()
        .and_then(|t| t.as_ref().and_then(|t| (t.last_container != 0).then(|| t.opened_at.clone()).flatten()));
    opened.or_else(|| {
        mapping()
            .in_frame(|| {
                let mut env = mapping().get_env()?;
                looked_at(&mut env)
            })
            .ok()
            .flatten()
    })
}

#[derive(Debug)]
pub struct AutoSortModule {
    pub module: ModuleData,
}

impl AutoSortModule {
    pub fn new() -> Self {
        Self {
            module: ModuleData {
                id: ModuleId::AutoSort,
                description: "Sorts chests and the inventory by your rules".to_string(),
                category: ModuleCategory::World,
                key_bind: KeyboardKey::KeyNone,
                enabled: false,
                settings: vec![
                    ModuleSetting::Toggle {
                        name: "Sort chests on open".to_string(),
                        value: true,
                    },
                    ModuleSetting::Toggle {
                        name: "Include hotbar".to_string(),
                        value: false,
                    },
                    ModuleSetting::Slider {
                        name: "Clicks/tick".to_string(),
                        value: 2.0,
                        min: 1.0,
                        max: 10.0,
                    },
                ],
            },
        }
    }

    fn toggle(&self, name: &str, fallback: bool) -> bool {
        self.module
            .get_setting(name)
            .and_then(ModuleSetting::get_toggle_value)
            .unwrap_or(fallback)
    }
}

impl Module for AutoSortModule {
    fn on_start(&self) -> anyhow::Result<()> {
        Ok(())
    }

    fn on_stop(&self) -> anyhow::Result<()> {
        if let Some(tracker) = TRACKER.lock().unwrap_or_else(|p| p.into_inner()).as_mut() {
            tracker.job = None;
        }
        Ok(())
    }

    fn on_tick(&self) -> anyhow::Result<()> {
        let mc = minecraft();
        let (Some(player), Some(game_mode)) = (mc.player()?, mc.game_mode()?) else {
            return Ok(());
        };
        let mut guard = TRACKER.lock().unwrap_or_else(|p| p.into_inner());
        let tracker = guard.get_or_insert_with(Tracker::default);
        let include_hotbar = self.toggle("Include hotbar", false);
        let budget = self
            .module
            .get_setting("Clicks/tick")
            .and_then(ModuleSetting::get_slider_value)
            .unwrap_or(2.0)
            .max(1.0) as u32;

        let (container_id, storage, slot_count) = mapping().in_frame(|| {
            let mut env = mapping().get_env()?;
            let menu = container_menu(&mut env, player.jni_ref().as_obj())?;
            let id = raw::field(&mut env, &menu, "containerId", "I")?.i()?;
            let storage = is_storage_menu(&mut env, &menu)?;
            let count = raw::call(&mut env, &menu, "getItems", "()Lnet/minecraft/core/NonNullList;", &[])
                .and_then(|list| {
                    let list = list.l()?;
                    Ok(raw::call(&mut env, &list, "size", "()I", &[])?.i()?)
                })?;
            Ok((id, storage, count as usize))
        })?;
        let inventory_open = container_id == 0 && mc.current_screen() == crate::mapping::client::screen::Screen::Inventory;

        // A container opened: remember where, and maybe start sorting it.
        let mut start = SORT_NOW.swap(false, Ordering::Relaxed);
        if container_id != tracker.last_container {
            tracker.last_container = container_id;
            tracker.job = None;
            if container_id != 0 && storage {
                tracker.opened_at = mapping()
                    .in_frame(|| {
                        let mut env = mapping().get_env()?;
                        looked_at(&mut env)
                    })
                    .ok()
                    .flatten();
                start |= self.toggle("Sort chests on open", true);
            }
        }
        if inventory_open && !tracker.inventory_open && rules().inventory.on_open {
            start = true;
        }
        tracker.inventory_open = inventory_open;

        if start && tracker.job.is_none() {
            tracker.job = new_job(tracker, container_id, storage, slot_count, inventory_open, &player);
        }
        let Some(job) = tracker.job.as_mut() else {
            return Ok(());
        };
        if job.container_id != container_id {
            tracker.job = None;
            return Ok(());
        }

        let chest_slots = if job.inventory { 0 } else { slot_count.saturating_sub(PLAYER_SLOTS) };
        let rules = rules();
        let mut clicks = 0;
        let mut plans = 0;
        while clicks < budget {
            if let Some(click) = job.pending.pop_front() {
                execute(&game_mode, &player, container_id, click)?;
                clicks += 1;
                job.steps += 1;
                continue;
            }
            if plans >= MAX_PLANS_PER_TICK || job.steps > MAX_STEPS {
                break;
            }
            plans += 1;
            let view = read_view(&player)?;
            if view.carried {
                tracker.status = "Stopped: an item is on the cursor".into();
                tracker.job = None;
                return Ok(());
            }
            match plan_step(job, &view.slots, chest_slots, include_hotbar, &rules) {
                Some(next) => job.pending.extend(next),
                None => {
                    job.phases.pop_front();
                    if job.phases.is_empty() {
                        tracker.status = format!("Sorted ({} clicks)", job.steps);
                        tracker.job = None;
                        Notification::send(NotificationType::Info, "AutoSort", "Sorted");
                        return Ok(());
                    }
                }
            }
        }
        if job.steps > MAX_STEPS {
            tracker.status = "Stopped: too many steps".into();
            tracker.job = None;
        } else {
            tracker.status = format!("Sorting… {} clicks", job.steps);
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

/// Works out what to sort for the open menu.
fn new_job(
    tracker: &mut Tracker,
    container_id: i32,
    storage: bool,
    slot_count: usize,
    inventory_open: bool,
    player: &LocalPlayer,
) -> Option<Job> {
    let rules = rules();
    let job = |rule, inventory, phases: &[Phase]| Job {
        container_id,
        rule,
        inventory,
        phases: phases.iter().copied().collect(),
        pending: VecDeque::new(),
        tried: HashSet::new(),
        steps: 0,
    };
    if container_id != 0 && storage {
        let double = slot_count.saturating_sub(PLAYER_SLOTS) == 54;
        let rule = tracker.opened_at.as_ref().and_then(|(dimension, pos)| {
            rules.chests.iter().position(|chest| {
                chest.dimension == *dimension && {
                    let d = [chest.pos[0] - pos[0], chest.pos[1] - pos[1], chest.pos[2] - pos[2]];
                    d == [0, 0, 0] || (double && d[1] == 0 && d[0].abs() + d[2].abs() == 1)
                }
            })
        });
        let Some(index) = rule else {
            if !rules.other.arrange {
                tracker.status = "No rule for this chest".into();
                return None;
            }
            tracker.status = "Sorting (other chest)…".into();
            return Some(job(None, false, &[Phase::Merge, Phase::Order]));
        };
        let chest = &rules.chests[index];
        let mut phases = Vec::new();
        if chest.deposit {
            phases.push(Phase::Deposit);
        }
        if chest.take {
            phases.push(Phase::Take);
        }
        if chest.arrange {
            phases.extend([Phase::Merge, Phase::Order]);
        }
        tracker.status = format!("Sorting {}…", chest.name);
        return (!phases.is_empty()).then(|| job(Some(index), false, &phases));
    }
    if inventory_open {
        let creative = mapping()
            .in_frame(|| {
                let mut env = mapping().get_env()?;
                Ok(raw::call(&mut env, player.jni_ref().as_obj(), "isCreative", "()Z", &[])?.z()?)
            })
            .unwrap_or(false);
        if creative {
            tracker.status = "Not in creative".into();
            return None;
        }
        tracker.status = "Sorting inventory…".into();
        return Some(job(None, true, &[Phase::Hotbar, Phase::Merge, Phase::Order]));
    }
    tracker.status = "Open a chest or the inventory first".into();
    None
}

/// Plans the next clicks of the job's current phase; `None` = phase done.
fn plan_step(
    job: &mut Job,
    view: &[Option<Slot>],
    chest_slots: usize,
    include_hotbar: bool,
    rules: &Rules,
) -> Option<Vec<Click>> {
    let phase = *job.phases.front()?;
    let chest_rule = job.rule.and_then(|i| rules.chests.get(i));
    let entries: &[Entry] = match (job.inventory, chest_rule) {
        (true, _) => &rules.inventory.order,
        (false, Some(chest)) => &chest.entries,
        (false, None) => &rules.other.order,
    };
    // The region being arranged, and the player's part of the menu.
    let region = if job.inventory { 9..36 } else { 0..chest_slots };
    let player_end = if include_hotbar { chest_slots + 36 } else { chest_slots + 27 };
    let player = chest_slots..player_end.min(view.len());

    match phase {
        Phase::Deposit => {
            let click = plan_deposit(view, player, entries, &job.tried)?;
            if let Click::QuickMove(slot) = click {
                job.tried.insert(slot);
            }
            Some(vec![click])
        }
        Phase::Take => {
            let all_player = chest_slots..(chest_slots + 36).min(view.len());
            let click = plan_take(view, 0..chest_slots, all_player, entries, &job.tried)?;
            if let Click::QuickMove(slot) = click {
                job.tried.insert(slot);
            }
            Some(vec![click])
        }
        Phase::Hotbar => {
            let click = plan_hotbar(view, &rules.inventory.hotbar, &job.tried)?;
            if let Click::Swap(_, h) = click {
                job.tried.insert(h);
            }
            Some(vec![click])
        }
        Phase::Merge => plan_merge(view, region),
        Phase::Order => plan_order(view, region, entries),
    }
}

// --- JNI side --------------------------------------------------------------------------

fn container_menu<'local>(env: &mut JNIEnv<'local>, player: &JObject) -> anyhow::Result<JObject<'local>> {
    Ok(raw::field(env, player, "containerMenu", "Lnet/minecraft/world/inventory/AbstractContainerMenu;")?.l()?)
}

/// Chest, barrel, ender chest and shulker box menus — the ones it makes sense
/// to deposit into.
fn is_storage_menu(env: &mut JNIEnv, menu: &JObject) -> anyhow::Result<bool> {
    for class in [
        "net/minecraft/world/inventory/ChestMenu",
        "net/minecraft/world/inventory/ShulkerBoxMenu",
    ] {
        if let Ok(class) = mapping().resolve_class(env, class) {
            if env.is_instance_of(menu, &class)? {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// The block under the crosshair, with the current dimension.
fn looked_at(env: &mut JNIEnv) -> anyhow::Result<Option<(String, [i32; 3])>> {
    let mc = minecraft().jni_ref().as_obj();
    let hit = raw::field(env, mc, "hitResult", "Lnet/minecraft/world/phys/HitResult;")?.l()?;
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
    let Some(world) = minecraft().world()? else {
        return Ok(None);
    };
    let key = raw::call(env, world.jni_ref().as_obj(), "dimension", "()Lnet/minecraft/resources/ResourceKey;", &[])?.l()?;
    let text = raw::to_string(env, &key)?;
    let dimension = text
        .split(" / ")
        .nth(1)
        .map(|rest| rest.trim_end_matches(']').to_string())
        .unwrap_or(text);
    Ok(Some((dimension, xyz)))
}

struct View {
    slots: Vec<Option<Slot>>,
    /// Whether an item is on the cursor.
    carried: bool,
}

/// Reads every slot of the open menu.
fn read_view(player: &LocalPlayer) -> anyhow::Result<View> {
    mapping().in_frame(|| {
        let mut env = mapping().get_env()?;
        let menu = container_menu(&mut env, player.jni_ref().as_obj())?;
        let carried = raw::call(&mut env, &menu, "getCarried", &format!("(){}", hands::STACK_SIG), &[])?.l()?;
        let carried = !raw::call(&mut env, &carried, "isEmpty", "()Z", &[])?.z()?;
        let list = raw::call(&mut env, &menu, "getItems", "()Lnet/minecraft/core/NonNullList;", &[])?.l()?;
        let size = raw::call(&mut env, &list, "size", "()I", &[])?.i()?;

        let mut slots = Vec::with_capacity(size as usize);
        // Same-item stacks, for the merge check: one representative per group.
        let mut groups: Vec<(String, GlobalRef)> = Vec::new();
        for i in 0..size {
            let slot = env.with_local_frame(8, |env| -> anyhow::Result<Option<Slot>> {
                let stack = raw::call(env, &list, "get", "(I)Ljava/lang/Object;", &[JValue::Int(i)])?.l()?;
                if raw::call(env, &stack, "isEmpty", "()Z", &[])?.z()? {
                    return Ok(None);
                }
                let id = hands::item_id(env, &stack)?;
                let count = raw::call(env, &stack, "getCount", "()I", &[])?.i()?;
                let max = raw::call(env, &stack, "getMaxStackSize", "()I", &[])?.i()?;
                let mut group = None;
                for (index, (group_id, representative)) in groups.iter().enumerate() {
                    if *group_id == id && same_item(env, representative.as_obj(), &stack)? {
                        group = Some(index);
                        break;
                    }
                }
                let group = match group {
                    Some(group) => group,
                    None => {
                        groups.push((id.clone(), env.new_global_ref(&stack)?));
                        groups.len() - 1
                    }
                };
                Ok(Some(Slot { id, count, max, group }))
            })?;
            slots.push(slot);
        }
        Ok(View { slots, carried })
    })
}

/// `ItemStack.isSameItemSameComponents` (1.20.5+), else `isSameItemSameTags`.
fn same_item(env: &mut JNIEnv, a: &JObject, b: &JObject) -> anyhow::Result<bool> {
    let class = mapping().resolve_class(env, "net/minecraft/world/item/ItemStack")?;
    let sig = format!("({0}{0})Z", hands::STACK_SIG);
    for name in ["isSameItemSameComponents", "isSameItemSameTags"] {
        match env.call_static_method(&class, name, &sig, &[JValue::Object(a), JValue::Object(b)]) {
            Ok(value) => return Ok(value.z()?),
            Err(_) => {
                let _ = env.exception_clear();
            }
        }
    }
    Err(anyhow::anyhow!("no ItemStack sameness check"))
}

fn execute(
    game_mode: &crate::mapping::client::gamemode::MultiPlayerGameMode,
    player: &LocalPlayer,
    container_id: i32,
    click: Click,
) -> anyhow::Result<()> {
    match click {
        Click::Pickup(slot) => game_mode.container_pickup(container_id, slot, player),
        Click::QuickMove(slot) => game_mode.container_quick_move(container_id, slot, player),
        Click::Swap(slot, hotbar) => game_mode.container_swap(container_id, slot, hotbar, player),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(id: &str, count: i32, group: usize) -> Option<Slot> {
        Some(Slot {
            id: id.to_string(),
            count,
            max: 64,
            group,
        })
    }

    #[test]
    fn glob_matches_wildcards() {
        assert!(glob("*_ore", "minecraft:iron_ore"));
        assert!(glob("minecraft:raw_*", "minecraft:raw_gold"));
        assert!(glob("*potato", "minecraft:baked_potato"));
        assert!(glob("minecraft:diamond", "minecraft:diamond"));
        assert!(!glob("minecraft:diamond", "minecraft:diamond_ore"));
        assert!(glob("*oak*", "minecraft:dark_oak_log"));
        assert!(!glob("*_ore", "minecraft:ore_thing"));
    }

    #[test]
    fn patterns_get_the_minecraft_namespace() {
        assert_eq!(normalize_pattern(" Diamond "), "minecraft:diamond");
        assert_eq!(normalize_pattern("*_ore"), "*_ore");
        assert_eq!(normalize_pattern("create:zinc_ore"), "create:zinc_ore");
    }

    /// Applies clicks to a view the way the game does (left-click pickup
    /// semantics, shift-click to another range left out).
    fn apply(view: &mut [Option<Slot>], clicks: &[Click]) {
        let mut cursor: Option<Slot> = None;
        for click in clicks {
            let Click::Pickup(i) = *click else { panic!("only pickups simulated") };
            let i = i as usize;
            match (cursor.take(), view[i].take()) {
                (None, slot) => cursor = slot,
                (Some(c), None) => view[i] = Some(c),
                (Some(mut c), Some(mut s)) if c.group == s.group => {
                    let moved = c.count.min(s.max - s.count);
                    s.count += moved;
                    c.count -= moved;
                    view[i] = Some(s);
                    cursor = (c.count > 0).then_some(c);
                }
                (Some(c), Some(s)) => {
                    view[i] = Some(c);
                    cursor = Some(s);
                }
            }
        }
        assert!(cursor.is_none(), "cursor left holding an item");
    }

    #[test]
    fn merge_then_order_sorts_a_messy_chest() {
        let entries = vec![Entry::new("diamond"), Entry::new("*_ore")];
        let mut view = vec![
            slot("minecraft:stone", 30, 0),
            None,
            slot("minecraft:iron_ore", 10, 1),
            slot("minecraft:diamond", 5, 2),
            slot("minecraft:stone", 40, 0),
            slot("minecraft:diamond", 3, 2),
        ];
        let mut guard = 0;
        while let Some(clicks) = plan_merge(&view, 0..view.len()) {
            apply(&mut view, &clicks);
            guard += 1;
            assert!(guard < 50);
        }
        while let Some(clicks) = plan_order(&view, 0..view.len(), &entries) {
            apply(&mut view, &clicks);
            guard += 1;
            assert!(guard < 50);
        }
        let ids: Vec<Option<(&str, i32)>> = view
            .iter()
            .map(|s| s.as_ref().map(|s| (s.id.as_str(), s.count)))
            .collect();
        assert_eq!(
            ids,
            vec![
                Some(("minecraft:diamond", 8)),
                Some(("minecraft:iron_ore", 10)),
                Some(("minecraft:stone", 64)),
                Some(("minecraft:stone", 6)),
                None,
                None,
            ]
        );
    }

    #[test]
    fn deposit_respects_keep() {
        let mut entry = Entry::new("cobblestone");
        entry.keep = 64;
        let view = vec![
            None, // chest slot
            slot("minecraft:cobblestone", 64, 0),
            slot("minecraft:cobblestone", 20, 0),
            slot("minecraft:dirt", 64, 1),
        ];
        let skip = HashSet::new();
        // 84 in total, keep 64: the 20-stack may go, the 64-stack may not.
        assert_eq!(plan_deposit(&view, 1..4, &[entry.clone()], &skip), Some(Click::QuickMove(2)));
        let mut skip = HashSet::new();
        skip.insert(2);
        assert_eq!(plan_deposit(&view, 1..4, &[entry], &skip), None);
    }

    #[test]
    fn take_refills_up_to_the_amount() {
        let mut entry = Entry::new("torch");
        entry.take = 32;
        let view = vec![slot("minecraft:torch", 64, 0), None, slot("minecraft:torch", 10, 0)];
        assert_eq!(plan_take(&view, 0..1, 1..3, &[entry.clone()], &HashSet::new()), Some(Click::QuickMove(0)));
        let full = vec![slot("minecraft:torch", 64, 0), None, slot("minecraft:torch", 40, 0)];
        assert_eq!(plan_take(&full, 0..1, 1..3, &[entry], &HashSet::new()), None);
    }

    #[test]
    fn hotbar_pulls_the_assigned_item_in() {
        let mut view: Vec<Option<Slot>> = vec![None; 45];
        view[20] = slot("minecraft:diamond_pickaxe", 1, 0);
        let mut hotbar: [String; 9] = Default::default();
        hotbar[0] = "*_pickaxe".into();
        assert_eq!(plan_hotbar(&view, &hotbar, &HashSet::new()), Some(Click::Swap(20, 0)));
        view[36] = view[20].take();
        assert_eq!(plan_hotbar(&view, &hotbar, &HashSet::new()), None);
    }
}
