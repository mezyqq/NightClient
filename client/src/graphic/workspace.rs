//! Workspaces — virtual desktops for the ClickGUI.
//!
//! Every category panel and tool window belongs to one workspace (1 by
//! default, persisted in the config by key); only the current workspace's
//! windows are shown. A switcher in the top-left corner shows the numbers.
//!
//! * **Ctrl + number** — switch to that workspace (the menu starts on 1
//!   after every inject).
//! * **Ctrl + Shift + number** — move the window under the pointer there.
//! * **Drag a window onto a number** in the switcher — move it there; it
//!   keeps the spot it was dragged from.
//! * **Click a number** — switch.
//!
//! Windows cross-fade on a switch: [`opacity`] is eased per workspace.

use crate::graphic::anim::{self, Easing};
use crate::graphic::theme;
use crate::module::ModuleCategory;
use egui::{Align2, Color32, Context, FontId, Id, Key, LayerId, Modifiers, Order, Pos2, Rect, Rounding, Sense, Stroke, Vec2};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Mutex;

/// Number of workspaces.
pub const COUNT: u8 = 2;
/// Cross-fade duration of a switch, in seconds.
const FADE: f32 = 0.16;

static CURRENT: AtomicU8 = AtomicU8::new(1);

/// A window that can live on a workspace — they keep their position
/// differently.
#[derive(Clone)]
enum Owner {
    Panel(ModuleCategory),
    Window(String),
}

impl Owner {
    fn key(&self) -> String {
        match self {
            Owner::Panel(category) => panel_key(*category),
            Owner::Window(title) => window_key(title),
        }
    }
}

struct State {
    /// Layers drawn this frame → the window on them.
    layers: HashMap<LayerId, Owner>,
    /// The window a primary press started on, and where it was then.
    pressed: Option<(Owner, Pos2)>,
    /// Tool windows to put back at a position on their next draw.
    restore: HashMap<String, Pos2>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> R {
    let mut guard = STATE.lock().unwrap_or_else(|p| p.into_inner());
    f(guard.get_or_insert_with(|| State {
        layers: HashMap::new(),
        pressed: None,
        restore: HashMap::new(),
    }))
}

/// The workspace on screen, `1..=COUNT`.
pub fn current() -> u8 {
    CURRENT.load(Ordering::Relaxed)
}

/// Pure: the workspace a stored value names, `1` for anything invalid.
pub fn parse(value: Option<&str>) -> u8 {
    value
        .and_then(|v| v.trim().parse::<u8>().ok())
        .filter(|n| (1..=COUNT).contains(n))
        .unwrap_or(1)
}

fn config_key(key: &str) -> String {
    format!("workspace:{key}")
}

/// The workspace window `key` lives on.
pub fn of(key: &str) -> u8 {
    parse(crate::config::ui_value(&config_key(key)).as_deref())
}

/// Moves window `key` to workspace `n`.
pub fn move_to(key: &str, n: u8) {
    if (1..=COUNT).contains(&n) {
        crate::config::set_ui_value(&config_key(key), &n.to_string());
    }
}

/// Key of a category panel.
pub fn panel_key(category: ModuleCategory) -> String {
    format!("panel:{}", category.display_name())
}

/// Key of a tool window.
pub fn window_key(title: &str) -> String {
    format!("window:{title}")
}

/// How visible window `key` is, 0..1 — 1 on the current workspace, easing
/// during a switch. 0 means: do not draw it.
pub fn opacity(ctx: &Context, key: &str) -> f32 {
    let n = of(key);
    anim::toggle(ctx, Id::new("workspace_fade").with(n), n == current(), FADE, Easing::InOut)
}

/// Records that category panel `category` was drawn this frame on `layer`.
pub fn drawn_panel(layer: LayerId, category: ModuleCategory) {
    with_state(|s| s.layers.insert(layer, Owner::Panel(category)));
}

/// Records that tool window `title` was drawn this frame on `layer`.
pub fn drawn_window(layer: LayerId, title: &str) {
    with_state(|s| s.layers.insert(layer, Owner::Window(title.to_string())));
}

/// A position to put tool window `title` back at, once.
pub fn take_restore(title: &str) -> Option<Pos2> {
    with_state(|s| s.restore.remove(&window_key(title)))
}

/// Starts a GUI frame: forgets last frame's layers.
pub fn begin_frame() {
    with_state(|s| s.layers.clear());
}

/// The window on the top layer at `pos`, and where it is now.
fn window_at(ctx: &Context, pos: Pos2) -> Option<(Owner, Pos2)> {
    let layer = ctx.layer_id_at(pos)?;
    let owner = with_state(|s| s.layers.get(&layer).cloned())?;
    let at = match &owner {
        Owner::Panel(category) => crate::config::panel_pos(*category).map(|[x, y]| Pos2::new(x, y)),
        Owner::Window(_) => egui::AreaState::load(ctx, layer.id).map(|state| state.left_top_pos()),
    }
    .unwrap_or(pos);
    Some((owner, at))
}

/// Moves a window to workspace `n` and puts it back at `at`.
fn send(owner: &Owner, at: Pos2, n: u8) {
    let key = owner.key();
    move_to(&key, n);
    match owner {
        Owner::Panel(category) => crate::config::set_panel_pos(*category, [at.x, at.y]),
        Owner::Window(_) => {
            with_state(|s| s.restore.insert(key, at));
        }
    }
}

/// The digit key for workspace `n`.
fn digit(n: u8) -> Option<Key> {
    Some(match n {
        1 => Key::Num1,
        2 => Key::Num2,
        3 => Key::Num3,
        4 => Key::Num4,
        5 => Key::Num5,
        6 => Key::Num6,
        7 => Key::Num7,
        8 => Key::Num8,
        9 => Key::Num9,
        _ => return None,
    })
}

/// Handles the shortcuts and draws the switcher. Call after every window has
/// been drawn, while the ClickGUI is open.
pub fn draw(ctx: &Context, progress: f32) {
    for n in 1..=COUNT {
        let Some(key) = digit(n) else { continue };
        // Ctrl+Shift first: a plain Ctrl match would also accept it.
        let (send_hovered, switch) = ctx.input_mut(|i| {
            let shifted = i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, key);
            (shifted, !shifted && i.consume_key(Modifiers::COMMAND, key))
        });
        if send_hovered {
            if let Some((owner, at)) = ctx.pointer_hover_pos().and_then(|pos| window_at(ctx, pos)) {
                send(&owner, at, n);
            }
        } else if switch {
            CURRENT.store(n, Ordering::Relaxed);
        }
    }

    // Remember the window a drag starts on.
    let (pressed, released, down, pointer) = ctx.input(|i| {
        (
            i.pointer.primary_pressed(),
            i.pointer.primary_released(),
            i.pointer.primary_down(),
            i.pointer.interact_pos(),
        )
    });
    if pressed {
        let start = pointer.and_then(|pos| window_at(ctx, pos));
        with_state(|s| s.pressed = start);
    }
    let dragging = (down || released) && with_state(|s| s.pressed.is_some());

    // The switcher chip, top-left.
    let cell = Vec2::new(26.0, 22.0);
    let pad = 4.0;
    let gap = 2.0;
    let origin = Pos2::new(12.0, 12.0 - 16.0 * (1.0 - progress));
    let chip = Rect::from_min_size(
        origin,
        Vec2::new(
            pad * 2.0 + cell.x * COUNT as f32 + gap * (COUNT - 1) as f32,
            cell.y + pad * 2.0,
        ),
    );
    let mut dropped_on = None;
    egui::Area::new(Id::new("workspace_switcher"))
        .order(Order::Tooltip)
        .fixed_pos(chip.min)
        .show(ctx, |ui| {
            ui.set_opacity(progress);
            let painter = ui.painter().clone();
            let round = Rounding::same(theme::RADIUS);
            painter.rect_filled(chip, round, theme::BASE);
            painter.rect_stroke(chip, round, Stroke::new(1.0_f32, theme::BORDER));
            for n in 1..=COUNT {
                let min = chip.min + Vec2::new(pad + (n - 1) as f32 * (cell.x + gap), pad);
                let rect = Rect::from_min_size(min, cell);
                let response = ui.interact(rect, Id::new("workspace_cell").with(n), Sense::click());
                let active = n == current();
                let over = pointer.is_some_and(|p| rect.contains(p));
                let target = dragging && over;
                let fill = if active {
                    theme::ACCENT
                } else if target || response.hovered() {
                    theme::ELEVATED
                } else {
                    Color32::TRANSPARENT
                };
                let inner = Rounding::same(theme::RADIUS - 2.0);
                painter.rect_filled(rect, inner, fill);
                if target {
                    painter.rect_stroke(rect, inner, Stroke::new(1.5_f32, theme::ACCENT));
                }
                painter.text(
                    rect.center(),
                    Align2::CENTER_CENTER,
                    n.to_string(),
                    FontId::proportional(13.0),
                    if active { Color32::WHITE } else { theme::TEXT_DIM },
                );
                if released && over {
                    dropped_on = Some(n);
                }
                if response.clicked() {
                    CURRENT.store(n, Ordering::Relaxed);
                }
            }
        });

    if released {
        let start = with_state(|s| s.pressed.take());
        if let (Some(n), Some((owner, at))) = (dropped_on, start) {
            send(&owner, at, n);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_workspaces_parse_with_one_as_the_default() {
        assert_eq!(parse(None), 1);
        assert_eq!(parse(Some("2")), 2);
        assert_eq!(parse(Some(" 1 ")), 1);
        assert_eq!(parse(Some("0")), 1);
        assert_eq!(parse(Some("9")), 1);
        assert_eq!(parse(Some("two")), 1);
    }
}
