//! Hit marker and damage numbers, on a shared damage tracker.
//!
//! The server syncs every living entity's health to the client. Once a tick
//! the tracker reads the health of the living entities nearby; a drop is
//! damage. A hit counts as **yours** when the entity was under your
//! crosshair or your aura's target within the last second — that is what
//! lights the hit marker; damage numbers can show every hit or only yours.
//!
//! The tracker runs only while one of its users (HitMarker, DamageNumbers,
//! TargetHUD) is enabled, once per game tick however many call it.

use crate::graphic::hook::game_clock;
use crate::mapping::entity::{EntityRef, LivingEntityRef};
use crate::mapping::entity::living::LivingEntity;
use crate::mapping::raw;
use crate::mapping::MappedObject;
use crate::module::{KeyboardKey, Module, ModuleCategory, ModuleData, ModuleId, ModuleSetting};
use crate::state::{client, mapping, minecraft};
use egui::{Align2, Color32, Context, FontId, Id, LayerId, Order, Painter, Pos2, Stroke, Vec2};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Entities farther than this are not tracked.
const RANGE: f64 = 32.0;
/// How long a crosshair / aura target still makes a hit "yours".
const YOURS_FOR: Duration = Duration::from_millis(1000);
/// How long a damage number floats.
const NUMBER_LIFE: Duration = Duration::from_millis(1100);

/// One drop in health.
#[derive(Debug, Clone)]
pub struct Damage {
    pub id: i32,
    /// Just above the entity's head.
    pub pos: [f64; 3],
    pub amount: f32,
    pub yours: bool,
    pub kill: bool,
    pub at: Instant,
}

struct Tracker {
    /// Game tick of the last scan.
    tick: Option<u64>,
    health: HashMap<i32, f32>,
    /// Entities lately under the crosshair or targeted, and when.
    aimed: HashMap<i32, Instant>,
    /// The entity under the crosshair at the last scan.
    crosshair: Option<i32>,
    damage: Vec<Damage>,
    /// The last hit of yours: when, and whether it killed.
    last_hit: Option<(Instant, bool)>,
}

static TRACKER: Mutex<Option<Tracker>> = Mutex::new(None);

fn with_tracker<R>(f: impl FnOnce(&mut Tracker) -> R) -> R {
    let mut guard = TRACKER.lock().unwrap_or_else(|p| p.into_inner());
    f(guard.get_or_insert_with(|| Tracker {
        tick: None,
        health: HashMap::new(),
        aimed: HashMap::new(),
        crosshair: None,
        damage: Vec::new(),
        last_hit: None,
    }))
}

/// Pure: the damage a health change means, if any — a drop larger than
/// rounding noise.
pub fn damage_from(before: f32, after: f32) -> Option<f32> {
    let drop = before - after;
    (drop > 0.01).then_some(drop)
}

/// Pure: a damage amount as a label — whole numbers without decimals.
pub fn label(amount: f32) -> String {
    if (amount - amount.round()).abs() < 0.05 {
        format!("{:.0}", amount.round())
    } else {
        format!("{amount:.1}")
    }
}

/// Scans the nearby entities once per game tick.
pub fn tick() {
    let now_tick = game_clock().0;
    if with_tracker(|t| t.tick == Some(now_tick)) {
        return;
    }
    with_tracker(|t| t.tick = Some(now_tick));
    if let Err(error) = scan() {
        log::debug!("Hit tracker: {error}");
    }
}

/// The entity under the crosshair, if it is one.
fn crosshair_entity() -> anyhow::Result<Option<i32>> {
    mapping().in_frame(|| {
        let mut env = mapping().get_env()?;
        let hit = raw::field(&mut env, minecraft().jni_ref().as_obj(), "hitResult", "Lnet/minecraft/world/phys/HitResult;")?.l()?;
        if hit.is_null() {
            return Ok(None);
        }
        let kind = raw::call(&mut env, &hit, "getType", "()Lnet/minecraft/world/phys/HitResult$Type;", &[])?.l()?;
        if raw::to_string(&mut env, &kind)? != "ENTITY" {
            return Ok(None);
        }
        let entity = raw::call(&mut env, &hit, "getEntity", "()Lnet/minecraft/world/entity/Entity;", &[])?.l()?;
        Ok(Some(raw::call(&mut env, &entity, "getId", "()I", &[])?.i()?))
    })
}

fn scan() -> anyhow::Result<()> {
    let mc = minecraft();
    let (Some(world), Some(player)) = (mc.world()?, mc.player()?) else {
        with_tracker(|t| t.health.clear());
        return Ok(());
    };
    let local = player.id()?;
    let me = player.get_position()?;
    let now = Instant::now();

    let crosshair = crosshair_entity().unwrap_or(None);
    with_tracker(|t| {
        t.crosshair = crosshair;
        if let Some(id) = crosshair {
            t.aimed.insert(id, now);
        }
        if let Some(id) = crate::module::combat::combat_target(Duration::from_millis(300)) {
            t.aimed.insert(id, now);
        }
        t.aimed.retain(|_, at| at.elapsed() <= YOURS_FOR);
    });

    let mut seen = HashMap::new();
    let mut fresh = Vec::new();
    for entity in world.get_entities()? {
        if !entity.instance_of::<LivingEntity>() {
            continue;
        }
        let Ok(id) = entity.id() else { continue };
        if id == local {
            continue;
        }
        if entity.distance_to_sqr(me.x(), me.y(), me.z()).unwrap_or(f64::MAX) > RANGE * RANGE {
            continue;
        }
        let Some(living) = entity.as_living() else { continue };
        let Ok(health) = living.get_health() else { continue };
        seen.insert(id, health);
        let before = with_tracker(|t| t.health.get(&id).copied());
        if let Some(amount) = before.and_then(|b| damage_from(b, health)) {
            let pos = entity.get_position()?;
            let height = entity.bb_height().unwrap_or(1.8) as f64;
            fresh.push((id, [pos.x(), pos.y() + height + 0.3, pos.z()], amount, health <= 0.0));
        }
    }

    with_tracker(|t| {
        t.health = seen;
        for (id, pos, amount, kill) in fresh {
            let yours = t.aimed.contains_key(&id);
            if yours {
                t.last_hit = Some((now, kill));
            }
            t.damage.push(Damage { id, pos, amount, yours, kill, at: now });
        }
        t.damage.retain(|d| d.at.elapsed() < NUMBER_LIFE);
        if t.damage.len() > 64 {
            let excess = t.damage.len() - 64;
            t.damage.drain(..excess);
        }
    });
    Ok(())
}

/// The entity under the crosshair at the last scan.
pub fn crosshair_target() -> Option<i32> {
    with_tracker(|t| t.crosshair)
}

/// The most recently hit entity of yours within `max_age`.
pub fn last_hit_entity(max_age: Duration) -> Option<i32> {
    with_tracker(|t| {
        t.damage
            .iter()
            .rev()
            .find(|d| d.yours && d.at.elapsed() <= max_age)
            .map(|d| d.id)
    })
}

fn enabled(id: ModuleId) -> Option<ModuleData> {
    let handle = client().modules.get(id)?;
    let module = handle.lock().ok()?;
    let data = module.get_module_data();
    data.enabled.then(|| data.clone())
}

fn color(data: &ModuleData, name: &str, fallback: [f32; 4]) -> Color32 {
    let [r, g, b, a] = match data.get_setting(name) {
        Some(ModuleSetting::Color { value, .. }) => *value,
        _ => fallback,
    };
    Color32::from_rgba_unmultiplied((r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8, (a * 255.0) as u8)
}

fn slider(data: &ModuleData, name: &str, fallback: f32) -> f32 {
    data.get_setting(name).and_then(ModuleSetting::get_slider_value).unwrap_or(fallback)
}

fn toggle(data: &ModuleData, name: &str, fallback: bool) -> bool {
    data.get_setting(name).and_then(ModuleSetting::get_toggle_value).unwrap_or(fallback)
}

/// Whether the damage numbers want to be drawn in the world this frame.
pub fn wants_world() -> bool {
    client().modules.is_active()
        && enabled(ModuleId::DamageNumbers).is_some()
        && with_tracker(|t| !t.damage.is_empty())
}

/// Draws the floating damage numbers; `project` maps a world point to the
/// screen (the ESP's projection).
pub fn draw_world(painter: &Painter, project: &dyn Fn([f64; 3]) -> Option<Pos2>) {
    let Some(data) = enabled(ModuleId::DamageNumbers) else {
        return;
    };
    let only_yours = toggle(&data, "Only my hits", false);
    let scale = slider(&data, "Size", 1.0);
    let yours_color = color(&data, "My hits", [1.0, 0.82, 0.25, 1.0]);
    let other_color = color(&data, "Other hits", [0.9, 0.9, 0.9, 1.0]);
    let kill_color = Color32::from_rgb(255, 80, 80);
    let numbers = with_tracker(|t| t.damage.clone());
    for d in numbers.iter().filter(|d| d.yours || !only_yours) {
        let t = (d.at.elapsed().as_secs_f32() / NUMBER_LIFE.as_secs_f32()).clamp(0.0, 1.0);
        // Float up, fade out over the last half.
        let pos = [d.pos[0], d.pos[1] + 0.8 * t as f64, d.pos[2]];
        let Some(screen) = project(pos) else { continue };
        let alpha = if t < 0.5 { 1.0 } else { 1.0 - (t - 0.5) * 2.0 };
        let base = if d.kill { kill_color } else if d.yours { yours_color } else { other_color };
        let text = label(d.amount);
        let font = FontId::proportional(15.0 * scale * if d.yours { 1.15 } else { 1.0 });
        let shadow = Color32::from_black_alpha((200.0 * alpha) as u8);
        painter.text(screen + Vec2::new(1.0, 1.0), Align2::CENTER_CENTER, &text, font.clone(), shadow);
        painter.text(screen, Align2::CENTER_CENTER, &text, font, base.gamma_multiply(alpha));
    }
}

/// Draws the hit marker around the screen centre.
pub fn draw_screen(ctx: &Context) {
    let Some(data) = enabled(ModuleId::HitMarker) else {
        return;
    };
    if !crate::module::render::crosshair::world_view_visible() {
        return;
    }
    let duration = Duration::from_millis(slider(&data, "Duration (ms)", 350.0) as u64);
    let Some((at, kill)) = with_tracker(|t| t.last_hit) else {
        return;
    };
    let age = at.elapsed();
    if age > duration {
        return;
    }
    let t = age.as_secs_f32() / duration.as_secs_f32().max(0.01);
    let alpha = 1.0 - t * t;
    let base = if kill {
        color(&data, "Kill color", [1.0, 0.3, 0.3, 1.0])
    } else {
        color(&data, "Color", [1.0, 1.0, 1.0, 1.0])
    };
    let size = slider(&data, "Size", 6.0);
    // Pops out slightly as it fades.
    let inner = 4.0 + 2.0 * t;
    let outer = inner + size;
    let center = ctx.screen_rect().center();
    let painter = ctx.layer_painter(LayerId::new(Order::Background, Id::new("hit_marker")));
    let stroke = Stroke::new(2.0_f32, base.gamma_multiply(alpha));
    let outline = Stroke::new(4.0_f32, Color32::from_black_alpha((140.0 * alpha) as u8));
    for (dx, dy) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
        let dir = Vec2::new(dx, dy).normalized();
        let segment = [center + dir * inner, center + dir * outer];
        painter.line_segment(segment, outline);
        painter.line_segment(segment, stroke);
    }
}

macro_rules! tracker_module {
    ($name:ident, $id:expr, $description:expr, $settings:expr) => {
        #[derive(Debug)]
        pub struct $name {
            pub module: ModuleData,
        }

        impl $name {
            pub fn new() -> Self {
                Self {
                    module: ModuleData {
                        id: $id,
                        description: $description.to_string(),
                        category: ModuleCategory::Render,
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
                tick();
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

fn color_setting(name: &str, value: [f32; 4]) -> ModuleSetting {
    ModuleSetting::Color { name: name.to_string(), value }
}

fn slider_setting(name: &str, value: f32, min: f32, max: f32) -> ModuleSetting {
    ModuleSetting::Slider { name: name.to_string(), value, min, max }
}

tracker_module!(
    HitMarkerModule,
    ModuleId::HitMarker,
    "Flashes an X at the crosshair when your hit lands",
    vec![
        color_setting("Color", [1.0, 1.0, 1.0, 1.0]),
        color_setting("Kill color", [1.0, 0.3, 0.3, 1.0]),
        slider_setting("Size", 6.0, 3.0, 16.0),
        slider_setting("Duration (ms)", 350.0, 100.0, 1000.0),
    ]
);

tracker_module!(
    DamageNumbersModule,
    ModuleId::DamageNumbers,
    "Shows damage numbers floating above hit mobs and players",
    vec![
        ModuleSetting::Toggle { name: "Only my hits".to_string(), value: false },
        color_setting("My hits", [1.0, 0.82, 0.25, 1.0]),
        color_setting("Other hits", [0.9, 0.9, 0.9, 1.0]),
        slider_setting("Size", 1.0, 0.6, 2.0),
    ]
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_drops_count_as_damage() {
        assert_eq!(damage_from(20.0, 14.5), Some(5.5));
        assert_eq!(damage_from(14.0, 20.0), None); // healing
        assert_eq!(damage_from(20.0, 19.999), None); // noise
    }

    #[test]
    fn labels_drop_needless_decimals() {
        assert_eq!(label(6.0), "6");
        assert_eq!(label(5.5), "5.5");
        assert_eq!(label(3.02), "3");
    }
}
