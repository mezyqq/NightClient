//! Friends — one list of players every combat module leaves alone (KillAura,
//! Aimbot, TriggerBot) and the ESP draws in their own color. Saved in
//! `friends.json` beside the client config, matched case-insensitively.
//!
//! Players are picked in the Friends tool window, or — with "Middle click" on
//! — by middle-clicking the player under the crosshair.
//!
//! The list applies whether or not the module is switched on; the module only
//! carries the settings and shows the window.

use crate::config;
use crate::graphic::notification::{Notification, NotificationType};
use crate::mapping::raw;
use crate::mapping::MappedObject;
use crate::module::{KeyboardKey, Module, ModuleCategory, ModuleData, ModuleId, ModuleSetting};
use crate::state::{client, mapping, minecraft};
use std::collections::BTreeSet;
use std::sync::Mutex;

const FILE: &str = "friends.json";

#[derive(Debug)]
pub struct FriendsModule {
    pub module: ModuleData,
}

impl FriendsModule {
    pub fn new() -> Self {
        Self {
            module: ModuleData {
                id: ModuleId::Friends,
                description: "Players that combat modules never attack".to_string(),
                category: ModuleCategory::Misc,
                key_bind: KeyboardKey::KeyNone,
                enabled: false,
                settings: vec![
                    ModuleSetting::Toggle {
                        name: "Middle click".to_string(),
                        value: true,
                    },
                    ModuleSetting::Color {
                        name: "ESP color".to_string(),
                        value: [0.3, 1.0, 0.45, 1.0],
                    },
                ],
            },
        }
    }
}

impl Module for FriendsModule {
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

/// Friend names, as typed; `None` until read from disk.
static FRIENDS: Mutex<Option<BTreeSet<String>>> = Mutex::new(None);

fn with_friends<T>(f: impl FnOnce(&mut BTreeSet<String>) -> T) -> T {
    let mut guard = FRIENDS.lock().unwrap_or_else(|p| p.into_inner());
    f(guard.get_or_insert_with(|| {
        std::fs::read_to_string(config::base_dir().join(FILE))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }))
}

/// Every friend.
pub fn list() -> BTreeSet<String> {
    with_friends(|friends| friends.clone())
}

/// Whether `name` is a friend.
pub fn is_friend(name: &str) -> bool {
    with_friends(|friends| friends.iter().any(|friend| friend.eq_ignore_ascii_case(name)))
}

/// Adds `name`, or removes it if already a friend. Returns whether it is a
/// friend afterwards.
pub fn toggle(name: &str) -> bool {
    with_friends(|friends| {
        let existing = friends.iter().find(|f| f.eq_ignore_ascii_case(name)).cloned();
        let now_friend = match existing {
            Some(existing) => {
                friends.remove(&existing);
                false
            }
            None => {
                friends.insert(name.to_string());
                true
            }
        };
        save(friends);
        now_friend
    })
}

/// Removes every friend.
pub fn clear() {
    with_friends(|friends| {
        friends.clear();
        save(friends);
    });
}

fn save(friends: &BTreeSet<String>) {
    let json = serde_json::to_string_pretty(friends).unwrap_or_else(|_| "[]".into());
    let path = config::base_dir().join(FILE);
    if let Err(error) = std::fs::write(&path, json) {
        log::warn!("Friends: could not write {}: {error}", path.display());
    }
}

/// The "ESP color" setting, when the module is on.
pub fn esp_color() -> Option<[f32; 4]> {
    let handle = client().modules.get(ModuleId::Friends)?;
    let module = handle.lock().ok()?;
    let data = module.get_module_data();
    if !data.enabled {
        return None;
    }
    match data.get_setting("ESP color") {
        Some(ModuleSetting::Color { value, .. }) => Some(*value),
        _ => None,
    }
}

/// Middle mouse button pressed in the world: toggles the player under the
/// crosshair, if the module is on with "Middle click".
pub fn middle_click() {
    let wanted = client()
        .modules
        .get(ModuleId::Friends)
        .and_then(|handle| {
            let module = handle.lock().ok()?;
            let data = module.get_module_data();
            Some(
                data.enabled
                    && data
                        .get_setting("Middle click")
                        .and_then(ModuleSetting::get_toggle_value)
                        .unwrap_or(true),
            )
        })
        .unwrap_or(false);
    if !wanted || !minecraft().current_screen_is_null() {
        return;
    }
    let name = mapping().in_frame(|| {
        let mut env = mapping().get_env()?;
        let entity = raw::field(
            &mut env,
            minecraft().jni_ref().as_obj(),
            "crosshairPickEntity",
            "Lnet/minecraft/world/entity/Entity;",
        )?
        .l()?;
        if entity.is_null() {
            return Ok(None);
        }
        let player_class = mapping().resolve_class(&mut env, "net/minecraft/world/entity/player/Player")?;
        if !env.is_instance_of(&entity, &player_class)? {
            return Ok(None);
        }
        let component = raw::call(&mut env, &entity, "getName", "()Lnet/minecraft/network/chat/Component;", &[])?.l()?;
        let text = raw::call(&mut env, &component, "getString", "()Ljava/lang/String;", &[])?.l()?;
        Ok(Some(mapping().get_string(text)?))
    });
    if let Ok(Some(name)) = name {
        let message = if toggle(&name) {
            format!("{name} added")
        } else {
            format!("{name} removed")
        };
        Notification::send(NotificationType::Info, "Friends", &message);
    }
}
