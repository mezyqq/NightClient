//! Persistent module configuration.
//!
//! The keybind, setting values and enabled state of every module — plus the
//! GUI layout (panel positions, which modules are expanded) — are written to
//! `dark_client_config.json` so they survive a re-injection. [`load`] runs once
//! after the modules are registered; [`save`] is called whenever the user
//! changes something in the GUI.
//!
//! The GUI layout — panel and tool-window positions, collapsed windows,
//! chosen tabs — is also written on its own by [`autosave_layout`], about a
//! second after it last changed, so it survives however the client ends
//! (Panic, re-inject with the menu open, a crash). That write touches only the
//! layout keys of the file, never the modules' state.

use crate::module::{KeyboardKey, ModuleCategory, ModuleId, ModuleSetting};
use crate::state::client;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// Category-panel positions, keyed by category — updated as the user drags a
/// panel, and persisted so the layout survives a re-injection.
static PANEL_POS: LazyLock<DashMap<ModuleCategory, [f32; 2]>> = LazyLock::new(DashMap::new);

/// Which modules have their settings panel expanded in the GUI, keyed by id.
static EXPANDED: LazyLock<DashMap<ModuleId, bool>> = LazyLock::new(DashMap::new);

/// The saved position of a category panel, if the user has moved it.
pub fn panel_pos(category: ModuleCategory) -> Option<[f32; 2]> {
    PANEL_POS.get(&category).map(|entry| *entry)
}

/// Records a category panel's position.
pub fn set_panel_pos(category: ModuleCategory, pos: [f32; 2]) {
    if PANEL_POS.insert(category, pos) != Some(pos) {
        layout_changed();
    }
}

/// Free-form GUI state (chosen tabs, modes, …), keyed by a dotted name such
/// as `baritone.tab`.
static UI_VALUES: LazyLock<DashMap<String, String>> = LazyLock::new(DashMap::new);

/// A remembered GUI value.
pub fn ui_value(key: &str) -> Option<String> {
    UI_VALUES.get(key).map(|entry| entry.clone())
}

/// Remembers a GUI value.
pub fn set_ui_value(key: &str, value: &str) {
    if UI_VALUES.insert(key.to_string(), value.to_string()).as_deref() != Some(value) {
        layout_changed();
    }
}

/// When the layout last changed and has not been written since.
static LAYOUT_DIRTY_SINCE: Mutex<Option<Instant>> = Mutex::new(None);
/// Quiet time after the last change before the layout is written — so a drag
/// is saved once, when it ends.
const LAYOUT_SAVE_DELAY: Duration = Duration::from_millis(800);

fn layout_changed() {
    if let Ok(mut since) = LAYOUT_DIRTY_SINCE.lock() {
        *since = Some(Instant::now());
    }
}

/// Writes the layout once it has stopped changing. Called every frame from
/// the overlay renderer; cheap when nothing changed.
pub fn autosave_layout() {
    let due = {
        let Ok(mut since) = LAYOUT_DIRTY_SINCE.lock() else {
            return;
        };
        match *since {
            Some(at) if at.elapsed() >= LAYOUT_SAVE_DELAY => {
                *since = None;
                true
            }
            _ => false,
        }
    };
    if due {
        save_layout();
    }
}

/// Rewrites only the layout keys of the config file (`panels`, `windows`,
/// `ui`), keeping everything else in it as it is.
pub fn save_layout() {
    let path = config_path();
    let mut root: serde_json::Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| serde_json::json!({ "modules": [] }));
    let layout = (|| -> serde_json::Result<()> {
        root["panels"] = serde_json::to_value(panel_configs())?;
        root["windows"] = serde_json::to_value(window_configs())?;
        root["ui"] = serde_json::to_value(ui_values())?;
        // Which modules' settings are expanded lives on the module entries.
        if let Some(modules) = root["modules"].as_array_mut() {
            for entry in modules {
                let expanded = EXPANDED
                    .iter()
                    .find(|e| serde_json::to_value(e.key()).ok().as_ref() == Some(&entry["id"]))
                    .map(|e| *e.value());
                if let Some(expanded) = expanded {
                    entry["expanded"] = serde_json::Value::Bool(expanded);
                }
            }
        }
        Ok(())
    })();
    if let Err(error) = layout {
        log::warn!("config: could not serialize the layout: {error}");
        return;
    }
    match serde_json::to_string_pretty(&root) {
        Ok(json) => {
            if let Err(error) = std::fs::write(&path, json) {
                log::warn!("config: could not write {}: {error}", path.display());
            }
        }
        Err(error) => log::warn!("config: could not serialize: {error}"),
    }
}

fn panel_configs() -> Vec<PanelConfig> {
    PANEL_POS
        .iter()
        .map(|entry| PanelConfig {
            category: *entry.key(),
            x: entry.value()[0],
            y: entry.value()[1],
        })
        .collect()
}

fn window_configs() -> Vec<WindowConfig> {
    WINDOW_POS
        .iter()
        .map(|entry| WindowConfig {
            title: entry.key().clone(),
            x: entry.value()[0],
            y: entry.value()[1],
            open: WINDOW_OPEN.get(entry.key()).map(|open| *open).unwrap_or(true),
        })
        .collect()
}

fn ui_values() -> BTreeMap<String, String> {
    UI_VALUES
        .iter()
        .map(|entry| (entry.key().clone(), entry.value().clone()))
        .collect()
}

/// Whether a module's settings panel is expanded.
pub fn is_expanded(id: ModuleId) -> bool {
    EXPANDED.get(&id).map(|entry| *entry).unwrap_or(false)
}

/// Records a module's expanded state.
pub fn set_expanded(id: ModuleId, expanded: bool) {
    if EXPANDED.insert(id, expanded) != Some(expanded) {
        layout_changed();
    }
}

/// Tool-window positions (top-left corner), keyed by window title — kept so
/// a window comes back where it was left, across a Panic and a re-inject.
static WINDOW_POS: LazyLock<DashMap<String, [f32; 2]>> = LazyLock::new(DashMap::new);

/// The saved position of tool window `title`, if it was ever moved / shown.
pub fn window_pos(title: &str) -> Option<[f32; 2]> {
    WINDOW_POS.get(title).map(|entry| *entry)
}

/// Records tool window `title`'s position.
pub fn set_window_pos(title: &str, pos: [f32; 2]) {
    if WINDOW_POS.insert(title.to_string(), pos) != Some(pos) {
        layout_changed();
    }
}

/// Whether each tool window is expanded (`false` = collapsed to its title).
static WINDOW_OPEN: LazyLock<DashMap<String, bool>> = LazyLock::new(DashMap::new);

/// Whether tool window `title` was left expanded.
pub fn window_open(title: &str) -> Option<bool> {
    WINDOW_OPEN.get(title).map(|open| *open)
}

/// Records whether tool window `title` is expanded.
pub fn set_window_open(title: &str, open: bool) {
    if WINDOW_OPEN.insert(title.to_string(), open) != Some(open) {
        layout_changed();
    }
}

/// Clears the persisted GUI layout — panel and window positions, expanded
/// modules. Backs the GUI's "Reset UI" button.
pub fn reset_ui_state() {
    PANEL_POS.clear();
    WINDOW_POS.clear();
    WINDOW_OPEN.clear();
    UI_VALUES.clear();
    layout_changed();
    EXPANDED.clear();
}

/// Config file name.
const CONFIG_FILE: &str = "dark_client_config.json";

/// The directory DarkClient keeps its files in — the config and the log. The
/// injector passes its own working directory through the `DARK_CONFIG_DIR` env
/// var (set by the agent loader); keeping files there, rather than inside
/// `.minecraft`, leaves no trace in the game directory. Falls back to the
/// process working directory if the variable is absent.
pub fn base_dir() -> PathBuf {
    match std::env::var_os("DARK_CONFIG_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from("."),
    }
}

/// Absolute path of the config file.
fn config_path() -> PathBuf {
    base_dir().join(CONFIG_FILE)
}

#[derive(Serialize, Deserialize, Default)]
struct Config {
    modules: Vec<ModuleConfig>,
    /// `#[serde(default)]` keeps configs written before panels were saved
    /// loadable.
    #[serde(default)]
    panels: Vec<PanelConfig>,
    /// Tool-window positions; `#[serde(default)]` for older configs.
    #[serde(default)]
    windows: Vec<WindowConfig>,
    /// Free-form GUI state — see [`ui_value`].
    #[serde(default)]
    ui: BTreeMap<String, String>,
}

#[derive(Serialize, Deserialize)]
struct WindowConfig {
    title: String,
    x: f32,
    y: f32,
    /// Expanded (`true`) or collapsed to its title bar.
    #[serde(default = "expanded")]
    open: bool,
}

fn expanded() -> bool {
    true
}

#[derive(Serialize, Deserialize)]
struct ModuleConfig {
    id: ModuleId,
    key_bind: i32,
    enabled: bool,
    /// Whether the module's settings panel is expanded. `#[serde(default)]`
    /// keeps older configs loadable.
    #[serde(default)]
    expanded: bool,
    settings: Vec<SavedSetting>,
}

#[derive(Serialize, Deserialize)]
struct PanelConfig {
    category: ModuleCategory,
    x: f32,
    y: f32,
}

#[derive(Serialize, Deserialize)]
struct SavedSetting {
    name: String,
    value: SettingValue,
}

/// The persisted value of a setting — only the value, never the bounds
/// (`min` / `max` / `options` always come from the code defaults).
#[derive(Serialize, Deserialize)]
enum SettingValue {
    Toggle(bool),
    Slider(f32),
    Choice(usize),
    Color([f32; 4]),
}

impl SettingValue {
    /// Snapshots a live setting's value.
    fn capture(setting: &ModuleSetting) -> SettingValue {
        match setting {
            ModuleSetting::Toggle { value, .. } => SettingValue::Toggle(*value),
            ModuleSetting::Slider { value, .. } => SettingValue::Slider(*value),
            ModuleSetting::Choice { value, .. } => SettingValue::Choice(*value),
            ModuleSetting::Color { value, .. } => SettingValue::Color(*value),
        }
    }

    /// Applies the saved value onto a live setting. A type mismatch — the code
    /// changed a setting's kind since the file was written — is ignored.
    fn apply(&self, setting: &mut ModuleSetting) {
        match (self, setting) {
            (SettingValue::Toggle(v), ModuleSetting::Toggle { value, .. }) => *value = *v,
            (
                SettingValue::Slider(v),
                ModuleSetting::Slider {
                    value, min, max, ..
                },
            ) => *value = v.clamp(*min, *max),
            (SettingValue::Choice(v), ModuleSetting::Choice { value, options, .. })
                if *v < options.len() =>
            {
                *value = *v;
            }
            (SettingValue::Color(v), ModuleSetting::Color { value, .. }) => *value = *v,
            _ => {}
        }
    }
}

/// Writes the current state of every registered module to disk. Returns
/// whether the file was written.
pub fn save() -> bool {
    let mut config = Config::default();
    for handle in client().modules.handles() {
        let Ok(module) = handle.lock() else {
            continue;
        };
        let data = module.get_module_data();
        config.modules.push(ModuleConfig {
            id: data.id,
            key_bind: data.key_bind as i32,
            // Spectate takes over the keyboard and mouse the moment it starts;
            // coming back on by itself at the next world join would only
            // surprise. It is never saved as enabled.
            enabled: data.enabled && data.id != ModuleId::Spectate,
            expanded: is_expanded(data.id),
            settings: data
                .settings
                .iter()
                .map(|setting| SavedSetting {
                    name: setting.name().to_string(),
                    value: SettingValue::capture(setting),
                })
                .collect(),
        });
    }

    config.panels = panel_configs();
    config.windows = window_configs();
    config.ui = ui_values();

    let path = config_path();
    match serde_json::to_string_pretty(&config) {
        Ok(json) => match std::fs::write(&path, json) {
            Ok(()) => true,
            Err(error) => {
                log::warn!("config: could not write {}: {error}", path.display());
                false
            }
        },
        Err(error) => {
            log::warn!("config: could not serialize: {error}");
            false
        }
    }
}

/// Loads the saved config, if any, and applies it to the registered modules.
/// Must be called after `register_modules()`. A missing file is the normal
/// first-run case and is silently ignored.
pub fn load() {
    match read() {
        Ok(Some(config)) => apply(&config, false),
        Ok(None) => {}
        Err(error) => log::warn!("config: {error}"),
    }
}

/// Reads and parses the config file; `Ok(None)` when it does not exist.
fn read() -> anyhow::Result<Option<Config>> {
    let path = config_path();
    let json = match std::fs::read_to_string(&path) {
        Ok(json) => json,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => anyhow::bail!("could not read {}: {error}", path.display()),
    };
    serde_json::from_str(&json)
        .map(Some)
        .map_err(|error| anyhow::anyhow!("could not parse {}: {error}", path.display()))
}

/// Applies `config` to the registered modules and the GUI layout.
///
/// With `live` set — a profile switched while the client runs — a module whose
/// enabled flag changes is also started or stopped right away when modules are
/// active, exactly as a GUI toggle would, and the previous layout is cleared
/// first so the profile's layout replaces it rather than merging into it.
fn apply(config: &Config, live: bool) {
    if live {
        reset_ui_state();
    }
    let active = live && client().modules.is_active();

    for handle in client().modules.handles() {
        let Ok(mut module) = handle.lock() else {
            continue;
        };
        let id = module.get_module_data().id;
        let Some(saved) = config.modules.iter().find(|entry| entry.id == id) else {
            continue;
        };
        let was_enabled = module.get_module_data().enabled;

        {
            let data = module.get_module_data_mut();
            data.key_bind = KeyboardKey::from(saved.key_bind);
            for setting in &mut data.settings {
                if let Some(saved_setting) = saved
                    .settings
                    .iter()
                    .find(|entry| entry.name == setting.name())
                {
                    saved_setting.value.apply(setting);
                }
            }
            data.set_enabled(saved.enabled);
        }

        set_expanded(id, saved.expanded);
        // `enabled` is only the armed flag here — at startup the module is
        // actually started later, on the menu→world transition (see
        // `ModuleRegistry`), never at the main menu. A live switch in a world
        // starts / stops it now.
        if active && was_enabled != saved.enabled {
            let result = if saved.enabled {
                module.on_start()
            } else {
                module.on_stop()
            };
            if let Err(error) = result {
                let name = module.get_module_data().name();
                log::warn!("config: module '{name}' failed to switch: {error}");
            }
        }
    }

    for panel in &config.panels {
        set_panel_pos(panel.category, [panel.x, panel.y]);
    }
    for window in &config.windows {
        set_window_pos(&window.title, [window.x, window.y]);
        set_window_open(&window.title, window.open);
    }
    for (key, value) in &config.ui {
        set_ui_value(key, value);
    }
    // Restoring is not a change worth writing back.
    if let Ok(mut since) = LAYOUT_DIRTY_SINCE.lock() {
        *since = None;
    }
}

// --- Live reload, requested from outside the game ---------------------------

/// A pending live-reload request: the sender the render thread answers on.
static PENDING_LOAD: std::sync::Mutex<Option<std::sync::mpsc::Sender<bool>>> =
    std::sync::Mutex::new(None);

/// Asks the render thread to re-read the config file and apply it live, and
/// waits for the outcome. Module start/stop runs JNI against the game, so it is
/// done on the render thread (in [`process_pending`]) rather than on the
/// caller's — the agent loader's command thread.
pub fn request_live_load() -> bool {
    let (sender, receiver) = std::sync::mpsc::channel();
    match PENDING_LOAD.lock() {
        Ok(mut slot) => *slot = Some(sender),
        Err(_) => return false,
    }
    receiver
        .recv_timeout(std::time::Duration::from_secs(3))
        .unwrap_or_else(|_| {
            log::warn!("config: live reload timed out — no frame rendered");
            false
        })
}

/// Serves a pending [`request_live_load`]. Called once per frame.
pub fn process_pending() {
    let Some(sender) = PENDING_LOAD.lock().ok().and_then(|mut slot| slot.take()) else {
        return;
    };
    let ok = match read() {
        Ok(Some(config)) => {
            apply(&config, true);
            log::info!("config: profile applied live");
            true
        }
        Ok(None) => {
            log::warn!("config: live reload requested but there is no config file");
            false
        }
        Err(error) => {
            log::warn!("config: {error}");
            false
        }
    };
    let _ = sender.send(ok);
}
