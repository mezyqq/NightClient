//! ElytraFly — steer an elytra flight with the movement keys.
//!
//! While gliding, each tick the velocity is set from the keys: WASD moves
//! horizontally at "Horizontal speed" relative to where you look, Jump
//! climbs and Sneak descends at "Vertical speed"; with no keys held you hang
//! in place. The elytra physics of the next tick still pulls the velocity
//! down and toward the look direction; the pull of gravity is compensated
//! in advance ([`gravity_pull`]) so hovering hardly sinks.
//!
//! Start gliding as usual (jump, then jump again in the air). The server
//! allows a gliding player up to ~17 blocks per tick before it calls the
//! move too fast; anti-cheats are far stricter.

use crate::mapping::entity::{EntityRef, LivingEntityRef};
use crate::module::movement::safewalk::input_direction;
use crate::module::{KeyboardKey, Module, ModuleCategory, ModuleData, ModuleId, ModuleSetting};
use crate::state::minecraft;
use crate::mapping::MappedObject;

/// Vanilla gravity on a gliding player, per tick.
const GRAVITY: f64 = 0.08;
/// Vertical drag the elytra applies after gravity.
const DRAG_Y: f64 = 0.98;

/// Pure: how much the next tick's elytra physics lowers the vertical
/// velocity when looking at `pitch` degrees — vanilla's
/// `gravity * (-1 + cos²(pitch) * 0.75)`, as a positive pull.
pub fn gravity_pull(pitch: f32) -> f64 {
    let cos = (pitch as f64).to_radians().cos();
    GRAVITY * (1.0 - cos * cos * 0.75)
}

/// Pure: the velocity to set so the next tick moves at `horizontal` (a world
/// direction scaled to the speed) and `vertical` blocks per tick.
pub fn target_velocity(horizontal: [f64; 2], vertical: f64, pitch: f32) -> [f64; 3] {
    [horizontal[0], vertical / DRAG_Y + gravity_pull(pitch), horizontal[1]]
}

#[derive(Debug)]
pub struct ElytraFlyModule {
    pub module: ModuleData,
}

impl ElytraFlyModule {
    pub fn new() -> Self {
        let slider = |name: &str, value: f32, min: f32, max: f32| ModuleSetting::Slider {
            name: name.to_string(),
            value,
            min,
            max,
        };
        Self {
            module: ModuleData {
                id: ModuleId::ElytraFly,
                description: "Steer elytra flight with WASD, Jump and Sneak".to_string(),
                category: ModuleCategory::Movement,
                key_bind: KeyboardKey::KeyNone,
                enabled: false,
                settings: vec![
                    slider("Horizontal speed", 1.0, 0.1, 5.0),
                    slider("Vertical speed", 0.5, 0.1, 3.0),
                    ModuleSetting::Toggle {
                        name: "Hover without keys".to_string(),
                        value: true,
                    },
                ],
            },
        }
    }

    fn slider(&self, name: &str, fallback: f32) -> f64 {
        self.module
            .get_setting(name)
            .and_then(ModuleSetting::get_slider_value)
            .unwrap_or(fallback) as f64
    }
}

impl Module for ElytraFlyModule {
    fn on_start(&self) -> anyhow::Result<()> {
        Ok(())
    }

    fn on_stop(&self) -> anyhow::Result<()> {
        Ok(())
    }

    fn on_tick(&self) -> anyhow::Result<()> {
        let Some(player) = minecraft().player()? else {
            return Ok(());
        };
        if !player.call_method("isFallFlying", &[])?.z()? {
            return Ok(());
        }
        let (strafe, forward) = player.move_input()?;
        let jumping = player.is_jumping()?;
        let sneaking = player.is_shift_key_down()?;
        let hover = self
            .module
            .get_setting("Hover without keys")
            .and_then(ModuleSetting::get_toggle_value)
            .unwrap_or(true);
        let any_key = strafe != 0.0 || forward != 0.0 || jumping || sneaking;
        if !any_key && !hover {
            return Ok(()); // glide normally
        }

        let speed = self.slider("Horizontal speed", 1.0);
        let horizontal = input_direction(player.get_yaw()?, strafe, forward)
            .map(|[x, z]| [x * speed, z * speed])
            .unwrap_or([0.0, 0.0]);
        let climb = self.slider("Vertical speed", 0.5);
        let vertical = match (jumping, sneaking) {
            (true, false) => climb,
            (false, true) => -climb,
            _ => 0.0,
        };
        let [x, y, z] = target_velocity(horizontal, vertical, player.get_pitch()?);
        player.set_delta_movement(x, y, z)
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
    fn hovering_cancels_the_next_ticks_gravity() {
        // Looking level, the elytra pulls by a quarter of gravity.
        assert!((gravity_pull(0.0) - 0.02).abs() < 1e-9);
        // Straight down: full gravity.
        assert!((gravity_pull(90.0) - 0.08).abs() < 1e-9);
        let [_, y, _] = target_velocity([0.0, 0.0], 0.0, 0.0);
        // Next tick: (y - pull) * drag ≈ 0.
        assert!(((y - gravity_pull(0.0)) * DRAG_Y).abs() < 1e-9);
    }

    #[test]
    fn climbing_reaches_the_requested_speed() {
        let [_, y, _] = target_velocity([0.0, 0.0], 0.5, 0.0);
        assert!((((y - gravity_pull(0.0)) * DRAG_Y) - 0.5).abs() < 1e-9);
    }
}
