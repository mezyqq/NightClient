//! SafeWalk — never walk off an edge by accident.
//!
//! Each tick the module looks where the player is about to be — along the
//! movement keys and along the current motion — and checks for ground under
//! every corner of the hitbox there. When there is none, it holds the sneak
//! key for the player: vanilla's own sneak logic then refuses to step off
//! the edge. It lets go as soon as the ground ahead is back, so you only
//! crouch right at the edge (others see that crouch).
//!
//! One-block steps down are allowed by default, so stairs and slopes work.

use crate::mapping::client::world::World;
use crate::mapping::entity::{EntityRef, LivingEntityRef};
use crate::mapping::math::BlockPos;
use crate::mapping::raw;
use crate::mapping::MappedObject;
use crate::module::{KeyboardKey, Module, ModuleCategory, ModuleData, ModuleId, ModuleSetting};
use crate::state::{mapping, minecraft};
use jni::objects::JValue;
use std::sync::atomic::{AtomicBool, Ordering};

/// How far ahead along the movement keys to look, in blocks.
const LOOKAHEAD: f64 = 0.5;
/// Half the player's hitbox width.
const HALF_WIDTH: f64 = 0.3;

/// Set while SafeWalk holds the sneak key.
static FORCED: AtomicBool = AtomicBool::new(false);

/// Pure: the world-space direction of the movement keys — `strafe` (`xxa`,
/// left positive) and `forward` (`zza`) for a player facing `yaw` degrees —
/// as a unit vector, or `None` when no key is held.
pub fn input_direction(yaw: f32, strafe: f32, forward: f32) -> Option<[f64; 2]> {
    let (sin, cos) = (yaw as f64).to_radians().sin_cos();
    let (strafe, forward) = (strafe as f64, forward as f64);
    // Facing +Z at yaw 0; "left" is +X.
    let x = -sin * forward + cos * strafe;
    let z = cos * forward + sin * strafe;
    let length = (x * x + z * z).sqrt();
    (length > 1e-6).then(|| [x / length, z / length])
}

/// Pure: the centre and the four corners of a hitbox of half-width `half`
/// centred at `(x, z)`.
pub fn footprint(x: f64, z: f64, half: f64) -> [[f64; 2]; 5] {
    [
        [x, z],
        [x - half, z - half],
        [x + half, z - half],
        [x - half, z + half],
        [x + half, z + half],
    ]
}

#[derive(Debug)]
pub struct SafeWalkModule {
    pub module: ModuleData,
}

impl SafeWalkModule {
    pub fn new() -> Self {
        Self {
            module: ModuleData {
                id: ModuleId::SafeWalk,
                description: "Stops you walking off edges".to_string(),
                category: ModuleCategory::Movement,
                key_bind: KeyboardKey::KeyNone,
                enabled: false,
                settings: vec![ModuleSetting::Toggle {
                    name: "Allow 1-block drops".to_string(),
                    value: true,
                }],
            },
        }
    }
}

/// Holds or releases the sneak key.
fn set_sneak(down: bool) -> anyhow::Result<()> {
    mapping().in_frame(|| {
        let mut env = mapping().get_env()?;
        let options = raw::field(&mut env, minecraft().jni_ref().as_obj(), "options", "Lnet/minecraft/client/Options;")?.l()?;
        let key = raw::field(&mut env, &options, "keyShift", "Lnet/minecraft/client/KeyMapping;")?.l()?;
        raw::call(&mut env, &key, "setDown", "(Z)V", &[JValue::Bool(down as u8)])?;
        Ok(())
    })
}

/// Whether there is ground under some part of a hitbox at `(x, feet_y, z)`.
fn supported(world: &World, x: f64, feet_y: f64, z: f64, allow_drop: bool) -> anyhow::Result<bool> {
    let below = (feet_y - 0.5).floor() as i32;
    for [cx, cz] in footprint(x, z, HALF_WIDTH - 0.01) {
        let (bx, bz) = (cx.floor() as i32, cz.floor() as i32);
        if !world.is_block_air(&BlockPos::new(bx, below, bz))? {
            return Ok(true);
        }
        if allow_drop && !world.is_block_air(&BlockPos::new(bx, below - 1, bz))? {
            return Ok(true);
        }
    }
    Ok(false)
}

impl Module for SafeWalkModule {
    fn on_start(&self) -> anyhow::Result<()> {
        Ok(())
    }

    fn on_stop(&self) -> anyhow::Result<()> {
        if FORCED.swap(false, Ordering::Relaxed) {
            set_sneak(false)?;
        }
        Ok(())
    }

    fn on_tick(&self) -> anyhow::Result<()> {
        let mc = minecraft();
        let (Some(player), Some(world)) = (mc.player()?, mc.world()?) else {
            return Ok(());
        };
        let allow_drop = self
            .module
            .get_setting("Allow 1-block drops")
            .and_then(ModuleSetting::get_toggle_value)
            .unwrap_or(true);

        let mut edge = false;
        if player.on_ground()? && !mc.current_screen().is_open() {
            let pos = player.get_position()?;
            let motion = player.get_delta_movement()?;
            let (strafe, forward) = player.move_input()?;
            let mut ahead = vec![[pos.x() + motion.x() * 2.0, pos.z() + motion.z() * 2.0]];
            if let Some([dx, dz]) = input_direction(player.get_yaw()?, strafe, forward) {
                ahead.push([pos.x() + dx * LOOKAHEAD, pos.z() + dz * LOOKAHEAD]);
            }
            for [x, z] in ahead {
                if !supported(&world, x, pos.y(), z, allow_drop)? {
                    edge = true;
                    break;
                }
            }
        }

        let forced = FORCED.load(Ordering::Relaxed);
        if edge && !forced {
            set_sneak(true)?;
            FORCED.store(true, Ordering::Relaxed);
        } else if !edge && forced {
            set_sneak(false)?;
            FORCED.store(false, Ordering::Relaxed);
        }
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
    fn movement_keys_map_to_world_directions() {
        // Facing +Z (yaw 0): forward is +Z, left strafe is +X.
        let [x, z] = input_direction(0.0, 0.0, 1.0).unwrap();
        assert!(x.abs() < 1e-9 && (z - 1.0).abs() < 1e-9);
        let [x, z] = input_direction(0.0, 1.0, 0.0).unwrap();
        assert!((x - 1.0).abs() < 1e-9 && z.abs() < 1e-9);
        // Facing -X (yaw 90): forward is -X.
        let [x, z] = input_direction(90.0, 0.0, 1.0).unwrap();
        assert!((x + 1.0).abs() < 1e-9 && z.abs() < 1e-9);
        assert!(input_direction(0.0, 0.0, 0.0).is_none());
    }

    #[test]
    fn the_footprint_covers_the_hitbox() {
        let points = footprint(0.5, 0.5, 0.3);
        assert_eq!(points[0], [0.5, 0.5]);
        assert!(points.iter().all(|[x, z]| (0.2..=0.8).contains(x) && (0.2..=0.8).contains(z)));
    }
}
