//! TargetHUD — a card with the current target: name, health (with
//! absorption), distance, armour and held item.
//!
//! The target is, in order: the aura's target, the entity under the
//! crosshair, the entity you last hit (for 3 s). Its details are read ten
//! times a second, not every frame; the health bar eases between readings.

use crate::graphic::anim;
use crate::graphic::theme;
use crate::mapping::raw;
use crate::mapping::MappedObject;
use crate::module::render::hit_effects;
use crate::module::{KeyboardKey, Module, ModuleCategory, ModuleData, ModuleId, ModuleSetting};
use crate::state::{client, mapping, minecraft};
use egui::{Align2, Color32, Context, FontId, Id, LayerId, Order, Pos2, Rect, Rounding, Stroke, Vec2};
use jni::objects::{JObject, JValue};
use jni::JNIEnv;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const POSITIONS: [&str; 2] = ["Near crosshair", "Above hotbar"];
/// How often the target's details are re-read.
const REFRESH: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Default)]
struct Info {
    name: String,
    health: f32,
    max_health: f32,
    absorption: f32,
    armor: i32,
    distance: f64,
    held: String,
}

struct Cache {
    at: Option<Instant>,
    target: Option<i32>,
    info: Option<Info>,
}

static CACHE: Mutex<Cache> = Mutex::new(Cache { at: None, target: None, info: None });

#[derive(Debug)]
pub struct TargetHudModule {
    pub module: ModuleData,
}

impl TargetHudModule {
    pub fn new() -> Self {
        Self {
            module: ModuleData {
                id: ModuleId::TargetHud,
                description: "Shows your target's health, armour and item".to_string(),
                category: ModuleCategory::Render,
                key_bind: KeyboardKey::KeyNone,
                enabled: false,
                settings: vec![ModuleSetting::Choice {
                    name: "TargetHUD position".to_string(),
                    value: 0,
                    options: POSITIONS.iter().map(|s| s.to_string()).collect(),
                }],
            },
        }
    }
}

impl Module for TargetHudModule {
    fn on_start(&self) -> anyhow::Result<()> {
        Ok(())
    }

    fn on_stop(&self) -> anyhow::Result<()> {
        let mut cache = CACHE.lock().unwrap_or_else(|p| p.into_inner());
        cache.info = None;
        cache.target = None;
        Ok(())
    }

    fn on_tick(&self) -> anyhow::Result<()> {
        hit_effects::tick();
        Ok(())
    }

    fn get_module_data(&self) -> &ModuleData {
        &self.module
    }

    fn get_module_data_mut(&mut self) -> &mut ModuleData {
        &mut self.module
    }
}

/// The entity to show, by priority.
fn pick_target() -> Option<i32> {
    crate::module::combat::combat_target(Duration::from_millis(500))
        .or_else(hit_effects::crosshair_target)
        .or_else(|| hit_effects::last_hit_entity(Duration::from_secs(3)))
}

fn read_info(id: i32) -> anyhow::Result<Option<Info>> {
    let mc = minecraft();
    let (Some(world), Some(player)) = (mc.world()?, mc.player()?) else {
        return Ok(None);
    };
    mapping().in_frame(|| {
        let mut env = mapping().get_env()?;
        let entity = raw::call(
            &mut env,
            world.jni_ref().as_obj(),
            "getEntity",
            "(I)Lnet/minecraft/world/entity/Entity;",
            &[JValue::Int(id)],
        )?
        .l()?;
        if entity.is_null() {
            return Ok(None);
        }
        let living = mapping().resolve_class(&mut env, "net/minecraft/world/entity/LivingEntity")?;
        if !env.is_instance_of(&entity, &living)? {
            return Ok(None);
        }
        let float = |env: &mut JNIEnv, name: &str| -> anyhow::Result<f32> { Ok(raw::call(env, &entity, name, "()F", &[])?.f()?) };
        let name = component_text(&mut env, &entity, "getName")?;
        let distance = raw::call(
            &mut env,
            &entity,
            "distanceTo",
            "(Lnet/minecraft/world/entity/Entity;)F",
            &[JValue::Object(player.jni_ref().as_obj())],
        )?
        .f()? as f64;
        let stack = raw::call(&mut env, &entity, "getMainHandItem", "()Lnet/minecraft/world/item/ItemStack;", &[])?.l()?;
        let held = if raw::call(&mut env, &stack, "isEmpty", "()Z", &[])?.z()? {
            String::new()
        } else {
            component_text(&mut env, &stack, "getHoverName")?
        };
        Ok(Some(Info {
            name,
            health: float(&mut env, "getHealth")?,
            max_health: float(&mut env, "getMaxHealth")?,
            absorption: float(&mut env, "getAbsorptionAmount").unwrap_or(0.0),
            armor: raw::call(&mut env, &entity, "getArmorValue", "()I", &[]).and_then(|v| Ok(v.i()?)).unwrap_or(0),
            distance,
            held,
        }))
    })
}

/// `obj.getter().getString()` — a `Component` as plain text.
fn component_text(env: &mut JNIEnv, obj: &JObject, getter: &str) -> anyhow::Result<String> {
    let component = raw::call(env, obj, getter, "()Lnet/minecraft/network/chat/Component;", &[])?.l()?;
    let text = raw::call(env, &component, "getString", "()Ljava/lang/String;", &[])?.l()?;
    mapping().get_string(text)
}

fn current() -> Option<Info> {
    let mut cache = CACHE.lock().unwrap_or_else(|p| p.into_inner());
    if cache.at.is_none_or(|at| at.elapsed() >= REFRESH) {
        cache.at = Some(Instant::now());
        let target = pick_target();
        cache.target = target;
        cache.info = match target.map(read_info) {
            Some(Ok(info)) => info,
            Some(Err(error)) => {
                log::debug!("TargetHUD: {error}");
                None
            }
            None => None,
        };
    }
    cache.info.clone()
}

/// Pure: the health bar colour — red when low, green when full.
pub fn health_color(fraction: f32) -> Color32 {
    let f = fraction.clamp(0.0, 1.0);
    let (r, g) = if f < 0.5 { (1.0, f * 2.0) } else { (2.0 - f * 2.0, 1.0) };
    Color32::from_rgb((r * 230.0) as u8, (g * 200.0) as u8, 70)
}

/// Draws the card, if enabled and there is a target.
pub fn draw(ctx: &Context) {
    let Some(handle) = client().modules.get(ModuleId::TargetHud) else { return };
    let position = {
        let Ok(module) = handle.lock() else { return };
        let data = module.get_module_data();
        if !data.enabled {
            return;
        }
        match data.get_setting("TargetHUD position") {
            Some(ModuleSetting::Choice { value, .. }) => *value,
            _ => 0,
        }
    };
    if !client().modules.is_active() || crate::graphic::input::GUI_OPEN.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let shown = current();
    let visible = anim::toggle(ctx, Id::new("target_hud_visible"), shown.is_some(), 0.15, anim::Easing::InOut);
    // Keep the last card while it fades out.
    static LAST: Mutex<Option<Info>> = Mutex::new(None);
    let info = {
        let mut last = LAST.lock().unwrap_or_else(|p| p.into_inner());
        if shown.is_some() {
            *last = shown;
        }
        if visible <= 0.0 {
            return;
        }
        match last.clone() {
            Some(info) => info,
            None => return,
        }
    };

    let size = Vec2::new(210.0, 58.0);
    let screen = ctx.screen_rect();
    let min = match position {
        1 => Pos2::new(screen.center().x - size.x * 0.5, screen.bottom() - 120.0 - size.y),
        _ => screen.center() + Vec2::new(28.0, 22.0),
    };
    let rect = Rect::from_min_size(min + Vec2::new(0.0, 8.0 * (1.0 - visible)), size);
    let painter = ctx.layer_painter(LayerId::new(Order::Background, Id::new("target_hud")));
    let fade = |c: Color32| c.gamma_multiply(visible);

    painter.rect_filled(rect, Rounding::same(theme::RADIUS), fade(Color32::from_black_alpha(185)));
    painter.rect_stroke(rect, Rounding::same(theme::RADIUS), Stroke::new(1.0_f32, fade(theme::BORDER)));

    let pad = 9.0;
    painter.text(rect.min + Vec2::new(pad, pad), Align2::LEFT_TOP, &info.name, FontId::proportional(14.0), fade(theme::TEXT));
    let hp = if info.absorption > 0.0 {
        format!("{:.1} +{:.0}", info.health, info.absorption)
    } else {
        format!("{:.1} / {:.0}", info.health, info.max_health)
    };
    painter.text(
        Pos2::new(rect.right() - pad, rect.top() + pad + 1.0),
        Align2::RIGHT_TOP,
        hp,
        FontId::proportional(12.5),
        fade(theme::TEXT_DIM),
    );

    // Health bar, eased between readings.
    let fraction = if info.max_health > 0.0 { info.health / info.max_health } else { 0.0 };
    let eased = anim::ease_to(ctx, Id::new("target_hud_health"), fraction.clamp(0.0, 1.0), 0.25, anim::Easing::Out);
    let bar = Rect::from_min_size(Pos2::new(rect.left() + pad, rect.top() + 29.0), Vec2::new(size.x - pad * 2.0, 6.0));
    painter.rect_filled(bar, Rounding::same(3.0), fade(Color32::from_white_alpha(25)));
    let filled = Rect::from_min_size(bar.min, Vec2::new(bar.width() * eased, bar.height()));
    painter.rect_filled(filled, Rounding::same(3.0), fade(health_color(eased)));
    if info.absorption > 0.0 && info.max_health > 0.0 {
        let extra = (info.absorption / info.max_health).min(1.0);
        let gold = Rect::from_min_size(bar.min, Vec2::new(bar.width() * extra, 2.0));
        painter.rect_filled(gold, Rounding::same(1.0), fade(Color32::from_rgb(255, 200, 60)));
    }

    let mut details = format!("{:.1} m", info.distance);
    if info.armor > 0 {
        details.push_str(&format!("  ·  armor {}", info.armor));
    }
    if !info.held.is_empty() {
        details.push_str(&format!("  ·  {}", info.held));
    }
    painter.text(
        Pos2::new(rect.left() + pad, rect.bottom() - pad),
        Align2::LEFT_BOTTOM,
        details,
        FontId::proportional(11.5),
        fade(theme::TEXT_DIM),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_goes_from_red_to_green() {
        let low = health_color(0.0);
        let full = health_color(1.0);
        assert!(low.r() > low.g());
        assert!(full.g() > full.r());
    }
}
