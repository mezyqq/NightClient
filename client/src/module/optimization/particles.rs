//! Particles — hide particles by group, by type (any mod's too, chosen in
//! the Particles window), all of them, or cap how many exist.
//!
//! A hidden *type* is never created at all: its provider is taken out of the
//! particle engine's provider map (and put back when shown again), so it
//! costs nothing — the best kind of optimization. A few particles are built
//! directly rather than through a provider (block-break debris, firework
//! bursts); those are hidden by *class*, removed from the engine's queues
//! every frame by a Java-side `removeIf`. "Hide all" clears the engine every
//! frame; the cap trims the oldest particles above it.
//!
//! Firework sparks are never hidden through their provider: the firework
//! starter dereferences the spark it asks for without a null check, and
//! would crash. Mods may do the same with their own types — the window warns.

use super::java::{self, contains_test, key_chain, proxy};
use super::lists::{self, List};
use super::{optimization_module, slider, toggle, Snapshot};
use crate::mapping::raw;
use crate::mapping::MappedObject;
use crate::state::{mapping, minecraft};
use jni::objects::{GlobalRef, JObject, JValue};
use jni::JNIEnv;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const ENGINE: &str = "Lnet/minecraft/client/particle/ParticleEngine;";
const PKG: &str = "net.minecraft.client.particle.";
/// How often provider removal is re-checked (a resource reload re-adds them).
const SYNC_EVERY: Duration = Duration::from_millis(500);
/// Most particles trimmed per frame by the cap.
const MAX_TRIM: usize = 4096;

/// A switchable group: particle type ids, plus classes for the particles not
/// made through a provider.
pub struct Group {
    pub setting: &'static str,
    pub types: &'static [&'static str],
    pub classes: &'static [&'static str],
}

pub const GROUPS: &[Group] = &[
    Group { setting: "Hide rain splashes", types: &["rain"], classes: &[] },
    Group {
        setting: "Hide block break & dust",
        types: &["block", "dust_pillar", "falling_dust"],
        classes: &["TerrainParticle"],
    },
    Group {
        setting: "Hide drips",
        types: &[
            "dripping_water", "falling_water", "dripping_lava", "falling_lava", "landing_lava",
            "dripping_honey", "falling_honey", "landing_honey", "dripping_obsidian_tear",
            "falling_obsidian_tear", "landing_obsidian_tear", "dripping_dripstone_water",
            "falling_dripstone_water", "dripping_dripstone_lava", "falling_dripstone_lava",
            "falling_nectar", "falling_spore_blossom",
        ],
        classes: &[],
    },
    Group { setting: "Hide explosions", types: &["explosion", "explosion_emitter"], classes: &[] },
    Group {
        setting: "Hide smoke & clouds",
        types: &[
            "smoke", "large_smoke", "white_smoke", "campfire_cosy_smoke", "campfire_signal_smoke",
            "poof", "cloud",
        ],
        classes: &[],
    },
    Group {
        setting: "Hide flames & lava pops",
        types: &["flame", "soul_fire_flame", "small_flame", "lava", "soul"],
        classes: &[],
    },
    Group {
        setting: "Hide crits & sweeps",
        types: &["crit", "enchanted_hit", "sweep_attack", "damage_indicator"],
        classes: &[],
    },
    Group {
        setting: "Hide potion swirls",
        types: &["effect", "instant_effect", "entity_effect", "witch"],
        classes: &[],
    },
    Group { setting: "Hide totem burst", types: &["totem_of_undying"], classes: &[] },
    Group {
        setting: "Hide portal & end",
        types: &["portal", "reverse_portal", "end_rod", "dragon_breath"],
        classes: &[],
    },
    Group {
        setting: "Hide water & bubbles",
        types: &[
            "bubble", "bubble_pop", "bubble_column_up", "current_down", "underwater", "splash",
            "fishing", "dolphin", "squid_ink", "glow_squid_ink", "nautilus", "glow",
        ],
        classes: &[],
    },
    Group {
        setting: "Hide hearts & mob emotes",
        types: &["heart", "angry_villager", "happy_villager", "note", "composter", "egg_crack", "sneeze", "spit"],
        classes: &[],
    },
    Group {
        setting: "Hide fireworks",
        types: &[],
        classes: &["FireworkParticles$Starter", "FireworkParticles$SparkParticle", "FireworkParticles$OverlayParticle"],
    },
    Group {
        setting: "Hide enchanting runes",
        types: &["enchant", "vault_connection", "ominous_spawning"],
        classes: &[],
    },
    Group {
        setting: "Hide redstone & dust",
        types: &["dust", "dust_color_transition", "dust_plume", "wax_on", "wax_off", "scrape", "electric_spark"],
        classes: &[],
    },
    Group {
        setting: "Hide ambient (ash, spores, leaves)",
        types: &["ash", "white_ash", "crimson_spore", "warped_spore", "spore_blossom_air", "mycelium", "cherry_leaves"],
        classes: &[],
    },
    Group { setting: "Hide snowflakes", types: &["snowflake", "item_snowball"], classes: &[] },
    Group {
        setting: "Hide item crumbs (eating, breaking)",
        types: &["item", "item_slime", "item_cobweb"],
        classes: &[],
    },
    Group {
        setting: "Hide sculk & warden",
        types: &["sculk_soul", "sculk_charge", "sculk_charge_pop", "shriek", "sonic_boom", "vibration"],
        classes: &[],
    },
    Group {
        setting: "Hide wind & trial",
        types: &[
            "gust", "small_gust", "gust_emitter_large", "gust_emitter_small", "trial_spawner_detection",
            "trial_spawner_detection_ominous", "trial_omen", "raid_omen", "infested",
        ],
        classes: &[],
    },
    Group { setting: "Hide elder guardian jumpscare", types: &["elder_guardian"], classes: &[] },
];

/// Types never hidden through their provider (see the module docs).
const PROVIDER_UNSAFE: &[&str] = &["minecraft:firework", "minecraft:flash"];

fn settings() -> Vec<crate::module::ModuleSetting> {
    let mut settings = vec![
        toggle("Hide all particles", false),
        toggle("Hide my list (Particles window)", true),
        slider("Max particles (0 = no cap)", 0.0, 0.0, 5000.0),
    ];
    for group in GROUPS {
        settings.push(toggle(group.setting, group.setting == "Hide rain splashes"));
    }
    settings
}

optimization_module!(ParticlesModule, Particles, "Hide particles by group or type, or cap them", settings());

/// What the snapshot asks to hide: `(type ids, class names)`.
pub fn hidden(s: &Snapshot, custom: &BTreeSet<String>) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut types = BTreeSet::new();
    let mut classes = BTreeSet::new();
    for group in GROUPS.iter().filter(|g| s.toggle(g.setting)) {
        types.extend(group.types.iter().map(|t| format!("minecraft:{t}")));
        classes.extend(group.classes.iter().map(|c| format!("{PKG}{c}")));
    }
    if s.toggle("Hide my list (Particles window)") {
        types.extend(custom.iter().cloned());
    }
    types.retain(|t| !PROVIDER_UNSAFE.contains(&t.as_str()));
    (types, classes)
}

/// How many to take from each queue so the total fits `cap`, taking from
/// every queue in proportion to its size (rounded up, never more than it has).
pub fn plan_trim(sizes: &[usize], cap: usize) -> Vec<usize> {
    let total: usize = sizes.iter().sum();
    if total <= cap {
        return vec![0; sizes.len()];
    }
    let excess = total - cap;
    sizes
        .iter()
        .map(|&size| (size * excess).div_ceil(total).min(size))
        .collect()
}

struct State {
    /// Providers taken out, by type id.
    removed: BTreeMap<String, GlobalRef>,
    hidden_types: BTreeSet<String>,
    last_sync: Option<Instant>,
    /// Class names currently filtered, the set they live in, and the filter.
    classes: BTreeSet<String>,
    class_set: Option<GlobalRef>,
    predicate: Option<GlobalRef>,
}

static STATE: Mutex<State> = Mutex::new(State {
    removed: BTreeMap::new(),
    hidden_types: BTreeSet::new(),
    last_sync: None,
    classes: BTreeSet::new(),
    class_set: None,
    predicate: None,
});

/// Particles alive at the last count (for the window).
static COUNT: AtomicUsize = AtomicUsize::new(0);

pub fn count() -> usize {
    COUNT.load(Ordering::Relaxed)
}

pub fn frame(snapshot: Option<Snapshot>) {
    let mut state = STATE.lock().unwrap_or_else(|p| p.into_inner());
    let (types, classes, hide_all, cap) = match &snapshot {
        Some(s) => {
            let (types, classes) = hidden(s, &lists::get(List::Particles));
            let cap = s.slider("Max particles (0 = no cap)").round() as usize;
            (types, classes, s.toggle("Hide all particles"), cap)
        }
        None => Default::default(),
    };
    if snapshot.is_none() && state.removed.is_empty() {
        return;
    }

    let due = types != state.hidden_types || state.last_sync.is_none_or(|last| last.elapsed() >= SYNC_EVERY);
    let result = mapping().in_frame(|| {
        let mut env = mapping().get_env()?;
        let engine = raw::field(&mut env, minecraft().jni_ref().as_obj(), "particleEngine", ENGINE)?.l()?;
        if due {
            state.last_sync = Some(Instant::now());
            sync_providers(&mut env, &engine, &mut state, &types)?;
            COUNT.store(count_particles(&mut env, &engine).unwrap_or(0), Ordering::Relaxed);
        }
        if hide_all {
            raw::call(&mut env, &engine, "clearParticles", "()V", &[])?;
            return Ok(());
        }
        if !classes.is_empty() {
            let predicate = class_filter(&mut env, &mut state, &classes)?;
            remove_matching(&mut env, &engine, predicate.as_obj())?;
        }
        if cap > 0 {
            trim(&mut env, &engine, cap)?;
        }
        Ok(())
    });
    if let Err(error) = result {
        log::debug!("Particles: {error}");
    }
}

/// Puts every removed provider back.
pub fn cleanup() {
    let mut state = STATE.lock().unwrap_or_else(|p| p.into_inner());
    if state.removed.is_empty() {
        return;
    }
    let result = mapping().in_frame(|| {
        let mut env = mapping().get_env()?;
        let engine = raw::field(&mut env, minecraft().jni_ref().as_obj(), "particleEngine", ENGINE)?.l()?;
        sync_providers(&mut env, &engine, &mut state, &BTreeSet::new())
    });
    if let Err(error) = result {
        log::warn!("Particles: could not restore providers: {error}");
    }
    state.removed.clear();
}

// --- providers --------------------------------------------------------------------------

/// The engine's provider map: keyed by `ResourceLocation` (NeoForge) or by
/// the registry's int id (vanilla `Int2ObjectMap`, also a `java.util.Map`).
fn providers<'l>(env: &mut JNIEnv<'l>, engine: &JObject) -> anyhow::Result<(JObject<'l>, bool)> {
    if let Ok(map) = raw::field(env, engine, "providers", "Ljava/util/Map;") {
        return Ok((map.l()?, false));
    }
    let map = raw::field(env, engine, "providers", "Lit/unimi/dsi/fastutil/ints/Int2ObjectMap;")?.l()?;
    Ok((map, true))
}

fn resource_location<'l>(env: &mut JNIEnv<'l>, id: &str) -> anyhow::Result<JObject<'l>> {
    let class = mapping().resolve_class(env, "net/minecraft/resources/ResourceLocation")?;
    let text: JObject = env.new_string(id)?.into();
    let parsed = env.call_static_method(
        &class,
        "parse",
        "(Ljava/lang/String;)Lnet/minecraft/resources/ResourceLocation;",
        &[JValue::Object(&text)],
    );
    match parsed {
        Ok(value) => Ok(value.l()?),
        Err(_) => {
            let _ = env.exception_clear();
            Ok(env
                .new_object(&class, "(Ljava/lang/String;)V", &[JValue::Object(&text)])
                .inspect_err(|_| {
                    let _ = env.exception_clear();
                })?)
        }
    }
}

/// The provider-map key for particle type `id`; `None` for an unknown type.
fn provider_key<'l>(env: &mut JNIEnv<'l>, id: &str, by_int: bool) -> anyhow::Result<Option<JObject<'l>>> {
    let location = resource_location(env, id)?;
    if !by_int {
        return Ok(Some(location));
    }
    let class = mapping().resolve_class(env, "net/minecraft/core/registries/BuiltInRegistries")?;
    let registry = env
        .get_static_field(class, "PARTICLE_TYPE", "Lnet/minecraft/core/Registry;")
        .inspect_err(|_| {
            let _ = env.exception_clear();
        })?
        .l()?;
    let kind = raw::call(
        env,
        &registry,
        "get",
        "(Lnet/minecraft/resources/ResourceLocation;)Ljava/lang/Object;",
        &[JValue::Object(&location)],
    )?
    .l()?;
    if kind.is_null() {
        return Ok(None);
    }
    let index = raw::call(env, &registry, "getId", "(Ljava/lang/Object;)I", &[JValue::Object(&kind)])?.i()?;
    Ok(Some(java::jstatic(env, "java/lang/Integer", "valueOf", "(I)Ljava/lang/Integer;", &[JValue::Int(index)])?))
}

/// Takes the providers of `types` out of the engine and puts back those of
/// types no longer hidden. Also re-takes any a resource reload put back.
fn sync_providers(env: &mut JNIEnv, engine: &JObject, state: &mut State, types: &BTreeSet<String>) -> anyhow::Result<()> {
    let (map, by_int) = providers(env, engine)?;
    for id in types {
        env.with_local_frame(16, |env| -> anyhow::Result<()> {
            let Some(key) = provider_key(env, id, by_int)? else {
                return Ok(());
            };
            let provider = raw::call(env, &map, "remove", "(Ljava/lang/Object;)Ljava/lang/Object;", &[JValue::Object(&key)])?.l()?;
            if !provider.is_null() {
                state.removed.insert(id.clone(), env.new_global_ref(provider)?);
            }
            Ok(())
        })?;
    }
    let shown: Vec<String> = state.removed.keys().filter(|id| !types.contains(*id)).cloned().collect();
    for id in shown {
        let Some(provider) = state.removed.remove(&id) else {
            continue;
        };
        env.with_local_frame(16, |env| -> anyhow::Result<()> {
            if let Some(key) = provider_key(env, &id, by_int)? {
                raw::call(
                    env,
                    &map,
                    "put",
                    "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                    &[JValue::Object(&key), JValue::Object(provider.as_obj())],
                )?;
            }
            Ok(())
        })?;
    }
    state.hidden_types = types.clone();
    Ok(())
}

// --- queues ----------------------------------------------------------------------------------

/// The engine's particle queues (the per-render-type queues, then the queue
/// of particles waiting to be added).
fn queues<'l>(env: &mut JNIEnv<'l>, engine: &JObject) -> anyhow::Result<Vec<JObject<'l>>> {
    let mut out = Vec::new();
    let map = raw::field(env, engine, "particles", "Ljava/util/Map;")?.l()?;
    let values = raw::call(env, &map, "values", "()Ljava/util/Collection;", &[])?.l()?;
    let array = raw::call(env, &values, "toArray", "()[Ljava/lang/Object;", &[])?.l()?;
    let array = jni::objects::JObjectArray::from(array);
    for index in 0..env.get_array_length(&array)? {
        let queue = env.get_object_array_element(&array, index)?;
        // Newer versions keep a non-collection "group" here — skipped.
        if env.is_instance_of(&queue, "java/util/Collection")? {
            out.push(queue);
        }
    }
    if let Ok(pending) = raw::field(env, engine, "particlesToAdd", "Ljava/util/Queue;") {
        out.push(pending.l()?);
    }
    Ok(out)
}

fn count_particles(env: &mut JNIEnv, engine: &JObject) -> anyhow::Result<usize> {
    let mut total = 0;
    for queue in queues(env, engine)? {
        total += raw::call(env, &queue, "size", "()I", &[])?.i()?.max(0) as usize;
    }
    Ok(total)
}

/// The `Predicate` removing particles whose class name is in `classes`,
/// rebuilt only when the names change (the set is refilled in place).
fn class_filter(env: &mut JNIEnv, state: &mut State, classes: &BTreeSet<String>) -> anyhow::Result<GlobalRef> {
    if state.predicate.is_none() {
        let set = java::new_set(env)?;
        let object = java::jclass(env, "java/lang/Object")?;
        let class = java::jclass(env, "java/lang/Class")?;
        let key = key_chain(env, &[(&object, "getClass"), (&class, "getName")])?;
        let test = contains_test(env, &set, &key)?;
        let predicate = proxy(env, "java/util/function/Predicate", &test)?;
        state.class_set = Some(env.new_global_ref(set)?);
        state.predicate = Some(env.new_global_ref(predicate)?);
        state.classes.clear();
    }
    if &state.classes != classes {
        let names: Vec<String> = classes.iter().cloned().collect();
        if let Some(set) = &state.class_set {
            java::fill_set(env, set.as_obj(), &names)?;
        }
        state.classes = classes.clone();
    }
    state.predicate.clone().ok_or_else(|| anyhow::anyhow!("no particle filter"))
}

fn remove_matching(env: &mut JNIEnv, engine: &JObject, predicate: &JObject) -> anyhow::Result<()> {
    for queue in queues(env, engine)? {
        raw::call(env, &queue, "removeIf", "(Ljava/util/function/Predicate;)Z", &[JValue::Object(predicate)])?;
    }
    Ok(())
}

/// Removes the oldest particles above `cap`.
fn trim(env: &mut JNIEnv, engine: &JObject, cap: usize) -> anyhow::Result<()> {
    let queues = queues(env, engine)?;
    let mut sizes = Vec::with_capacity(queues.len());
    for queue in &queues {
        sizes.push(raw::call(env, queue, "size", "()I", &[])?.i()?.max(0) as usize);
    }
    let mut budget = MAX_TRIM;
    for (queue, take) in queues.iter().zip(plan_trim(&sizes, cap)) {
        let take = take.min(budget);
        budget -= take;
        for _ in 0..take {
            let removed = raw::call(env, queue, "poll", "()Ljava/lang/Object;", &[])?.l()?;
            env.delete_local_ref(removed)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trimming_is_proportional_and_reaches_the_cap() {
        assert_eq!(plan_trim(&[100, 50], 200), vec![0, 0]);
        let take = plan_trim(&[300, 100], 200);
        assert_eq!(take, vec![150, 50]);
        let take = plan_trim(&[7, 3, 0], 5);
        assert!(take.iter().sum::<usize>() >= 5);
        assert!(take.iter().zip([7, 3, 0]).all(|(t, s)| *t <= s));
        assert_eq!(plan_trim(&[10], 0), vec![10]);
    }

    #[test]
    fn group_types_are_unique_and_namespaced_once() {
        let mut seen = BTreeSet::new();
        for group in GROUPS {
            for kind in group.types {
                assert!(!kind.contains(':'), "{kind}");
                assert!(seen.insert(*kind), "{kind} is in two groups");
            }
        }
    }

    #[test]
    fn fireworks_are_never_hidden_through_providers() {
        let mut custom = BTreeSet::new();
        custom.insert("minecraft:firework".to_string());
        custom.insert("mymod:sparkle".to_string());
        let snapshot = Snapshot::for_test(&[("Hide my list (Particles window)", true), ("Hide fireworks", true)]);
        let (types, classes) = hidden(&snapshot, &custom);
        assert!(!types.contains("minecraft:firework"));
        assert!(types.contains("mymod:sparkle"));
        assert!(classes.contains("net.minecraft.client.particle.FireworkParticles$Starter"));
    }
}
