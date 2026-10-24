use crate::mapping::raw;
use crate::module::player::godmode;
use crate::module::{KeyboardKey, Module, ModuleCategory, ModuleData, ModuleId};
use crate::net::packet::{Packet, PacketAction};
use crate::state::mapping;

/// Prevents fall damage, in two layers:
///
/// * [`NoFallModule::handle_packet`]: every outbound movement packet reports
///   the player as on the ground, so the server never accumulates the fall
///   distance it would turn into damage. Works on any server.
/// * `on_tick`, in singleplayer / LAN host only: the integrated server's own
///   player has its fall distance reset every tick — a backstop that does not
///   depend on the packet layer at all.
#[derive(Debug)]
pub struct NoFallModule {
    pub module: ModuleData,
}

impl NoFallModule {
    pub fn new() -> Self {
        Self {
            module: ModuleData {
                id: ModuleId::NoFall,
                description: "Prevents fall damage".to_string(),
                category: ModuleCategory::Movement,
                key_bind: KeyboardKey::KeyN,
                enabled: false,
                settings: vec![],
            },
        }
    }
}

impl Module for NoFallModule {
    fn on_start(&self) -> anyhow::Result<()> {
        Ok(())
    }

    fn on_stop(&self) -> anyhow::Result<()> {
        Ok(())
    }

    fn on_tick(&self) -> anyhow::Result<()> {
        let Some(server_player) = godmode::server_player()? else {
            return Ok(());
        };
        mapping().in_frame(|| {
            let mut env = mapping().get_env()?;
            raw::call(&mut env, server_player.as_obj(), "resetFallDistance", "()V", &[])?;
            Ok(())
        })
    }

    fn handle_packet(&self, packet: &mut Packet) -> PacketAction {
        // Report the player as on the ground on every movement packet, so the
        // server never accumulates the fall distance it turns into damage.
        if let Packet::ServerboundMovePlayer(move_packet) = packet {
            move_packet.on_ground = true;
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
