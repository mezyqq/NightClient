//! Game options (video settings, accessibility sliders, volumes) forced by
//! the optimization modules — and put back when no module wants them any
//! more.
//!
//! Each frame the modules call [`want`] for every option they force; then
//! [`commit`] applies the result (a later `want` of the same key wins, so
//! Background FPS overrides FPS Boost's frame cap while unfocused). The first
//! time a key is forced its original value is recorded — in
//! `optimization_originals.json` as well, so a crash or a re-inject still
//! restores it — and once nothing wants the key it is restored.
//!
//! Values go through `OptionInstance.set`, so the game reacts as if the user
//! had moved the slider (chunks reload after a graphics change, the sound
//! engine picks up a volume). `set` replaces an out-of-range value with the
//! option's *default*, so callers clamp to the vanilla ranges, and a value the
//! game still does not take is not retried until it changes.

use crate::config;
use crate::mapping::raw;
use crate::mapping::MappedObject;
use crate::state::{mapping, minecraft};
use jni::objects::{JObject, JValue};
use jni::JNIEnv;
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const FILE: &str = "optimization_originals.json";
const OPTION_INSTANCE: &str = "Lnet/minecraft/client/OptionInstance;";
/// How often unchanged wants are re-checked (the user may move a slider).
const RECHECK: Duration = Duration::from_millis(500);

/// An option value.
#[derive(Debug, Clone, PartialEq)]
pub enum OptVal {
    Int(i32),
    Double(f64),
    Bool(bool),
    /// An enum constant, by name (`FAST`, `OFF`, …).
    Enum(String),
}

impl OptVal {
    /// Text form, as stored in the originals file.
    pub fn repr(&self) -> String {
        match self {
            OptVal::Int(value) => format!("i:{value}"),
            OptVal::Double(value) => format!("d:{value}"),
            OptVal::Bool(value) => format!("b:{value}"),
            OptVal::Enum(name) => format!("e:{name}"),
        }
    }

    pub fn parse(text: &str) -> Option<OptVal> {
        let (kind, value) = text.split_once(':')?;
        Some(match kind {
            "i" => OptVal::Int(value.parse().ok()?),
            "d" => OptVal::Double(value.parse().ok()?),
            "b" => OptVal::Bool(value.parse().ok()?),
            "e" if !value.is_empty() => OptVal::Enum(value.to_string()),
            _ => return None,
        })
    }

    /// Equal, with doubles compared loosely (the game rounds some of them).
    pub fn same(&self, other: &OptVal) -> bool {
        match (self, other) {
            (OptVal::Double(a), OptVal::Double(b)) => (a - b).abs() < 1e-4,
            _ => self == other,
        }
    }
}

/// The wants of the frame being built; the last one per key wins.
static WANTED: Mutex<Vec<(String, OptVal)>> = Mutex::new(Vec::new());

/// Forces option `key` to `value` for this frame. A key is an `Options`
/// field name (`renderDistance`, `bobView`, …) or `volume:<SOURCE>`.
pub fn want(key: &str, value: OptVal) {
    WANTED.lock().unwrap_or_else(|p| p.into_inner()).push((key.to_string(), value));
}

/// Collapses wants to one per key, keeping the last.
pub fn last_wins(wants: Vec<(String, OptVal)>) -> BTreeMap<String, OptVal> {
    wants.into_iter().collect()
}

struct State {
    /// `None` until read from disk.
    originals: Option<BTreeMap<String, String>>,
    /// What the last commit applied.
    applied: BTreeMap<String, OptVal>,
    /// Values the game refused, per key — not retried until they change.
    rejected: BTreeMap<String, String>,
    last_run: Option<Instant>,
}

static STATE: Mutex<State> = Mutex::new(State {
    originals: None,
    applied: BTreeMap::new(),
    rejected: BTreeMap::new(),
    last_run: None,
});

fn load_originals() -> BTreeMap<String, String> {
    std::fs::read_to_string(config::base_dir().join(FILE))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn save_originals(originals: &BTreeMap<String, String>) {
    let path = config::base_dir().join(FILE);
    let result = if originals.is_empty() {
        match std::fs::remove_file(&path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    } else {
        std::fs::write(&path, serde_json::to_string_pretty(originals).unwrap_or_default())
    };
    if let Err(error) = result {
        log::warn!("Optimization: could not write {}: {error}", path.display());
    }
}

/// Applies this frame's wants and restores what is no longer wanted. Called
/// once per frame, after every module made its wants, on the render thread.
pub fn commit() {
    let wants = last_wins(std::mem::take(&mut *WANTED.lock().unwrap_or_else(|p| p.into_inner())));
    let mut state = STATE.lock().unwrap_or_else(|p| p.into_inner());
    let originals = state.originals.get_or_insert_with(load_originals);
    if wants.is_empty() && originals.is_empty() {
        state.applied.clear();
        return;
    }
    let nothing_to_restore = originals.keys().all(|key| wants.contains_key(key));
    let unchanged = wants.len() == state.applied.len()
        && wants.iter().all(|(key, value)| state.applied.get(key).is_some_and(|v| v.same(value)));
    if unchanged && nothing_to_restore && state.last_run.is_some_and(|last| last.elapsed() < RECHECK) {
        return;
    }
    state.last_run = Some(Instant::now());
    if let Err(error) = run(&mut state, &wants) {
        log::debug!("Optimization: options: {error}");
    }
    state.applied = wants;
}

fn run(state: &mut State, wants: &BTreeMap<String, OptVal>) -> anyhow::Result<()> {
    mapping().in_frame(|| {
        let mut env = mapping().get_env()?;
        let options = raw::field(&mut env, minecraft().jni_ref().as_obj(), "options", "Lnet/minecraft/client/Options;")?.l()?;
        let originals = state.originals.get_or_insert_with(BTreeMap::new);
        let mut dirty = false;

        for (key, value) in wants {
            let result = env.with_local_frame(16, |env| -> anyhow::Result<()> {
                let option = instance(env, &options, key)?;
                let current_object = raw::call(env, &option, "get", "()Ljava/lang/Object;", &[])?.l()?;
                let Some(current) = read(env, &current_object)? else {
                    return Ok(());
                };
                if !originals.contains_key(key) {
                    originals.insert(key.clone(), current.repr());
                    dirty = true;
                }
                if current.same(value) || state.rejected.get(key) == Some(&value.repr()) {
                    return Ok(());
                }
                let object = make(env, value, &current_object)?;
                raw::call(env, &option, "set", "(Ljava/lang/Object;)V", &[JValue::Object(&object)])?;
                let after = raw::call(env, &option, "get", "()Ljava/lang/Object;", &[])?.l()?;
                if !read(env, &after)?.is_some_and(|after| after.same(value)) {
                    log::warn!("Optimization: the game did not accept {key} = {}", value.repr());
                    state.rejected.insert(key.clone(), value.repr());
                }
                Ok(())
            });
            if let Err(error) = result {
                log::debug!("Optimization: option {key}: {error}");
            }
        }

        let stale: Vec<String> = originals.keys().filter(|key| !wants.contains_key(*key)).cloned().collect();
        for key in stale {
            let original = originals.remove(&key).unwrap_or_default();
            state.rejected.remove(&key);
            dirty = true;
            let Some(value) = OptVal::parse(&original) else {
                continue;
            };
            let result = env.with_local_frame(16, |env| -> anyhow::Result<()> {
                let option = instance(env, &options, &key)?;
                let current = raw::call(env, &option, "get", "()Ljava/lang/Object;", &[])?.l()?;
                let object = make(env, &value, &current)?;
                raw::call(env, &option, "set", "(Ljava/lang/Object;)V", &[JValue::Object(&object)])?;
                Ok(())
            });
            if let Err(error) = result {
                log::warn!("Optimization: could not restore {key}: {error}");
            }
        }
        if dirty {
            save_originals(originals);
        }
        Ok(())
    })
}

/// Puts every forced option back by writing its stored value directly —
/// no change callbacks, so it is safe off the render thread. Used when the
/// client unloads; a re-inject applies the modules' values again.
pub fn restore_all() {
    let mut state = STATE.lock().unwrap_or_else(|p| p.into_inner());
    let originals = state.originals.take().unwrap_or_else(load_originals);
    state.applied.clear();
    state.rejected.clear();
    if originals.is_empty() {
        return;
    }
    let result = mapping().in_frame(|| {
        let mut env = mapping().get_env()?;
        let options = raw::field(&mut env, minecraft().jni_ref().as_obj(), "options", "Lnet/minecraft/client/Options;")?.l()?;
        for (key, original) in &originals {
            let Some(value) = OptVal::parse(original) else {
                continue;
            };
            let result = env.with_local_frame(16, |env| -> anyhow::Result<()> {
                let option = instance(env, &options, key)?;
                let current = raw::field(env, &option, "value", "Ljava/lang/Object;")?.l()?;
                let object = make(env, &value, &current)?;
                raw::set_field(env, &option, "value", "Ljava/lang/Object;", JValue::Object(&object))
            });
            if let Err(error) = result {
                log::warn!("Optimization: could not restore {key}: {error}");
            }
        }
        Ok(())
    });
    match result {
        Ok(()) => save_originals(&BTreeMap::new()),
        Err(error) => log::warn!("Optimization: restoring options failed: {error}"),
    }
}

/// The `OptionInstance` behind `key`.
fn instance<'l>(env: &mut JNIEnv<'l>, options: &JObject, key: &str) -> anyhow::Result<JObject<'l>> {
    if let Some(source) = key.strip_prefix("volume:") {
        let class = mapping().resolve_class(env, "net/minecraft/sounds/SoundSource")?;
        let source = env
            .get_static_field(class, source, "Lnet/minecraft/sounds/SoundSource;")
            .map_err(|error| {
                let _ = env.exception_clear();
                anyhow::anyhow!("SoundSource.{source}: {error}")
            })?
            .l()?;
        return Ok(raw::call(
            env,
            options,
            "getSoundSourceOptionInstance",
            &format!("(Lnet/minecraft/sounds/SoundSource;){OPTION_INSTANCE}"),
            &[JValue::Object(&source)],
        )?
        .l()?);
    }
    Ok(raw::field(env, options, key, OPTION_INSTANCE)?.l()?)
}

/// Reads a boxed option value; `None` for a type this does not handle.
fn read(env: &mut JNIEnv, value: &JObject) -> anyhow::Result<Option<OptVal>> {
    if value.is_null() {
        return Ok(None);
    }
    if env.is_instance_of(value, "java/lang/Integer")? {
        return Ok(Some(OptVal::Int(raw::call(env, value, "intValue", "()I", &[])?.i()?)));
    }
    if env.is_instance_of(value, "java/lang/Double")? {
        return Ok(Some(OptVal::Double(raw::call(env, value, "doubleValue", "()D", &[])?.d()?)));
    }
    if env.is_instance_of(value, "java/lang/Boolean")? {
        return Ok(Some(OptVal::Bool(raw::call(env, value, "booleanValue", "()Z", &[])?.z()?)));
    }
    if env.is_instance_of(value, "java/lang/Enum")? {
        let name = raw::call(env, value, "name", "()Ljava/lang/String;", &[])?.l()?;
        return Ok(Some(OptVal::Enum(mapping().get_string(name)?)));
    }
    Ok(None)
}

/// Boxes `value`; an enum constant is looked up in the enum of `template`
/// (the option's current value).
fn make<'l>(env: &mut JNIEnv<'l>, value: &OptVal, template: &JObject) -> anyhow::Result<JObject<'l>> {
    use super::java::jstatic;
    Ok(match value {
        OptVal::Int(v) => jstatic(env, "java/lang/Integer", "valueOf", "(I)Ljava/lang/Integer;", &[JValue::Int(*v)])?,
        OptVal::Double(v) => jstatic(env, "java/lang/Double", "valueOf", "(D)Ljava/lang/Double;", &[JValue::Double(*v)])?,
        OptVal::Bool(v) => jstatic(env, "java/lang/Boolean", "valueOf", "(Z)Ljava/lang/Boolean;", &[JValue::Bool(*v as u8)])?,
        OptVal::Enum(name) => {
            let class = raw::call(env, template, "getDeclaringClass", "()Ljava/lang/Class;", &[])?.l()?;
            let name: JObject = env.new_string(name)?.into();
            jstatic(
                env,
                "java/lang/Enum",
                "valueOf",
                "(Ljava/lang/Class;Ljava/lang/String;)Ljava/lang/Enum;",
                &[JValue::Object(&class), JValue::Object(&name)],
            )?
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_round_trip_through_text() {
        for value in [
            OptVal::Int(12),
            OptVal::Double(0.25),
            OptVal::Bool(false),
            OptVal::Enum("FAST".into()),
        ] {
            assert_eq!(OptVal::parse(&value.repr()), Some(value));
        }
        assert_eq!(OptVal::parse("x:1"), None);
        assert_eq!(OptVal::parse("e:"), None);
        assert_eq!(OptVal::parse("i:abc"), None);
    }

    #[test]
    fn doubles_compare_loosely() {
        assert!(OptVal::Double(0.1 + 0.2).same(&OptVal::Double(0.3)));
        assert!(!OptVal::Double(0.3).same(&OptVal::Double(0.31)));
        assert!(!OptVal::Int(1).same(&OptVal::Double(1.0)));
    }

    #[test]
    fn the_last_want_of_a_key_wins() {
        let wants = last_wins(vec![
            ("framerateLimit".into(), OptVal::Int(260)),
            ("bobView".into(), OptVal::Bool(false)),
            ("framerateLimit".into(), OptVal::Int(10)),
        ]);
        assert_eq!(wants.len(), 2);
        assert_eq!(wants["framerateLimit"], OptVal::Int(10));
    }
}
