//! Baritone bridge.
//!
//! The `standalone` Baritone builds obfuscate even the `baritone.api`
//! interfaces, so there is no stable Java API to call. Baritone's own chat
//! hook is stable, though: it intercepts `ClientPacketListener.sendChat` and
//! consumes — never sends — any message starting with its prefix (`#`). Every
//! command therefore goes through [`execute`], exactly as if typed in chat.

use crate::state::minecraft;

/// Baritone's default command prefix.
const PREFIX: &str = "#";

/// Whether Baritone is loaded in the game.
pub fn is_installed() -> bool {
    super::class_present("baritone/api/IBaritoneProvider")
}

/// Runs a Baritone command (without the `#` prefix), e.g. `goto 0 64 0`.
/// Must be called on the game (render) thread — Baritone handles the command
/// synchronously inside `sendChat`.
pub fn execute(command: &str) -> anyhow::Result<()> {
    if !is_installed() {
        // Without Baritone the message would go to the server as plain chat.
        anyhow::bail!("Baritone is not installed");
    }
    minecraft().send_chat(&format!("{PREFIX}{}", command.trim()))
}
