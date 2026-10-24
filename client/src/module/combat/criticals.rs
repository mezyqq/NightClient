//! Criticals — makes the combat auras land critical hits.
//!
//! A hit only registers as critical when the attacker is airborne and
//! *descending* (`fallDistance > 0`). When armed, this module hops the player
//! off the ground and the aura holds its hit until that window opens — see
//! [`prepare`], which the auras call right before they attack.

use crate::mapping::entity::player::LocalPlayer;
use crate::mapping::entity::EntityRef;
use crate::module::{KeyboardKey, Module, ModuleCategory, ModuleData, ModuleId};
use crate::mapping::raw;
use crate::mapping::MappedObject;
use crate::state::{client, mapping};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Upward velocity of the crit hop — Minecraft's vanilla jump velocity.
const HOP_VELOCITY: f64 = 0.42;

/// Whether to attack now, or hold the hit while a crit is set up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CritState {
    /// Attack now.
    Ready,
    /// Hold the hit — the player is being hopped into the crit window.
    Charging,
}

/// Whether the Criticals module is currently armed.
fn enabled() -> bool {
    client()
        .modules
        .get(ModuleId::Criticals)
        .and_then(|arc| {
            arc.lock()
                .ok()
                .map(|module| module.get_module_data().enabled)
        })
        .unwrap_or(false)
}

/// Longest a hit is held for a crit. A full hop takes ~0.5 s to start
/// falling; past this the hop failed (a ceiling, a slab edge, server
/// rubber-band) and the hit goes out uncritical rather than never.
const MAX_CHARGE: Duration = Duration::from_millis(700);

/// When the current crit set-up began.
static CHARGE_SINCE: Mutex<Option<Instant>> = Mutex::new(None);

/// What to do about a pending hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Attack,
    Hop,
    Wait,
}

/// Pure: the crit decision. `possible` — a crit can happen at all here (not
/// in water / lava, climbing, flying, riding); `charged_for` — how long this
/// hit has already been held.
pub fn decide(possible: bool, on_ground: bool, falling: bool, charged_for: Option<Duration>) -> Decision {
    let expired = charged_for.is_some_and(|held| held >= MAX_CHARGE);
    if !possible || expired {
        return Decision::Attack;
    }
    match (on_ground, falling) {
        (true, _) => Decision::Hop,
        (false, true) => Decision::Attack,
        (false, false) => Decision::Wait,
    }
}

/// Whether a critical hit can happen in the player's situation at all —
/// vanilla never crits in water or lava, on ladders / vines, while riding or
/// flying (creative flight, elytra).
fn crit_possible(player: &LocalPlayer) -> anyhow::Result<bool> {
    mapping().in_frame(|| {
        let mut env = mapping().get_env()?;
        let p = player.jni_ref().as_obj();
        for check in ["isInWater", "isInLava", "onClimbable", "isPassenger", "isFallFlying"] {
            if raw::call(&mut env, p, check, "()Z", &[])?.z()? {
                return Ok(false);
            }
        }
        let abilities = raw::call(&mut env, p, "getAbilities", "()Lnet/minecraft/world/entity/player/Abilities;", &[])?.l()?;
        Ok(!raw::field(&mut env, &abilities, "flying", "Z")?.z()?)
    })
}

/// Called by a combat aura just before it attacks.
///
/// With Criticals off this is always [`CritState::Ready`]. With it on, the
/// player is hopped off the ground and the hit held ([`CritState::Charging`])
/// until they are airborne and descending — the window in which a hit
/// registers as a critical. Where no crit is possible, or the hop does not
/// bring one within [`MAX_CHARGE`], the hit goes out anyway.
pub fn prepare(player: &LocalPlayer) -> anyhow::Result<CritState> {
    if !enabled() {
        return Ok(CritState::Ready);
    }
    let possible = crit_possible(player).unwrap_or(true);
    let on_ground = player.on_ground()?;
    let falling = !on_ground && player.get_fall_distance()? > 0.0;
    let mut since = CHARGE_SINCE.lock().unwrap_or_else(|p| p.into_inner());
    // A set-up abandoned long ago (the target was lost) starts over.
    if since.is_some_and(|at| at.elapsed() > MAX_CHARGE * 3) {
        *since = None;
    }
    match decide(possible, on_ground, falling, since.map(|at| at.elapsed())) {
        Decision::Attack => {
            *since = None;
            Ok(CritState::Ready)
        }
        Decision::Hop => {
            since.get_or_insert_with(Instant::now);
            // Hop straight up, keeping the current horizontal motion.
            let motion = player.get_delta_movement()?;
            player.set_delta_movement(motion.x(), HOP_VELOCITY, motion.z())?;
            Ok(CritState::Charging)
        }
        Decision::Wait => {
            since.get_or_insert_with(Instant::now);
            Ok(CritState::Charging)
        }
    }
}

/// Highlights nothing and draws nothing — a pure modifier consulted by the
/// combat auras through [`prepare`].
#[derive(Debug)]
pub struct CriticalsModule {
    pub module: ModuleData,
}

impl CriticalsModule {
    pub fn new() -> Self {
        Self {
            module: ModuleData {
                id: ModuleId::Criticals,
                description: "Makes the combat auras land critical hits".to_string(),
                category: ModuleCategory::Combat,
                key_bind: KeyboardKey::KeyNone,
                enabled: false,
                settings: vec![],
            },
        }
    }
}

impl Module for CriticalsModule {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_crit_is_set_up_when_possible() {
        assert_eq!(decide(true, true, false, None), Decision::Hop);
        assert_eq!(decide(true, false, false, Some(Duration::from_millis(200))), Decision::Wait);
        assert_eq!(decide(true, false, true, Some(Duration::from_millis(400))), Decision::Attack);
    }

    #[test]
    fn the_hit_is_never_held_forever() {
        // In water, on a ladder, flying: attack at once.
        assert_eq!(decide(false, false, false, None), Decision::Attack);
        assert_eq!(decide(false, true, false, None), Decision::Attack);
        // The hop failed (ceiling): give up on the crit.
        assert_eq!(decide(true, true, false, Some(MAX_CHARGE)), Decision::Attack);
        assert_eq!(decide(true, false, false, Some(Duration::from_secs(5))), Decision::Attack);
    }
}
