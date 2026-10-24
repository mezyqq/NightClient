//! The module registry — every registered module, keyed by [`ModuleId`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use dashmap::DashMap;
use log::error;

use crate::module::{KeyboardKey, Module, ModuleId, ModuleSetting, ModuleType};
use crate::net::packet::{Packet, PacketAction};

/// One-shot modules that asked to switch themselves off — applied at the
/// start of the next [`ModuleRegistry::tick`], outside the module's own lock.
static DISABLE_REQUESTS: Mutex<Vec<ModuleId>> = Mutex::new(Vec::new());

/// Asks for module `id` to be switched off on the next tick. Lets a module
/// that is an action rather than a state (e.g. a teleport) turn itself off
/// after running — it cannot from inside its own `on_start`, which runs while
/// its lock is held.
pub fn request_disable(id: ModuleId) {
    DISABLE_REQUESTS
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .push(id);
}

/// A shared, lockable handle to one module.
pub type ModuleHandle = Arc<Mutex<ModuleType>>;

/// A module's factory defaults — the keybind and settings it was registered
/// with. Kept so "Reset Settings" can restore them.
struct ModuleDefaults {
    key_bind: KeyboardKey,
    settings: Vec<ModuleSetting>,
}

/// Holds every registered module. Backed by a `DashMap` so the render thread
/// can read it each frame without contending on a single global lock.
#[derive(Default)]
pub struct ModuleRegistry {
    modules: DashMap<ModuleId, ModuleHandle>,
    /// Factory defaults captured at registration, keyed like `modules`.
    defaults: DashMap<ModuleId, ModuleDefaults>,
    /// Whether modules may currently run — true only in a world with a living
    /// player. At the main menu, or while the player is dead on the respawn
    /// screen, modules stay armed (`enabled`) but inactive.
    active: AtomicBool,
}

impl ModuleRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a module under its [`ModuleId`], capturing its factory
    /// defaults so they can be restored later.
    pub fn register<M>(&self, module: M)
    where
        M: Module + Send + Sync + 'static,
    {
        let module: ModuleType = Box::new(module);
        let data = module.get_module_data();
        let id = data.id;
        self.defaults.insert(
            id,
            ModuleDefaults {
                key_bind: data.key_bind,
                settings: data.settings.clone(),
            },
        );
        self.modules.insert(id, Arc::new(Mutex::new(module)));
    }

    /// Restores every module to its factory defaults: default keybind, default
    /// setting values, and disabled. Backs the GUI's "Reset Settings" button.
    pub fn reset_settings(&self) {
        for entry in self.modules.iter() {
            let Some(defaults) = self.defaults.get(entry.key()) else {
                continue;
            };
            let Ok(mut module) = entry.value().lock() else {
                continue;
            };
            if module.get_module_data().enabled && self.is_active() {
                let _ = module.on_stop();
            }
            let data = module.get_module_data_mut();
            data.key_bind = defaults.key_bind;
            data.settings = defaults.settings.clone();
            data.enabled = false;
        }
    }

    /// Whether modules may currently run — in a world, with a living player.
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }

    /// Updates whether modules may run. On every transition each *enabled*
    /// module is started or stopped — so a module armed at the menu only truly
    /// runs once the player is in-game and alive, and stands down again when
    /// they leave the world or die.
    pub fn set_active(&self, active: bool) {
        if self.active.swap(active, Ordering::SeqCst) == active {
            return; // no transition
        }
        for handle in self.handles() {
            let Ok(module) = handle.lock() else {
                continue;
            };
            if !module.get_module_data().enabled {
                continue;
            }
            let result = if active {
                module.on_start()
            } else {
                module.on_stop()
            };
            if let Err(e) = result {
                let name = module.get_module_data().name();
                let verb = if active { "start" } else { "stop" };
                error!("module '{name}' failed to {verb} on world transition: {e}");
            }
        }
    }

    /// A handle to one module by id.
    pub fn get(&self, id: ModuleId) -> Option<ModuleHandle> {
        self.modules.get(&id).map(|entry| Arc::clone(entry.value()))
    }

    /// Handles to every module. Snapshotted, so the caller holds no shard
    /// locks while it works with the modules.
    pub fn handles(&self) -> Vec<ModuleHandle> {
        self.modules
            .iter()
            .map(|entry| Arc::clone(entry.value()))
            .collect()
    }

    /// Every module keyed by id — an owned snapshot.
    pub fn by_id(&self) -> HashMap<ModuleId, ModuleHandle> {
        self.modules
            .iter()
            .map(|entry| (*entry.key(), Arc::clone(entry.value())))
            .collect()
    }

    /// Ticks every enabled module once. A module whose tick fails is stopped
    /// rather than aborting the whole pass.
    pub fn tick(&self) {
        self.apply_disable_requests();
        for handle in self.handles() {
            let Ok(module) = handle.lock() else {
                continue;
            };
            if !module.get_module_data().enabled {
                continue;
            }
            if let Err(e) = module.on_tick() {
                let name = module.get_module_data().name();
                error!("module '{name}' tick failed, stopping it: {e}");
                if let Err(e) = module.on_stop() {
                    error!("module '{name}' also failed to stop: {e}");
                }
            }
        }
    }

    /// Switches off every module that called [`request_disable`].
    fn apply_disable_requests(&self) {
        let requests = std::mem::take(
            &mut *DISABLE_REQUESTS
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()),
        );
        let mut changed = false;
        for id in requests {
            let Some(handle) = self.get(id) else {
                continue;
            };
            let Ok(mut module) = handle.lock() else {
                continue;
            };
            if !module.get_module_data().enabled {
                continue;
            }
            module.get_module_data_mut().set_enabled(false);
            if let Err(e) = module.on_stop() {
                error!("module '{}' failed to stop: {e}", module.get_module_data().name());
            }
            changed = true;
        }
        // Every module lock is released by now — `save` locks them all.
        if changed {
            crate::config::save();
        }
    }

    /// Offers `packet` to every enabled module's `handle_packet`. Called from
    /// the connection's packet dispatch, on the Netty thread. Returns
    /// [`PacketAction::Cancel`] as soon as any module asks to drop the packet
    /// (the remaining modules are then skipped), otherwise
    /// [`PacketAction::Forward`].
    pub fn handle_packet(&self, packet: &mut Packet) -> PacketAction {
        if !self.is_active() {
            return PacketAction::Forward;
        }
        for handle in self.handles() {
            let Ok(module) = handle.lock() else {
                continue;
            };
            if !module.get_module_data().enabled {
                continue;
            }
            if module.handle_packet(packet) == PacketAction::Cancel {
                return PacketAction::Cancel;
            }
        }
        PacketAction::Forward
    }
}
