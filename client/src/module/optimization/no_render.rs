//! NoRender — fog, screen overlays and HUD parts, each switchable.
//!
//! Works through NeoForge's render events, with listeners made of method
//! handles (see `java`), so the decision runs inside the game at no JNI cost:
//!
//! * fog — `ViewportEvent.RenderFog`: removed or set to your own start / end,
//!   per medium (air, water, lava, powder snow) and for the sky;
//! * fire / water / in-wall overlays — `RenderBlockScreenEffectEvent`;
//! * HUD layers — `RenderGuiLayerEvent.Pre` (boss bars, scoreboard, …);
//! * your hand — `RenderHandEvent`.
//!
//! Without NeoForge nothing happens (logged once per change).

use super::java::{
    self, cancel, cancel_when, contains_test, event_action, guard, key_chain, proxy, sequence, EventListeners,
};
use super::{choice, optimization_module, slider, toggle, Snapshot};
use jni::objects::{JObject, JValue};
use jni::JNIEnv;
use std::sync::Mutex;

/// HUD switches → NeoForge's vanilla GUI layer ids.
pub const HUD_LAYERS: &[(&str, &[&str])] = &[
    ("Hide pumpkin, vignette & spyglass", &["minecraft:camera_overlays"]),
    ("Hide boss bars", &["minecraft:boss_overlay"]),
    ("Hide scoreboard", &["minecraft:scoreboard_sidebar"]),
    ("Hide titles", &["minecraft:title", "minecraft:subtitle_overlay"]),
    ("Hide action bar", &["minecraft:overlay_message"]),
    ("Hide effect icons", &["minecraft:effects"]),
    ("Hide held item name", &["minecraft:selected_item_name"]),
    ("Hide sleep fade", &["minecraft:sleep_overlay"]),
    ("Hide crosshair", &["minecraft:crosshair"]),
];

/// Screen overlay switches → `RenderBlockScreenEffectEvent.OverlayType` names.
const OVERLAYS: &[(&str, &str)] = &[
    ("No fire overlay", "FIRE"),
    ("No water overlay", "WATER"),
    ("No in-wall overlay", "BLOCK"),
];

/// Fog medium switches → `FogType` names.
const FOG_MEDIA: &[(&str, &str)] = &[
    ("Fog: in air", "NONE"),
    ("Fog: in water", "WATER"),
    ("Fog: in lava", "LAVA"),
    ("Fog: in powder snow", "POWDER_SNOW"),
];

fn settings() -> Vec<crate::module::ModuleSetting> {
    let mut settings = vec![
        choice("Fog", 1, &["Keep", "Off", "Custom"]),
        slider("Fog start (Custom)", 64.0, 0.0, 512.0),
        slider("Fog end (Custom)", 256.0, 0.0, 1024.0),
        toggle("Fog: in air", true),
        toggle("Fog: in water", false),
        toggle("Fog: in lava", false),
        toggle("Fog: in powder snow", false),
        toggle("Fog: sky too", true),
    ];
    for (name, _) in OVERLAYS {
        settings.push(toggle(name, true));
    }
    for (name, _) in HUD_LAYERS {
        settings.push(toggle(name, false));
    }
    settings.push(toggle("Hide hand", false));
    settings
}

optimization_module!(NoRenderModule, NoRender, "Remove fog, screen overlays and HUD parts", settings());

/// What the fog listener should do; `None` = leave fog alone.
#[derive(Debug, Clone, PartialEq)]
pub struct FogPlan {
    pub start: f32,
    pub end: f32,
    pub media: Vec<&'static str>,
    pub modes: Vec<&'static str>,
}

/// Pure: fog settings → plan.
pub fn fog_plan(mode: usize, start: f32, end: f32, media: Vec<&'static str>, sky: bool) -> Option<FogPlan> {
    let (start, end) = match mode {
        1 => (1.0e6, 1.0e6 + 1.0),
        2 => {
            let start = start.max(0.0).round();
            (start, end.round().max(start + 1.0))
        }
        _ => return None,
    };
    if media.is_empty() {
        return None;
    }
    let mut modes = vec!["FOG_TERRAIN"];
    if sky {
        modes.push("FOG_SKY");
    }
    Some(FogPlan { start, end, media, modes })
}

static LISTENERS: Mutex<Option<EventListeners>> = Mutex::new(None);

pub fn frame(snapshot: Option<Snapshot>) {
    let mut guard_listeners = LISTENERS.lock().unwrap_or_else(|p| p.into_inner());
    let Some(s) = snapshot else {
        if let Some(listeners) = guard_listeners.as_mut() {
            listeners.clear();
        }
        return;
    };
    let listeners = guard_listeners.get_or_insert_with(EventListeners::default);

    let media: Vec<&'static str> = FOG_MEDIA.iter().filter(|(name, _)| s.toggle(name)).map(|(_, m)| *m).collect();
    let fog = fog_plan(
        s.choice("Fog"),
        s.slider("Fog start (Custom)"),
        s.slider("Fog end (Custom)"),
        media,
        s.toggle("Fog: sky too"),
    );
    let signature = fog.as_ref().map(|f| format!("{f:?}")).unwrap_or_default();
    listeners.ensure("fog", "net/neoforged/neoforge/client/event/ViewportEvent$RenderFog", signature, |env, class| {
        let plan = fog.clone().ok_or_else(|| anyhow::anyhow!("no fog plan"))?;
        fog_listener(env, class, &plan)
    });

    let overlays: Vec<String> = OVERLAYS.iter().filter(|(name, _)| s.toggle(name)).map(|(_, o)| o.to_string()).collect();
    listeners.ensure(
        "overlay",
        "net/neoforged/neoforge/client/event/RenderBlockScreenEffectEvent",
        overlays.join(","),
        |env, class| {
            let enum_class = java::jclass(env, "java/lang/Enum")?;
            cancel_when(env, class, &[(class, "getOverlayType"), (&enum_class, "name")], &overlays)
        },
    );

    let layers: Vec<String> = HUD_LAYERS
        .iter()
        .filter(|(name, _)| s.toggle(name))
        .flat_map(|(_, ids)| ids.iter().map(|id| id.to_string()))
        .collect();
    listeners.ensure(
        "hud",
        "net/neoforged/neoforge/client/event/RenderGuiLayerEvent$Pre",
        layers.join(","),
        |env, class| {
            let object = java::jclass(env, "java/lang/Object")?;
            cancel_when(env, class, &[(class, "getName"), (&object, "toString")], &layers)
        },
    );

    let hand = if s.toggle("Hide hand") { "on" } else { "" };
    listeners.ensure("hand", "net/neoforged/neoforge/client/event/RenderHandEvent", hand.to_string(), |env, class| {
        let action = cancel(env, class)?;
        proxy(env, "java/util/function/Consumer", &action)
    });
}

pub fn cleanup() {
    if let Some(listeners) = LISTENERS.lock().unwrap_or_else(|p| p.into_inner()).as_mut() {
        listeners.clear();
    }
}

fn fog_listener<'l>(env: &mut JNIEnv<'l>, class: &JObject, plan: &FogPlan) -> anyhow::Result<JObject<'l>> {
    let float = env.get_static_field("java/lang/Float", "TYPE", "Ljava/lang/Class;")?.l()?;
    let mut boxed = |value: f32| java::jstatic(env, "java/lang/Float", "valueOf", "(F)Ljava/lang/Float;", &[JValue::Float(value)]);
    let start = boxed(plan.start)?;
    let end = boxed(plan.end)?;
    let near = event_action(env, class, "setNearPlaneDistance", Some((&float, &start)))?;
    let far = event_action(env, class, "setFarPlaneDistance", Some((&float, &end)))?;
    let stop = cancel(env, class)?;
    let both = sequence(env, &near, &far)?;
    let action = sequence(env, &both, &stop)?;

    let enum_class = java::jclass(env, "java/lang/Enum")?;
    let media: Vec<String> = plan.media.iter().map(|m| m.to_string()).collect();
    let media_set = java::new_set(env)?;
    java::fill_set(env, &media_set, &media)?;
    let media_key = key_chain(env, &[(class, "getType"), (&enum_class, "name")])?;
    let media_test = contains_test(env, &media_set, &media_key)?;

    let modes: Vec<String> = plan.modes.iter().map(|m| m.to_string()).collect();
    let mode_set = java::new_set(env)?;
    java::fill_set(env, &mode_set, &modes)?;
    let mode_key = key_chain(env, &[(class, "getMode"), (&enum_class, "name")])?;
    let mode_test = contains_test(env, &mode_set, &mode_key)?;

    let inner = guard(env, &mode_test, &action)?;
    let handle = guard(env, &media_test, &inner)?;
    proxy(env, "java/util/function/Consumer", &handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keep_or_no_medium_means_no_fog_listener() {
        assert_eq!(fog_plan(0, 10.0, 20.0, vec!["NONE"], true), None);
        assert_eq!(fog_plan(1, 10.0, 20.0, vec![], true), None);
    }

    #[test]
    fn custom_fog_keeps_end_past_start() {
        let plan = fog_plan(2, 100.0, 40.0, vec!["NONE", "WATER"], false).unwrap();
        assert_eq!((plan.start, plan.end), (100.0, 101.0));
        assert_eq!(plan.modes, vec!["FOG_TERRAIN"]);
        let plan = fog_plan(1, 0.0, 0.0, vec!["NONE"], true).unwrap();
        assert!(plan.start > 10_000.0 && plan.end > plan.start);
        assert_eq!(plan.modes, vec!["FOG_TERRAIN", "FOG_SKY"]);
    }
}
