//! Shared helpers for modules that act through the player's hands and keys:
//! the hotbar selection, what an item stack is, and holding the use key.
//!
//! Direct JNI with Mojmap descriptors (see [`crate::mapping::raw`]); where a
//! member changed across versions both spellings are tried.

use crate::mapping::raw;
use crate::mapping::MappedObject;
use crate::state::{mapping, minecraft};
use jni::objects::{JObject, JValue};
use jni::JNIEnv;

pub const INVENTORY_SIG: &str = "()Lnet/minecraft/world/entity/player/Inventory;";
pub const STACK_SIG: &str = "Lnet/minecraft/world/item/ItemStack;";

/// The local player's `Inventory` (a local reference).
pub fn inventory<'local>(env: &mut JNIEnv<'local>, player: &JObject) -> anyhow::Result<JObject<'local>> {
    Ok(raw::call(env, player, "getInventory", INVENTORY_SIG, &[])?.l()?)
}

/// The selected hotbar slot, 0-8. A getter from 1.21.5 on, a public field before.
pub fn selected_slot(env: &mut JNIEnv, inventory: &JObject) -> anyhow::Result<i32> {
    if let Ok(value) = raw::call(env, inventory, "getSelectedSlot", "()I", &[]) {
        return Ok(value.i()?);
    }
    Ok(raw::field(env, inventory, "selected", "I")?.i()?)
}

/// Selects hotbar slot `slot` (0-8). The client tells the server on its own
/// before the next use / attack / dig.
pub fn set_selected_slot(env: &mut JNIEnv, inventory: &JObject, slot: i32) -> anyhow::Result<()> {
    if raw::call(env, inventory, "setSelectedSlot", "(I)V", &[JValue::Int(slot)]).is_ok() {
        return Ok(());
    }
    raw::set_field(env, inventory, "selected", "I", JValue::Int(slot))
}

/// The stack in inventory slot `slot` (a local reference).
pub fn stack_in<'local>(env: &mut JNIEnv<'local>, inventory: &JObject, slot: i32) -> anyhow::Result<JObject<'local>> {
    Ok(raw::call(env, inventory, "getItem", &format!("(I){STACK_SIG}"), &[JValue::Int(slot)])?.l()?)
}

/// The id of the item in `stack`, e.g. `minecraft:ender_pearl`.
/// `Item.toString` gives the full id from 1.20.5 on and the bare path before.
pub fn item_id(env: &mut JNIEnv, stack: &JObject) -> anyhow::Result<String> {
    let item = raw::call(env, stack, "getItem", "()Lnet/minecraft/world/item/Item;", &[])?.l()?;
    Ok(normalize_id(&raw::to_string(env, &item)?))
}

/// `apple` → `minecraft:apple`; full ids pass through.
pub fn normalize_id(id: &str) -> String {
    if id.contains(':') {
        id.to_string()
    } else {
        format!("minecraft:{id}")
    }
}

/// Holds (`true`) or releases the use key (right mouse by default), as if the
/// player pressed it.
pub fn hold_use_key(down: bool) -> anyhow::Result<()> {
    mapping().in_frame(|| {
        let mut env = mapping().get_env()?;
        let options = raw::field(
            &mut env,
            minecraft().jni_ref().as_obj(),
            "options",
            "Lnet/minecraft/client/Options;",
        )?
        .l()?;
        let key = raw::field(&mut env, &options, "keyUse", "Lnet/minecraft/client/KeyMapping;")?.l()?;
        raw::call(&mut env, &key, "setDown", "(Z)V", &[JValue::Bool(down as u8)])?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_item_paths_get_the_minecraft_namespace() {
        assert_eq!(normalize_id("apple"), "minecraft:apple");
        assert_eq!(normalize_id("create:wrench"), "create:wrench");
    }
}
