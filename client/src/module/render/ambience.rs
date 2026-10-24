//! Ambience — your own time of day and weather, seen only by you.
//!
//! The client's world copy is overwritten every frame (from the frame hook),
//! so the server's periodic time updates and weather changes never show.
//! Switching off hands time back to the server (it resyncs within a second)
//! and restores the rain / thunder levels seen before.

use crate::mapping::raw;
use crate::mapping::MappedObject;
use crate::module::{KeyboardKey, Module, ModuleCategory, ModuleData, ModuleId, ModuleSetting};
use crate::state::{mapping, minecraft};
use jni::objects::JValue;
use std::sync::Mutex;

/// What the frame hook applies while running.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Applied {
    /// Day time in ticks (0 = sunrise, 6000 = noon, 18000 = midnight).
    time: Option<i64>,
    /// `(rain, thunder)` levels, 0..1.
    weather: Option<(f32, f32)>,
}

/// `Some` while the module runs; the frame hook reads it every frame.
static APPLIED: Mutex<Option<Applied>> = Mutex::new(None);
/// Rain / thunder levels from before the module started.
static PREVIOUS_WEATHER: Mutex<Option<(f32, f32)>> = Mutex::new(None);

const WEATHERS: [&str; 4] = ["Unchanged", "Clear", "Rain", "Thunder"];

#[derive(Debug)]
pub struct AmbienceModule {
    pub module: ModuleData,
}

impl AmbienceModule {
    pub fn new() -> Self {
        Self {
            module: ModuleData {
                id: ModuleId::Ambience,
                description: "Your own time of day and weather, client-side".to_string(),
                category: ModuleCategory::Render,
                key_bind: KeyboardKey::KeyNone,
                enabled: false,
                settings: vec![
                    ModuleSetting::Toggle {
                        name: "Change time".to_string(),
                        value: true,
                    },
                    ModuleSetting::Slider {
                        name: "Time".to_string(),
                        value: 6000.0,
                        min: 0.0,
                        max: 23999.0,
                    },
                    ModuleSetting::Choice {
                        name: "Weather".to_string(),
                        value: 1,
                        options: WEATHERS.iter().map(|w| w.to_string()).collect(),
                    },
                ],
            },
        }
    }

    fn applied(&self) -> Applied {
        let data = &self.module;
        let change_time = data
            .get_setting("Change time")
            .and_then(ModuleSetting::get_toggle_value)
            .unwrap_or(true);
        let time = data
            .get_setting("Time")
            .and_then(ModuleSetting::get_slider_value)
            .unwrap_or(6000.0);
        let weather = match data.get_setting("Weather") {
            Some(ModuleSetting::Choice { value, .. }) => *value,
            _ => 0,
        };
        Applied {
            time: change_time.then_some(time as i64),
            weather: match weather {
                1 => Some((0.0, 0.0)),
                2 => Some((1.0, 0.0)),
                3 => Some((1.0, 1.0)),
                _ => None,
            },
        }
    }
}

impl Module for AmbienceModule {
    fn on_start(&self) -> anyhow::Result<()> {
        if let Some(world) = minecraft().world()? {
            let levels = mapping().in_frame(|| {
                let mut env = mapping().get_env()?;
                let level = world.jni_ref().as_obj();
                let rain = raw::call(&mut env, level, "getRainLevel", "(F)F", &[JValue::Float(1.0)])?.f()?;
                let thunder = raw::call(&mut env, level, "getThunderLevel", "(F)F", &[JValue::Float(1.0)])?.f()?;
                Ok((rain, thunder))
            });
            *PREVIOUS_WEATHER.lock().unwrap() = levels.ok();
        }
        *APPLIED.lock().unwrap() = Some(self.applied());
        Ok(())
    }

    fn on_stop(&self) -> anyhow::Result<()> {
        *APPLIED.lock().unwrap() = None;
        if let (Some((rain, thunder)), Some(world)) =
            (PREVIOUS_WEATHER.lock().unwrap().take(), minecraft().world()?)
        {
            set_weather(world.jni_ref().as_obj(), rain, thunder)?;
        }
        Ok(())
    }

    fn on_tick(&self) -> anyhow::Result<()> {
        // Pick up setting changes.
        *APPLIED.lock().unwrap() = Some(self.applied());
        Ok(())
    }

    fn get_module_data(&self) -> &ModuleData {
        &self.module
    }

    fn get_module_data_mut(&mut self) -> &mut ModuleData {
        &mut self.module
    }
}

fn set_weather(level: &jni::objects::JObject, rain: f32, thunder: f32) -> anyhow::Result<()> {
    mapping().in_frame(|| {
        let mut env = mapping().get_env()?;
        raw::call(&mut env, level, "setRainLevel", "(F)V", &[JValue::Float(rain)])?;
        raw::call(&mut env, level, "setThunderLevel", "(F)V", &[JValue::Float(thunder)])?;
        Ok(())
    })
}

/// Applies the chosen time and weather. Called every frame from the frame
/// hook, so nothing the server sends shows for even a frame.
pub fn frame() {
    let Some(applied) = *APPLIED.lock().unwrap_or_else(|p| p.into_inner()) else {
        return;
    };
    let Ok(Some(world)) = minecraft().world() else {
        return;
    };
    let level = world.jni_ref().as_obj();
    let result = mapping().in_frame(|| {
        let mut env = mapping().get_env()?;
        if let Some(time) = applied.time {
            raw::call(&mut env, level, "setDayTime", "(J)V", &[JValue::Long(time)])?;
        }
        Ok(())
    });
    if let Err(error) = result {
        log::debug!("Ambience: {error}");
    }
    if let Some((rain, thunder)) = applied.weather {
        let _ = set_weather(level, rain, thunder);
    }
}
