//! AutoFish — reels in on a bite and casts again.
//!
//! The server flags a bite on the player's fishing hook (`DATA_BITING`); the
//! client keeps it in `FishingHook.biting`. Each tick the module looks at
//! `player.fishing`: on a bite it uses the rod (as a right click would) after
//! "Reel delay", then casts again after "Recast delay". It only recasts a
//! line it reeled in itself, so it never starts fishing on its own.

use crate::graphic::hook::game_clock;
use crate::mapping::raw;
use crate::mapping::MappedObject;
use crate::module::player::hands;
use crate::module::{KeyboardKey, Module, ModuleCategory, ModuleData, ModuleId, ModuleSetting};
use crate::state::{mapping, minecraft};
use jni::objects::{JObject, JValue};
use jni::JNIEnv;
use std::sync::Mutex;

const ROD: &str = "minecraft:fishing_rod";

/// What the module is waiting for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Plan {
    Idle,
    /// Reel in at this game tick.
    Reel(u64),
    /// Cast again at this game tick.
    Recast(u64),
}

static PLAN: Mutex<Plan> = Mutex::new(Plan::Idle);

/// Pure: the next plan, given the current one, the tick, whether a hook is
/// out and biting, and the delays.
pub fn next(plan: Plan, now: u64, hook_out: bool, biting: bool, reel_delay: u64, recast_delay: u64) -> (Plan, Option<Action>) {
    match plan {
        Plan::Idle if hook_out && biting => (Plan::Reel(now + reel_delay), None),
        Plan::Reel(at) if now >= at => {
            if hook_out {
                (Plan::Recast(now + recast_delay), Some(Action::Use))
            } else {
                (Plan::Idle, None) // the line is gone already
            }
        }
        Plan::Recast(at) if now >= at => {
            if hook_out {
                (Plan::Idle, None) // cast by hand meanwhile
            } else {
                (Plan::Idle, Some(Action::Use))
            }
        }
        other => (other, None),
    }
}

/// Something to do with the rod.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Action {
    Use,
}

#[derive(Debug)]
pub struct AutoFishModule {
    pub module: ModuleData,
}

impl AutoFishModule {
    pub fn new() -> Self {
        let slider = |name: &str, value: f32, min: f32, max: f32| ModuleSetting::Slider {
            name: name.to_string(),
            value,
            min,
            max,
        };
        Self {
            module: ModuleData {
                id: ModuleId::AutoFish,
                description: "Reels in on a bite and casts again".to_string(),
                category: ModuleCategory::Player,
                key_bind: KeyboardKey::KeyNone,
                enabled: false,
                settings: vec![
                    slider("Reel delay (ticks)", 2.0, 0.0, 20.0),
                    slider("Recast delay (ticks)", 15.0, 2.0, 60.0),
                ],
            },
        }
    }

    fn ticks(&self, name: &str, fallback: f32) -> u64 {
        self.module
            .get_setting(name)
            .and_then(ModuleSetting::get_slider_value)
            .unwrap_or(fallback)
            .round()
            .max(0.0) as u64
    }
}

/// The hand holding a fishing rod, main hand first.
fn rod_hand<'l>(env: &mut JNIEnv<'l>, player: &JObject) -> anyhow::Result<Option<JObject<'l>>> {
    for (getter, hand) in [("getMainHandItem", "MAIN_HAND"), ("getOffhandItem", "OFF_HAND")] {
        let stack = raw::call(env, player, getter, &format!("(){}", hands::STACK_SIG), &[])?.l()?;
        if raw::call(env, &stack, "isEmpty", "()Z", &[])?.z()? || hands::item_id(env, &stack)? != ROD {
            continue;
        }
        let class = mapping().resolve_class(env, "net/minecraft/world/InteractionHand")?;
        let value = env
            .get_static_field(class, hand, "Lnet/minecraft/world/InteractionHand;")
            .map_err(|error| {
                let _ = env.exception_clear();
                anyhow::anyhow!("InteractionHand.{hand}: {error}")
            })?
            .l()?;
        return Ok(Some(value));
    }
    Ok(None)
}

impl Module for AutoFishModule {
    fn on_start(&self) -> anyhow::Result<()> {
        *PLAN.lock().unwrap_or_else(|p| p.into_inner()) = Plan::Idle;
        Ok(())
    }

    fn on_stop(&self) -> anyhow::Result<()> {
        *PLAN.lock().unwrap_or_else(|p| p.into_inner()) = Plan::Idle;
        Ok(())
    }

    fn on_tick(&self) -> anyhow::Result<()> {
        let mc = minecraft();
        let (Some(player), Some(game_mode)) = (mc.player()?, mc.game_mode()?) else {
            return Ok(());
        };
        let reel_delay = self.ticks("Reel delay (ticks)", 2.0);
        let recast_delay = self.ticks("Recast delay (ticks)", 15.0);
        let now = game_clock().0;

        mapping().in_frame(|| {
            let mut env = mapping().get_env()?;
            let p = player.jni_ref().as_obj();
            let hook = raw::field(&mut env, p, "fishing", "Lnet/minecraft/world/entity/projectile/FishingHook;")?.l()?;
            let hook_out = !hook.is_null();
            let biting = hook_out && raw::field(&mut env, &hook, "biting", "Z")?.z()?;

            let mut plan = PLAN.lock().unwrap_or_else(|p| p.into_inner());
            let (next_plan, action) = next(*plan, now, hook_out, biting, reel_delay, recast_delay);
            *plan = next_plan;
            drop(plan);

            if action == Some(Action::Use) {
                let Some(hand) = rod_hand(&mut env, p)? else {
                    *PLAN.lock().unwrap_or_else(|p| p.into_inner()) = Plan::Idle;
                    return Ok(()); // the rod was put away
                };
                raw::call(
                    &mut env,
                    game_mode.jni_ref().as_obj(),
                    "useItem",
                    "(Lnet/minecraft/world/entity/player/Player;Lnet/minecraft/world/InteractionHand;)Lnet/minecraft/world/InteractionResult;",
                    &[JValue::Object(p), JValue::Object(&hand)],
                )?;
                raw::call(&mut env, p, "swing", "(Lnet/minecraft/world/InteractionHand;)V", &[JValue::Object(&hand)])?;
            }
            Ok(())
        })
    }

    fn get_module_data(&self) -> &ModuleData {
        &self.module
    }

    fn get_module_data_mut(&mut self) -> &mut ModuleData {
        &mut self.module
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bite_is_reeled_in_then_recast() {
        let (plan, action) = next(Plan::Idle, 100, true, true, 2, 15);
        assert_eq!((plan, action), (Plan::Reel(102), None));
        assert_eq!(next(plan, 101, true, true, 2, 15), (Plan::Reel(102), None));
        let (plan, action) = next(plan, 102, true, false, 2, 15);
        assert_eq!((plan, action), (Plan::Recast(117), Some(Action::Use)));
        assert_eq!(next(plan, 110, false, false, 2, 15), (Plan::Recast(117), None));
        assert_eq!(next(plan, 117, false, false, 2, 15), (Plan::Idle, Some(Action::Use)));
    }

    #[test]
    fn it_never_casts_on_its_own() {
        assert_eq!(next(Plan::Idle, 5, false, false, 2, 15), (Plan::Idle, None));
        assert_eq!(next(Plan::Idle, 5, true, false, 2, 15), (Plan::Idle, None));
        // Cast by hand before the recast: leave it.
        assert_eq!(next(Plan::Recast(10), 10, true, false, 2, 15), (Plan::Idle, None));
    }
}
