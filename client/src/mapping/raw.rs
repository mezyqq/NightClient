//! Direct JNI calls with explicit Mojmap descriptors, for classes the mapping
//! layer does not model and for hot paths where its per-call lookup would
//! cost too much. Like the packet layer, these only resolve on builds that
//! run with Mojmap names — unobfuscated Minecraft (26.1+) and Mojmap-based
//! mod loaders such as NeoForge.
//!
//! Every helper clears a pending Java exception on failure, so a failed call
//! never poisons the next JNI call on this thread.

use jni::objects::{JObject, JValue, JValueOwned};
use jni::JNIEnv;

/// Calls the instance method `name` with descriptor `sig` on `obj`.
pub fn call<'local>(
    env: &mut JNIEnv<'local>,
    obj: &JObject,
    name: &str,
    sig: &str,
    args: &[JValue],
) -> anyhow::Result<JValueOwned<'local>> {
    super::member_cache::call(env, obj, name, sig, args).map_err(|error| {
        let _ = env.exception_clear();
        anyhow::anyhow!("{name}{sig}: {error}")
    })
}

/// Reads the instance field `name` of type `sig` from `obj`.
pub fn field<'local>(
    env: &mut JNIEnv<'local>,
    obj: &JObject,
    name: &str,
    sig: &str,
) -> anyhow::Result<JValueOwned<'local>> {
    super::member_cache::get_field(env, obj, name, sig).map_err(|error| {
        let _ = env.exception_clear();
        anyhow::anyhow!("field {name} {sig}: {error}")
    })
}

/// Writes the instance field `name` of type `sig` on `obj`.
pub fn set_field(
    env: &mut JNIEnv,
    obj: &JObject,
    name: &str,
    sig: &str,
    value: JValue,
) -> anyhow::Result<()> {
    super::member_cache::set_field(env, obj, name, sig, value).map_err(|error| {
        let _ = env.exception_clear();
        anyhow::anyhow!("field {name} {sig}: {error}")
    })
}

/// `obj.toString()` as a Rust string.
pub fn to_string(env: &mut JNIEnv, obj: &JObject) -> anyhow::Result<String> {
    let text = call(env, obj, "toString", "()Ljava/lang/String;", &[])?.l()?;
    crate::state::mapping().get_string(text)
}
