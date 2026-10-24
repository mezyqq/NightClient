use crate::mapping::client::game_renderer::GameRenderer;
use crate::mapping::client::gamemode::MultiPlayerGameMode;
use crate::mapping::client::options::Options;
use crate::mapping::client::screen::Screen;
use crate::mapping::client::window::Window;
use crate::mapping::client::world::World;
use crate::mapping::entity::player::LocalPlayer;
use crate::mapping::{FieldType, MappedObject, MinecraftClassType};
use crate::state::mapping;
use jni::objects::{GlobalRef, JObject, JValue};
use std::sync::RwLock;

/// The running Minecraft client.
///
/// Only the things that exist from the main menu onward are held eagerly: the
/// `Minecraft.getInstance()` handle and the game [`Window`]. The world-scoped
/// objects — player, level, game mode — are null in the menu and on world
/// exit, so they are fetched lazily and reported as `Ok(None)` when absent.
/// This is what lets the client be injected from the main menu.
#[derive(Debug, MappedObject)]
#[mapped(class = Minecraft)]
pub struct Minecraft {
    jni_ref: GlobalRef,
    pub window: Window,
    /// Cached local player, refreshed when the underlying JVM object changes.
    player: RwLock<Option<LocalPlayer>>,
}

impl Minecraft {
    /// Builds the game wrapper from the live `Minecraft.getInstance()`.
    /// Succeeds whether or not a world is loaded.
    pub fn new() -> anyhow::Result<Minecraft> {
        let jni_ref = mapping().in_frame(|| {
            let minecraft = mapping()
                .call_static_method(MinecraftClassType::Minecraft, "getInstance", &[])?
                .l()?;
            if minecraft.is_null() {
                return Err(anyhow::anyhow!("Minecraft.getInstance() returned null"));
            }
            mapping().new_global_ref(minecraft)
        })?;
        let window = Window::new(&jni_ref)?;

        Ok(Minecraft {
            jni_ref,
            window,
            player: RwLock::new(None),
        })
    }

    /// The local player, or `Ok(None)` when not in a world.
    ///
    /// The result is cached and refreshed when the underlying JVM object
    /// changes (a new world join produces a fresh instance).
    pub fn player(&self) -> anyhow::Result<Option<LocalPlayer>> {
        let Some(player_ref) = self.world_object("player", MinecraftClassType::LocalPlayer)? else {
            // Left the world: drop the cached `LocalPlayer` so its JVM object
            // is no longer pinned by a global reference until the next join.
            if let Ok(mut cache) = self.player.write() {
                *cache = None;
            }
            return Ok(None);
        };

        {
            let cache = self
                .player
                .read()
                .map_err(|_| anyhow::anyhow!("Lock poisoned"))?;
            if let Some(cached) = cache.as_ref() {
                if mapping()
                    .get_env()?
                    .is_same_object(cached.jni_ref(), &player_ref)?
                {
                    return Ok(Some(cached.clone()));
                }
            }
        }

        let player = LocalPlayer::new(player_ref)?;
        *self
            .player
            .write()
            .map_err(|_| anyhow::anyhow!("Lock poisoned"))? = Some(player.clone());
        Ok(Some(player))
    }

    /// The current world / level, or `Ok(None)` when not in a world.
    pub fn world(&self) -> anyhow::Result<Option<World>> {
        Ok(self
            .world_object("level", MinecraftClassType::Level)?
            .map(World::new))
    }

    /// The interaction controller, or `Ok(None)` when not in a world.
    pub fn game_mode(&self) -> anyhow::Result<Option<MultiPlayerGameMode>> {
        Ok(self
            .world_object("gameMode", MinecraftClassType::MultiPlayerGameMode)?
            .map(MultiPlayerGameMode::new))
    }

    /// The game renderer. Present from the main menu onward.
    pub fn game_renderer(&self) -> anyhow::Result<GameRenderer> {
        self.in_frame(|| {
            let obj = self
                .get_field(
                    "gameRenderer",
                    FieldType::Object(MinecraftClassType::GameRenderer),
                )?
                .l()?;
            Ok(GameRenderer::new(mapping().new_global_ref(obj)?))
        })
    }

    /// The game options. Present from the main menu onward.
    pub fn options(&self) -> anyhow::Result<Options> {
        self.in_frame(|| {
            let obj = self
                .get_field("options", FieldType::Object(MinecraftClassType::Options))?
                .l()?;
            Ok(Options::new(mapping().new_global_ref(obj)?))
        })
    }

    /// Sends `message` as if typed into chat (`ClientPacketListener.sendChat`).
    /// Mods hooking chat — Baritone's `#` commands — see it first.
    pub fn send_chat(&self, message: &str) -> anyhow::Result<()> {
        self.in_frame(|| {
            let mut env = mapping().get_env()?;
            let Some(listener) = self.packet_listener(&mut env)? else {
                anyhow::bail!("not connected to a world");
            };
            let text: JObject = env.new_string(message)?.into();
            env.call_method(
                &listener,
                "sendChat",
                "(Ljava/lang/String;)V",
                &[JValue::Object(&text)],
            )
            .map_err(|_| clear_exception("sendChat"))?;
            Ok(())
        })
    }

    /// Names of every player on the server's tab list — including players
    /// far outside entity-tracking range. Empty when not connected.
    pub fn online_player_names(&self) -> anyhow::Result<Vec<String>> {
        self.in_frame(|| {
            let mut env = mapping().get_env()?;
            let Some(listener) = self.packet_listener(&mut env)? else {
                return Ok(Vec::new());
            };
            let players = env
                .call_method(
                    &listener,
                    "getOnlinePlayers",
                    "()Ljava/util/Collection;",
                    &[],
                )
                .map_err(|_| clear_exception("getOnlinePlayers"))?
                .l()?;
            let iterator = env
                .call_method(&players, "iterator", "()Ljava/util/Iterator;", &[])?
                .l()?;
            let mut names = Vec::new();
            while env.call_method(&iterator, "hasNext", "()Z", &[])?.z()? {
                let name = env.with_local_frame(4, |env| -> anyhow::Result<String> {
                    let info = env
                        .call_method(&iterator, "next", "()Ljava/lang/Object;", &[])?
                        .l()?;
                    let profile = env
                        .call_method(
                            &info,
                            "getProfile",
                            "()Lcom/mojang/authlib/GameProfile;",
                            &[],
                        )?
                        .l()?;
                    let name = env
                        .call_method(&profile, "getName", "()Ljava/lang/String;", &[])?
                        .l()?;
                    mapping().get_string(name)
                });
                match name {
                    Ok(name) => names.push(name),
                    Err(_) => {
                        let _ = env.exception_clear();
                    }
                }
            }
            names.sort_by_key(|name| name.to_lowercase());
            Ok(names)
        })
    }

    /// The current perspective (`Options.getCameraType`) — a `CameraType`.
    pub fn camera_type(&self) -> anyhow::Result<GlobalRef> {
        self.in_frame(|| {
            let mut env = mapping().get_env()?;
            let options = self.options()?;
            let kind = env
                .call_method(
                    options.jni_ref().as_obj(),
                    "getCameraType",
                    "()Lnet/minecraft/client/CameraType;",
                    &[],
                )
                .map_err(|_| clear_exception("getCameraType"))?
                .l()?;
            Ok(env.new_global_ref(kind)?)
        })
    }

    /// Sets the perspective to `kind`, a `CameraType` from [`camera_type`] or
    /// [`third_person_camera`].
    pub fn set_camera_type(&self, kind: &GlobalRef) -> anyhow::Result<()> {
        self.in_frame(|| {
            let mut env = mapping().get_env()?;
            let options = self.options()?;
            env.call_method(
                options.jni_ref().as_obj(),
                "setCameraType",
                "(Lnet/minecraft/client/CameraType;)V",
                &[JValue::Object(kind.as_obj())],
            )
            .map_err(|_| clear_exception("setCameraType"))?;
            Ok(())
        })
    }

    /// `CameraType.THIRD_PERSON_BACK`, or `THIRD_PERSON_FRONT` when `front`.
    pub fn third_person_camera(&self, front: bool) -> anyhow::Result<GlobalRef> {
        let name = if front { "THIRD_PERSON_FRONT" } else { "THIRD_PERSON_BACK" };
        self.in_frame(|| {
            let mut env = mapping().get_env()?;
            let class = mapping().resolve_class(&mut env, "net/minecraft/client/CameraType")?;
            let kind = env
                .get_static_field(
                    class,
                    name,
                    "Lnet/minecraft/client/CameraType;",
                )
                .map_err(|_| clear_exception(name))?
                .l()?;
            Ok(env.new_global_ref(kind)?)
        })
    }

    /// Re-syncs every key binding with the physical keyboard: releases them
    /// all (`KeyMapping.releaseAll`), then marks held keyboard keys down again
    /// (`KeyMapping.setAll`). Used after input was withheld from the game, so
    /// no binding stays stuck "down" on a key released in the meantime.
    pub fn resync_key_mappings(&self) -> anyhow::Result<()> {
        self.in_frame(|| {
            let mut env = mapping().get_env()?;
            let class = mapping().resolve_class(&mut env, "net/minecraft/client/KeyMapping")?;
            env.call_static_method(&class, "releaseAll", "()V", &[])
                .map_err(|_| clear_exception("KeyMapping.releaseAll"))?;
            env.call_static_method(&class, "setAll", "()V", &[])
                .map_err(|_| clear_exception("KeyMapping.setAll"))?;
            Ok(())
        })
    }

    /// The local player's `ClientPacketListener`, or `None` when not in a world.
    fn packet_listener<'a>(
        &self,
        env: &mut jni::JNIEnv<'a>,
    ) -> anyhow::Result<Option<JObject<'a>>> {
        let Some(player) = self.player()? else {
            return Ok(None);
        };
        let listener = env
            .get_field(
                player.jni_ref().as_obj(),
                "connection",
                "Lnet/minecraft/client/multiplayer/ClientPacketListener;",
            )
            .map_err(|_| clear_exception("LocalPlayer.connection"))?
            .l()?;
        Ok((!listener.is_null()).then_some(listener))
    }

    /// Whether a world is currently loaded.
    pub fn in_world(&self) -> bool {
        matches!(self.player(), Ok(Some(_)))
    }

    /// Drops the cached local player — a held global reference. Called from
    /// `cleanup_client` before the library is unloaded.
    pub fn teardown(&self) {
        if let Ok(mut cache) = self.player.write() {
            *cache = None;
        }
    }

    /// Whether no screen (menu / inventory / …) is currently open.
    pub fn current_screen_is_null(&self) -> bool {
        self.current_screen() == Screen::None
    }

    /// The Minecraft screen currently open. Best-effort: any JNI failure — or
    /// an unrecognized screen — reports [`Screen::Unknown`], which callers
    /// treat as "a screen is open".
    pub fn current_screen(&self) -> Screen {
        self.in_frame(|| {
            let screen = self
                .get_field("screen", FieldType::Object(MinecraftClassType::Screen))?
                .l()?;
            if screen.is_null() {
                return Ok(Screen::None);
            }

            let is = |class| mapping().is_instance_of(class, &screen).unwrap_or(false);
            // Specific screens first — `InventoryScreen` / `CraftingScreen`
            // both extend `AbstractContainerScreen`.
            let screen = if is(MinecraftClassType::ChatScreen) {
                Screen::Chat
            } else if is(MinecraftClassType::CreativeModeInventoryScreen)
                || is(MinecraftClassType::InventoryScreen)
            {
                Screen::Inventory
            } else if is(MinecraftClassType::CraftingScreen) {
                Screen::Crafting
            } else if is(MinecraftClassType::AbstractContainerScreen) {
                Screen::Container
            } else if is(MinecraftClassType::PauseScreen) {
                Screen::Menu
            } else {
                Screen::Unknown
            };
            Ok(screen)
        })
        .unwrap_or(Screen::Unknown)
    }

    /// Reads a world-scoped object field of `Minecraft`, returning `Ok(None)`
    /// when it is null — i.e. when there is no world loaded.
    fn world_object(
        &self,
        field: &str,
        class: MinecraftClassType,
    ) -> anyhow::Result<Option<GlobalRef>> {
        self.in_frame(|| {
            let obj = self.get_field(field, FieldType::Object(class))?.l()?;
            if obj.is_null() {
                return Ok(None);
            }
            Ok(Some(mapping().new_global_ref(obj)?))
        })
    }
}

/// Clears the pending Java exception after a failed JNI call and describes it.
fn clear_exception(what: &str) -> anyhow::Error {
    if let Ok(env) = mapping().get_env() {
        let _ = env.exception_clear();
    }
    anyhow::anyhow!("JNI call {what} failed")
}
