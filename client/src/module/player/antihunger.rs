//! AntiHunger — the server burns much less hunger. Reduces, does not remove.
//! Same technique as Meteor Client's AntiHunger, with extra safety.
//!
//! Hunger ("exhaustion") is counted by the server from what your client
//! reports:
//!
//! * **Sprint** — the "start sprinting" command is never sent, so the server
//!   thinks you walk: sprinting costs 0.1 exhaustion per metre, walking none.
//!   You still run at full speed. Side effect: the server's sprint-hit bonus
//!   knockback is gone, and a strict anti-cheat may notice running speed
//!   without the sprint flag.
//! * **Ground** — while you stand or walk on the ground, movement packets say
//!   "in the air". A jump only costs exhaustion (0.05, or 0.2 sprint-jumping)
//!   when the server sees you leave the ground, so jumps become free.
//!   Not spoofed — so nothing else breaks — while you mine (the server mines
//!   5× slower for players in the air), while going down (the server would
//!   build up fall distance), on the first tick after landing (fall damage
//!   is taken normally), in water, while riding, or with NoFall on.
//!
//! The decision is made each tick on the render thread and read by the packet
//! hook (on the network thread) through atomics.

use crate::mapping::entity::EntityRef;
use crate::mapping::raw;
use crate::mapping::MappedObject;
use crate::module::{KeyboardKey, Module, ModuleCategory, ModuleData, ModuleId, ModuleSetting};
use crate::net::packet::{Packet, PacketAction};
use crate::state::{client, mapping, minecraft};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// What decides whether "on ground" may be spoofed this tick.
#[derive(Debug, Clone, Copy, Default)]
pub struct GroundState {
    pub on_ground: bool,
    pub was_on_ground: bool,
    pub fall_distance: f64,
    pub descending: bool,
    pub destroying: bool,
    pub exempt: bool,
    pub nofall: bool,
}

/// Pure: whether movement packets may claim "in the air" this tick.
pub fn spoof_ground(s: GroundState) -> bool {
    s.on_ground
        && s.was_on_ground
        && s.fall_distance <= 0.0
        && !s.descending
        && !s.destroying
        && !s.exempt
        && !s.nofall
}

/// Set each tick, read by the packet hook.
static SPOOF_GROUND: AtomicBool = AtomicBool::new(false);
/// Riding or in water — leave sprint packets alone too (as Meteor does).
static EXEMPT: AtomicBool = AtomicBool::new(true);

#[derive(Debug)]
pub struct AntiHungerModule {
    pub module: ModuleData,
    /// `(on_ground, y)` of the previous tick.
    last: Mutex<Option<(bool, f64)>>,
}

impl AntiHungerModule {
    pub fn new() -> Self {
        Self {
            module: ModuleData {
                id: ModuleId::AntiHunger,
                description: "Server burns much less hunger (sprint / ground spoof)".to_string(),
                category: ModuleCategory::Player,
                key_bind: KeyboardKey::KeyNone,
                enabled: false,
                settings: vec![
                    ModuleSetting::Toggle {
                        name: "Spoof sprint".to_string(),
                        value: true,
                    },
                    ModuleSetting::Toggle {
                        name: "Spoof ground".to_string(),
                        value: true,
                    },
                ],
            },
            last: Mutex::new(None),
        }
    }

    fn toggle(&self, name: &str) -> bool {
        self.module
            .get_setting(name)
            .and_then(ModuleSetting::get_toggle_value)
            .unwrap_or(true)
    }
}

fn nofall_on() -> bool {
    client()
        .modules
        .get(ModuleId::NoFall)
        .and_then(|handle| handle.lock().ok().map(|m| m.get_module_data().enabled))
        .unwrap_or(false)
}

impl Module for AntiHungerModule {
    fn on_start(&self) -> anyhow::Result<()> {
        *self.last.lock().unwrap() = None;
        SPOOF_GROUND.store(false, Ordering::Relaxed);
        Ok(())
    }

    fn on_stop(&self) -> anyhow::Result<()> {
        SPOOF_GROUND.store(false, Ordering::Relaxed);
        EXEMPT.store(true, Ordering::Relaxed);
        Ok(())
    }

    fn on_tick(&self) -> anyhow::Result<()> {
        let mc = minecraft();
        let (Some(player), Some(game_mode)) = (mc.player()?, mc.game_mode()?) else {
            SPOOF_GROUND.store(false, Ordering::Relaxed);
            return Ok(());
        };
        let fall_distance = player.get_fall_distance()?;
        let (on_ground, y, exempt, destroying) = mapping().in_frame(|| {
            let mut env = mapping().get_env()?;
            let p = player.jni_ref().as_obj();
            let on_ground = raw::call(&mut env, p, "onGround", "()Z", &[])?.z()?;
            let y = raw::call(&mut env, p, "getY", "()D", &[])?.d()?;
            let exempt = raw::call(&mut env, p, "isPassenger", "()Z", &[])?.z()?
                || raw::call(&mut env, p, "isInWater", "()Z", &[])?.z()?
                || raw::call(&mut env, p, "isUnderWater", "()Z", &[])?.z()?;
            let destroying = raw::call(&mut env, game_mode.jni_ref().as_obj(), "isDestroying", "()Z", &[])?.z()?;
            Ok((on_ground, y, exempt, destroying))
        })?;

        let mut last = self.last.lock().unwrap();
        let (was_on_ground, last_y) = last.unwrap_or((false, y));
        *last = Some((on_ground, y));
        EXEMPT.store(exempt, Ordering::Relaxed);
        SPOOF_GROUND.store(
            self.toggle("Spoof ground")
                && spoof_ground(GroundState {
                    on_ground,
                    was_on_ground,
                    fall_distance,
                    descending: y < last_y - 1e-3,
                    destroying,
                    exempt,
                    nofall: nofall_on(),
                }),
            Ordering::Relaxed,
        );
        Ok(())
    }

    fn handle_packet(&self, packet: &mut Packet) -> PacketAction {
        match packet {
            Packet::ServerboundPlayerCommand(command)
                if command.action == "START_SPRINTING"
                    && self.toggle("Spoof sprint")
                    && !EXEMPT.load(Ordering::Relaxed) =>
            {
                PacketAction::Cancel
            }
            Packet::ServerboundMovePlayer(movement) if SPOOF_GROUND.load(Ordering::Relaxed) => {
                movement.on_ground = false;
                PacketAction::Forward
            }
            _ => PacketAction::Forward,
        }
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

    fn standing() -> GroundState {
        GroundState {
            on_ground: true,
            was_on_ground: true,
            ..Default::default()
        }
    }

    #[test]
    fn standing_still_is_spoofed() {
        assert!(spoof_ground(standing()));
    }

    #[test]
    fn anything_risky_is_not() {
        for state in [
            GroundState { on_ground: false, ..standing() },
            GroundState { was_on_ground: false, ..standing() }, // just landed: take fall damage
            GroundState { fall_distance: 0.5, ..standing() },
            GroundState { descending: true, ..standing() },
            GroundState { destroying: true, ..standing() },
            GroundState { exempt: true, ..standing() },
            GroundState { nofall: true, ..standing() },
        ] {
            assert!(!spoof_ground(state), "{state:?}");
        }
    }
}
