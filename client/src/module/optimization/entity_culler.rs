//! Entity Culler — stop drawing entity types, and clear clutter entities.
//!
//! * **Hide** — living entity types from your list (Entity Culler window) are
//!   not drawn: their NeoForge `RenderLivingEvent.Pre` is cancelled. They are
//!   still there — you can hit them, they can hit you. Shadows still show.
//! * **Remove** — dropped items, XP orbs, arrows, lightning, … (and the
//!   non-living types from your list) are discarded from *your* world copy.
//!   Purely visual, the server keeps them; but a removed entity only comes
//!   back when the server sends it again (walk away and return, relog). Pick
//!   up items as usual — the server does that.
//!
//! Optionally keep the nearest dropped items, or only remove far ones.

use super::java::{self, cancel_when, contains_test, key_chain, proxy, EventListeners};
use super::lists::{self, List};
use super::{optimization_module, slider, toggle, Snapshot};
use crate::mapping::raw;
use crate::mapping::MappedObject;
use crate::state::{mapping, minecraft};
use jni::objects::{GlobalRef, JObject, JObjectArray, JValue};
use jni::JNIEnv;
use std::collections::BTreeSet;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const ENTITY: &str = "net/minecraft/world/entity/Entity";
const ENTITY_TYPE: &str = "net/minecraft/world/entity/EntityType";
const LIVING: &str = "net/minecraft/world/entity/LivingEntity";
const ITEM: &str = "entity.minecraft.item";
/// How often entities are swept.
const SWEEP_EVERY: Duration = Duration::from_millis(250);

/// Remove switches → entity type ids.
const REMOVALS: &[(&str, &[&str])] = &[
    ("Remove dropped items", &["minecraft:item"]),
    ("Remove XP orbs", &["minecraft:experience_orb"]),
    ("Remove arrows & tridents", &["minecraft:arrow", "minecraft:spectral_arrow", "minecraft:trident"]),
    ("Remove falling blocks", &["minecraft:falling_block"]),
    ("Remove lightning", &["minecraft:lightning_bolt"]),
    ("Remove firework rockets", &["minecraft:firework_rocket"]),
    ("Remove primed TNT", &["minecraft:tnt"]),
    ("Remove thrown items", &["minecraft:snowball", "minecraft:egg", "minecraft:ender_pearl", "minecraft:potion", "minecraft:experience_bottle"]),
];

fn settings() -> Vec<crate::module::ModuleSetting> {
    let mut settings = vec![toggle("Hide my list (Entity Culler window)", true)];
    for (name, _) in REMOVALS {
        settings.push(toggle(name, *name == "Remove lightning"));
    }
    settings.push(slider("Keep nearest items (0 = none)", 0.0, 0.0, 200.0));
    settings.push(slider("Remove only farther than (blocks)", 0.0, 0.0, 128.0));
    settings
}

optimization_module!(EntityCullerModule, EntityCuller, "Hide entity types, clear items, orbs, arrows, lightning", settings());

/// `minecraft:zombie` → `entity.minecraft.zombie`, the type's description id.
pub fn description_id(id: &str) -> String {
    let (namespace, path) = id.split_once(':').unwrap_or(("minecraft", id));
    format!("entity.{namespace}.{}", path.replace('/', "."))
}

/// Pure: which of `entities` (`(is_item, distance)`) to remove — everything
/// at least `min_distance` away, except the `keep_items` nearest items.
pub fn plan_removal(entities: &[(bool, f64)], keep_items: usize, min_distance: f64) -> Vec<bool> {
    let mut items: Vec<usize> = (0..entities.len()).filter(|&i| entities[i].0).collect();
    items.sort_by(|&a, &b| entities[a].1.total_cmp(&entities[b].1));
    let kept: BTreeSet<usize> = items.into_iter().take(keep_items).collect();
    entities
        .iter()
        .enumerate()
        .map(|(index, (_, distance))| *distance >= min_distance && !kept.contains(&index))
        .collect()
}

struct State {
    listeners: EventListeners,
    /// Description ids being removed, their Java set, and the filter.
    removal_ids: Vec<String>,
    set: Option<GlobalRef>,
    predicate: Option<GlobalRef>,
    last_sweep: Option<Instant>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

pub fn frame(snapshot: Option<Snapshot>) {
    let mut guard = STATE.lock().unwrap_or_else(|p| p.into_inner());
    let Some(s) = snapshot else {
        if let Some(state) = guard.as_mut() {
            state.listeners.clear();
        }
        return;
    };
    let state = guard.get_or_insert_with(|| State {
        listeners: EventListeners::default(),
        removal_ids: Vec::new(),
        set: None,
        predicate: None,
        last_sweep: None,
    });

    let list = lists::get(List::Entities);
    let hidden: Vec<String> = if s.toggle("Hide my list (Entity Culler window)") {
        list.iter().map(|id| description_id(id)).collect()
    } else {
        Vec::new()
    };
    state.listeners.ensure(
        "living",
        "net/neoforged/neoforge/client/event/RenderLivingEvent$Pre",
        hidden.join(","),
        |env, class| {
            let entity = java::game_class(env, ENTITY)?;
            let entity_type = java::game_class(env, ENTITY_TYPE)?;
            cancel_when(
                env,
                class,
                &[(class, "getEntity"), (&entity, "getType"), (&entity_type, "getDescriptionId")],
                &hidden,
            )
        },
    );

    if state.last_sweep.is_some_and(|last| last.elapsed() < SWEEP_EVERY) {
        return;
    }
    state.last_sweep = Some(Instant::now());
    let mut removal: BTreeSet<String> = REMOVALS
        .iter()
        .filter(|(name, _)| s.toggle(name))
        .flat_map(|(_, ids)| ids.iter().map(|id| description_id(id)))
        .collect();
    // Non-living entries of the list are removed (living ones are skipped
    // during the sweep — those are only hidden).
    removal.extend(hidden.iter().cloned());
    if removal.is_empty() {
        return;
    }
    let keep = s.slider("Keep nearest items (0 = none)").round().max(0.0) as usize;
    let min_distance = s.slider("Remove only farther than (blocks)").max(0.0) as f64;
    if let Err(error) = sweep(state, removal.into_iter().collect(), keep, min_distance) {
        log::debug!("Entity Culler: {error}");
    }
}

pub fn cleanup() {
    if let Some(state) = STATE.lock().unwrap_or_else(|p| p.into_inner()).as_mut() {
        state.listeners.clear();
    }
}

fn sweep(state: &mut State, ids: Vec<String>, keep: usize, min_distance: f64) -> anyhow::Result<()> {
    mapping().in_frame(|| {
        let mut env = mapping().get_env()?;
        let mc = minecraft().jni_ref().as_obj();
        let level = raw::field(&mut env, mc, "level", "Lnet/minecraft/client/multiplayer/ClientLevel;")?.l()?;
        let player = raw::field(&mut env, mc, "player", "Lnet/minecraft/client/player/LocalPlayer;")?.l()?;
        if level.is_null() || player.is_null() {
            return Ok(());
        }
        let predicate = filter(&mut env, state, &ids)?;
        let found = matching(&mut env, &level, predicate.as_obj())?;
        let count = env.get_array_length(&found)?;
        if count == 0 {
            return Ok(());
        }
        let me = position(&mut env, &player)?;
        let living = jni::objects::JClass::from(java::game_class(&mut env, LIVING)?);

        let mut candidates = Vec::new();
        let mut facts = Vec::new();
        for index in 0..count {
            let entity = env.get_object_array_element(&found, index)?;
            if env.is_instance_of(&entity, &living)? {
                env.delete_local_ref(entity)?;
                continue;
            }
            let kind = raw::call(&mut env, &entity, "getType", "()Lnet/minecraft/world/entity/EntityType;", &[])?.l()?;
            let description = raw::call(&mut env, &kind, "getDescriptionId", "()Ljava/lang/String;", &[])?.l()?;
            let is_item = mapping().get_string(description)? == ITEM;
            env.delete_local_ref(kind)?;
            let [x, y, z] = position(&mut env, &entity)?;
            let distance = ((x - me[0]).powi(2) + (y - me[1]).powi(2) + (z - me[2]).powi(2)).sqrt();
            facts.push((is_item, distance));
            candidates.push(entity);
        }
        for (entity, remove) in candidates.into_iter().zip(plan_removal(&facts, keep, min_distance)) {
            if remove {
                raw::call(&mut env, &entity, "discard", "()V", &[])?;
            }
            env.delete_local_ref(entity)?;
        }
        Ok(())
    })
}

fn position(env: &mut JNIEnv, entity: &JObject) -> anyhow::Result<[f64; 3]> {
    let mut out = [0.0; 3];
    for (slot, name) in out.iter_mut().zip(["getX", "getY", "getZ"]) {
        *slot = raw::call(env, entity, name, "()D", &[])?.d()?;
    }
    Ok(out)
}

/// The `Predicate` matching entities whose type description id is in `ids`.
fn filter(env: &mut JNIEnv, state: &mut State, ids: &[String]) -> anyhow::Result<GlobalRef> {
    if state.predicate.is_none() {
        let set = java::new_set(env)?;
        let entity = java::game_class(env, ENTITY)?;
        let entity_type = java::game_class(env, ENTITY_TYPE)?;
        let key = key_chain(env, &[(&entity, "getType"), (&entity_type, "getDescriptionId")])?;
        let test = contains_test(env, &set, &key)?;
        let predicate = proxy(env, "java/util/function/Predicate", &test)?;
        state.set = Some(env.new_global_ref(set)?);
        state.predicate = Some(env.new_global_ref(predicate)?);
        state.removal_ids.clear();
    }
    if state.removal_ids != ids {
        if let Some(set) = &state.set {
            java::fill_set(env, set.as_obj(), ids)?;
        }
        state.removal_ids = ids.to_vec();
    }
    state.predicate.clone().ok_or_else(|| anyhow::anyhow!("no entity filter"))
}

/// The world's entities that `predicate` accepts — filtered in Java
/// (`stream().filter(…).toArray()`), so only matches cross JNI.
fn matching<'l>(env: &mut JNIEnv<'l>, level: &JObject, predicate: &JObject) -> anyhow::Result<JObjectArray<'l>> {
    let all = raw::call(env, level, "entitiesForRendering", "()Ljava/lang/Iterable;", &[])?.l()?;
    let spliterator = raw::call(env, &all, "spliterator", "()Ljava/util/Spliterator;", &[])?.l()?;
    let stream = java::jstatic(
        env,
        "java/util/stream/StreamSupport",
        "stream",
        "(Ljava/util/Spliterator;Z)Ljava/util/stream/Stream;",
        &[JValue::Object(&spliterator), JValue::Bool(0)],
    )?;
    let filtered = raw::call(
        env,
        &stream,
        "filter",
        "(Ljava/util/function/Predicate;)Ljava/util/stream/Stream;",
        &[JValue::Object(predicate)],
    )?
    .l()?;
    let array = raw::call(env, &filtered, "toArray", "()[Ljava/lang/Object;", &[])?.l()?;
    Ok(JObjectArray::from(array))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn description_ids_follow_the_game() {
        assert_eq!(description_id("minecraft:armor_stand"), "entity.minecraft.armor_stand");
        assert_eq!(description_id("create:some/thing"), "entity.create.some.thing");
        assert_eq!(description_id("zombie"), "entity.minecraft.zombie");
    }

    #[test]
    fn the_nearest_items_are_kept() {
        // (is_item, distance)
        let entities = [(true, 30.0), (false, 1.0), (true, 2.0), (true, 10.0)];
        assert_eq!(plan_removal(&entities, 2, 0.0), vec![true, true, false, false]);
        assert_eq!(plan_removal(&entities, 0, 0.0), vec![true; 4]);
    }

    #[test]
    fn near_entities_can_be_spared() {
        let entities = [(false, 5.0), (true, 50.0), (false, 40.0)];
        assert_eq!(plan_removal(&entities, 0, 16.0), vec![false, true, true]);
    }
}
