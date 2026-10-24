//! AntiBot — keeps combat modules off the fake players anti-cheats spawn to
//! catch auras. Such bots are entities with a player model that are not on
//! the server's tab list, and are often invisible.
//!
//! [`is_bot`] is consulted by KillAura, Aimbot and TriggerBot while the module
//! is on.

use crate::mapping::entity::Entity;
use crate::mapping::raw;
use crate::mapping::MappedObject;
use crate::module::{KeyboardKey, Module, ModuleCategory, ModuleData, ModuleId, ModuleSetting};
use crate::state::{client, mapping, minecraft};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long the tab list snapshot is reused.
const TAB_LIST_TTL: Duration = Duration::from_millis(1000);

#[derive(Debug)]
pub struct AntiBotModule {
    pub module: ModuleData,
}

impl AntiBotModule {
    pub fn new() -> Self {
        Self {
            module: ModuleData {
                id: ModuleId::AntiBot,
                description: "Combat modules ignore anti-cheat bots".to_string(),
                category: ModuleCategory::Combat,
                key_bind: KeyboardKey::KeyNone,
                enabled: false,
                settings: vec![
                    ModuleSetting::Toggle {
                        name: "Not in tab list".to_string(),
                        value: true,
                    },
                    ModuleSetting::Toggle {
                        name: "Invisible".to_string(),
                        value: false,
                    },
                ],
            },
        }
    }
}

impl Module for AntiBotModule {
    fn on_start(&self) -> anyhow::Result<()> {
        Ok(())
    }

    fn on_stop(&self) -> anyhow::Result<()> {
        Ok(())
    }

    fn on_tick(&self) -> anyhow::Result<()> {
        Ok(())
    }

    fn get_module_data(&self) -> &ModuleData {
        &self.module
    }

    fn get_module_data_mut(&mut self) -> &mut ModuleData {
        &mut self.module
    }
}

/// The checks switched on, or `None` while the module is off.
fn checks() -> Option<(bool, bool)> {
    let handle = client().modules.get(ModuleId::AntiBot)?;
    let module = handle.lock().ok()?;
    let data = module.get_module_data();
    if !data.enabled {
        return None;
    }
    let on = |name: &str, fallback: bool| {
        data.get_setting(name)
            .and_then(ModuleSetting::get_toggle_value)
            .unwrap_or(fallback)
    };
    Some((on("Not in tab list", true), on("Invisible", false)))
}

struct TabList {
    at: Option<Instant>,
    /// Lower-cased names; a few dozen at most, so a `Vec` is fine.
    names: Vec<String>,
}

static TAB_LIST: Mutex<TabList> = Mutex::new(TabList {
    at: None,
    names: Vec::new(),
});

fn on_tab_list(name: &str) -> bool {
    let mut tab = TAB_LIST.lock().unwrap_or_else(|p| p.into_inner());
    if tab.at.is_none_or(|at| at.elapsed() > TAB_LIST_TTL) {
        tab.names = minecraft()
            .online_player_names()
            .unwrap_or_default()
            .into_iter()
            .map(|name| name.to_lowercase())
            .collect();
        tab.at = Some(Instant::now());
    }
    // An empty list means it could not be read — then nobody counts as a bot.
    tab.names.is_empty() || tab.names.contains(&name.to_lowercase())
}

/// Whether player `entity` named `name` looks like a bot. Always `false`
/// while the module is off.
pub fn is_bot(entity: &Entity, name: &str) -> bool {
    let Some((tab_check, invisible_check)) = checks() else {
        return false;
    };
    if tab_check && !on_tab_list(name) {
        return true;
    }
    invisible_check
        && mapping()
            .in_frame(|| {
                let mut env = mapping().get_env()?;
                Ok(raw::call(&mut env, entity.jni_ref().as_obj(), "isInvisible", "()Z", &[])?.z()?)
            })
            .unwrap_or(false)
}
