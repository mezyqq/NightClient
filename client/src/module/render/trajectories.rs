//! Trajectories — draws where a thrown / shot projectile will fly and land,
//! for the item in hand: ender pearls (on by default), and optionally
//! snowballs / eggs, splash potions and experience bottles, bows (while
//! drawn) and tridents (while charged).
//!
//! The flight is simulated in Rust with the game's own constants (launch
//! speed, gravity, air drag); blocks along the way are looked up through JNI,
//! cached per computation, until one blocks motion. The path is recomputed a
//! few times a second; [`crate::graphic::esp`] draws it.

use crate::mapping::math::BlockPos;
use crate::mapping::raw;
use crate::mapping::MappedObject;
use crate::module::player::hands;
use crate::module::{KeyboardKey, Module, ModuleCategory, ModuleData, ModuleId, ModuleSetting};
use crate::state::{mapping, minecraft};
use jni::objects::{JObject, JValue};
use jni::JNIEnv;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How often the path is recomputed.
const REFRESH: Duration = Duration::from_millis(50);
/// Longest simulated flight, in ticks.
const MAX_TICKS: usize = 300;
/// Spacing of the block checks along a tick's movement.
const SAMPLE: f64 = 0.25;

/// Which projectile kinds to show — read from the module's toggles.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Options {
    pub pearls: bool,
    pub throwables: bool,
    pub potions: bool,
    pub bows: bool,
    pub tridents: bool,
}

impl Options {
    pub fn from_module(data: &ModuleData) -> Options {
        let on = |name: &str, fallback: bool| {
            data.get_setting(name)
                .and_then(ModuleSetting::get_toggle_value)
                .unwrap_or(fallback)
        };
        Options {
            pearls: on("Pearls", true),
            throwables: on("Snowballs & eggs", false),
            potions: on("Potions & XP bottles", false),
            bows: on("Bows", false),
            tridents: on("Tridents", false),
        }
    }
}

/// A projectile's launch and flight constants.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Ballistics {
    speed: f64,
    /// Extra pitch the game adds when throwing (potions are lobbed upward).
    pitch_offset: f64,
    gravity: f64,
    drag: f64,
}

/// A computed flight path in world coordinates.
#[derive(Debug, Clone)]
pub struct Path {
    pub points: Vec<[f64; 3]>,
    /// Whether the flight ends on a block (vs. running out of time).
    pub hit: bool,
}

#[derive(Debug)]
pub struct TrajectoriesModule {
    pub module: ModuleData,
}

impl TrajectoriesModule {
    pub fn new() -> Self {
        let toggle = |name: &str, value: bool| ModuleSetting::Toggle {
            name: name.to_string(),
            value,
        };
        Self {
            module: ModuleData {
                id: ModuleId::Trajectories,
                description: "Shows where a pearl / projectile will land".to_string(),
                category: ModuleCategory::Render,
                key_bind: KeyboardKey::KeyNone,
                enabled: false,
                settings: vec![
                    ModuleSetting::Color {
                        name: "Color".to_string(),
                        value: [0.75, 0.35, 1.0, 1.0],
                    },
                    toggle("Pearls", true),
                    toggle("Snowballs & eggs", false),
                    toggle("Potions & XP bottles", false),
                    toggle("Bows", false),
                    toggle("Tridents", false),
                ],
            },
        }
    }
}

impl Module for TrajectoriesModule {
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

// --- cached computation --------------------------------------------------------

struct Cache {
    at: Option<Instant>,
    path: Option<Arc<Path>>,
}

static CACHE: Mutex<Cache> = Mutex::new(Cache { at: None, path: None });

/// The last outcome written to the log — see [`report`].
static STATUS: Mutex<std::collections::BTreeMap<&'static str, String>> = Mutex::new(std::collections::BTreeMap::new());

/// Logs why a path is (or is not) shown, once per change — the trail to
/// follow when nothing appears in game.
pub fn report(stage: &'static str, status: String) {
    let mut last = STATUS.lock().unwrap_or_else(|p| p.into_inner());
    if last.get(stage) != Some(&status) {
        log::info!("Trajectories ({stage}): {status}");
        last.insert(stage, status);
    }
}

/// The current path for the held item, recomputed at most every [`REFRESH`].
pub fn current(options: Options) -> Option<Arc<Path>> {
    let mut cache = CACHE.lock().unwrap_or_else(|p| p.into_inner());
    let now = Instant::now();
    if cache.at.is_none_or(|at| now - at >= REFRESH) {
        cache.at = Some(now);
        cache.path = match compute(options) {
            Ok(path) => {
                if let Some(path) = &path {
                    report("compute", format!(
                        "path computed, {}",
                        if path.hit { "lands on a block" } else { "flies off" },
                    ));
                }
                path.map(Arc::new)
            }
            Err(error) => {
                report("compute", format!("failed: {error:#}"));
                None
            }
        };
    }
    cache.path.clone()
}

fn compute(options: Options) -> anyhow::Result<Option<Path>> {
    let mc = minecraft();
    let (Some(player), Some(world)) = (mc.player()?, mc.world()?) else {
        report("compute", "no player or level".to_string());
        return Ok(None);
    };
    mapping().in_frame(|| {
        let mut env = mapping().get_env()?;
        let player = player.jni_ref().as_obj();
        let Some(ballistics) = held_ballistics(&mut env, player, options)? else {
            return Ok(None);
        };

        let read_vec = |env: &mut JNIEnv, name: &str| -> anyhow::Result<[f64; 3]> {
            let vec = raw::call(env, player, name, "()Lnet/minecraft/world/phys/Vec3;", &[])?.l()?;
            let mut out = [0.0; 3];
            for (slot, field) in out.iter_mut().zip(["x", "y", "z"]) {
                *slot = raw::field(env, &vec, field, "D")?.d()?;
            }
            Ok(out)
        };
        let feet = read_vec(&mut env, "position")?;
        let motion = read_vec(&mut env, "getDeltaMovement")?;
        let eye_height = raw::call(&mut env, player, "getEyeHeight", "()F", &[])?.f()? as f64;
        let yaw = raw::call(&mut env, player, "getYRot", "()F", &[])?.f()? as f64;
        let pitch = raw::call(&mut env, player, "getXRot", "()F", &[])?.f()? as f64;
        let on_ground = raw::call(&mut env, player, "onGround", "()Z", &[])?.z()?;

        // Projectiles spawn at eye height minus 0.1 and inherit the
        // shooter's motion (vertical only while airborne).
        let start = [feet[0], feet[1] + eye_height - 0.1, feet[2]];
        let mut velocity = launch_velocity(yaw, pitch, ballistics);
        velocity[0] += motion[0];
        velocity[2] += motion[2];
        if !on_ground {
            velocity[1] += motion[1];
        }

        let level = world.jni_ref().as_obj();
        let mut solid_cache: HashMap<[i32; 3], bool> = HashMap::new();
        let mut is_solid = |block: [i32; 3]| -> bool {
            *solid_cache.entry(block).or_insert_with(|| {
                blocks_motion(&mut env, level, block).unwrap_or(false)
            })
        };
        Ok(Some(simulate(start, velocity, ballistics, &mut is_solid)))
    })
}

/// The ballistics of the item in either hand, if it is a shown projectile.
fn held_ballistics(env: &mut JNIEnv, player: &JObject, options: Options) -> anyhow::Result<Option<Ballistics>> {
    let mut held = Vec::new();
    for hand in ["getMainHandItem", "getOffhandItem"] {
        let stack = raw::call(env, player, hand, &format!("(){}", hands::STACK_SIG), &[])?.l()?;
        if raw::call(env, &stack, "isEmpty", "()Z", &[])?.z()? {
            continue;
        }
        let id = hands::item_id(env, &stack)?;
        held.push(id.clone());
        let ticks_using = |env: &mut JNIEnv| -> anyhow::Result<i32> {
            if !raw::call(env, player, "isUsingItem", "()Z", &[])?.z()? {
                return Ok(0);
            }
            Ok(raw::call(env, player, "getTicksUsingItem", "()I", &[])?.i()?)
        };
        let ballistics = match id.as_str() {
            "minecraft:ender_pearl" if options.pearls => Some(throwable(1.5, 0.0, 0.03)),
            "minecraft:snowball" | "minecraft:egg" | "minecraft:blue_egg" | "minecraft:brown_egg"
                if options.throwables =>
            {
                Some(throwable(1.5, 0.0, 0.03))
            }
            "minecraft:splash_potion" | "minecraft:lingering_potion" if options.potions => {
                Some(throwable(0.5, -20.0, 0.05))
            }
            "minecraft:experience_bottle" if options.potions => Some(throwable(0.7, -20.0, 0.07)),
            "minecraft:bow" if options.bows => {
                let power = bow_power(ticks_using(env)?);
                (power >= 0.1).then(|| throwable(3.0 * power, 0.0, 0.05))
            }
            "minecraft:trident" if options.tridents => {
                (ticks_using(env)? >= 10).then(|| throwable(2.5, 0.0, 0.05))
            }
            _ => None,
        };
        if ballistics.is_some() {
            return Ok(ballistics);
        }
    }
    report("compute", format!("nothing to show for held items {held:?} with {options:?}"));
    Ok(None)
}

fn throwable(speed: f64, pitch_offset: f64, gravity: f64) -> Ballistics {
    Ballistics {
        speed,
        pitch_offset,
        gravity,
        drag: 0.99,
    }
}

/// `BowItem.getPowerForTime`: draw strength 0..1 after `ticks` of drawing.
fn bow_power(ticks: i32) -> f64 {
    let t = ticks as f64 / 20.0;
    ((t * t + t * 2.0) / 3.0).min(1.0)
}

/// `Projectile.shootFromRotation` without inaccuracy.
fn launch_velocity(yaw: f64, pitch: f64, b: Ballistics) -> [f64; 3] {
    let rad = std::f64::consts::PI / 180.0;
    let dir = [
        -(yaw * rad).sin() * (pitch * rad).cos(),
        -((pitch + b.pitch_offset) * rad).sin(),
        (yaw * rad).cos() * (pitch * rad).cos(),
    ];
    let len = (dir[0] * dir[0] + dir[1] * dir[1] + dir[2] * dir[2]).sqrt();
    dir.map(|c| c / len * b.speed)
}

/// Steps the flight tick by tick (move, then drag, then gravity — the
/// game's order) until a block that blocks motion is reached.
fn simulate(
    start: [f64; 3],
    mut velocity: [f64; 3],
    b: Ballistics,
    is_solid: &mut impl FnMut([i32; 3]) -> bool,
) -> Path {
    let mut points = vec![start];
    let mut pos = start;
    for _ in 0..MAX_TICKS {
        let step = (velocity[0].powi(2) + velocity[1].powi(2) + velocity[2].powi(2)).sqrt();
        let samples = ((step / SAMPLE).ceil() as usize).max(1);
        for i in 1..=samples {
            let t = i as f64 / samples as f64;
            let p = [
                pos[0] + velocity[0] * t,
                pos[1] + velocity[1] * t,
                pos[2] + velocity[2] * t,
            ];
            if is_solid(p.map(|c| c.floor() as i32)) {
                points.push(p);
                return Path { points, hit: true };
            }
        }
        pos = [pos[0] + velocity[0], pos[1] + velocity[1], pos[2] + velocity[2]];
        points.push(pos);
        velocity = velocity.map(|c| c * b.drag);
        velocity[1] -= b.gravity;
        if pos[1] < -128.0 {
            break;
        }
    }
    Path { points, hit: false }
}

/// Whether the block at `pos` stops a projectile (`BlockState.blocksMotion`).
fn blocks_motion(env: &mut JNIEnv, level: &JObject, [x, y, z]: [i32; 3]) -> anyhow::Result<bool> {
    env.with_local_frame(4, |env| -> anyhow::Result<bool> {
        let pos = BlockPos::new(x, y, z).to_java(env)?;
        let state = raw::call(
            env,
            level,
            "getBlockState",
            "(Lnet/minecraft/core/BlockPos;)Lnet/minecraft/world/level/block/state/BlockState;",
            &[JValue::Object(&pos)],
        )?
        .l()?;
        Ok(raw::call(env, &state, "blocksMotion", "()Z", &[])?.z()?)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_bow_draw_is_power_one() {
        assert_eq!(bow_power(20), 1.0);
        assert_eq!(bow_power(100), 1.0);
        assert!(bow_power(2) < 0.2);
    }

    #[test]
    fn looking_straight_ahead_launches_along_plus_z() {
        let v = launch_velocity(0.0, 0.0, throwable(1.5, 0.0, 0.03));
        assert!(v[0].abs() < 1e-9 && v[1].abs() < 1e-9);
        assert!((v[2] - 1.5).abs() < 1e-9);
    }

    #[test]
    fn a_pearl_thrown_flat_lands_on_the_floor_ahead() {
        // Floor: every block below y = 0 is solid.
        let mut floor = |b: [i32; 3]| b[1] < 0;
        let start = [0.5, 1.52, 0.5];
        let path = simulate(start, launch_velocity(0.0, 0.0, throwable(1.5, 0.0, 0.03)), throwable(1.5, 0.0, 0.03), &mut floor);
        assert!(path.hit);
        let end = path.points.last().unwrap();
        assert!(end[1] < 0.0 && end[1] > -1.0);
        assert!(end[2] > 5.0, "lands ahead, got z = {}", end[2]);
    }
}
