//! Read-only views of Minecraft's built-in registries — the ids of every
//! block and entity type the game knows (modded ones included). Used to offer
//! searchable lists in the GUI.
//!
//! The registry fields are declared with their exact `DefaultedRegistry` type,
//! so the lookups use explicit descriptors.

use crate::state::mapping;
use std::sync::OnceLock;

const REGISTRIES: &str = "net/minecraft/core/registries/BuiltInRegistries";
const DEFAULTED: &str = "Lnet/minecraft/core/DefaultedRegistry;";

/// Every block id (`minecraft:stone`, `create:andesite_casing`, …), sorted.
/// Read once — registries are frozen after startup.
pub fn block_ids() -> &'static [String] {
    static IDS: OnceLock<Vec<String>> = OnceLock::new();
    cached(&IDS, "BLOCK", DEFAULTED)
}

/// Every item id (`minecraft:diamond`, …), sorted.
pub fn item_ids() -> &'static [String] {
    static IDS: OnceLock<Vec<String>> = OnceLock::new();
    cached(&IDS, "ITEM", DEFAULTED)
}

/// Every entity type id (`minecraft:zombie`, …), sorted.
pub fn entity_type_ids() -> &'static [String] {
    static IDS: OnceLock<Vec<String>> = OnceLock::new();
    cached(&IDS, "ENTITY_TYPE", DEFAULTED)
}

/// Every particle type id (`minecraft:rain`, …), sorted.
pub fn particle_type_ids() -> &'static [String] {
    static IDS: OnceLock<Vec<String>> = OnceLock::new();
    cached(&IDS, "PARTICLE_TYPE", "Lnet/minecraft/core/Registry;")
}

fn cached(slot: &'static OnceLock<Vec<String>>, field: &str, descriptor: &str) -> &'static [String] {
    if let Some(ids) = slot.get() {
        return ids;
    }
    match read_ids(field, descriptor) {
        Ok(ids) => slot.get_or_init(|| ids),
        // Not cached on failure, so a later call can retry.
        Err(error) => {
            if let Ok(env) = mapping().get_env() {
                let _ = env.exception_clear();
            }
            log::warn!("registry: could not read {field}: {error}");
            &[]
        }
    }
}

fn read_ids(field: &str, descriptor: &str) -> anyhow::Result<Vec<String>> {
    mapping().in_frame(|| {
        let mut env = mapping().get_env()?;
        let class = mapping().resolve_class(&mut env, REGISTRIES)?;
        let registry = env.get_static_field(class, field, descriptor)?.l()?;
        let keys = env
            .call_method(&registry, "keySet", "()Ljava/util/Set;", &[])?
            .l()?;
        let iterator = env
            .call_method(&keys, "iterator", "()Ljava/util/Iterator;", &[])?
            .l()?;
        let mut ids = Vec::new();
        while env.call_method(&iterator, "hasNext", "()Z", &[])?.z()? {
            // Each key gets its own small frame: thousands of entries would
            // otherwise overflow the enclosing local-reference frame.
            let id = env.with_local_frame(4, |env| -> anyhow::Result<String> {
                let key = env
                    .call_method(&iterator, "next", "()Ljava/lang/Object;", &[])?
                    .l()?;
                let text = env
                    .call_method(&key, "toString", "()Ljava/lang/String;", &[])?
                    .l()?;
                mapping().get_string(text)
            })?;
            ids.push(id);
        }
        ids.sort();
        Ok(ids)
    })
}
