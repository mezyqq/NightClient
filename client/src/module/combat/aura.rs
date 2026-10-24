use crate::mapping::entity::mob::Mob;
use crate::mapping::entity::player::{LocalPlayer, Player};
use crate::mapping::entity::{Entity, EntityRef, LivingEntityRef, PlayerRef};
use jni::objects::JValue;
use crate::mapping::MappedObject;
use crate::module::combat::criticals::{self, CritState};
use crate::module::combat::targets::{self, TargetList};
use crate::module::combat::rotation::Rotation;
use crate::module::combat::{is_protected, look_at, pick_target_by, set_combat_target};
use crate::mapping::math::Vec3;
use crate::module::{KeyboardKey, Module, ModuleCategory, ModuleData, ModuleId, ModuleSetting};
use crate::mapping::raw;
use crate::state::{mapping, minecraft};
use std::collections::BTreeSet;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Which entities an aura attacks.
#[derive(Debug, Clone, Copy)]
pub enum AuraTarget {
    Players,
    Mobs,
}

impl AuraTarget {
    /// Name of the choice setting picking which of these to attack.
    pub fn filter_setting(self) -> &'static str {
        match self {
            AuraTarget::Players => "Players",
            AuraTarget::Mobs => "Mobs",
        }
    }

    /// The list the "listed" filter modes read.
    pub fn list(self) -> TargetList {
        match self {
            AuraTarget::Players => TargetList::Players,
            AuraTarget::Mobs => TargetList::Mobs,
        }
    }

    /// Settings only this kind of aura has.
    fn extra_settings(self) -> Vec<ModuleSetting> {
        let toggle = |name: &str, value: bool| ModuleSetting::Toggle { name: name.to_string(), value };
        let choice = |name: &str, options: &[&str]| ModuleSetting::Choice {
            name: name.to_string(),
            value: 0,
            options: options.iter().map(|o| o.to_string()).collect(),
        };
        match self {
            AuraTarget::Players => vec![],
            AuraTarget::Mobs => vec![
                choice("Neutral mobs", &["Attack", "Only when angry", "Skip"]),
                choice("Mob priority", &["Nearest", "Lowest health", "Crosshair"]),
                toggle("Skip pets", true),
                toggle("Skip babies", false),
                toggle("Skip named", false),
                toggle("Through walls", true),
            ],
        }
    }

    fn filter_options(self) -> Vec<String> {
        let options: &[&str] = match self {
            AuraTarget::Players => &["All", "Only listed", "All except listed"],
            AuraTarget::Mobs => &["Hostile", "Passive", "All", "Only listed", "All except listed"],
        };
        options.iter().map(|o| o.to_string()).collect()
    }
}

/// The filter chosen in an aura's choice setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    All,
    Hostile,
    Passive,
    OnlyListed,
    ExceptListed,
}

impl Filter {
    /// Whether this filter reads the target list (and so shows its window).
    pub fn uses_list(self) -> bool {
        matches!(self, Filter::OnlyListed | Filter::ExceptListed)
    }
}

/// How MobAura treats neutral mobs (`NeutralMob`: endermen, zombified
/// piglins, wolves, bees, iron golems, polar bears, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Neutral {
    Attack,
    /// Only while angry at someone.
    WhenAngry,
    Skip,
}

/// Which of the target candidates MobAura goes for first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    Nearest,
    LowestHealth,
    Crosshair,
}

/// MobAura's per-mob rules, read from its settings once per tick.
#[derive(Debug, Clone, Copy)]
pub struct MobRules {
    pub filter: Filter,
    pub neutral: Neutral,
    pub skip_babies: bool,
    pub skip_named: bool,
    pub skip_pets: bool,
}

/// What MobAura knows about a candidate mob.
#[derive(Debug, Clone, Default)]
pub struct MobFacts {
    /// Implements `Enemy` (hostile).
    pub enemy: bool,
    /// Implements `NeutralMob`.
    pub neutral: bool,
    pub angry: bool,
    pub baby: bool,
    /// Carries a name tag.
    pub named: bool,
    /// Tamed / owned by a player.
    pub owned: bool,
    /// `minecraft:zombie`, …
    pub type_id: Option<String>,
}

/// Pure: whether MobAura attacks a mob. The "never attack" list always wins;
/// with the "Only listed" filter the target list is an explicit choice and
/// overrides the neutral rule (but never protects pets / babies / named mobs
/// less).
pub fn mob_allowed(
    rules: &MobRules,
    facts: &MobFacts,
    listed: &BTreeSet<String>,
    ignored: &BTreeSet<String>,
) -> bool {
    let id = facts.type_id.as_deref();
    if id.is_some_and(|id| ignored.contains(id)) {
        return false;
    }
    if (rules.skip_babies && facts.baby)
        || (rules.skip_named && facts.named)
        || (rules.skip_pets && facts.owned)
    {
        return false;
    }
    if rules.filter != Filter::OnlyListed && facts.neutral {
        match rules.neutral {
            Neutral::Skip => return false,
            Neutral::WhenAngry if !facts.angry => return false,
            _ => {}
        }
    }
    let listed_here = id.is_some_and(|id| listed.contains(id));
    match rules.filter {
        Filter::All => true,
        Filter::Hostile => facts.enemy,
        // A neutral mob that passed the rule above counts as fair game here.
        Filter::Passive => !facts.enemy,
        Filter::OnlyListed => listed_here,
        Filter::ExceptListed => !listed_here,
    }
}

/// Decodes the choice index of `target`'s filter setting.
pub fn filter_from_choice(target: AuraTarget, index: usize) -> Filter {
    match (target, index) {
        (AuraTarget::Players, 1) => Filter::OnlyListed,
        (AuraTarget::Players, 2) => Filter::ExceptListed,
        (AuraTarget::Players, _) => Filter::All,
        (AuraTarget::Mobs, 0) => Filter::Hostile,
        (AuraTarget::Mobs, 1) => Filter::Passive,
        (AuraTarget::Mobs, 3) => Filter::OnlyListed,
        (AuraTarget::Mobs, 4) => Filter::ExceptListed,
        (AuraTarget::Mobs, _) => Filter::All,
    }
}

/// What an aura is looking for this tick.
struct Selection {
    /// Only `filter` matters to KillAura.
    rules: MobRules,
    /// The target list (lower-cased player names / entity type ids).
    listed: BTreeSet<String>,
    /// MobAura's "never attack" list.
    ignored: BTreeSet<String>,
}

/// Cross-tick combat state.
#[derive(Debug, Default)]
struct AuraState {
    /// Network id of the locked target, if any.
    target: Option<i32>,
    /// When the last hit landed — drives the CPS limiter.
    last_attack: Option<Instant>,
}

#[derive(Debug)]
pub struct BaseAura {
    pub module: ModuleData,
    pub target: AuraTarget,
    state: Mutex<AuraState>,
}

impl BaseAura {
    pub fn new(
        id: ModuleId,
        description: String,
        key_bind: KeyboardKey,
        target: AuraTarget,
    ) -> Self {
        Self {
            module: ModuleData {
                id,
                description,
                category: ModuleCategory::Combat,
                key_bind,
                enabled: false,
                settings: vec![
                    ModuleSetting::Choice {
                        name: target.filter_setting().to_string(),
                        value: 0,
                        options: target.filter_options(),
                    },
                    ModuleSetting::Slider {
                        name: "Range".to_string(),
                        value: 3.5,
                        min: 3.0,
                        max: 6.0,
                    },
                    ModuleSetting::Slider {
                        name: "Speed".to_string(),
                        value: 7.0,
                        min: 2.0,
                        max: 20.0,
                    },
                    ModuleSetting::Slider {
                        name: "Attack Angle".to_string(),
                        value: 50.0,
                        min: 10.0,
                        max: 180.0,
                    },
                    ModuleSetting::Slider {
                        name: "CPS".to_string(),
                        value: 12.0,
                        min: 1.0,
                        max: 20.0,
                    },
                    ModuleSetting::Toggle {
                        name: "Cooldown".to_string(),
                        value: true,
                    },
                ]
                .into_iter()
                .chain(target.extra_settings())
                .collect(),
            },
            target,
            state: Mutex::new(AuraState::default()),
        }
    }

    fn slider(&self, name: &str, fallback: f32) -> f32 {
        self.module
            .get_setting(name)
            .and_then(|setting| setting.get_slider_value())
            .unwrap_or(fallback)
    }

    pub fn get_range(&self) -> f32 {
        self.slider("Range", 3.5)
    }

    fn speed(&self) -> f32 {
        self.slider("Speed", 7.0)
    }

    fn attack_angle(&self) -> f32 {
        self.slider("Attack Angle", 50.0)
    }

    fn cps(&self) -> f32 {
        self.slider("CPS", 12.0)
    }

    fn respect_cooldown(&self) -> bool {
        self.module
            .get_setting("Cooldown")
            .and_then(|setting| setting.get_toggle_value())
            .unwrap_or(true)
    }

    /// The filter currently chosen in the settings.
    pub fn filter(&self) -> Filter {
        let index = match self.module.get_setting(self.target.filter_setting()) {
            Some(ModuleSetting::Choice { value, .. }) => *value,
            _ => 0,
        };
        filter_from_choice(self.target, index)
    }

    fn toggle(&self, name: &str, fallback: bool) -> bool {
        self.module
            .get_setting(name)
            .and_then(|setting| setting.get_toggle_value())
            .unwrap_or(fallback)
    }

    fn choice(&self, name: &str) -> usize {
        match self.module.get_setting(name) {
            Some(ModuleSetting::Choice { value, .. }) => *value,
            _ => 0,
        }
    }

    /// MobAura's per-mob rules.
    fn mob_rules(&self, filter: Filter) -> MobRules {
        MobRules {
            filter,
            neutral: match self.choice("Neutral mobs") {
                1 => Neutral::WhenAngry,
                2 => Neutral::Skip,
                _ => Neutral::Attack,
            },
            skip_babies: self.toggle("Skip babies", false),
            skip_named: self.toggle("Skip named", false),
            skip_pets: self.toggle("Skip pets", true),
        }
    }

    fn priority(&self) -> Priority {
        match self.target {
            AuraTarget::Players => Priority::Nearest,
            AuraTarget::Mobs => match self.choice("Mob priority") {
                1 => Priority::LowestHealth,
                2 => Priority::Crosshair,
                _ => Priority::Nearest,
            },
        }
    }

    /// Whether `entity` is something this aura attacks. `listed` is a
    /// snapshot of the target list, lower-cased for players.
    fn is_target(&self, entity: &Entity, selection: &Selection) -> bool {
        match self.target {
            AuraTarget::Players => {
                if !entity.instance_of::<Player>() {
                    return false;
                }
                if !selection.rules.filter.uses_list() {
                    return true;
                }
                let name = entity
                    .get_name()
                    .and_then(|component| component.get_string())
                    .unwrap_or_default()
                    .to_lowercase();
                selection.listed.contains(&name) == (selection.rules.filter == Filter::OnlyListed)
            }
            AuraTarget::Mobs => {
                entity.instance_of::<Mob>()
                    && mob_facts(entity).is_some_and(|facts| {
                        mob_allowed(&selection.rules, &facts, &selection.listed, &selection.ignored)
                    })
            }
        }
    }

    /// Whether a new hit is allowed now — the CPS limiter, plus the optional
    /// 1.9+ attack-cooldown check.
    fn can_attack(&self, player: &LocalPlayer) -> anyhow::Result<bool> {
        let interval = Duration::from_secs_f32(1.0 / self.cps());
        let cps_ready = self
            .state
            .lock()
            .unwrap()
            .last_attack
            .is_none_or(|last| last.elapsed() >= interval);
        if !cps_ready {
            return Ok(false);
        }
        if self.respect_cooldown() && player.attack_strength_scale()? < 1.0 {
            return Ok(false);
        }
        Ok(true)
    }
}

impl Module for BaseAura {
    fn on_start(&self) -> anyhow::Result<()> {
        Ok(())
    }

    fn on_stop(&self) -> anyhow::Result<()> {
        *self.state.lock().unwrap() = AuraState::default();
        Ok(())
    }

    fn on_tick(&self) -> anyhow::Result<()> {
        let minecraft = minecraft();
        // Stand down while a menu (inventory, chest, crafting, chat, …) is open.
        if minecraft.current_screen().is_open() {
            return Ok(());
        }
        let (Some(player), Some(world), Some(game_mode)) = (
            minecraft.player()?,
            minecraft.world()?,
            minecraft.game_mode()?,
        ) else {
            *self.state.lock().unwrap() = AuraState::default();
            return Ok(()); // not in a world — nothing to do
        };

        let entities = world.get_entities()?;
        let eye = player.get_eye_position()?;
        let self_id = player.id()?;
        let range = self.get_range() as f64;

        let filter = self.filter();
        let listed: BTreeSet<String> = if filter.uses_list() {
            let list = targets::list(self.target.list());
            match self.target {
                AuraTarget::Players => list.iter().map(|name| name.to_lowercase()).collect(),
                AuraTarget::Mobs => list,
            }
        } else {
            BTreeSet::new()
        };
        let selection = Selection {
            rules: self.mob_rules(filter),
            listed,
            ignored: match self.target {
                AuraTarget::Mobs => targets::list(TargetList::MobIgnore),
                AuraTarget::Players => BTreeSet::new(),
            },
        };
        let through_walls = self.toggle("Through walls", true);
        let priority = self.priority();
        let look = Rotation::new(player.get_yaw()?, player.get_pitch()?);

        let locked = self.state.lock().unwrap().target;
        let Some((target_id, target)) = pick_target_by(
            &entities,
            eye,
            range * range,
            self_id,
            locked,
            |entity| {
                self.is_target(entity, &selection)
                    && !is_protected(entity)
                    && (through_walls || can_see(&player, entity))
            },
            |entity, distance_sq| priority_score(priority, entity, distance_sq, eye, look),
        ) else {
            self.state.lock().unwrap().target = None;
            return Ok(());
        };
        self.state.lock().unwrap().target = Some(target_id);
        set_combat_target(target_id);

        // Aim smoothly toward the target (the rotation controller eases the
        // camera there frame by frame).
        let angle = look_at(&player, &target, self.speed(), 180.0)?;

        // Attack once roughly aligned and the timers allow it. With Criticals
        // armed the hit is held back until the player is hopped into the crit
        // window; the CPS/cooldown timers only advance on a hit that lands.
        if angle <= self.attack_angle()
            && self.can_attack(&player)?
            && criticals::prepare(&player)? == CritState::Ready
        {
            game_mode.attack(&player, &target)?;
            player.swing()?;
            self.state.lock().unwrap().last_attack = Some(Instant::now());
        }

        Ok(())
    }

    fn get_module_data(&self) -> &ModuleData {
        &self.module
    }

    fn get_module_data_mut(&mut self) -> &mut ModuleData {
        &mut self.module
    }
}

/// Whether `entity` is a hostile mob — implements Minecraft's `Enemy` marker
/// interface (zombies, creepers, slimes, …).
pub(crate) fn is_enemy(entity: &Entity) -> bool {
    mapping()
        .in_frame(|| {
            let mut env = mapping().get_env()?;
            let enemy = mapping().resolve_class(&mut env, "net/minecraft/world/entity/monster/Enemy")?;
            Ok(env.is_instance_of(entity.jni_ref().as_obj(), &enemy)?)
        })
        .unwrap_or(false)
}

/// The entity's type id, e.g. `minecraft:zombie`, read from the type's
/// description id (`entity.minecraft.zombie`) — stable across versions.
fn entity_type_id(entity: &Entity) -> Option<String> {
    mapping()
        .in_frame(|| {
            let mut env = mapping().get_env()?;
            let kind = raw::call(
                &mut env,
                entity.jni_ref().as_obj(),
                "getType",
                "()Lnet/minecraft/world/entity/EntityType;",
                &[],
            )?
            .l()?;
            let description =
                raw::call(&mut env, &kind, "getDescriptionId", "()Ljava/lang/String;", &[])?.l()?;
            Ok(type_id_from_description(&mapping().get_string(description)?))
        })
        .ok()
        .flatten()
}

/// `entity.minecraft.zombie` → `minecraft:zombie`.
fn type_id_from_description(description: &str) -> Option<String> {
    let (namespace, path) = description.strip_prefix("entity.")?.split_once('.')?;
    Some(format!("{namespace}:{path}"))
}

/// Reads what [`mob_allowed`] needs about a mob, in one JNI frame.
fn mob_facts(entity: &Entity) -> Option<MobFacts> {
    mapping()
        .in_frame(|| {
            let mut env = mapping().get_env()?;
            let obj = entity.jni_ref().as_obj();
            let enemy_class = mapping().resolve_class(&mut env, "net/minecraft/world/entity/monster/Enemy")?;
            let neutral_class = mapping().resolve_class(&mut env, "net/minecraft/world/entity/NeutralMob")?;
            let ownable_class = mapping().resolve_class(&mut env, "net/minecraft/world/entity/OwnableEntity")?;
            let neutral = env.is_instance_of(obj, &neutral_class)?;
            let flag = |env: &mut jni::JNIEnv, name: &str| {
                raw::call(env, obj, name, "()Z", &[]).and_then(|v| Ok(v.z()?)).unwrap_or(false)
            };
            let owned = env.is_instance_of(obj, &ownable_class)? && {
                // 1.21.1: getOwnerUUID; newer: getOwnerReference. Null = wild.
                let owner = raw::call(&mut env, obj, "getOwnerUUID", "()Ljava/util/UUID;", &[])
                    .or_else(|_| {
                        raw::call(
                            &mut env,
                            obj,
                            "getOwnerReference",
                            "()Lnet/minecraft/world/entity/EntityReference;",
                            &[],
                        )
                    })
                    .and_then(|v| Ok(v.l()?));
                owner.is_ok_and(|owner| !owner.is_null())
            };
            Ok(MobFacts {
                enemy: env.is_instance_of(obj, &enemy_class)?,
                neutral,
                angry: neutral && flag(&mut env, "isAngry"),
                baby: flag(&mut env, "isBaby"),
                named: flag(&mut env, "hasCustomName"),
                owned,
                type_id: None,
            })
        })
        .ok()
        .map(|facts| MobFacts { type_id: entity_type_id(entity), ..facts })
}

/// Whether the player has a clear line of sight to `entity`.
fn can_see(player: &LocalPlayer, entity: &Entity) -> bool {
    mapping()
        .in_frame(|| {
            let mut env = mapping().get_env()?;
            Ok(raw::call(
                &mut env,
                player.jni_ref().as_obj(),
                "hasLineOfSight",
                "(Lnet/minecraft/world/entity/Entity;)Z",
                &[JValue::Object(entity.jni_ref().as_obj())],
            )?
            .z()?)
        })
        .unwrap_or(true)
}

/// The candidate ranking for `priority` — lower goes first.
fn priority_score(priority: Priority, entity: &Entity, distance_sq: f64, eye: Vec3, look: Rotation) -> f64 {
    match priority {
        Priority::Nearest => distance_sq,
        Priority::LowestHealth => entity
            .as_living()
            .and_then(|living| living.get_health().ok())
            .map_or(f64::MAX, f64::from),
        Priority::Crosshair => {
            let Ok(feet) = entity.get_position() else {
                return f64::MAX;
            };
            let height = entity.bb_height().unwrap_or(1.8) as f64;
            let aim = Vec3::new(feet.x(), feet.y() + height * 0.5, feet.z());
            look.angle_to(Rotation::towards(eye, aim)) as f64
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(filter: Filter, neutral: Neutral) -> MobRules {
        MobRules { filter, neutral, skip_babies: true, skip_named: false, skip_pets: true }
    }

    fn mob(id: &str, enemy: bool, neutral: bool) -> MobFacts {
        MobFacts { enemy, neutral, type_id: Some(id.to_string()), ..Default::default() }
    }

    #[test]
    fn hostile_except_endermen_and_zombified_piglins() {
        let none = BTreeSet::new();
        let zombie = mob("minecraft:zombie", true, false);
        let enderman = mob("minecraft:enderman", true, true);
        let piglin = mob("minecraft:zombified_piglin", true, true);
        let cow = mob("minecraft:cow", false, false);

        // Through the neutral rule.
        let skip = rules(Filter::Hostile, Neutral::Skip);
        assert!(mob_allowed(&skip, &zombie, &none, &none));
        assert!(!mob_allowed(&skip, &enderman, &none, &none));
        assert!(!mob_allowed(&skip, &piglin, &none, &none));
        assert!(!mob_allowed(&skip, &cow, &none, &none));

        // Through the "never attack" list.
        let ignored: BTreeSet<String> = ["minecraft:enderman".to_string()].into();
        let attack = rules(Filter::Hostile, Neutral::Attack);
        assert!(!mob_allowed(&attack, &enderman, &none, &ignored));
        assert!(mob_allowed(&attack, &piglin, &none, &ignored));
    }

    #[test]
    fn neutral_mobs_only_when_angry() {
        let none = BTreeSet::new();
        let rules = rules(Filter::Hostile, Neutral::WhenAngry);
        let calm = mob("minecraft:enderman", true, true);
        let angry = MobFacts { angry: true, ..calm.clone() };
        assert!(!mob_allowed(&rules, &calm, &none, &none));
        assert!(mob_allowed(&rules, &angry, &none, &none));
    }

    #[test]
    fn pets_babies_and_the_ignore_list_always_win() {
        let none = BTreeSet::new();
        let listed: BTreeSet<String> = ["minecraft:wolf".to_string()].into();
        let only = rules(Filter::OnlyListed, Neutral::Skip);
        let wolf = mob("minecraft:wolf", false, true);
        // An explicit "only listed" choice overrides the neutral rule …
        assert!(mob_allowed(&only, &wolf, &listed, &none));
        // … but not pet / baby protection, nor the ignore list.
        assert!(!mob_allowed(&only, &MobFacts { owned: true, ..wolf.clone() }, &listed, &none));
        assert!(!mob_allowed(&only, &MobFacts { baby: true, ..wolf.clone() }, &listed, &none));
        assert!(!mob_allowed(&only, &wolf, &listed, &listed));
    }

    #[test]
    fn type_id_is_read_from_a_description_id() {
        assert_eq!(
            type_id_from_description("entity.minecraft.zombie").as_deref(),
            Some("minecraft:zombie")
        );
        assert_eq!(
            type_id_from_description("entity.create.contraption").as_deref(),
            Some("create:contraption")
        );
        assert_eq!(type_id_from_description("block.minecraft.stone"), None);
    }
}
