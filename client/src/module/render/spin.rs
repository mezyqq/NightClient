//! Spin — spins the player around while the camera stays put.
//!
//! * **Visual** — the player model (body and head) spins in third person,
//!   for you only. A NeoForge `RenderLivingEvent.Pre` listener, built from
//!   method handles (see [`crate::module::optimization::java`]), writes the
//!   spin angle into the player's body / head rotation right before the model
//!   is drawn; the angle lives in a Java `float[1]` rewritten every frame, so
//!   no native code runs inside the render. NeoForge only.
//! * **Server** — other players see you spin: the yaw of every outgoing
//!   movement packet is replaced with the spin angle, and a rotation packet
//!   is sent each tick so the spin continues while you stand still. Your
//!   camera, movement and aim are untouched; blocks you place may face the
//!   spun direction (the server reads your facing from the last rotation it
//!   was sent). "Pause while using" sends your real rotation while the use or
//!   attack key is held. Anti-cheats may flag the rotation speed.

use crate::mapping::entity::player::LocalPlayer;
use crate::mapping::entity::EntityRef;
use crate::mapping::raw;
use crate::mapping::MappedObject;
use crate::module::optimization::java::{self, EventListeners};
use crate::module::{KeyboardKey, Module, ModuleCategory, ModuleData, ModuleId, ModuleSetting};
use crate::net::packet::serverbound_move_player::ServerboundMovePlayerPacket;
use crate::net::packet::{Packet, PacketAction};
use crate::state::{mapping, minecraft};
use jni::objects::{GlobalRef, JFloatArray, JObject, JValue};
use jni::JNIEnv;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

const MODES: [&str; 3] = ["Visual + server", "Visual only (F5)", "Server only"];
const DIRECTIONS: [&str; 2] = ["Right (clockwise)", "Left (counter-clockwise)"];

const EVENT: &str = "net/neoforged/neoforge/client/event/RenderLivingEvent$Pre";
const LIVING: &str = "net/minecraft/world/entity/LivingEntity";
/// The rotation fields the living renderer interpolates between.
const FIELDS: [&str; 4] = ["yBodyRot", "yBodyRotO", "yHeadRot", "yHeadRotO"];

// Settings, mirrored for the packet thread and the frame hook (neither may
// lock the module).
static ACTIVE: AtomicBool = AtomicBool::new(false);
static MODE: AtomicU8 = AtomicU8::new(0);
static LEFT: AtomicBool = AtomicBool::new(false);
static SPEED: AtomicU32 = AtomicU32::new(0);
/// The player is using / attacking — server spin pauses.
static PAUSED: AtomicBool = AtomicBool::new(false);
/// The player's real pitch, for movement packets that carried none.
static PITCH: AtomicU32 = AtomicU32::new(0);

fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

/// Pure: the spin angle in degrees, `0..360`, after `seconds` at `speed`
/// degrees per second. Turning right raises Minecraft's yaw.
pub fn angle(seconds: f64, speed: f32, left: bool) -> f32 {
    let turned = (seconds * speed as f64).rem_euclid(360.0) as f32;
    if left {
        (360.0 - turned) % 360.0
    } else {
        turned
    }
}

fn current_angle() -> f32 {
    angle(
        epoch().elapsed().as_secs_f64(),
        f32::from_bits(SPEED.load(Ordering::Relaxed)),
        LEFT.load(Ordering::Relaxed),
    )
}

fn visual() -> bool {
    ACTIVE.load(Ordering::Relaxed) && MODE.load(Ordering::Relaxed) != 2
}

fn server() -> bool {
    ACTIVE.load(Ordering::Relaxed) && MODE.load(Ordering::Relaxed) != 1
}

#[derive(Debug)]
pub struct SpinModule {
    pub module: ModuleData,
}

impl SpinModule {
    pub fn new() -> Self {
        Self {
            module: ModuleData {
                id: ModuleId::Spin,
                description: "Spins your player around; the camera stays".to_string(),
                category: ModuleCategory::Render,
                key_bind: KeyboardKey::KeyNone,
                enabled: false,
                settings: vec![
                    ModuleSetting::Slider {
                        name: "Speed (°/s)".to_string(),
                        value: 720.0,
                        min: 30.0,
                        max: 3600.0,
                    },
                    ModuleSetting::Choice {
                        name: "Spin direction".to_string(),
                        value: 0,
                        options: DIRECTIONS.iter().map(|s| s.to_string()).collect(),
                    },
                    ModuleSetting::Choice {
                        name: "Spin mode".to_string(),
                        value: 0,
                        options: MODES.iter().map(|s| s.to_string()).collect(),
                    },
                    ModuleSetting::Toggle {
                        name: "Pause while using".to_string(),
                        value: true,
                    },
                ],
            },
        }
    }

    /// Copies the settings into the statics the other threads read.
    fn publish(&self) {
        let data = &self.module;
        let choice = |name: &str| match data.get_setting(name) {
            Some(ModuleSetting::Choice { value, .. }) => *value,
            _ => 0,
        };
        let speed = data
            .get_setting("Speed (°/s)")
            .and_then(ModuleSetting::get_slider_value)
            .unwrap_or(720.0);
        SPEED.store(speed.to_bits(), Ordering::Relaxed);
        LEFT.store(choice("Spin direction") == 1, Ordering::Relaxed);
        MODE.store(choice("Spin mode").min(2) as u8, Ordering::Relaxed);
    }

    fn pause_on_use(&self) -> bool {
        self.module
            .get_setting("Pause while using")
            .and_then(ModuleSetting::get_toggle_value)
            .unwrap_or(true)
    }
}

impl Module for SpinModule {
    fn on_start(&self) -> anyhow::Result<()> {
        self.publish();
        PAUSED.store(false, Ordering::Relaxed);
        ACTIVE.store(true, Ordering::Relaxed);
        Ok(())
    }

    fn on_stop(&self) -> anyhow::Result<()> {
        let was_server = server();
        ACTIVE.store(false, Ordering::Relaxed);
        // Tell the server where we really look again.
        if was_server {
            if let Ok(Some(player)) = minecraft().player() {
                let _ = send_real_rotation(&player);
            }
        }
        Ok(())
    }

    fn on_tick(&self) -> anyhow::Result<()> {
        self.publish();
        let Some(player) = minecraft().player()? else {
            return Ok(());
        };
        let (pitch, busy) = mapping().in_frame(|| {
            let mut env = mapping().get_env()?;
            let p = player.jni_ref().as_obj();
            let pitch = raw::call(&mut env, p, "getXRot", "()F", &[])?.f()?;
            let busy = raw::call(&mut env, p, "isUsingItem", "()Z", &[])?.z()?
                || key_down(&mut env, "keyUse")?
                || key_down(&mut env, "keyAttack")?;
            Ok((pitch, busy))
        })?;
        PITCH.store(pitch.to_bits(), Ordering::Relaxed);

        let paused = self.pause_on_use() && busy;
        let was_paused = PAUSED.swap(paused, Ordering::Relaxed);
        if !server() {
            return Ok(());
        }
        if paused {
            if !was_paused {
                send_real_rotation(&player)?;
            }
            return Ok(());
        }
        // Keep spinning while standing still, when the game sends nothing.
        send_rotation(current_angle(), pitch, player.on_ground()?)
    }

    fn handle_packet(&self, packet: &mut Packet) -> PacketAction {
        if server() && !PAUSED.load(Ordering::Relaxed) {
            if let Packet::ServerboundMovePlayer(movement) = packet {
                if !movement.has_rotation {
                    movement.x_rot = f32::from_bits(PITCH.load(Ordering::Relaxed));
                    movement.has_rotation = true;
                }
                movement.y_rot = current_angle();
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

fn key_down(env: &mut JNIEnv, key: &str) -> anyhow::Result<bool> {
    let options = raw::field(env, minecraft().jni_ref().as_obj(), "options", "Lnet/minecraft/client/Options;")?.l()?;
    let key = raw::field(env, &options, key, "Lnet/minecraft/client/KeyMapping;")?.l()?;
    Ok(raw::call(env, &key, "isDown", "()Z", &[])?.z()?)
}

/// Sends a rotation-only movement packet. It passes through
/// [`SpinModule::handle_packet`] like any other, so while spinning its yaw
/// becomes the spin angle anyway.
fn send_rotation(yaw: f32, pitch: f32, on_ground: bool) -> anyhow::Result<()> {
    mapping().in_frame(|| {
        let mut env = mapping().get_env()?;
        let listener = raw::call(
            &mut env,
            minecraft().jni_ref().as_obj(),
            "getConnection",
            "()Lnet/minecraft/client/multiplayer/ClientPacketListener;",
            &[],
        )?
        .l()?;
        if listener.is_null() {
            return Ok(());
        }
        let packet = ServerboundMovePlayerPacket {
            x: 0.0,
            y: 0.0,
            z: 0.0,
            y_rot: yaw,
            x_rot: pitch,
            on_ground,
            horizontal_collision: false,
            has_position: false,
            has_rotation: true,
        }
        .to_java(&mut env)?;
        let packet = unsafe { JObject::from_raw(packet) };
        raw::call(
            &mut env,
            &listener,
            "send",
            "(Lnet/minecraft/network/protocol/Packet;)V",
            &[JValue::Object(&packet)],
        )?;
        Ok(())
    })
}

/// Sends the player's real rotation — while paused this is not rewritten.
fn send_real_rotation(player: &LocalPlayer) -> anyhow::Result<()> {
    let (yaw, pitch) = mapping().in_frame(|| {
        let mut env = mapping().get_env()?;
        let p = player.jni_ref().as_obj();
        Ok((
            raw::call(&mut env, p, "getYRot", "()F", &[])?.f()?,
            raw::call(&mut env, p, "getXRot", "()F", &[])?.f()?,
        ))
    })?;
    send_rotation(yaw, pitch, player.on_ground()?)
}

// --- visual spin -----------------------------------------------------------------------

struct Visual {
    listeners: EventListeners,
    /// The `float[1]` the listener reads the angle from.
    holder: Option<GlobalRef>,
}

static VISUAL: Mutex<Option<Visual>> = Mutex::new(None);

/// Per frame, from the frame hook: keeps the render listener in place for
/// the current player and feeds it the angle.
pub fn frame() {
    let mut guard = VISUAL.lock().unwrap_or_else(|p| p.into_inner());
    let want = visual() && crate::state::client().modules.is_active();
    if !want {
        if let Some(state) = guard.as_mut() {
            state.listeners.clear();
        }
        return;
    }
    let state = guard.get_or_insert_with(|| Visual {
        listeners: EventListeners::default(),
        holder: None,
    });
    let Ok(Some(player)) = minecraft().player() else {
        return;
    };
    let result = mapping().in_frame(|| {
        let mut env = mapping().get_env()?;
        if state.holder.is_none() {
            let array = env.new_float_array(1)?;
            state.holder = Some(env.new_global_ref(array)?);
        }
        let holder = state.holder.clone().expect("holder set above");
        let array: &JFloatArray = holder.as_obj().into();
        env.set_float_array_region(array, 0, &[current_angle()])?;

        // Rebuilt when the player object changes (respawn, new world).
        let id = env
            .call_static_method(
                "java/lang/System",
                "identityHashCode",
                "(Ljava/lang/Object;)I",
                &[JValue::Object(player.jni_ref().as_obj())],
            )?
            .i()?;
        let player_ref = player.jni_ref().clone();
        state.listeners.ensure("spin", EVENT, format!("player {id:x}"), move |env, event| {
            build_listener(env, event, player_ref.as_obj(), holder.as_obj())
        });
        Ok(())
    });
    if let Err(error) = result {
        log::debug!("Spin: {error}");
    }
}

/// `event -> { if (event.getEntity() == player) entity.yBodyRot… = holder[0] }`.
fn build_listener<'l>(env: &mut JNIEnv<'l>, event: &JObject<'l>, player: &JObject, holder: &JObject) -> anyhow::Result<JObject<'l>> {
    let living = java::game_class(env, LIVING)?;
    let entity = java::key_chain(env, &[(event, "getEntity")])?;
    let test = java::is_test(env, &entity, player)?;
    let mut action: Option<JObject<'l>> = None;
    for field in FIELDS {
        let set = java::set_float_from(env, &living, field, holder, &entity)?;
        action = Some(match action {
            None => set,
            Some(previous) => java::sequence(env, &previous, &set)?,
        });
    }
    let action = action.ok_or_else(|| anyhow::anyhow!("no fields"))?;
    let handle = java::guard(env, &test, &action)?;
    java::proxy(env, "java/util/function/Consumer", &handle)
}

/// Unregisters the render listener — on unload.
pub fn cleanup() {
    ACTIVE.store(false, Ordering::Relaxed);
    if let Some(mut state) = VISUAL.lock().unwrap_or_else(|p| p.into_inner()).take() {
        state.listeners.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_angle_turns_at_the_chosen_speed() {
        assert_eq!(angle(0.0, 720.0, false), 0.0);
        assert!((angle(0.125, 720.0, false) - 90.0).abs() < 1e-3);
        assert!((angle(0.5, 720.0, false)).abs() < 1e-3); // a full turn
        assert!((angle(0.125, 720.0, true) - 270.0).abs() < 1e-3);
        assert_eq!(angle(0.0, 720.0, true), 0.0);
        let a = angle(1234.567, 3600.0, false);
        assert!((0.0..360.0).contains(&a));
    }
}
