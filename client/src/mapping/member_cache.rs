//! Cache of JNI member ids for calls made by name and descriptor.
//!
//! `JNIEnv::call_method` / `get_field` look the member up on every call:
//! two C-string conversions, a `GetMethodID` / `GetFieldID` (a symbol-table
//! lookup plus a search through the class and its supertypes) and a parse of
//! the descriptor. A *missing* member is worse — the JVM builds a
//! `NoSuchMethodError` / `NoSuchFieldError` with a stack trace, and the
//! version fallbacks ("try the new name, then the old one") hit that on
//! every call. Here each `(runtime class, name, descriptor)` is resolved
//! once, and misses are remembered too.
//!
//! Classes are keyed by their identity hash (`Object.hashCode`, which `Class`
//! does not override) and confirmed with `IsSameObject`; the cache holds a
//! global reference to each class, so the ids stay valid.

use jni::objects::{GlobalRef, JClass, JFieldID, JMethodID, JObject, JValue, JValueOwned};
use jni::signature::{Primitive, ReturnType};
use jni::sys::jvalue;
use jni::JNIEnv;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Mutex, OnceLock};

#[derive(Clone, Copy)]
enum Member {
    Method(JMethodID),
    Field(JFieldID),
    Missing,
}

struct Entry {
    class: GlobalRef,
    field: bool,
    name: Box<str>,
    sig: Box<str>,
    member: Member,
}

static CACHE: Mutex<Option<HashMap<u64, Vec<Entry>>>> = Mutex::new(None);

/// `Object.hashCode()I`, resolved once (stored as the raw id).
static HASH_CODE: OnceLock<usize> = OnceLock::new();

fn identity_hash(env: &mut JNIEnv, class: &JClass) -> jni::errors::Result<i32> {
    let id = match HASH_CODE.get() {
        Some(id) => *id,
        None => {
            let object = env.find_class("java/lang/Object")?;
            let id = env.get_method_id(&object, "hashCode", "()I")?;
            let _ = env.delete_local_ref(object);
            *HASH_CODE.get_or_init(|| id.into_raw() as usize)
        }
    };
    let id = unsafe { JMethodID::from_raw(id as jni::sys::jmethodID) };
    unsafe { env.call_method_unchecked(class, id, ReturnType::Primitive(Primitive::Int), &[]) }?.i()
}

fn key(class_hash: i32, field: bool, name: &str, sig: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    (class_hash, field, name, sig).hash(&mut hasher);
    hasher.finish()
}

/// The member `name` / `sig` of `obj`'s runtime class, resolved at most once.
fn resolve(env: &mut JNIEnv, obj: &JObject, field: bool, name: &str, sig: &str) -> anyhow::Result<Member> {
    let class = env.get_object_class(obj)?;
    let result = resolve_on(env, &class, field, name, sig);
    let _ = env.delete_local_ref(class);
    result
}

fn resolve_on(env: &mut JNIEnv, class: &JClass, field: bool, name: &str, sig: &str) -> anyhow::Result<Member> {
    let key = key(identity_hash(env, class)?, field, name, sig);
    {
        let cache = CACHE.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(entries) = cache.as_ref().and_then(|map| map.get(&key)) {
            for entry in entries {
                if entry.field == field
                    && *entry.name == *name
                    && *entry.sig == *sig
                    && env.is_same_object(entry.class.as_obj(), class)?
                {
                    return Ok(entry.member);
                }
            }
        }
    }

    // Resolved outside the lock: a lookup may initialise the class, which
    // runs Java code.
    let member = if field {
        match env.get_field_id(class, name, sig) {
            Ok(id) => Member::Field(id),
            Err(_) => Member::Missing,
        }
    } else {
        match env.get_method_id(class, name, sig) {
            Ok(id) => Member::Method(id),
            Err(_) => Member::Missing,
        }
    };
    if matches!(member, Member::Missing) {
        let _ = env.exception_clear();
    }
    let entry = Entry {
        class: env.new_global_ref(class)?,
        field,
        name: name.into(),
        sig: sig.into(),
        member,
    };
    CACHE
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get_or_insert_with(HashMap::new)
        .entry(key)
        .or_default()
        .push(entry);
    Ok(member)
}

/// Pure: the JNI value kind a descriptor names — a field type (`I`,
/// `Ljava/lang/String;`) or a method's return type (`(I)V` → `V`).
pub fn value_type(sig: &str) -> ReturnType {
    let ty = sig.rsplit(')').next().unwrap_or(sig);
    match ty.as_bytes().first() {
        Some(b'Z') => ReturnType::Primitive(Primitive::Boolean),
        Some(b'B') => ReturnType::Primitive(Primitive::Byte),
        Some(b'C') => ReturnType::Primitive(Primitive::Char),
        Some(b'D') => ReturnType::Primitive(Primitive::Double),
        Some(b'F') => ReturnType::Primitive(Primitive::Float),
        Some(b'I') => ReturnType::Primitive(Primitive::Int),
        Some(b'J') => ReturnType::Primitive(Primitive::Long),
        Some(b'S') => ReturnType::Primitive(Primitive::Short),
        Some(b'V') => ReturnType::Primitive(Primitive::Void),
        Some(b'[') => ReturnType::Array,
        _ => ReturnType::Object,
    }
}

/// The primitive a single descriptor character names, `None` for a
/// reference type (`L…;` or an array).
fn primitive(ch: u8) -> Option<Primitive> {
    Some(match ch {
        b'Z' => Primitive::Boolean,
        b'B' => Primitive::Byte,
        b'C' => Primitive::Char,
        b'D' => Primitive::Double,
        b'F' => Primitive::Float,
        b'I' => Primitive::Int,
        b'J' => Primitive::Long,
        b'S' => Primitive::Short,
        _ => return None,
    })
}

/// Pure: whether `args` fit the parameters of method descriptor `sig` — the
/// count and, per argument, primitive kind vs. reference. Calling through a
/// cached id skips JNI's own check, and a mismatch there corrupts the JVM.
pub fn args_match(sig: &str, args: &[Option<Primitive>]) -> bool {
    let Some(params) = sig.strip_prefix('(').and_then(|rest| rest.split(')').next()) else {
        return false;
    };
    let bytes = params.as_bytes();
    let mut i = 0;
    let mut given = args.iter();
    while i < bytes.len() {
        let start = i;
        while bytes.get(i) == Some(&b'[') {
            i += 1;
        }
        if bytes.get(i) == Some(&b'L') {
            while bytes.get(i).is_some_and(|b| *b != b';') {
                i += 1;
            }
        }
        let Some(&ch) = bytes.get(i) else {
            return false;
        };
        let expected = if i > start { None } else { primitive(ch) };
        i += 1;
        match given.next() {
            Some(actual) if *actual == expected => {}
            _ => return false,
        }
    }
    given.next().is_none()
}

fn missing(what: &str, name: &str, sig: &str) -> anyhow::Error {
    anyhow::anyhow!("{what} {name} {sig} not found")
}

/// Calls the instance method `name` / `sig` on `obj`.
pub fn call<'local>(
    env: &mut JNIEnv<'local>,
    obj: &JObject,
    name: &str,
    sig: &str,
    args: &[JValue],
) -> anyhow::Result<JValueOwned<'local>> {
    if obj.is_null() {
        anyhow::bail!("{name}{sig}: null receiver");
    }
    let kinds: Vec<Option<Primitive>> = args.iter().map(JValue::primitive_type).collect();
    if !args_match(sig, &kinds) {
        anyhow::bail!("{name}{sig}: arguments do not match the descriptor");
    }
    let Member::Method(id) = resolve(env, obj, false, name, sig)? else {
        return Err(missing("method", name, sig));
    };
    let args: Vec<jvalue> = args.iter().map(JValue::as_jni).collect();
    Ok(unsafe { env.call_method_unchecked(obj, id, value_type(sig), &args) }?)
}

/// Reads the instance field `name` of type `sig` from `obj`.
pub fn get_field<'local>(env: &mut JNIEnv<'local>, obj: &JObject, name: &str, sig: &str) -> anyhow::Result<JValueOwned<'local>> {
    if obj.is_null() {
        anyhow::bail!("field {name}: null receiver");
    }
    let Member::Field(id) = resolve(env, obj, true, name, sig)? else {
        return Err(missing("field", name, sig));
    };
    Ok(env.get_field_unchecked(obj, id, value_type(sig))?)
}

/// Writes the instance field `name` of type `sig` on `obj`.
pub fn set_field(env: &mut JNIEnv, obj: &JObject, name: &str, sig: &str, value: JValue) -> anyhow::Result<()> {
    if obj.is_null() {
        anyhow::bail!("field {name}: null receiver");
    }
    if value.primitive_type() != sig.bytes().next().and_then(primitive) {
        anyhow::bail!("field {name} {sig}: value of the wrong type");
    }
    let Member::Field(id) = resolve(env, obj, true, name, sig)? else {
        return Err(missing("field", name, sig));
    };
    Ok(env.set_field_unchecked(obj, id, value)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptors_map_to_value_types() {
        assert!(matches!(value_type("()Z"), ReturnType::Primitive(Primitive::Boolean)));
        assert!(matches!(value_type("(Lnet/minecraft/core/BlockPos;)V"), ReturnType::Primitive(Primitive::Void)));
        assert!(matches!(value_type("(F)Lnet/minecraft/world/phys/Vec3;"), ReturnType::Object));
        assert!(matches!(value_type("()[I"), ReturnType::Array));
        assert!(matches!(value_type("D"), ReturnType::Primitive(Primitive::Double)));
        assert!(matches!(value_type("Lnet/minecraft/client/Options;"), ReturnType::Object));
    }

    #[test]
    fn arguments_are_checked_against_the_descriptor() {
        let int = Some(Primitive::Int);
        let float = Some(Primitive::Float);
        assert!(args_match("()Z", &[]));
        assert!(args_match("(F)Lnet/minecraft/world/phys/Vec3;", &[float]));
        assert!(args_match("(ILnet/minecraft/core/BlockPos;[I[Ljava/lang/String;Z)V", &[int, None, None, None, Some(Primitive::Boolean)]));
        assert!(!args_match("(F)V", &[int]));
        assert!(!args_match("(F)V", &[]));
        assert!(!args_match("()V", &[None]));
        assert!(!args_match("(Lnet/minecraft/core/BlockPos;)V", &[int]));
        assert!(!args_match("(Ljava/lang/String", &[None]));
        assert!(!args_match("I", &[]));
    }
}
