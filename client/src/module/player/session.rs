//! Session modules — the ones that must act when ordinary modules are asleep:
//! on the death screen, on the "disconnected" screen, or right on joining.
//!
//! * **AutoRespawn** — respawns as soon as the player dies.
//! * **AutoReconnect** — after a kick or lost connection, reconnects to the
//!   last server after a delay.
//! * **AutoCommand** — on joining a server, types saved commands / messages
//!   (e.g. `/login …`), per server or for every server. Saved in
//!   `auto_commands.json` beside the client config — in plain text.
//!
//! Module registry ticks only run in a world with a living player, so these
//! three are plain switches + settings; the work happens in [`frame`], which
//! the frame hook calls every frame.

use crate::config;
use crate::graphic::notification::{Notification, NotificationType};
use crate::mapping::raw;
use crate::mapping::MappedObject;
use crate::module::{KeyboardKey, Module, ModuleCategory, ModuleData, ModuleId, ModuleSetting};
use crate::state::{client, mapping, minecraft};
use jni::objects::{GlobalRef, JObject, JValue};
use jni::JNIEnv;
use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Key of the commands typed on every server.
pub const ALL_SERVERS: &str = "*";
const COMMANDS_FILE: &str = "auto_commands.json";
/// How often [`frame`] does its work.
const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Gap between two auto-typed lines — avoids the server's spam kick.
const COMMAND_GAP: Duration = Duration::from_millis(700);
/// Wait on the death screen before respawning.
const RESPAWN_DELAY: Duration = Duration::from_millis(300);

const SCREEN_SIG: &str = "Lnet/minecraft/client/gui/screens/Screen;";
const LISTENER_SIG: &str = "Lnet/minecraft/client/multiplayer/ClientPacketListener;";

// --- the three modules: switches + settings --------------------------------

macro_rules! session_module {
    ($name:ident, $id:ident, $description:literal, $settings:expr) => {
        #[derive(Debug)]
        pub struct $name {
            pub module: ModuleData,
        }

        impl $name {
            pub fn new() -> Self {
                Self {
                    module: ModuleData {
                        id: ModuleId::$id,
                        description: $description.to_string(),
                        category: ModuleCategory::Player,
                        key_bind: KeyboardKey::KeyNone,
                        enabled: false,
                        settings: $settings,
                    },
                }
            }
        }

        impl Module for $name {
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
    };
}

session_module!(AutoRespawnModule, AutoRespawn, "Respawns right after death", vec![]);
session_module!(
    AutoReconnectModule,
    AutoReconnect,
    "Reconnects to the server after a kick or lost connection",
    vec![ModuleSetting::Slider {
        name: "Delay (s)".to_string(),
        value: 5.0,
        min: 1.0,
        max: 60.0,
    }]
);
session_module!(
    AutoCommandModule,
    AutoCommand,
    "Types saved commands when joining a server (e.g. /login)",
    vec![ModuleSetting::Slider {
        name: "Delay (s)".to_string(),
        value: 2.0,
        min: 0.0,
        max: 15.0,
    }]
);

// --- saved commands -----------------------------------------------------------

/// Saved lines by server address (lower-case), [`ALL_SERVERS`] for every one.
static COMMANDS: Mutex<Option<BTreeMap<String, Vec<String>>>> = Mutex::new(None);

fn with_commands<T>(f: impl FnOnce(&mut BTreeMap<String, Vec<String>>) -> T) -> T {
    let mut guard = COMMANDS.lock().unwrap_or_else(|p| p.into_inner());
    f(guard.get_or_insert_with(|| {
        std::fs::read_to_string(config::base_dir().join(COMMANDS_FILE))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }))
}

fn save_commands(map: &BTreeMap<String, Vec<String>>) {
    let json = serde_json::to_string_pretty(map).unwrap_or_else(|_| "{}".into());
    let path = config::base_dir().join(COMMANDS_FILE);
    if let Err(error) = std::fs::write(&path, json) {
        log::warn!("AutoCommand: could not write {}: {error}", path.display());
    }
}

/// The lines saved under `key`.
pub fn commands(key: &str) -> Vec<String> {
    with_commands(|map| map.get(key).cloned().unwrap_or_default())
}

/// Appends `line` under `key`.
pub fn add_command(key: &str, line: &str) {
    let line = line.trim();
    if line.is_empty() {
        return;
    }
    with_commands(|map| {
        map.entry(key.to_string()).or_default().push(line.to_string());
        save_commands(map);
    });
}

/// Removes line `index` under `key`.
pub fn remove_command(key: &str, index: usize) {
    with_commands(|map| {
        if let Some(lines) = map.get_mut(key) {
            if index < lines.len() {
                lines.remove(index);
            }
            if lines.is_empty() {
                map.remove(key);
            }
        }
        save_commands(map);
    });
}

/// Address of the current — or, when disconnected, the last — server.
pub fn server_address() -> Option<String> {
    SESSION
        .lock()
        .ok()
        .and_then(|session| session.server.as_ref().map(|(address, _)| address.clone()))
}

// --- per-frame work ------------------------------------------------------------

#[derive(Default)]
struct Session {
    last_poll: Option<Instant>,
    /// Address (lower-case) and `ServerData` of the current / last server.
    server: Option<(String, GlobalRef)>,
    /// The connection seen last frame — a new one means a fresh join.
    connection: Option<GlobalRef>,
    /// Lines waiting to be typed, and when the next may go.
    pending: VecDeque<String>,
    next_line_at: Option<Instant>,
    dead_since: Option<Instant>,
    respawned: bool,
    disconnected_since: Option<Instant>,
}

static SESSION: Mutex<Session> = Mutex::new(Session {
    last_poll: None,
    server: None,
    connection: None,
    pending: VecDeque::new(),
    next_line_at: None,
    dead_since: None,
    respawned: false,
    disconnected_since: None,
});

/// The data of module `id` if it is switched on.
fn armed(id: ModuleId) -> Option<ModuleData> {
    let handle = client().modules.get(id)?;
    let module = handle.lock().ok()?;
    let data = module.get_module_data();
    data.enabled.then(|| data.clone())
}

fn slider(data: &ModuleData, name: &str, fallback: f32) -> f32 {
    data.get_setting(name)
        .and_then(ModuleSetting::get_slider_value)
        .unwrap_or(fallback)
}

/// Called every frame from the frame hook.
pub fn frame() {
    let Ok(mut session) = SESSION.lock() else {
        return;
    };
    let now = Instant::now();
    if session.last_poll.is_some_and(|at| now - at < POLL_INTERVAL) {
        return;
    }
    session.last_poll = Some(now);
    if let Err(error) = poll(&mut session, now) {
        log::debug!("session: {error}");
    }
}

fn poll(session: &mut Session, now: Instant) -> anyhow::Result<()> {
    let mc = minecraft();
    match mc.player()? {
        Some(player) => {
            session.disconnected_since = None;
            mapping().in_frame(|| {
                let mut env = mapping().get_env()?;
                let player = player.jni_ref().as_obj();
                track_server(&mut env, session)?;
                track_connection(&mut env, session, player, now)?;
                type_pending(&mut env, session, player, now)?;
                respawn_if_dead(&mut env, session, player, now)
            })
        }
        None => {
            session.connection = None;
            session.pending.clear();
            session.dead_since = None;
            mapping().in_frame(|| {
                let mut env = mapping().get_env()?;
                reconnect_if_kicked(&mut env, session, now)
            })
        }
    }
}

/// Remembers which remote server we are on (none in singleplayer).
fn track_server(env: &mut JNIEnv, session: &mut Session) -> anyhow::Result<()> {
    let data = raw::call(
        env,
        minecraft().jni_ref().as_obj(),
        "getCurrentServer",
        "()Lnet/minecraft/client/multiplayer/ServerData;",
        &[],
    )?
    .l()?;
    if data.is_null() {
        session.server = None;
        return Ok(());
    }
    let ip = raw::field(env, &data, "ip", "Ljava/lang/String;")?.l()?;
    let address = mapping().get_string(ip)?.to_lowercase();
    let same = session
        .server
        .as_ref()
        .is_some_and(|(known, _)| *known == address);
    if !same {
        session.server = Some((address, env.new_global_ref(&data)?));
    }
    Ok(())
}

/// Detects a fresh join and queues the AutoCommand lines for it.
fn track_connection(env: &mut JNIEnv, session: &mut Session, player: &JObject, now: Instant) -> anyhow::Result<()> {
    let connection = raw::field(env, player, "connection", LISTENER_SIG)?.l()?;
    if connection.is_null() {
        return Ok(());
    }
    let known = match &session.connection {
        Some(known) => env.is_same_object(known.as_obj(), &connection)?,
        None => false,
    };
    if known {
        return Ok(());
    }
    session.connection = Some(env.new_global_ref(&connection)?);
    session.pending.clear();

    let (Some(data), Some((address, _))) = (armed(ModuleId::AutoCommand), session.server.as_ref()) else {
        return Ok(());
    };
    let lines: Vec<String> = commands(ALL_SERVERS).into_iter().chain(commands(address)).collect();
    if lines.is_empty() {
        return Ok(());
    }
    session.pending.extend(lines);
    session.next_line_at = Some(now + Duration::from_secs_f32(slider(&data, "Delay (s)", 2.0)));
    Ok(())
}

/// Types the next queued line once its time has come.
fn type_pending(env: &mut JNIEnv, session: &mut Session, player: &JObject, now: Instant) -> anyhow::Result<()> {
    if session.next_line_at.is_none_or(|at| now < at) {
        return Ok(());
    }
    let Some(line) = session.pending.pop_front() else {
        session.next_line_at = None;
        return Ok(());
    };
    session.next_line_at = Some(now + COMMAND_GAP);
    let connection = raw::field(env, player, "connection", LISTENER_SIG)?.l()?;
    let (method, text) = match line.strip_prefix('/') {
        Some(command) => ("sendCommand", command),
        None => ("sendChat", line.as_str()),
    };
    let text: JObject = env.new_string(text)?.into();
    raw::call(env, &connection, method, "(Ljava/lang/String;)V", &[JValue::Object(&text)])?;
    Ok(())
}

fn respawn_if_dead(env: &mut JNIEnv, session: &mut Session, player: &JObject, now: Instant) -> anyhow::Result<()> {
    if raw::call(env, player, "isAlive", "()Z", &[])?.z()? {
        session.dead_since = None;
        session.respawned = false;
        return Ok(());
    }
    if armed(ModuleId::AutoRespawn).is_none() || session.respawned {
        return Ok(());
    }
    let since = *session.dead_since.get_or_insert(now);
    if now - since < RESPAWN_DELAY {
        return Ok(());
    }
    session.respawned = true;
    raw::call(env, player, "respawn", "()V", &[])?;
    set_screen(env, &JObject::null())
}

fn set_screen(env: &mut JNIEnv, screen: &JObject) -> anyhow::Result<()> {
    raw::call(
        env,
        minecraft().jni_ref().as_obj(),
        "setScreen",
        &format!("({SCREEN_SIG})V"),
        &[JValue::Object(screen)],
    )?;
    Ok(())
}

/// On the "disconnected" screen, counts down and reconnects to the last server.
fn reconnect_if_kicked(env: &mut JNIEnv, session: &mut Session, now: Instant) -> anyhow::Result<()> {
    let (Some(data), Some((address, server_data))) = (armed(ModuleId::AutoReconnect), session.server.clone()) else {
        session.disconnected_since = None;
        return Ok(());
    };
    let screen = raw::field(env, minecraft().jni_ref().as_obj(), "screen", SCREEN_SIG)?.l()?;
    let disconnected = !screen.is_null() && {
        let class = mapping().resolve_class(env, "net/minecraft/client/gui/screens/DisconnectedScreen")?;
        env.is_instance_of(&screen, &class)?
    };
    if !disconnected {
        session.disconnected_since = None;
        return Ok(());
    }

    let delay = Duration::from_secs_f32(slider(&data, "Delay (s)", 5.0));
    let Some(since) = session.disconnected_since else {
        session.disconnected_since = Some(now);
        Notification::send(
            NotificationType::Info,
            "AutoReconnect",
            &format!("Reconnecting to {address} in {:.0}s", delay.as_secs_f32()),
        );
        return Ok(());
    };
    if now - since < delay {
        return Ok(());
    }
    session.disconnected_since = None;
    connect(env, &address, server_data.as_obj())
}

/// `ConnectScreen.startConnecting`, with the multiplayer list as the screen to
/// return to on cancel.
fn connect(env: &mut JNIEnv, address: &str, server_data: &JObject) -> anyhow::Result<()> {
    let text: JObject = env.new_string(address)?.into();
    let server_address = game_static(
        env,
        "net/minecraft/client/multiplayer/resolver/ServerAddress",
        "parseString",
        "(Ljava/lang/String;)Lnet/minecraft/client/multiplayer/resolver/ServerAddress;",
        &[JValue::Object(&text)],
    )?
    .l()?;
    let parent = multiplayer_screen(env).unwrap_or(JObject::null());
    let mc = minecraft().jni_ref().as_obj();

    let base = format!(
        "({SCREEN_SIG}Lnet/minecraft/client/Minecraft;Lnet/minecraft/client/multiplayer/resolver/ServerAddress;Lnet/minecraft/client/multiplayer/ServerData;Z"
    );
    let null = JObject::null();
    let modern = game_static(
        env,
        "net/minecraft/client/gui/screens/ConnectScreen",
        "startConnecting",
        &format!("{base}Lnet/minecraft/client/multiplayer/TransferState;)V"),
        &[
            JValue::Object(&parent),
            JValue::Object(mc),
            JValue::Object(&server_address),
            JValue::Object(server_data),
            JValue::Bool(0),
            JValue::Object(&null),
        ],
    );
    if modern.is_ok() {
        return Ok(());
    }
    // Before 1.20.5 there is no `TransferState` parameter.
    game_static(
        env,
        "net/minecraft/client/gui/screens/ConnectScreen",
        "startConnecting",
        &format!("{base})V"),
        &[
            JValue::Object(&parent),
            JValue::Object(mc),
            JValue::Object(&server_address),
            JValue::Object(server_data),
            JValue::Bool(0),
        ],
    )?;
    Ok(())
}

/// A fresh `JoinMultiplayerScreen` over the title screen.
fn multiplayer_screen<'local>(env: &mut JNIEnv<'local>) -> anyhow::Result<JObject<'local>> {
    let title_class = mapping().resolve_class(env, "net/minecraft/client/gui/screens/TitleScreen")?;
    let title = env.new_object(&title_class, "()V", &[]).inspect_err(|_| {
        let _ = env.exception_clear();
    })?;
    let list_class =
        mapping().resolve_class(env, "net/minecraft/client/gui/screens/multiplayer/JoinMultiplayerScreen")?;
    let list = env
        .new_object(&list_class, &format!("({SCREEN_SIG})V"), &[JValue::Object(&title)])
        .inspect_err(|_| {
            let _ = env.exception_clear();
        })?;
    Ok(list)
}

/// A static call on a game class (resolved through the game class loader).
fn game_static<'local>(
    env: &mut JNIEnv<'local>,
    class: &str,
    name: &str,
    sig: &str,
    args: &[JValue],
) -> anyhow::Result<jni::objects::JValueOwned<'local>> {
    let class = mapping().resolve_class(env, class)?;
    env.call_static_method(&class, name, sig, args).map_err(|error| {
        let _ = env.exception_clear();
        anyhow::anyhow!("{name}{sig}: {error}")
    })
}
