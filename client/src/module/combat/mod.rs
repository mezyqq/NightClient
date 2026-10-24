pub mod aimbot;
pub mod antibot;
pub mod aura;
pub mod criticals;
pub mod killaura;
pub mod mobaura;
pub mod rotation;
pub mod targets;
pub mod triggerbot;
pub mod velocity;

use crate::mapping::entity::player::{LocalPlayer, Player};
use crate::mapping::entity::{Entity, EntityRef};
use crate::mapping::math::Vec3;
use crate::mapping::MappedObject;
use crate::module::misc::friends;
use rotation::Rotation;

/// Aims the camera at `target` — eased, through the shared rotation controller
/// — when the target is within `max_fov` degrees of the current look. Returns
/// the angle, in degrees, currently between the look direction and the target,
/// which combat modules use to decide when they are aligned enough to attack.
pub fn look_at(
    player: &LocalPlayer,
    target: &Entity,
    speed: f32,
    max_fov: f32,
) -> anyhow::Result<f32> {
    let eye = player.get_eye_position()?;
    let feet = target.get_position()?;
    let height = target.bb_height()? as f64;
    // Aim at the upper body — reliable hit registration, natural-looking.
    let aim = Vec3::new(feet.x(), feet.y() + height * 0.7, feet.z());
    let target_rotation = Rotation::towards(eye, aim);

    let current = Rotation::new(player.get_yaw()?, player.get_pitch()?);
    let angle = current.angle_to(target_rotation);
    if angle <= max_fov {
        rotation::aim(target_rotation, speed);
    }
    Ok(angle)
}

/// Picks a combat target from `entities`: keeps the `locked` target while it is
/// still a valid in-range candidate, otherwise the nearest one. `accept`
/// filters by kind; `exclude_id` drops the player's own entity. Returns the
/// chosen entity's network id together with the entity.
pub fn pick_target(
    entities: &[Entity],
    eye: Vec3,
    range_sq: f64,
    exclude_id: i32,
    locked: Option<i32>,
    accept: impl Fn(&Entity) -> bool,
) -> Option<(i32, Entity)> {
    pick_target_by(entities, eye, range_sq, exclude_id, locked, accept, |_, distance_sq| distance_sq)
}

/// [`pick_target`] with a custom priority: among the candidates (when the
/// locked one is gone) the one with the lowest `score(entity, distance_sq)`
/// wins.
pub fn pick_target_by(
    entities: &[Entity],
    eye: Vec3,
    range_sq: f64,
    exclude_id: i32,
    locked: Option<i32>,
    accept: impl Fn(&Entity) -> bool,
    score: impl Fn(&Entity, f64) -> f64,
) -> Option<(i32, Entity)> {
    let mut candidates: Vec<(i32, Entity, f64)> = Vec::new();
    for entity in entities {
        let distance = match entity.distance_to_sqr(eye.x(), eye.y(), eye.z()) {
            Ok(distance) if distance <= range_sq => distance,
            _ => continue,
        };
        if !accept(entity) {
            continue;
        }
        let Ok(id) = entity.id() else {
            continue;
        };
        if id == exclude_id {
            continue;
        }
        candidates.push((id, entity.clone(), distance));
    }

    // Keep the locked target while it is still in the candidate set.
    if let Some(locked) = locked {
        if let Some((id, entity, _)) = candidates.iter().find(|(id, _, _)| *id == locked) {
            return Some((*id, entity.clone()));
        }
    }
    candidates
        .into_iter()
        .map(|(id, entity, distance)| {
            let score = score(&entity, distance);
            (id, entity, score)
        })
        .min_by(|a, b| a.2.total_cmp(&b.2))
        .map(|(id, entity, _)| (id, entity))
}

/// Whether combat modules must leave `entity` alone: a player on the Friends
/// list, or — with AntiBot on — an anti-cheat bot.
pub fn is_protected(entity: &Entity) -> bool {
    if !entity.instance_of::<Player>() {
        return false;
    }
    let name = entity
        .get_name()
        .and_then(|component| component.get_string())
        .unwrap_or_default();
    friends::is_friend(&name) || antibot::is_bot(entity, &name)
}

/// The entity a combat module is fighting, and when it last said so.
static COMBAT_TARGET: std::sync::Mutex<Option<(i32, std::time::Instant)>> = std::sync::Mutex::new(None);

/// Marks entity `id` as the current combat target — read by TargetHUD and
/// the hit marker.
pub fn set_combat_target(id: i32) {
    *COMBAT_TARGET.lock().unwrap_or_else(|p| p.into_inner()) = Some((id, std::time::Instant::now()));
}

/// The combat target, if one was set within `max_age`.
pub fn combat_target(max_age: std::time::Duration) -> Option<i32> {
    COMBAT_TARGET
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .filter(|(_, at)| at.elapsed() <= max_age)
        .map(|(id, _)| id)
}
