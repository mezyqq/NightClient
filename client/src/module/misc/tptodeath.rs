//! TP to Death — moves the player to where they last died.
//!
//! The death spot comes from the game itself (`Player.getLastDeathLocation`,
//! the same record the recovery compass uses), which the server sends to the
//! client — so it is known even after a relog or a dimension change.
//!
//! Two ways of getting there, picked automatically:
//!
//! * **Singleplayer / LAN host** — the integrated server teleports its own
//!   player (`ServerPlayer.teleportTo`), on the server thread. Works across
//!   dimensions; switching the module on does it instantly.
//! * **Remote server** — a sky flight: up above the build limit, across in
//!   steps of at most [`STEP`] blocks, then down onto the spot, a few steps a
//!   second. Each step is a short legal move as far as the server's speed
//!   check goes, and every movement packet claims to be on the ground, so the
//!   descent deals no fall damage and the server's anti-fly kick never counts
//!   up. Only the current dimension: on a remote server, only the server can
//!   move a player between dimensions. The module stays on while flying and
//!   switches itself off on arrival; switching it off cancels the flight.

use crate::graphic::notification::{Notification, NotificationType};
use crate::mapping::entity::player::LocalPlayer;
use crate::mapping::entity::EntityRef;
use crate::mapping::raw;
use crate::mapping::{FieldType, MappedObject};
use crate::module::player::godmode;
use crate::module::registry::request_disable;
use crate::module::{KeyboardKey, Module, ModuleCategory, ModuleData, ModuleId, ModuleSetting};
use crate::net::packet::{Packet, PacketAction};
use crate::state::{mapping, minecraft};
use jni::objects::{JClass, JObject, JObjectArray, JValue};
use jni::JNIEnv;
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Longest single step, in blocks. The vanilla speed check rejects a move of
/// more than 10 blocks per packet; this stays just under.
const STEP: f64 = 9.9;
/// How far the player may drift from the current waypoint before the flight
/// counts as pushed back by the server.
const PUSHBACK_TOLERANCE: f64 = 3.0;

/// Where the player last died.
#[derive(Debug, Clone, PartialEq)]
struct DeathSpot {
    /// Dimension id, e.g. `minecraft:the_nether`.
    dimension: String,
    pos: [i32; 3],
}

/// An ongoing sky flight.
#[derive(Debug)]
struct Flight {
    /// Points still to visit, in order; the last one is the death spot.
    waypoints: VecDeque<[f64; 3]>,
    /// The point the player is held at until the next step.
    current: [f64; 3],
    last_step: Instant,
    /// Abilities before the flight, restored afterwards.
    was_flying: bool,
    could_fly: bool,
    spot: DeathSpot,
}

#[derive(Debug)]
pub struct TpToDeathModule {
    pub module: ModuleData,
    flight: Mutex<Option<Flight>>,
}

impl TpToDeathModule {
    pub fn new() -> Self {
        Self {
            module: ModuleData {
                id: ModuleId::TpToDeath,
                description: "Takes you to your last death spot".to_string(),
                category: ModuleCategory::Misc,
                key_bind: KeyboardKey::KeyNone,
                enabled: false,
                settings: vec![ModuleSetting::Slider {
                    name: "Steps/sec".to_string(),
                    value: 5.0,
                    min: 1.0,
                    max: 10.0,
                }],
            },
            flight: Mutex::new(None),
        }
    }

    fn step_interval(&self) -> Duration {
        let rate = self
            .module
            .get_setting("Steps/sec")
            .and_then(ModuleSetting::get_slider_value)
            .unwrap_or(5.0)
            .clamp(1.0, 10.0);
        Duration::from_secs_f32(1.0 / rate)
    }

    /// Ends the flight (if any), restoring the player's abilities.
    fn end_flight(&self) -> Option<Flight> {
        let flight = self.flight.lock().unwrap().take()?;
        if let Ok(Some(player)) = minecraft().player() {
            let abilities = &player.abilities;
            let _ = abilities.set_field("flying", FieldType::Boolean, JValue::Bool(flight.was_flying as u8));
            let _ = abilities.set_field("mayfly", FieldType::Boolean, JValue::Bool(flight.could_fly as u8));
            let _ = player.set_delta_movement(0.0, 0.0, 0.0);
        }
        Some(flight)
    }
}

fn notify(kind: NotificationType, message: &str) {
    Notification::send(kind, "TP to Death", message);
}

fn describe(spot: &DeathSpot) -> String {
    let [x, y, z] = spot.pos;
    let dimension = spot.dimension.strip_prefix("minecraft:").unwrap_or(&spot.dimension);
    format!("{x} {y} {z} ({dimension})")
}

impl Module for TpToDeathModule {
    fn on_start(&self) -> anyhow::Result<()> {
        match self.start() {
            Ok(()) => {}
            Err(error) => {
                notify(NotificationType::Warning, &error.to_string());
                request_disable(ModuleId::TpToDeath);
            }
        }
        Ok(())
    }

    fn on_stop(&self) -> anyhow::Result<()> {
        if let Some(flight) = self.end_flight() {
            if !flight.waypoints.is_empty() {
                notify(NotificationType::Info, "Flight cancelled");
            }
        }
        Ok(())
    }

    fn on_tick(&self) -> anyhow::Result<()> {
        let Some(player) = minecraft().player()? else {
            return Ok(());
        };
        let mut guard = self.flight.lock().unwrap();
        let Some(flight) = guard.as_mut() else {
            return Ok(());
        };

        // The server answers an illegal move by teleporting us back; seeing
        // the player far from where we hold them means the flight failed.
        let pos = player.get_position()?;
        let drift = distance([pos.x(), pos.y(), pos.z()], flight.current);
        if drift > PUSHBACK_TOLERANCE {
            drop(guard);
            self.end_flight();
            notify(NotificationType::Warning, "The server pushed you back — path blocked?");
            request_disable(ModuleId::TpToDeath);
            return Ok(());
        }

        if flight.last_step.elapsed() >= self.step_interval() {
            match flight.waypoints.pop_front() {
                Some(next) => {
                    flight.current = next;
                    flight.last_step = Instant::now();
                }
                None => {
                    let spot = flight.spot.clone();
                    drop(guard);
                    self.end_flight();
                    notify(NotificationType::Info, &format!("Arrived: {}", describe(&spot)));
                    request_disable(ModuleId::TpToDeath);
                    return Ok(());
                }
            }
        }

        // Hold the player at the current waypoint: no drift, no gravity. The
        // client then reports this position in its own movement packet.
        let [x, y, z] = flight.current;
        player.set_pos(x, y, z)?;
        player.set_delta_movement(0.0, 0.0, 0.0)?;
        Ok(())
    }

    fn handle_packet(&self, packet: &mut Packet) -> PacketAction {
        // While flying, every movement packet claims ground contact: no fall
        // damage on the way down, and the anti-fly counter never runs.
        if let Packet::ServerboundMovePlayer(move_packet) = packet {
            if self.flight.lock().is_ok_and(|flight| flight.is_some()) {
                move_packet.on_ground = true;
            }
        }
        PacketAction::Forward
    }

    fn get_module_data(&self) -> &ModuleData {
        &self.module
    }

    fn get_module_data_mut(&mut self) -> &mut ModuleData {
        &mut self.module
    }
}

impl TpToDeathModule {
    /// Starts the trip: instant in singleplayer, a flight elsewhere.
    fn start(&self) -> anyhow::Result<()> {
        if self.flight.lock().unwrap().is_some() {
            return Ok(()); // already on the way
        }
        let Some(player) = minecraft().player()? else {
            anyhow::bail!("Not in a world");
        };

        if let Some(server_player) = godmode::server_player()? {
            let spot = server_teleport(&player, server_player.as_obj())?;
            notify(NotificationType::Info, &describe(&spot));
            request_disable(ModuleId::TpToDeath);
            return Ok(());
        }

        let (spot, here, cruise) = mapping().in_frame(|| {
            let mut env = mapping().get_env()?;
            let (spot, _) = last_death(&mut env, player.jni_ref().as_obj())?
                .ok_or_else(|| anyhow::anyhow!("No death recorded yet"))?;
            let world = minecraft()
                .world()?
                .ok_or_else(|| anyhow::anyhow!("Not in a world"))?;
            let level = world.jni_ref().as_obj();
            let key = raw::call(&mut env, level, "dimension", "()Lnet/minecraft/resources/ResourceKey;", &[])?.l()?;
            let here = dimension_id(&raw::to_string(&mut env, &key)?).unwrap_or_default();
            // The Nether has a bedrock roof: no flying above it.
            let cruise = if here == "minecraft:the_nether" {
                None
            } else {
                Some(max_build_height(&mut env, level)? as f64 + 1.0)
            };
            Ok((spot, here, cruise))
        })?;
        if spot.dimension != here {
            anyhow::bail!(
                "Died in {} — other dimensions work only in singleplayer / LAN host",
                spot.dimension.strip_prefix("minecraft:").unwrap_or(&spot.dimension)
            );
        }

        let pos = player.get_position()?;
        let from = [pos.x(), pos.y(), pos.z()];
        let to = [spot.pos[0] as f64 + 0.5, spot.pos[1] as f64, spot.pos[2] as f64 + 0.5];
        let waypoints = route(from, to, cruise, STEP);

        let abilities = &player.abilities;
        let was_flying = abilities.is_flying()?;
        let could_fly = abilities.get_may_fly()?;
        // Client-side flight only removes gravity between steps; the server
        // ignores the flag for a player who may not fly.
        abilities.fly(true)?;

        let seconds = waypoints.len() as f32 * self.step_interval().as_secs_f32();
        notify(
            NotificationType::Info,
            &format!("Flying to {} — ~{seconds:.0}s", describe(&spot)),
        );
        *self.flight.lock().unwrap() = Some(Flight {
            waypoints: waypoints.into(),
            current: from,
            last_step: Instant::now(),
            was_flying,
            could_fly,
            spot,
        });
        Ok(())
    }
}

// --- the death spot ----------------------------------------------------------

/// Reads `Player.getLastDeathLocation()` — an `Optional<GlobalPos>` — as the
/// spot plus the dimension's `ResourceKey` (a local reference).
fn last_death<'local>(
    env: &mut JNIEnv<'local>,
    player: &JObject,
) -> anyhow::Result<Option<(DeathSpot, JObject<'local>)>> {
    let optional = raw::call(env, player, "getLastDeathLocation", "()Ljava/util/Optional;", &[])?.l()?;
    if !raw::call(env, &optional, "isPresent", "()Z", &[])?.z()? {
        return Ok(None);
    }
    let global_pos = raw::call(env, &optional, "get", "()Ljava/lang/Object;", &[])?.l()?;
    let key = raw::call(env, &global_pos, "dimension", "()Lnet/minecraft/resources/ResourceKey;", &[])?.l()?;
    let block_pos = raw::call(env, &global_pos, "pos", "()Lnet/minecraft/core/BlockPos;", &[])?.l()?;
    let mut pos = [0; 3];
    for (slot, name) in pos.iter_mut().zip(["getX", "getY", "getZ"]) {
        *slot = raw::call(env, &block_pos, name, "()I", &[])?.i()?;
    }
    let dimension = dimension_id(&raw::to_string(env, &key)?)
        .ok_or_else(|| anyhow::anyhow!("unreadable dimension key"))?;
    Ok(Some((DeathSpot { dimension, pos }, key)))
}

/// `ResourceKey[minecraft:dimension / minecraft:the_nether]` →
/// `minecraft:the_nether`. Parsed from `toString` because the accessor
/// changed name across versions (`location()` → `identifier()`).
fn dimension_id(key: &str) -> Option<String> {
    let start = key.find(" / ")? + 3;
    let end = start + key[start..].find(']')?;
    Some(key[start..end].to_string())
}

/// The world's top block height: `getMaxY` from 1.21.2 on,
/// `getMaxBuildHeight` (one above the top block) before.
fn max_build_height(env: &mut JNIEnv, level: &JObject) -> anyhow::Result<i32> {
    if let Ok(value) = raw::call(env, level, "getMaxY", "()I", &[]) {
        return Ok(value.i()?);
    }
    Ok(raw::call(env, level, "getMaxBuildHeight", "()I", &[])?.i()?)
}

// --- the sky flight -----------------------------------------------------------

fn distance(a: [f64; 3], b: [f64; 3]) -> f64 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

/// Waypoints from `from` to `to`, at most `step` apart: straight up to
/// `cruise`, across, then straight down — or one straight line without a
/// cruise height. `from` itself is not included; `to` is the last point.
fn route(from: [f64; 3], to: [f64; 3], cruise: Option<f64>, step: f64) -> Vec<[f64; 3]> {
    let mut corners = Vec::new();
    if let Some(cruise) = cruise {
        let high = cruise.max(from[1]).max(to[1]);
        corners.push([from[0], high, from[2]]);
        corners.push([to[0], high, to[2]]);
    }
    corners.push(to);

    let mut points = Vec::new();
    let mut start = from;
    for corner in corners {
        let steps = (distance(start, corner) / step).ceil() as usize;
        for i in 1..=steps {
            let t = i as f64 / steps as f64;
            points.push([
                start[0] + (corner[0] - start[0]) * t,
                start[1] + (corner[1] - start[1]) * t,
                start[2] + (corner[2] - start[2]) * t,
            ]);
        }
        start = corner;
    }
    if points.last() != Some(&to) {
        points.push(to);
    }
    points
}

// --- singleplayer: the server teleports its own player ---------------------

/// Teleports the integrated server's player to the death spot, any dimension.
fn server_teleport(local: &LocalPlayer, server_player: &JObject) -> anyhow::Result<DeathSpot> {
    mapping().in_frame(|| {
        let mut env = mapping().get_env()?;
        let (spot, key) = last_death(&mut env, local.jni_ref().as_obj())?
            .ok_or_else(|| anyhow::anyhow!("No death recorded yet"))?;
        let server = raw::call(
            &mut env,
            minecraft().jni_ref().as_obj(),
            "getSingleplayerServer",
            "()Lnet/minecraft/client/server/IntegratedServer;",
            &[],
        )?
        .l()?;
        let level = raw::call(
            &mut env,
            &server,
            "getLevel",
            "(Lnet/minecraft/resources/ResourceKey;)Lnet/minecraft/server/level/ServerLevel;",
            &[JValue::Object(&key)],
        )?
        .l()?;
        if level.is_null() {
            anyhow::bail!("That dimension is not loaded");
        }
        let target = [spot.pos[0] as f64 + 0.5, spot.pos[1] as f64, spot.pos[2] as f64 + 0.5];
        let yaw = raw::call(&mut env, server_player, "getYRot", "()F", &[])?.f()?;
        let pitch = raw::call(&mut env, server_player, "getXRot", "()F", &[])?.f()?;

        let call = TeleportCall::find(&mut env, server_player, &level)?;
        // Run on the server thread; fall back to a direct call if a task
        // cannot be built on this JVM.
        match call.as_task(&mut env, server_player, &level, target, yaw, pitch) {
            Ok(task) => {
                raw::call(&mut env, &server, "execute", "(Ljava/lang/Runnable;)V", &[JValue::Object(&task)])?;
            }
            Err(error) => {
                log::warn!("TP to Death: server-thread task unavailable ({error}); calling directly");
                call.invoke(&mut env, server_player, &level, target, yaw, pitch)?;
            }
        }
        Ok(spot)
    })
}

/// The `ServerPlayer.teleportTo(ServerLevel, …)` overload of this version.
struct TeleportCall<'local> {
    method: JObject<'local>,
    /// 1.21.2+: `(ServerLevel, x, y, z, Set<Relative>, yaw, pitch, setCamera)`;
    /// before: `(ServerLevel, x, y, z, yaw, pitch)`.
    modern: bool,
}

impl<'local> TeleportCall<'local> {
    const MODERN_SIG: &'static str = "(Lnet/minecraft/server/level/ServerLevel;DDDLjava/util/Set;FFZ)Z";
    const LEGACY_SIG: &'static str = "(Lnet/minecraft/server/level/ServerLevel;DDDFF)V";

    fn find(env: &mut JNIEnv<'local>, player: &JObject, level: &JObject) -> anyhow::Result<Self> {
        let player_class = env.get_object_class(player)?;
        let level_class = env.get_object_class(level)?;
        let set_class: JObject = env.find_class("java/util/Set")?.into();
        let double = primitive(env, "java/lang/Double")?;
        let float = primitive(env, "java/lang/Float")?;
        let boolean = primitive(env, "java/lang/Boolean")?;

        let shapes: [(bool, Vec<&JObject>); 2] = [
            (
                true,
                vec![&level_class, &double, &double, &double, &set_class, &float, &float, &boolean],
            ),
            (false, vec![&level_class, &double, &double, &double, &float, &float]),
        ];
        let name: JObject = env.new_string("teleportTo")?.into();
        let class_class = env.find_class("java/lang/Class")?;
        for (modern, params) in shapes {
            let array = env.new_object_array(params.len() as i32, &class_class, JObject::null())?;
            for (i, param) in params.iter().enumerate() {
                env.set_object_array_element(&array, i as i32, *param)?;
            }
            let found = raw::call(
                env,
                &player_class,
                "getMethod",
                "(Ljava/lang/String;[Ljava/lang/Class;)Ljava/lang/reflect/Method;",
                &[JValue::Object(&name), JValue::Object(&array)],
            );
            if let Ok(method) = found {
                return Ok(TeleportCall { method: method.l()?, modern });
            }
        }
        anyhow::bail!("no known ServerPlayer.teleportTo overload")
    }

    /// The call's arguments, boxed, receiver first.
    fn boxed_args(
        &self,
        env: &mut JNIEnv<'local>,
        player: &JObject,
        level: &JObject,
        [x, y, z]: [f64; 3],
        yaw: f32,
        pitch: f32,
    ) -> anyhow::Result<JObjectArray<'local>> {
        let mut values: Vec<JObject> = vec![env.new_local_ref(player)?, env.new_local_ref(level)?];
        for coordinate in [x, y, z] {
            values.push(boxed(env, "java/lang/Double", "(D)Ljava/lang/Double;", JValue::Double(coordinate))?);
        }
        if self.modern {
            let empty = env
                .call_static_method("java/util/Collections", "emptySet", "()Ljava/util/Set;", &[])?
                .l()?;
            values.push(empty);
        }
        for angle in [yaw, pitch] {
            values.push(boxed(env, "java/lang/Float", "(F)Ljava/lang/Float;", JValue::Float(angle))?);
        }
        if self.modern {
            values.push(boxed(env, "java/lang/Boolean", "(Z)Ljava/lang/Boolean;", JValue::Bool(1))?);
        }
        let array = env.new_object_array(values.len() as i32, "java/lang/Object", JObject::null())?;
        for (i, value) in values.iter().enumerate() {
            env.set_object_array_element(&array, i as i32, value)?;
        }
        Ok(array)
    }

    /// A `Runnable` performing the teleport: the method as a `MethodHandle`
    /// with every argument bound, wrapped by `MethodHandleProxies`.
    fn as_task(
        &self,
        env: &mut JNIEnv<'local>,
        player: &JObject,
        level: &JObject,
        target: [f64; 3],
        yaw: f32,
        pitch: f32,
    ) -> anyhow::Result<JObject<'local>> {
        let lookup = static_call(
            env,
            "java/lang/invoke/MethodHandles",
            "publicLookup",
            "()Ljava/lang/invoke/MethodHandles$Lookup;",
            &[],
        )?;
        let handle = raw::call(
            env,
            &lookup,
            "unreflect",
            "(Ljava/lang/reflect/Method;)Ljava/lang/invoke/MethodHandle;",
            &[JValue::Object(&self.method)],
        )?
        .l()?;
        let args = self.boxed_args(env, player, level, target, yaw, pitch)?;
        let bound = static_call(
            env,
            "java/lang/invoke/MethodHandles",
            "insertArguments",
            "(Ljava/lang/invoke/MethodHandle;I[Ljava/lang/Object;)Ljava/lang/invoke/MethodHandle;",
            &[JValue::Object(&handle), JValue::Int(0), JValue::Object(&args)],
        )?;
        let runnable: JObject = env.find_class("java/lang/Runnable")?.into();
        static_call(
            env,
            "java/lang/invoke/MethodHandleProxies",
            "asInterfaceInstance",
            "(Ljava/lang/Class;Ljava/lang/invoke/MethodHandle;)Ljava/lang/Object;",
            &[JValue::Object(&runnable), JValue::Object(&bound)],
        )
    }

    /// Calls the method right here, on the calling thread.
    fn invoke(
        &self,
        env: &mut JNIEnv<'local>,
        player: &JObject,
        level: &JObject,
        [x, y, z]: [f64; 3],
        yaw: f32,
        pitch: f32,
    ) -> anyhow::Result<()> {
        let (x, y, z) = (JValue::Double(x), JValue::Double(y), JValue::Double(z));
        let (yaw, pitch) = (JValue::Float(yaw), JValue::Float(pitch));
        if self.modern {
            let empty = env
                .call_static_method("java/util/Collections", "emptySet", "()Ljava/util/Set;", &[])?
                .l()?;
            raw::call(
                env,
                player,
                "teleportTo",
                Self::MODERN_SIG,
                &[JValue::Object(level), x, y, z, JValue::Object(&empty), yaw, pitch, JValue::Bool(1)],
            )?;
        } else {
            raw::call(env, player, "teleportTo", Self::LEGACY_SIG, &[JValue::Object(level), x, y, z, yaw, pitch])?;
        }
        Ok(())
    }
}

/// The `Class` of a primitive, read from its wrapper's `TYPE` field.
fn primitive<'local>(env: &mut JNIEnv<'local>, wrapper: &str) -> anyhow::Result<JObject<'local>> {
    Ok(env.get_static_field(wrapper, "TYPE", "Ljava/lang/Class;")?.l()?)
}

fn boxed<'local>(
    env: &mut JNIEnv<'local>,
    class: &str,
    sig: &str,
    value: JValue,
) -> anyhow::Result<JObject<'local>> {
    Ok(env.call_static_method(class, "valueOf", sig, &[value])?.l()?)
}

/// A static call that clears a pending exception on failure.
fn static_call<'local>(
    env: &mut JNIEnv<'local>,
    class: &str,
    name: &str,
    sig: &str,
    args: &[JValue],
) -> anyhow::Result<JObject<'local>> {
    let class: JClass = env.find_class(class).inspect_err(|_| {
        let _ = env.exception_clear();
    })?;
    env.call_static_method(&class, name, sig, args)
        .and_then(|value| value.l())
        .map_err(|error| {
            let _ = env.exception_clear();
            anyhow::anyhow!("{name}{sig}: {error}")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dimension_is_read_from_a_resource_key() {
        assert_eq!(
            dimension_id("ResourceKey[minecraft:dimension / minecraft:the_nether]").as_deref(),
            Some("minecraft:the_nether")
        );
        assert_eq!(dimension_id("nonsense"), None);
    }

    #[test]
    fn sky_route_climbs_crosses_and_descends_in_short_steps() {
        let from = [0.0, 64.0, 0.0];
        let to = [100.5, 70.0, -30.5];
        let points = route(from, to, Some(320.0), 9.9);
        assert_eq!(points.last(), Some(&to));
        let mut previous = from;
        for point in &points {
            assert!(distance(previous, *point) <= 9.9 + 1e-9);
            previous = *point;
        }
        // Climbs straight up first, crosses at the cruise height.
        assert_eq!((points[0][0], points[0][2]), (0.0, 0.0));
        assert!(points.iter().any(|p| p[1] == 320.0 && p[0] == 100.5));
    }

    #[test]
    fn route_without_cruise_is_a_straight_line() {
        let points = route([0.0, 0.0, 0.0], [20.0, 0.0, 0.0], None, 9.9);
        assert_eq!(points.len(), 3);
        assert_eq!(points.last(), Some(&[20.0, 0.0, 0.0]));
    }
}
