//! Top-level overlay renderer.
//!
//! This module owns only the per-frame *dispatch order* — HUD underneath,
//! ClickGUI in the middle, notifications always on top. Visual styling lives
//! in [`theme`](crate::graphic::theme), animation in
//! [`anim`](crate::graphic::anim); each widget owns its own module.

use crate::graphic::anim::{self, Easing};
use crate::graphic::input::GUI_OPEN;
use crate::graphic::{esp, hud, menu, notification, windows, workspace};
use egui::{Context, Id};
use std::sync::atomic::Ordering;

/// Renders the whole overlay for a single frame.
pub fn render_all(ctx: &Context) {
    let hidden = hud_hidden();
    // World-space ESP sits underneath every piece of 2D UI.
    if !hidden {
        esp::draw(ctx);
        hud::draw(ctx);
    }
    // Always called: under Hide HUD it gives the vanilla crosshair back.
    crate::module::render::crosshair::frame(ctx, hidden);
    if !hidden {
        crate::module::render::hit_effects::draw_screen(ctx);
        crate::module::render::target_hud::draw(ctx);
    }

    // A single tween turns the open/close toggle into the 0..1 factor the
    // menu uses to fade and slide itself in.
    let open = GUI_OPEN.load(Ordering::Relaxed);
    let progress = anim::toggle(ctx, Id::new("clickgui_open"), open, 0.18, Easing::InOut);
    if progress > 0.0 {
        workspace::begin_frame();
        menu::draw(ctx, progress);
        windows::draw(ctx, progress);
        workspace::draw(ctx, progress);
    }

    if !hidden {
        notification::draw(ctx);
    }

    // Write the layout once it has settled after a change.
    crate::config::autosave_layout();

    // Reclaim animation state for anything that stopped being drawn.
    anim::gc(ctx);
}

/// Config key of the Hide HUD switch.
const HIDE_HUD_KEY: &str = "hide_hud";

/// Whether Hide HUD is on: the client draws nothing outside the ClickGUI —
/// no watermark, module list, ESP or notifications.
pub fn hud_hidden() -> bool {
    crate::config::ui_value(HIDE_HUD_KEY).as_deref() == Some("1")
}

/// Flips Hide HUD (persisted in the config).
pub fn toggle_hud_hidden() {
    let hidden = !hud_hidden();
    crate::config::set_ui_value(HIDE_HUD_KEY, if hidden { "1" } else { "0" });
}
