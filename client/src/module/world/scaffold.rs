use crate::mapping::block::Direction;
use crate::mapping::entity::EntityRef;
use crate::mapping::math::{BlockPos, Vec3};
use crate::mapping::entity::player::LocalPlayer;
use crate::mapping::{raw, MappedObject};
use crate::module::player::hands;
use crate::module::{KeyboardKey, Module, ModuleCategory, ModuleData, ModuleId, ModuleSetting};
use crate::state::{mapping, minecraft};

/// Horizontal speed below which the player counts as standing still.
const MOVING_THRESHOLD: f64 = 0.01;

/// Bridges automatically: extends a line of blocks ahead of the player in the
/// direction they are moving, so they can keep walking over a gap. The held
/// item is what gets placed, so the player should be holding blocks — in the
/// main hand, or in the off hand with "Off hand" on.
#[derive(Debug)]
pub struct ScaffoldModule {
    pub module: ModuleData,
}

impl ScaffoldModule {
    pub fn new() -> Self {
        Self {
            module: ModuleData {
                id: ModuleId::Scaffold,
                description: "Bridges blocks ahead of the player".to_string(),
                category: ModuleCategory::World,
                key_bind: KeyboardKey::KeyNone,
                enabled: false,
                settings: vec![ModuleSetting::Toggle {
                    name: "Off hand".to_string(),
                    value: true,
                }],
            },
        }
    }
}

impl Module for ScaffoldModule {
    fn on_start(&self) -> anyhow::Result<()> {
        Ok(())
    }

    fn on_stop(&self) -> anyhow::Result<()> {
        Ok(())
    }

    fn on_tick(&self) -> anyhow::Result<()> {
        let minecraft = minecraft();
        // Stand down while a menu is open.
        if minecraft.current_screen().is_open() {
            return Ok(());
        }
        let (Some(player), Some(world), Some(game_mode)) = (
            minecraft.player()?,
            minecraft.world()?,
            minecraft.game_mode()?,
        ) else {
            return Ok(());
        };

        let feet = player.get_position()?;
        let support = BlockPos::new(
            feet.x().floor() as i32,
            feet.y().round() as i32 - 1,
            feet.z().floor() as i32,
        );

        // The block to place: the support under the feet if it has gone
        // missing, otherwise the next block ahead in the movement direction —
        // placed *before* the player reaches it, so the bridge is always ready.
        let target = if world.is_block_air(&support)? {
            support
        } else {
            let motion = player.get_delta_movement()?;
            let Some((dx, dz)) = dominant_step(motion.x(), motion.z()) else {
                return Ok(()); // standing still and supported — nothing to do
            };
            support.offset(dx, 0, dz)
        };

        // Already a block there — nothing to place.
        if !world.is_block_air(&target)? {
            return Ok(());
        }

        // Place against the first solid neighbour found.
        for direction in Direction::ALL {
            let (dx, dy, dz) = direction.offset();
            let anchor = target.offset(dx, dy, dz);
            if world.is_block_air(&anchor)? {
                continue; // need a solid block to place against
            }
            // The anchor face pointing back at the target is where the new
            // block attaches.
            let face = direction.opposite();
            let off_hand = self
                .module
                .get_setting("Off hand")
                .and_then(ModuleSetting::get_toggle_value)
                .unwrap_or(false);
            let hand = if off_hand && !holds_block(&player, "getMainHandItem")? && holds_block(&player, "getOffhandItem")? {
                "OFF_HAND"
            } else {
                "MAIN_HAND"
            };
            return game_mode.place_block_on(&player, hand, &anchor, face, face_center(anchor, face));
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

/// Whether the stack from `getter` (`getMainHandItem` / `getOffhandItem`) is a
/// placeable block.
fn holds_block(player: &LocalPlayer, getter: &str) -> anyhow::Result<bool> {
    mapping().in_frame(|| {
        let mut env = mapping().get_env()?;
        let stack = raw::call(&mut env, player.jni_ref().as_obj(), getter, &format!("(){}", hands::STACK_SIG), &[])?.l()?;
        let item = raw::call(&mut env, &stack, "getItem", "()Lnet/minecraft/world/item/Item;", &[])?.l()?;
        let block_item = mapping().resolve_class(&mut env, "net/minecraft/world/item/BlockItem")?;
        Ok(env.is_instance_of(&item, &block_item)?)
    })
}

/// The unit step `(dx, dz)` along the dominant horizontal movement axis, or
/// `None` when the player is not moving.
fn dominant_step(vx: f64, vz: f64) -> Option<(i32, i32)> {
    if vx.abs() < MOVING_THRESHOLD && vz.abs() < MOVING_THRESHOLD {
        return None;
    }
    if vx.abs() >= vz.abs() {
        Some((vx.signum() as i32, 0))
    } else {
        Some((0, vz.signum() as i32))
    }
}

/// The world point at the centre of the `face` face of block `anchor`.
fn face_center(anchor: BlockPos, face: Direction) -> Vec3 {
    let (fx, fy, fz) = face.offset();
    Vec3::new(
        anchor.x() as f64 + 0.5 + fx as f64 * 0.5,
        anchor.y() as f64 + 0.5 + fy as f64 * 0.5,
        anchor.z() as f64 + 0.5 + fz as f64 * 0.5,
    )
}
