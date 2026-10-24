//! Custom Crosshair — replaces the vanilla crosshair with a configurable one.
//!
//! The vanilla crosshair layer is cancelled through NeoForge's
//! `RenderGuiLayerEvent.Pre` (`minecraft:crosshair`, as NoRender does); ours
//! is drawn on the overlay. It shows only where the vanilla one would: in a
//! world, first person, with no screen open and the HUD not hidden (F1).

use crate::graphic::input::GUI_OPEN;
use crate::mapping::raw;
use crate::mapping::MappedObject;
use crate::module::optimization::java::{self, EventListeners};
use crate::module::{KeyboardKey, Module, ModuleCategory, ModuleData, ModuleId, ModuleSetting};
use crate::state::{client, mapping, minecraft};
use egui::{Color32, Context, Id, LayerId, Order, Pos2, Stroke, Vec2};
use std::sync::atomic::Ordering;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const STYLES: [&str; 6] = ["Cross", "Cross + dot", "Dot", "Circle", "Circle + dot", "T-shape"];

#[derive(Debug)]
pub struct CrosshairModule {
    pub module: ModuleData,
}

impl CrosshairModule {
    pub fn new() -> Self {
        let slider = |name: &str, value: f32, min: f32, max: f32| ModuleSetting::Slider {
            name: name.to_string(),
            value,
            min,
            max,
        };
        Self {
            module: ModuleData {
                id: ModuleId::Crosshair,
                description: "Your own crosshair instead of the vanilla one".to_string(),
                category: ModuleCategory::Render,
                key_bind: KeyboardKey::KeyNone,
                enabled: false,
                settings: vec![
                    ModuleSetting::Choice {
                        name: "Crosshair style".to_string(),
                        value: 0,
                        options: STYLES.iter().map(|s| s.to_string()).collect(),
                    },
                    slider("Length", 6.0, 2.0, 20.0),
                    slider("Gap", 3.0, 0.0, 12.0),
                    slider("Thickness", 2.0, 1.0, 5.0),
                    ModuleSetting::Color {
                        name: "Crosshair color".to_string(),
                        value: [0.3, 1.0, 0.45, 1.0],
                    },
                    ModuleSetting::Toggle {
                        name: "Outline".to_string(),
                        value: true,
                    },
                ],
            },
        }
    }
}

impl Module for CrosshairModule {
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

/// Last answer of [`world_view_visible`] and when it was computed.
static VISIBLE: Mutex<Option<(Instant, bool)>> = Mutex::new(None);

/// Whether the game's own view is on screen as the crosshair sees it: in a
/// world, first person, no screen or menu open, HUD not hidden with F1.
/// Cached for a few milliseconds — asked several times a frame.
pub fn world_view_visible() -> bool {
    let mut cache = VISIBLE.lock().unwrap_or_else(|p| p.into_inner());
    if let Some((at, value)) = *cache {
        if at.elapsed() < Duration::from_millis(8) {
            return value;
        }
    }
    let value = compute_visible().unwrap_or(false);
    *cache = Some((Instant::now(), value));
    value
}

fn compute_visible() -> anyhow::Result<bool> {
    if !client().modules.is_active() || GUI_OPEN.load(Ordering::Relaxed) {
        return Ok(false);
    }
    if minecraft().current_screen().is_open() {
        return Ok(false);
    }
    mapping().in_frame(|| {
        let mut env = mapping().get_env()?;
        let options = raw::field(&mut env, minecraft().jni_ref().as_obj(), "options", "Lnet/minecraft/client/Options;")?.l()?;
        if raw::field(&mut env, &options, "hideGui", "Z")?.z()? {
            return Ok(false);
        }
        let camera = raw::call(&mut env, &options, "getCameraType", "()Lnet/minecraft/client/CameraType;", &[])?.l()?;
        Ok(raw::call(&mut env, &camera, "isFirstPerson", "()Z", &[])?.z()?)
    })
}

struct Vanilla {
    listeners: EventListeners,
}

static VANILLA: Mutex<Option<Vanilla>> = Mutex::new(None);

fn settings() -> Option<ModuleData> {
    let handle = client().modules.get(ModuleId::Crosshair)?;
    let module = handle.lock().ok()?;
    let data = module.get_module_data();
    data.enabled.then(|| data.clone())
}

/// Per frame: hides the vanilla crosshair while ours is in use and draws
/// ours. Runs even under Hide HUD — then the vanilla one comes back.
pub fn frame(ctx: &Context, hidden: bool) {
    let data = settings().filter(|_| !hidden && client().modules.is_active());
    {
        let mut guard = VANILLA.lock().unwrap_or_else(|p| p.into_inner());
        let vanilla = guard.get_or_insert_with(|| Vanilla { listeners: EventListeners::default() });
        let signature = if data.is_some() { "hide" } else { "" };
        vanilla.listeners.ensure(
            "crosshair",
            "net/neoforged/neoforge/client/event/RenderGuiLayerEvent$Pre",
            signature.to_string(),
            |env, class| {
                let object = java::jclass(env, "java/lang/Object")?;
                java::cancel_when(env, class, &[(class, "getName"), (&object, "toString")], &["minecraft:crosshair".to_string()])
            },
        );
    }
    let Some(data) = data else { return };
    if !world_view_visible() {
        return;
    }
    draw(ctx, &data);
}

fn draw(ctx: &Context, data: &ModuleData) {
    let slider = |name: &str, fallback: f32| data.get_setting(name).and_then(ModuleSetting::get_slider_value).unwrap_or(fallback);
    let style = match data.get_setting("Crosshair style") {
        Some(ModuleSetting::Choice { value, .. }) => *value,
        _ => 0,
    };
    let color = match data.get_setting("Crosshair color") {
        Some(ModuleSetting::Color { value: [r, g, b, a], .. }) => {
            Color32::from_rgba_unmultiplied((r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8, (a * 255.0) as u8)
        }
        _ => Color32::from_rgb(76, 255, 115),
    };
    let outline = data.get_setting("Outline").and_then(ModuleSetting::get_toggle_value).unwrap_or(true);
    let (length, gap, thickness) = (slider("Length", 6.0), slider("Gap", 3.0), slider("Thickness", 2.0));

    let center = ctx.screen_rect().center();
    // Snap to the pixel grid so thin lines stay crisp.
    let center = Pos2::new(center.x.round() + 0.5 * (thickness as i32 % 2) as f32, center.y.round() + 0.5 * (thickness as i32 % 2) as f32);
    let painter = ctx.layer_painter(LayerId::new(Order::Background, Id::new("custom_crosshair")));
    let line = |from: Vec2, to: Vec2| {
        let segment = [center + from, center + to];
        if outline {
            painter.line_segment(segment, Stroke::new(thickness + 2.0, Color32::from_black_alpha(170)));
        }
        painter.line_segment(segment, Stroke::new(thickness, color));
    };
    let arms = |directions: &[(f32, f32)]| {
        for &(dx, dy) in directions {
            let dir = Vec2::new(dx, dy);
            line(dir * gap, dir * (gap + length));
        }
    };
    let dot = || {
        let radius = (thickness * 0.75).max(1.2);
        if outline {
            painter.circle_filled(center, radius + 1.0, Color32::from_black_alpha(170));
        }
        painter.circle_filled(center, radius, color);
    };
    let circle = || {
        let radius = gap + length * 0.5;
        if outline {
            painter.circle_stroke(center, radius, Stroke::new(thickness + 2.0, Color32::from_black_alpha(170)));
        }
        painter.circle_stroke(center, radius, Stroke::new(thickness, color));
    };
    const CROSS: [(f32, f32); 4] = [(1.0, 0.0), (-1.0, 0.0), (0.0, 1.0), (0.0, -1.0)];
    match STYLES.get(style).copied().unwrap_or("Cross") {
        "Cross + dot" => {
            arms(&CROSS);
            dot();
        }
        "Dot" => dot(),
        "Circle" => circle(),
        "Circle + dot" => {
            circle();
            dot();
        }
        "T-shape" => arms(&[(1.0, 0.0), (-1.0, 0.0), (0.0, 1.0)]),
        _ => arms(&CROSS),
    }
}

/// Unregisters the listener — on unload, so the vanilla crosshair returns.
pub fn cleanup() {
    if let Some(mut vanilla) = VANILLA.lock().unwrap_or_else(|p| p.into_inner()).take() {
        vanilla.listeners.clear();
    }
}
