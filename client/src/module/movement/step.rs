//! Step — walk straight up blocks without jumping.
//!
//! Raises the base value of the player's `step_height` attribute (vanilla
//! 0.6) and puts the original back when turned off. The player object is
//! replaced on respawn and on every world change, so the value is checked
//! and re-applied each tick.
//!
//! The server re-runs the move with its own step height of 0.6 but lets the
//! vertical result differ by up to 0.5 block, so up to ~1.1 passes on a
//! vanilla server; higher is pulled back ("moved wrongly").

use crate::mapping::raw;
use crate::mapping::MappedObject;
use crate::module::{KeyboardKey, Module, ModuleCategory, ModuleData, ModuleId, ModuleSetting};
use crate::state::{mapping, minecraft};
use jni::objects::{JObject, JValue};
use jni::JNIEnv;
use std::sync::Mutex;

/// Vanilla step height — what is restored if the original was never read.
const VANILLA_STEP: f64 = 0.6;

/// The step height before Step changed it.
static ORIGINAL: Mutex<Option<f64>> = Mutex::new(None);

#[derive(Debug)]
pub struct StepModule {
    pub module: ModuleData,
}

impl StepModule {
    pub fn new() -> Self {
        Self {
            module: ModuleData {
                id: ModuleId::Step,
                description: "Walk up blocks without jumping".to_string(),
                category: ModuleCategory::Movement,
                key_bind: KeyboardKey::KeyNone,
                enabled: false,
                settings: vec![ModuleSetting::Slider {
                    name: "Height (blocks)".to_string(),
                    value: 1.0,
                    min: 0.6,
                    max: 2.5,
                }],
            },
        }
    }
}

/// The local player's `step_height` attribute instance.
fn step_attribute<'l>(env: &mut JNIEnv<'l>, player: &JObject) -> anyhow::Result<JObject<'l>> {
    let attributes = mapping().resolve_class(env, "net/minecraft/world/entity/ai/attributes/Attributes")?;
    let holder = env
        .get_static_field(attributes, "STEP_HEIGHT", "Lnet/minecraft/core/Holder;")
        .map_err(|error| {
            let _ = env.exception_clear();
            anyhow::anyhow!("Attributes.STEP_HEIGHT: {error}")
        })?
        .l()?;
    let instance = raw::call(
        env,
        player,
        "getAttribute",
        "(Lnet/minecraft/core/Holder;)Lnet/minecraft/world/entity/ai/attributes/AttributeInstance;",
        &[JValue::Object(&holder)],
    )?
    .l()?;
    if instance.is_null() {
        anyhow::bail!("the player has no step_height attribute");
    }
    Ok(instance)
}

/// Sets the step height's base value; returns the value it had before.
fn set_step(value: Option<f64>) -> anyhow::Result<Option<f64>> {
    let Some(player) = minecraft().player()? else {
        return Ok(None);
    };
    mapping().in_frame(|| {
        let mut env = mapping().get_env()?;
        let attribute = step_attribute(&mut env, player.jni_ref().as_obj())?;
        let before = raw::call(&mut env, &attribute, "getBaseValue", "()D", &[])?.d()?;
        if let Some(value) = value {
            if (before - value).abs() > 1e-6 {
                raw::call(&mut env, &attribute, "setBaseValue", "(D)V", &[JValue::Double(value)])?;
            }
        }
        Ok(Some(before))
    })
}

impl Module for StepModule {
    fn on_start(&self) -> anyhow::Result<()> {
        Ok(())
    }

    fn on_stop(&self) -> anyhow::Result<()> {
        let original = ORIGINAL.lock().unwrap_or_else(|p| p.into_inner()).take();
        set_step(Some(original.unwrap_or(VANILLA_STEP)))?;
        Ok(())
    }

    fn on_tick(&self) -> anyhow::Result<()> {
        let height = self
            .module
            .get_setting("Height (blocks)")
            .and_then(ModuleSetting::get_slider_value)
            .unwrap_or(1.0) as f64;
        let before = set_step(Some(height))?;
        let mut original = ORIGINAL.lock().unwrap_or_else(|p| p.into_inner());
        if original.is_none() {
            // The first value seen is the player's own — unless it already is
            // ours (a re-inject while Step was on).
            *original = before.filter(|b| (b - height).abs() > 1e-6).or(Some(VANILLA_STEP));
        }
        Ok(())
    }

    fn get_module_data(&self) -> &ModuleData {
        &self.module
    }

    fn get_module_data_mut(&mut self) -> &mut ModuleData {
        &mut self.module
    }
}
