//! Small Java building blocks made from `java.lang.invoke` method handles, so
//! that per-object work (testing thousands of particles or entities, reacting
//! to a render event) runs inside the JVM instead of costing a JNI round trip
//! per object.
//!
//! Everything is built in an *erased* shape — handles take and return
//! `Object` — so handles from unrelated classes (an event, an interface's
//! default method, an entity) compose without exact type matching; `asType`
//! inserts the casts.
//!
//! * [`key_chain`] — `x -> x.a().b()…` (each step a no-argument method).
//! * [`contains_test`] — `x -> set.contains(key(x))`, the test behind every
//!   filter here; the set is a live Java `HashSet` that can be refilled
//!   without rebuilding the test.
//! * [`event_action`] / [`guard`] / [`sequence`] — `event -> { if (test) … }`.
//! * [`proxy`] — wraps a handle as a `Predicate` / `Consumer`.
//! * [`EventListeners`] — NeoForge event-bus listeners built from the above.

use crate::mapping::raw;
use crate::state::mapping;
use jni::objects::{GlobalRef, JObject, JValue};
use jni::JNIEnv;
use std::collections::HashMap;

const MH: &str = "Ljava/lang/invoke/MethodHandle;";

/// Calls a static method of a `java.*` class.
pub fn jstatic<'l>(
    env: &mut JNIEnv<'l>,
    class: &str,
    name: &str,
    sig: &str,
    args: &[JValue],
) -> anyhow::Result<JObject<'l>> {
    let result = env.call_static_method(class, name, sig, args).map_err(|error| {
        let _ = env.exception_clear();
        anyhow::anyhow!("{class}.{name}{sig}: {error}")
    })?;
    Ok(result.l()?)
}

/// A `java.*` class as a `Class` object.
pub fn jclass<'l>(env: &mut JNIEnv<'l>, name: &str) -> anyhow::Result<JObject<'l>> {
    let class = env.find_class(name).map_err(|error| {
        let _ = env.exception_clear();
        anyhow::anyhow!("class {name}: {error}")
    })?;
    Ok(class.into())
}

/// A primitive's `Class` (`boolean.class`, `void.class`, …) — the `TYPE`
/// field of its wrapper.
fn primitive<'l>(env: &mut JNIEnv<'l>, wrapper: &str) -> anyhow::Result<JObject<'l>> {
    let value = env.get_static_field(wrapper, "TYPE", "Ljava/lang/Class;").map_err(|error| {
        let _ = env.exception_clear();
        anyhow::anyhow!("{wrapper}.TYPE: {error}")
    })?;
    Ok(value.l()?)
}

fn class_array<'l>(env: &mut JNIEnv<'l>, classes: &[&JObject]) -> anyhow::Result<JObject<'l>> {
    let array = env.new_object_array(classes.len() as i32, "java/lang/Class", JObject::null())?;
    for (index, class) in classes.iter().enumerate() {
        env.set_object_array_element(&array, index as i32, *class)?;
    }
    Ok(array.into())
}

/// `class.getMethod(name, params…)` as a method handle.
pub fn method<'l>(
    env: &mut JNIEnv<'l>,
    class: &JObject,
    name: &str,
    params: &[&JObject],
) -> anyhow::Result<JObject<'l>> {
    let name_object: JObject = env.new_string(name)?.into();
    let params = class_array(env, params)?;
    let reflected = raw::call(
        env,
        class,
        "getMethod",
        "(Ljava/lang/String;[Ljava/lang/Class;)Ljava/lang/reflect/Method;",
        &[JValue::Object(&name_object), JValue::Object(&params)],
    )?
    .l()?;
    let lookup = jstatic(
        env,
        "java/lang/invoke/MethodHandles",
        "publicLookup",
        "()Ljava/lang/invoke/MethodHandles$Lookup;",
        &[],
    )?;
    Ok(raw::call(
        env,
        &lookup,
        "unreflect",
        &format!("(Ljava/lang/reflect/Method;){MH}"),
        &[JValue::Object(&reflected)],
    )?
    .l()?)
}

/// `handle.asType(methodType(ret, params…))`.
fn as_type<'l>(
    env: &mut JNIEnv<'l>,
    handle: &JObject,
    ret: &JObject,
    params: &[&JObject],
) -> anyhow::Result<JObject<'l>> {
    let params = class_array(env, params)?;
    let method_type = jstatic(
        env,
        "java/lang/invoke/MethodType",
        "methodType",
        "(Ljava/lang/Class;[Ljava/lang/Class;)Ljava/lang/invoke/MethodType;",
        &[JValue::Object(ret), JValue::Object(&params)],
    )?;
    Ok(raw::call(
        env,
        handle,
        "asType",
        &format!("(Ljava/lang/invoke/MethodType;){MH}"),
        &[JValue::Object(&method_type)],
    )?
    .l()?)
}

/// Erases a one-argument handle to `(Object)Object`.
fn erase_function<'l>(env: &mut JNIEnv<'l>, handle: &JObject) -> anyhow::Result<JObject<'l>> {
    let object = jclass(env, "java/lang/Object")?;
    as_type(env, handle, &object, &[&object])
}

/// Erases a one-argument handle to `(Object)void`.
fn erase_action<'l>(env: &mut JNIEnv<'l>, handle: &JObject) -> anyhow::Result<JObject<'l>> {
    let object = jclass(env, "java/lang/Object")?;
    let void = primitive(env, "java/lang/Void")?;
    as_type(env, handle, &void, &[&object])
}

/// `x -> x.first().second()…`, erased to `(Object)Object`. Each step names
/// a public no-argument method on the given class.
pub fn key_chain<'l>(env: &mut JNIEnv<'l>, steps: &[(&JObject, &str)]) -> anyhow::Result<JObject<'l>> {
    let mut chain: Option<JObject<'l>> = None;
    for (class, name) in steps {
        let step = method(env, class, name, &[])?;
        let step = erase_function(env, &step)?;
        chain = Some(match chain {
            None => step,
            Some(previous) => jstatic(
                env,
                "java/lang/invoke/MethodHandles",
                "filterReturnValue",
                &format!("({MH}{MH}){MH}"),
                &[JValue::Object(&previous), JValue::Object(&step)],
            )?,
        });
    }
    chain.ok_or_else(|| anyhow::anyhow!("empty key chain"))
}

/// `x -> set.contains(key(x))` as an `(Object)boolean` handle.
pub fn contains_test<'l>(env: &mut JNIEnv<'l>, set: &JObject, key: &JObject) -> anyhow::Result<JObject<'l>> {
    let set_class = jclass(env, "java/util/Set")?;
    let object = jclass(env, "java/lang/Object")?;
    let contains = method(env, &set_class, "contains", &[&object])?;
    let bound = raw::call(
        env,
        &contains,
        "bindTo",
        &format!("(Ljava/lang/Object;){MH}"),
        &[JValue::Object(set)],
    )?
    .l()?;
    let filters = env.new_object_array(1, "java/lang/invoke/MethodHandle", key)?;
    let test = jstatic(
        env,
        "java/lang/invoke/MethodHandles",
        "filterArguments",
        &format!("({MH}I[Ljava/lang/invoke/MethodHandle;){MH}"),
        &[JValue::Object(&bound), JValue::Int(0), JValue::Object(&filters)],
    )?;
    let boolean = primitive(env, "java/lang/Boolean")?;
    as_type(env, &test, &boolean, &[&object])
}

/// `event -> event.name(argument)` erased to `(Object)void`; the argument is
/// boxed and unboxed to the parameter's primitive type.
pub fn event_action<'l>(
    env: &mut JNIEnv<'l>,
    event_class: &JObject,
    name: &str,
    argument: Option<(&JObject, &JObject)>,
) -> anyhow::Result<JObject<'l>> {
    let handle = match argument {
        None => method(env, event_class, name, &[])?,
        Some((param_class, value)) => {
            let handle = method(env, event_class, name, &[param_class])?;
            let values = env.new_object_array(1, "java/lang/Object", value)?;
            jstatic(
                env,
                "java/lang/invoke/MethodHandles",
                "insertArguments",
                &format!("({MH}I[Ljava/lang/Object;){MH}"),
                &[JValue::Object(&handle), JValue::Int(1), JValue::Object(&values)],
            )?
        }
    };
    erase_action(env, &handle)
}

/// `event -> event.setCanceled(true)`.
pub fn cancel<'l>(env: &mut JNIEnv<'l>, event_class: &JObject) -> anyhow::Result<JObject<'l>> {
    let boolean = primitive(env, "java/lang/Boolean")?;
    let yes = jstatic(env, "java/lang/Boolean", "valueOf", "(Z)Ljava/lang/Boolean;", &[JValue::Bool(1)])?;
    event_action(env, event_class, "setCanceled", Some((&boolean, &yes)))
}

/// Runs `first`, then `second`, on the same argument.
pub fn sequence<'l>(env: &mut JNIEnv<'l>, first: &JObject, second: &JObject) -> anyhow::Result<JObject<'l>> {
    jstatic(
        env,
        "java/lang/invoke/MethodHandles",
        "foldArguments",
        &format!("({MH}{MH}){MH}"),
        &[JValue::Object(second), JValue::Object(first)],
    )
}

/// `x -> { if (test(x)) action(x); }`.
pub fn guard<'l>(env: &mut JNIEnv<'l>, test: &JObject, action: &JObject) -> anyhow::Result<JObject<'l>> {
    let action_type = raw::call(env, action, "type", "()Ljava/lang/invoke/MethodType;", &[])?.l()?;
    let nothing = jstatic(
        env,
        "java/lang/invoke/MethodHandles",
        "empty",
        &format!("(Ljava/lang/invoke/MethodType;){MH}"),
        &[JValue::Object(&action_type)],
    )?;
    jstatic(
        env,
        "java/lang/invoke/MethodHandles",
        "guardWithTest",
        &format!("({MH}{MH}{MH}){MH}"),
        &[JValue::Object(test), JValue::Object(action), JValue::Object(&nothing)],
    )
}

/// A `Consumer` cancelling an event when `key(event)` — a chain of getters
/// ending in a `String` — is one of `values`.
pub fn cancel_when<'l>(
    env: &mut JNIEnv<'l>,
    event_class: &JObject,
    key: &[(&JObject, &str)],
    values: &[String],
) -> anyhow::Result<JObject<'l>> {
    let set = new_set(env)?;
    fill_set(env, &set, values)?;
    let key = key_chain(env, key)?;
    let test = contains_test(env, &set, &key)?;
    let action = cancel(env, event_class)?;
    let handle = guard(env, &test, &action)?;
    proxy(env, "java/util/function/Consumer", &handle)
}

/// Wraps `handle` as an instance of the `java.*` functional interface
/// `interface` (`java/util/function/Predicate`, …).
pub fn proxy<'l>(env: &mut JNIEnv<'l>, interface: &str, handle: &JObject) -> anyhow::Result<JObject<'l>> {
    let interface = jclass(env, interface)?;
    jstatic(
        env,
        "java/lang/invoke/MethodHandleProxies",
        "asInterfaceInstance",
        &format!("(Ljava/lang/Class;{MH})Ljava/lang/Object;"),
        &[JValue::Object(&interface), JValue::Object(handle)],
    )
}

/// A new, empty `java.util.HashSet`.
pub fn new_set<'l>(env: &mut JNIEnv<'l>) -> anyhow::Result<JObject<'l>> {
    Ok(env.new_object("java/util/HashSet", "()V", &[])?)
}

/// Replaces the contents of a Java `Set<String>` with `values`.
pub fn fill_set(env: &mut JNIEnv, set: &JObject, values: &[String]) -> anyhow::Result<()> {
    raw::call(env, set, "clear", "()V", &[])?;
    for value in values {
        env.with_local_frame(4, |env| -> anyhow::Result<()> {
            let text: JObject = env.new_string(value)?.into();
            raw::call(env, set, "add", "(Ljava/lang/Object;)Z", &[JValue::Object(&text)])?;
            Ok(())
        })?;
    }
    Ok(())
}

/// A game class (through the game class loader) as a `Class` object.
pub fn game_class<'l>(env: &mut JNIEnv<'l>, name: &str) -> anyhow::Result<JObject<'l>> {
    Ok(mapping().resolve_class(env, name)?.into())
}

// --- NeoForge event listeners ---------------------------------------------------------

const NEOFORGE: &str = "net/neoforged/neoforge/common/NeoForge";
const BUS: &str = "Lnet/neoforged/bus/api/IEventBus;";

struct Listener {
    /// What the listener was built from; a different one rebuilds it.
    signature: String,
    /// The registered `Consumer`, `None` when building it failed.
    consumer: Option<GlobalRef>,
}

/// Listeners on NeoForge's game event bus, one per named slot, rebuilt only
/// when what they should do changes. Without NeoForge every build fails once
/// per change and the slot stays empty.
#[derive(Default)]
pub struct EventListeners {
    slots: HashMap<&'static str, Listener>,
}

impl EventListeners {
    /// Makes slot `slot` run the consumer `build` makes, unless the slot
    /// already holds one built from `signature`. An empty signature means
    /// "no listener".
    pub fn ensure(
        &mut self,
        slot: &'static str,
        event_class: &str,
        signature: String,
        build: impl for<'l> FnOnce(&mut JNIEnv<'l>, &JObject<'l>) -> anyhow::Result<JObject<'l>>,
    ) {
        if self.slots.get(slot).is_some_and(|listener| listener.signature == signature) {
            return;
        }
        if let Some(old) = self.slots.remove(slot) {
            if let Some(consumer) = old.consumer {
                unregister(&consumer);
            }
        }
        if signature.is_empty() {
            return;
        }
        let consumer = mapping()
            .in_frame(|| {
                let mut env = mapping().get_env()?;
                let class = game_class(&mut env, event_class)?;
                let consumer = build(&mut env, &class)?;
                register(&mut env, &class, &consumer)?;
                Ok(env.new_global_ref(consumer)?)
            })
            .map_err(|error| log::warn!("Optimization: listener {slot} ({event_class}): {error}"))
            .ok();
        self.slots.insert(slot, Listener { signature, consumer });
    }

    /// Whether slot `slot` holds a registered listener.
    pub fn is_registered(&self, slot: &str) -> bool {
        self.slots.get(slot).is_some_and(|listener| listener.consumer.is_some())
    }

    /// Unregisters every listener.
    pub fn clear(&mut self) {
        for (_, listener) in self.slots.drain() {
            if let Some(consumer) = listener.consumer {
                unregister(&consumer);
            }
        }
    }
}

fn bus<'l>(env: &mut JNIEnv<'l>) -> anyhow::Result<JObject<'l>> {
    let class = mapping().resolve_class(env, NEOFORGE)?;
    let bus = env.get_static_field(class, "EVENT_BUS", BUS).map_err(|error| {
        let _ = env.exception_clear();
        anyhow::anyhow!("NeoForge.EVENT_BUS: {error}")
    })?;
    Ok(bus.l()?)
}

/// `EVENT_BUS.addListener(LOWEST, true, eventClass, consumer)` — lowest
/// priority so ours runs last and has the final say.
fn register(env: &mut JNIEnv, event_class: &JObject, consumer: &JObject) -> anyhow::Result<()> {
    let bus = bus(env)?;
    let priority_class = mapping().resolve_class(env, "net/neoforged/bus/api/EventPriority")?;
    let lowest = env
        .get_static_field(priority_class, "LOWEST", "Lnet/neoforged/bus/api/EventPriority;")
        .map_err(|error| {
            let _ = env.exception_clear();
            anyhow::anyhow!("EventPriority.LOWEST: {error}")
        })?
        .l()?;
    let full = raw::call(
        env,
        &bus,
        "addListener",
        "(Lnet/neoforged/bus/api/EventPriority;ZLjava/lang/Class;Ljava/util/function/Consumer;)V",
        &[JValue::Object(&lowest), JValue::Bool(1), JValue::Object(event_class), JValue::Object(consumer)],
    );
    if full.is_ok() {
        return Ok(());
    }
    raw::call(
        env,
        &bus,
        "addListener",
        "(Ljava/lang/Class;Ljava/util/function/Consumer;)V",
        &[JValue::Object(event_class), JValue::Object(consumer)],
    )?;
    Ok(())
}

fn unregister(consumer: &GlobalRef) {
    let result = mapping().in_frame(|| {
        let mut env = mapping().get_env()?;
        let bus = bus(&mut env)?;
        raw::call(&mut env, &bus, "unregister", "(Ljava/lang/Object;)V", &[JValue::Object(consumer.as_obj())])?;
        Ok(())
    });
    if let Err(error) = result {
        log::warn!("Optimization: could not unregister a listener: {error}");
    }
}

// --- live values ------------------------------------------------------------------------

/// `x -> key(x) == target` (by `Objects.equals`) as an `(Object)boolean`
/// handle.
pub fn is_test<'l>(env: &mut JNIEnv<'l>, key: &JObject, target: &JObject) -> anyhow::Result<JObject<'l>> {
    let objects = jclass(env, "java/util/Objects")?;
    let object = jclass(env, "java/lang/Object")?;
    let equals = method(env, &objects, "equals", &[&object, &object])?;
    let bound = raw::call(env, &equals, "bindTo", &format!("(Ljava/lang/Object;){MH}"), &[JValue::Object(target)])?.l()?;
    let filters = env.new_object_array(1, "java/lang/invoke/MethodHandle", key)?;
    let test = jstatic(
        env,
        "java/lang/invoke/MethodHandles",
        "filterArguments",
        &format!("({MH}I[Ljava/lang/invoke/MethodHandle;){MH}"),
        &[JValue::Object(&bound), JValue::Int(0), JValue::Object(&filters)],
    )?;
    let boolean = primitive(env, "java/lang/Boolean")?;
    as_type(env, &test, &boolean, &[&object])
}

/// `x -> key(x).field = holder[0]` erased to `(Object)void` — `field` is a
/// public `float` field of `owner`, and `holder` a `float[1]` that Rust
/// rewrites whenever the value should change, without rebuilding anything.
pub fn set_float_from<'l>(
    env: &mut JNIEnv<'l>,
    owner: &JObject,
    field: &str,
    holder: &JObject,
    key: &JObject,
) -> anyhow::Result<JObject<'l>> {
    let float = primitive(env, "java/lang/Float")?;
    let lookup = jstatic(
        env,
        "java/lang/invoke/MethodHandles",
        "publicLookup",
        "()Ljava/lang/invoke/MethodHandles$Lookup;",
        &[],
    )?;
    let name: JObject = env.new_string(field)?.into();
    let setter = raw::call(
        env,
        &lookup,
        "findSetter",
        &format!("(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;){MH}"),
        &[JValue::Object(owner), JValue::Object(&name), JValue::Object(&float)],
    )?
    .l()?;
    // () -> holder[0]
    let array_class = raw::call(env, holder, "getClass", "()Ljava/lang/Class;", &[])?.l()?;
    let getter = jstatic(
        env,
        "java/lang/invoke/MethodHandles",
        "arrayElementGetter",
        &format!("(Ljava/lang/Class;){MH}"),
        &[JValue::Object(&array_class)],
    )?;
    let zero = env.new_object("java/lang/Integer", "(I)V", &[JValue::Int(0)])?;
    let values = env.new_object_array(2, "java/lang/Object", JObject::null())?;
    env.set_object_array_element(&values, 0, holder)?;
    env.set_object_array_element(&values, 1, &zero)?;
    let value = jstatic(
        env,
        "java/lang/invoke/MethodHandles",
        "insertArguments",
        &format!("({MH}I[Ljava/lang/Object;){MH}"),
        &[JValue::Object(&getter), JValue::Int(0), JValue::Object(&values)],
    )?;
    // (owner) -> owner.field = holder[0]
    let store = jstatic(
        env,
        "java/lang/invoke/MethodHandles",
        "collectArguments",
        &format!("({MH}I{MH}){MH}"),
        &[JValue::Object(&setter), JValue::Int(1), JValue::Object(&value)],
    )?;
    let store = erase_action(env, &store)?;
    let filters = env.new_object_array(1, "java/lang/invoke/MethodHandle", key)?;
    jstatic(
        env,
        "java/lang/invoke/MethodHandles",
        "filterArguments",
        &format!("({MH}I[Ljava/lang/invoke/MethodHandle;){MH}"),
        &[JValue::Object(&store), JValue::Int(0), JValue::Object(&filters)],
    )
}

/// `x -> key(x).name(argument)` erased to `(Object)void` — a one-parameter
/// method of `class`, with a fixed argument (its result is dropped).
pub fn call_with<'l>(
    env: &mut JNIEnv<'l>,
    class: &JObject,
    name: &str,
    param_class: &JObject,
    argument: &JObject,
    key: &JObject,
) -> anyhow::Result<JObject<'l>> {
    let handle = method(env, class, name, &[param_class])?;
    let values = env.new_object_array(1, "java/lang/Object", argument)?;
    let bound = jstatic(
        env,
        "java/lang/invoke/MethodHandles",
        "insertArguments",
        &format!("({MH}I[Ljava/lang/Object;){MH}"),
        &[JValue::Object(&handle), JValue::Int(1), JValue::Object(&values)],
    )?;
    let action = erase_action(env, &bound)?;
    filter_first(env, &action, key)
}

/// `x -> holder[index] = key(x)` erased to `(Object)void`, for a `double[]`
/// holder and a key returning a (boxed) double.
pub fn store_double<'l>(env: &mut JNIEnv<'l>, holder: &JObject, index: i32, key: &JObject) -> anyhow::Result<JObject<'l>> {
    let array_class = raw::call(env, holder, "getClass", "()Ljava/lang/Class;", &[])?.l()?;
    let setter = jstatic(
        env,
        "java/lang/invoke/MethodHandles",
        "arrayElementSetter",
        &format!("(Ljava/lang/Class;){MH}"),
        &[JValue::Object(&array_class)],
    )?;
    let index = env.new_object("java/lang/Integer", "(I)V", &[JValue::Int(index)])?;
    let values = env.new_object_array(2, "java/lang/Object", JObject::null())?;
    env.set_object_array_element(&values, 0, holder)?;
    env.set_object_array_element(&values, 1, &index)?;
    let bound = jstatic(
        env,
        "java/lang/invoke/MethodHandles",
        "insertArguments",
        &format!("({MH}I[Ljava/lang/Object;){MH}"),
        &[JValue::Object(&setter), JValue::Int(0), JValue::Object(&values)],
    )?;
    let action = erase_action(env, &bound)?;
    filter_first(env, &action, key)
}

/// `handle` with its first argument computed by `key`.
fn filter_first<'l>(env: &mut JNIEnv<'l>, handle: &JObject, key: &JObject) -> anyhow::Result<JObject<'l>> {
    let filters = env.new_object_array(1, "java/lang/invoke/MethodHandle", key)?;
    jstatic(
        env,
        "java/lang/invoke/MethodHandles",
        "filterArguments",
        &format!("({MH}I[Ljava/lang/invoke/MethodHandle;){MH}"),
        &[JValue::Object(handle), JValue::Int(0), JValue::Object(&filters)],
    )
}
