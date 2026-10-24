//! Clean View — screen effects off, each on its own switch.
//!
//! Most entries force the game's accessibility options (hurt tilt, bobbing,
//! distortion, darkness pulse, FOV effects, glint), restored when off. The
//! rest act every frame on your client only: the totem pop animation is cut
//! short, blindness / darkness / nausea are removed from your player (the
//! server still has them; when they expire nothing changes), and the rain
//! level is held at zero (restored when off).

use super::options::{want, OptVal};
use super::{optimization_module, slider, toggle, Snapshot};
use crate::mapping::raw;
use crate::mapping::MappedObject;
use crate::state::{mapping, minecraft};
use jni::objects::{JObject, JValue};
use jni::JNIEnv;
use std::sync::Mutex;

optimization_module!(
    CleanViewModule,
    CleanView,
    "Turn off screen effects one by one",
    vec![
        toggle("No hurt camera", true),
        toggle("No view bobbing", false),
        toggle("No nausea / portal wobble", true),
        toggle("No darkness pulsing", true),
        toggle("No FOV changes", false),
        slider("Glint strength % (100 = keep)", 100.0, 0.0, 100.0),
        slider("Glint speed % (100 = keep)", 100.0, 0.0, 100.0),
        toggle("No totem animation", true),
        toggle("Remove blindness", true),
        toggle("Remove darkness", true),
        toggle("Remove nausea", false),
        toggle("Hide rain & snow", false),
    ]
);

/// Rain / thunder levels from before "Hide rain & snow" took over.
static SAVED_WEATHER: Mutex<Option<(f32, f32)>> = Mutex::new(None);

pub fn frame(snapshot: Option<Snapshot>) {
    let hide_weather = snapshot.as_ref().is_some_and(|s| s.toggle("Hide rain & snow"));
    if !hide_weather {
        if let Some((rain, thunder)) = SAVED_WEATHER.lock().unwrap_or_else(|p| p.into_inner()).take() {
            let _ = with_level(|env, level| set_weather(env, level, rain, thunder));
        }
    }
    let Some(s) = snapshot else {
        return;
    };

    for (setting, key) in [
        ("No hurt camera", "damageTiltStrength"),
        ("No nausea / portal wobble", "screenEffectScale"),
        ("No darkness pulsing", "darknessEffectScale"),
        ("No FOV changes", "fovEffectScale"),
    ] {
        if s.toggle(setting) {
            want(key, OptVal::Double(0.0));
        }
    }
    if s.toggle("No view bobbing") {
        want("bobView", OptVal::Bool(false));
    }
    for (setting, key) in [
        ("Glint strength % (100 = keep)", "glintStrength"),
        ("Glint speed % (100 = keep)", "glintSpeed"),
    ] {
        let percent = s.slider(setting).round();
        if percent < 100.0 {
            want(key, OptVal::Double((percent.max(0.0) / 100.0) as f64));
        }
    }

    let effects: Vec<&str> = [
        ("Remove blindness", "BLINDNESS"),
        ("Remove darkness", "DARKNESS"),
        ("Remove nausea", "CONFUSION"),
    ]
    .into_iter()
    .filter(|(setting, _)| s.toggle(setting))
    .map(|(_, field)| field)
    .collect();
    let totem = s.toggle("No totem animation");

    let result = mapping().in_frame(|| {
        let mut env = mapping().get_env()?;
        let mc = minecraft().jni_ref().as_obj();
        if totem {
            let renderer = raw::field(&mut env, mc, "gameRenderer", "Lnet/minecraft/client/renderer/GameRenderer;")?.l()?;
            if raw::field(&mut env, &renderer, "itemActivationTicks", "I")?.i()? > 0 {
                raw::set_field(&mut env, &renderer, "itemActivationTicks", "I", JValue::Int(0))?;
            }
        }
        if !effects.is_empty() {
            let player = raw::field(&mut env, mc, "player", "Lnet/minecraft/client/player/LocalPlayer;")?.l()?;
            if !player.is_null() {
                for effect in &effects {
                    remove_effect(&mut env, &player, effect)?;
                }
            }
        }
        Ok(())
    });
    if let Err(error) = result {
        log::debug!("Clean View: {error}");
    }

    if hide_weather {
        let _ = with_level(|env, level| {
            let mut saved = SAVED_WEATHER.lock().unwrap_or_else(|p| p.into_inner());
            if saved.is_none() {
                let rain = raw::call(env, level, "getRainLevel", "(F)F", &[JValue::Float(1.0)])?.f()?;
                let thunder = raw::call(env, level, "getThunderLevel", "(F)F", &[JValue::Float(1.0)])?.f()?;
                *saved = Some((rain, thunder));
            }
            set_weather(env, level, 0.0, 0.0)
        });
    }
}

/// Runs `f` with the client world, if there is one.
fn with_level(f: impl FnOnce(&mut JNIEnv, &JObject) -> anyhow::Result<()>) -> anyhow::Result<()> {
    mapping().in_frame(|| {
        let mut env = mapping().get_env()?;
        let level = raw::field(
            &mut env,
            minecraft().jni_ref().as_obj(),
            "level",
            "Lnet/minecraft/client/multiplayer/ClientLevel;",
        )?
        .l()?;
        if level.is_null() {
            return Ok(());
        }
        f(&mut env, &level)
    })
}

fn set_weather(env: &mut JNIEnv, level: &JObject, rain: f32, thunder: f32) -> anyhow::Result<()> {
    raw::call(env, level, "setRainLevel", "(F)V", &[JValue::Float(rain)])?;
    raw::call(env, level, "setThunderLevel", "(F)V", &[JValue::Float(thunder)])?;
    Ok(())
}

/// Removes the effect `MobEffects.<field>` from the player, client-side. A
/// `Holder` since 1.20.5, the `MobEffect` itself before.
fn remove_effect(env: &mut JNIEnv, player: &JObject, field: &str) -> anyhow::Result<()> {
    let class = mapping().resolve_class(env, "net/minecraft/world/effect/MobEffects")?;
    for (field_sig, method_sig) in [
        (
            "Lnet/minecraft/core/Holder;",
            "(Lnet/minecraft/core/Holder;)Lnet/minecraft/world/effect/MobEffectInstance;",
        ),
        (
            "Lnet/minecraft/world/effect/MobEffect;",
            "(Lnet/minecraft/world/effect/MobEffect;)Lnet/minecraft/world/effect/MobEffectInstance;",
        ),
    ] {
        let Ok(effect) = env.get_static_field(&class, field, field_sig) else {
            let _ = env.exception_clear();
            continue;
        };
        let effect = effect.l()?;
        raw::call(env, player, "removeEffectNoUpdate", method_sig, &[JValue::Object(&effect)])?;
        return Ok(());
    }
    Err(anyhow::anyhow!("MobEffects.{field} not found"))
}
