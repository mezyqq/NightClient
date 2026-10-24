//! TriggerBot — attacks only what the crosshair is already on. It never turns
//! the camera, so on a server it looks like ordinary, well-timed clicking.

use crate::mapping::entity::mob::Mob;
use crate::mapping::entity::player::Player;
use crate::mapping::entity::{Entity, EntityRef, LivingEntityRef, PlayerRef};
use crate::mapping::raw;
use crate::mapping::MappedObject;
use crate::module::combat::aura::is_enemy;
use crate::module::combat::criticals::{self, CritState};
use crate::module::combat::is_protected;
use crate::module::{KeyboardKey, Module, ModuleCategory, ModuleData, ModuleId, ModuleSetting};
use crate::state::{mapping, minecraft};
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct TriggerBotModule {
    pub module: ModuleData,
    last_attack: Mutex<Option<Instant>>,
}

impl TriggerBotModule {
    pub fn new() -> Self {
        Self {
            module: ModuleData {
                id: ModuleId::TriggerBot,
                description: "Attacks whatever the crosshair is on".to_string(),
                category: ModuleCategory::Combat,
                key_bind: KeyboardKey::KeyNone,
                enabled: false,
                settings: vec![
                    ModuleSetting::Toggle {
                        name: "Players".to_string(),
                        value: true,
                    },
                    ModuleSetting::Choice {
                        name: "Mob targets".to_string(),
                        value: 1,
                        options: vec!["Off".into(), "Hostile".into(), "All".into()],
                    },
                    ModuleSetting::Slider {
                        name: "CPS".to_string(),
                        value: 10.0,
                        min: 1.0,
                        max: 20.0,
                    },
                    ModuleSetting::Toggle {
                        name: "Cooldown".to_string(),
                        value: true,
                    },
                ],
            },
            last_attack: Mutex::new(None),
        }
    }

    fn toggle(&self, name: &str, fallback: bool) -> bool {
        self.module
            .get_setting(name)
            .and_then(ModuleSetting::get_toggle_value)
            .unwrap_or(fallback)
    }

    fn mob_mode(&self) -> usize {
        match self.module.get_setting("Mob targets") {
            Some(ModuleSetting::Choice { value, .. }) => *value,
            _ => 1,
        }
    }

    fn wants(&self, entity: &Entity) -> bool {
        if entity.instance_of::<Player>() {
            return self.toggle("Players", true) && !is_protected(entity);
        }
        if entity.instance_of::<Mob>() {
            return match self.mob_mode() {
                0 => false,
                1 => is_enemy(entity),
                _ => true,
            };
        }
        false
    }
}

impl Module for TriggerBotModule {
    fn on_start(&self) -> anyhow::Result<()> {
        Ok(())
    }

    fn on_stop(&self) -> anyhow::Result<()> {
        *self.last_attack.lock().unwrap() = None;
        Ok(())
    }

    fn on_tick(&self) -> anyhow::Result<()> {
        let mc = minecraft();
        if mc.current_screen().is_open() {
            return Ok(());
        }
        let (Some(player), Some(game_mode)) = (mc.player()?, mc.game_mode()?) else {
            return Ok(());
        };

        let target = mapping().in_frame(|| {
            let mut env = mapping().get_env()?;
            let entity = raw::field(
                &mut env,
                mc.jni_ref().as_obj(),
                "crosshairPickEntity",
                "Lnet/minecraft/world/entity/Entity;",
            )?
            .l()?;
            if entity.is_null() {
                return Ok(None);
            }
            Ok(Some(Entity::new(env.new_global_ref(&entity)?)))
        })?;
        let Some(target) = target else {
            return Ok(());
        };
        if !self.wants(&target) || !target.as_living().is_some_and(|living| living.is_alive().unwrap_or(false)) {
            return Ok(());
        }

        let cps = self
            .module
            .get_setting("CPS")
            .and_then(ModuleSetting::get_slider_value)
            .unwrap_or(10.0)
            .max(1.0);
        let interval = Duration::from_secs_f32(1.0 / cps);
        if self.last_attack.lock().unwrap().is_some_and(|last| last.elapsed() < interval) {
            return Ok(());
        }
        if self.toggle("Cooldown", true) && player.attack_strength_scale()? < 1.0 {
            return Ok(());
        }
        if criticals::prepare(&player)? != CritState::Ready {
            return Ok(());
        }

        game_mode.attack(&player, &target)?;
        player.swing()?;
        *self.last_attack.lock().unwrap() = Some(Instant::now());
        Ok(())
    }

    fn get_module_data(&self) -> &ModuleData {
        &self.module
    }

    fn get_module_data_mut(&mut self) -> &mut ModuleData {
        &mut self.module
    }
}
