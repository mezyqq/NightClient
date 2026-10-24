//! Optimization — frame rate, and stripping what you do not want to see.
//!
//! These modules are driven from the frame hook ([`frame`]), not the tick:
//! a switched-on module acts everywhere — main menu, loading, the death
//! screen — and undoes what it changed as soon as it is switched off. Their
//! `on_start` / `on_stop` / `on_tick` do nothing.
//!
//! * [`fps_boost`] — video settings as a profile (render distance, graphics,
//!   clouds, particles, shadows, …), restored when off.
//! * [`clean_view`] — screen effects: hurt tilt, bobbing, nausea, darkness,
//!   FOV changes, glint, totem animation, blindness, rain.
//! * [`particles`] — hide particles by group or by type; a cap.
//! * [`no_render`] — fog, fire / water / in-wall overlays, HUD parts, hand
//!   (NeoForge render events).
//! * [`entity_culler`] — hide entity types (NeoForge), remove dropped items,
//!   XP orbs, arrows, lightning, … client-side.
//! * [`background`] — frame cap and volume while the game is unfocused.
//! * [`ram_cleaner`] — runs the garbage collector past a heap threshold.
//!
//! Shared pieces: [`options`] (forced game options and their undo), [`java`]
//! (method-handle filters and NeoForge listeners), [`lists`] (the
//! user-chosen particle and entity lists).

pub mod background;
pub mod clean_view;
pub mod entity_culler;
pub mod fps_boost;
pub mod java;
pub mod lists;
pub mod no_render;
pub mod options;
pub mod particles;
pub mod ram_cleaner;

use crate::module::{ModuleId, ModuleSetting};
use crate::state::client;

/// A module's switch and settings, copied for one frame.
pub struct Snapshot {
    pub enabled: bool,
    settings: Vec<ModuleSetting>,
}

impl Snapshot {
    pub fn of(id: ModuleId) -> Option<Snapshot> {
        let handle = client().modules.get(id)?;
        let module = handle.lock().ok()?;
        let data = module.get_module_data();
        Some(Snapshot {
            enabled: data.enabled,
            settings: data.settings.clone(),
        })
    }

    fn find(&self, name: &str) -> Option<&ModuleSetting> {
        self.settings.iter().find(|setting| setting.name() == name)
    }

    pub fn toggle(&self, name: &str) -> bool {
        self.find(name).and_then(ModuleSetting::get_toggle_value).unwrap_or(false)
    }

    pub fn slider(&self, name: &str) -> f32 {
        self.find(name).and_then(ModuleSetting::get_slider_value).unwrap_or(0.0)
    }

    pub fn choice(&self, name: &str) -> usize {
        match self.find(name) {
            Some(ModuleSetting::Choice { value, .. }) => *value,
            _ => 0,
        }
    }

    /// An enabled snapshot with just these toggles.
    #[cfg(test)]
    pub fn for_test(toggles: &[(&str, bool)]) -> Snapshot {
        Snapshot {
            enabled: true,
            settings: toggles.iter().map(|(name, value)| toggle(name, *value)).collect(),
        }
    }
}

/// Settings constructors, to keep the module tables short.
pub(crate) fn toggle(name: &str, value: bool) -> ModuleSetting {
    ModuleSetting::Toggle { name: name.to_string(), value }
}

pub(crate) fn slider(name: &str, value: f32, min: f32, max: f32) -> ModuleSetting {
    ModuleSetting::Slider { name: name.to_string(), value, min, max }
}

pub(crate) fn choice(name: &str, value: usize, options: &[&str]) -> ModuleSetting {
    ModuleSetting::Choice {
        name: name.to_string(),
        value,
        options: options.iter().map(|o| o.to_string()).collect(),
    }
}

/// Declares a frame-driven module: data only, its work is done in [`frame`].
macro_rules! optimization_module {
    ($name:ident, $id:ident, $description:literal, $settings:expr) => {
        #[derive(Debug)]
        pub struct $name {
            pub module: crate::module::ModuleData,
        }

        impl $name {
            pub fn new() -> Self {
                Self {
                    module: crate::module::ModuleData {
                        id: crate::module::ModuleId::$id,
                        description: $description.to_string(),
                        category: crate::module::ModuleCategory::Optimization,
                        key_bind: crate::module::KeyboardKey::KeyNone,
                        enabled: false,
                        settings: $settings,
                    },
                }
            }
        }

        impl crate::module::Module for $name {
            fn on_start(&self) -> anyhow::Result<()> {
                Ok(())
            }

            fn on_stop(&self) -> anyhow::Result<()> {
                Ok(())
            }

            fn on_tick(&self) -> anyhow::Result<()> {
                Ok(())
            }

            fn get_module_data(&self) -> &crate::module::ModuleData {
                &self.module
            }

            fn get_module_data_mut(&mut self) -> &mut crate::module::ModuleData {
                &mut self.module
            }
        }
    };
}
pub(crate) use optimization_module;

/// Runs every optimization module. Called each frame from the frame hook.
pub fn frame() {
    let snapshot = |id| Snapshot::of(id).filter(|s| s.enabled);
    fps_boost::frame(snapshot(ModuleId::FpsBoost));
    clean_view::frame(snapshot(ModuleId::CleanView));
    particles::frame(snapshot(ModuleId::Particles));
    no_render::frame(snapshot(ModuleId::NoRender));
    entity_culler::frame(snapshot(ModuleId::EntityCuller));
    // After FPS Boost, so its frame cap wins while the window is unfocused.
    background::frame(snapshot(ModuleId::BackgroundFps));
    ram_cleaner::frame(snapshot(ModuleId::RamCleaner));
    options::commit();
}

/// Undoes everything before the client unloads. Runs on the unloading
/// thread, so it only touches state that is safe to change from there.
pub fn cleanup() {
    particles::cleanup();
    no_render::cleanup();
    entity_culler::cleanup();
    options::restore_all();
}
