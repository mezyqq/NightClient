//! Tool windows shown alongside the ClickGUI for modules that need more than
//! a settings list: the Baritone control panel and the Spectate player list.
//! A window is shown while its module is enabled or its row is expanded.

use crate::graphic::notification::{Notification, NotificationType};
use crate::graphic::theme;
use crate::graphic::workspace;
use crate::mapping::compat::{baritone, free_camera};
use crate::mapping::entity::EntityRef;
use crate::mapping::registry;
use crate::module::player::spectate::{self, Mode};
use crate::module::combat::aura::{filter_from_choice, AuraTarget, Filter};
use crate::module::combat::targets::{self, TargetList};
use crate::graphic::input::LAST_KEY_PRESSED;
use crate::module::misc::friends;
use crate::module::misc::macros;
use crate::module::KeyboardKey;
use std::sync::atomic::Ordering;
use crate::module::player::session;
use crate::module::render::block_esp;
use crate::module::{ModuleId, ModuleSetting};
use std::collections::BTreeSet;
use egui::{Align2, Button, Color32, Context, Id, RichText, Stroke, TextEdit, Ui};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Width of a tool window.
pub(crate) const WINDOW_W: f32 = 330.0;
/// How many search results a list shows at most.
pub(crate) const MAX_RESULTS: usize = 120;

/// Draws every tool window whose module is enabled or expanded.
pub fn draw(ctx: &Context, progress: f32) {
    if wanted(ModuleId::Baritone) {
        draw_baritone(ctx, progress);
    }
    if wanted(ModuleId::Spectate) {
        draw_spectate(ctx, progress);
    }
    if wanted(ModuleId::BlockEsp) {
        draw_block_esp(ctx, progress);
    }
    if wanted(ModuleId::AutoSort) {
        crate::graphic::autosort_window::draw(ctx, progress);
    }
    if wanted(ModuleId::Macros) {
        draw_macros(ctx, progress);
    }
    if wanted(ModuleId::Friends) {
        draw_friends(ctx, progress);
    }
    if wanted(ModuleId::Particles) {
        draw_particles(ctx, progress);
    }
    if wanted(ModuleId::EntityCuller) {
        draw_entity_culler(ctx, progress);
    }
    if wanted(ModuleId::ItemEsp) {
        draw_item_esp(ctx, progress);
    }
    if wanted(ModuleId::AutoCommand) {
        draw_auto_command(ctx, progress);
    }
    // KillAura's window only matters while a list-based filter is chosen.
    if wanted(ModuleId::KillAura) {
        let filter = aura_filter(ModuleId::KillAura, AuraTarget::Players);
        if filter.uses_list() {
            draw_killaura_targets(ctx, progress, filter);
        }
    }
    if wanted(ModuleId::MobAura) {
        // Always: its "Never attack" list applies under every filter.
        draw_mobaura_targets(ctx, progress, aura_filter(ModuleId::MobAura, AuraTarget::Mobs));
    }
}

fn wanted(id: ModuleId) -> bool {
    if crate::config::is_expanded(id) {
        return true;
    }
    crate::state::client()
        .modules
        .get(id)
        .and_then(|handle| handle.lock().ok().map(|m| m.get_module_data().enabled))
        .unwrap_or(false)
}

/// A themed, draggable window anchored to the right edge of the screen.
pub(crate) fn tool_window(
    ctx: &Context,
    progress: f32,
    title: &str,
    anchor_y: f32,
    body: impl FnOnce(&mut Ui),
) {
    // Only the current workspace's windows are shown (cross-fading on a switch).
    let fade = workspace::opacity(ctx, &workspace::window_key(title));
    if fade <= 0.0 {
        return;
    }
    let progress = progress * fade;
    workspace::drawn_window(egui::LayerId::new(egui::Order::Foreground, Id::new("tool_window").with(title)), title);

    // Where the user left it (persisted in the config, so it survives a
    // Panic / re-inject, when egui's own memory starts empty), else a
    // default spot on the right.
    let start = crate::config::window_pos(title)
        .map(|[x, y]| egui::pos2(x, y))
        .unwrap_or_else(|| egui::pos2(ctx.screen_rect().right() - WINDOW_W - 24.0, anchor_y));
    let mut window = egui::Window::new(RichText::new(title).strong().color(theme::TEXT));
    // Sent to another workspace by a drag: back to where the drag started.
    if let Some(at) = workspace::take_restore(title) {
        window = window.current_pos(at);
    }
    let shown = window
        .id(Id::new("tool_window").with(title))
        .order(egui::Order::Foreground)
        .default_pos(start)
        .default_open(crate::config::window_open(title).unwrap_or(true))
        .pivot(Align2::LEFT_TOP)
        .resizable(false)
        .collapsible(true)
        .frame(
            egui::Frame::window(&ctx.style())
                .fill(theme::BASE)
                .stroke(Stroke::new(1.0_f32, theme::BORDER))
                .rounding(theme::RADIUS),
        )
        .show(ctx, |ui| {
            ui.set_opacity(progress);
            ui.set_width(WINDOW_W);
            ui.spacing_mut().item_spacing.y = 6.0;
            body(ui);
        });
    if let Some(shown) = shown {
        let corner = shown.response.rect.left_top();
        crate::config::set_window_pos(title, [corner.x, corner.y]);
    }
    let collapsing = Id::new("tool_window").with(title).with("collapsing");
    if let Some(state) = egui::collapsing_header::CollapsingState::load(ctx, collapsing) {
        crate::config::set_window_open(title, state.is_open());
    }
}

/// Small helper: a secondary-styled button.
pub(crate) fn button(ui: &mut Ui, text: &str) -> bool {
    ui.add(Button::new(RichText::new(text).size(12.5)).fill(theme::ELEVATED))
        .clicked()
}

pub(crate) fn dim(text: &str) -> RichText {
    RichText::new(text).size(12.0).color(theme::TEXT_DIM)
}

/// A search box filtering `items` (case-insensitive substring), listing up to
/// [`MAX_RESULTS`] selectable matches. Returns nothing — selection is written
/// into `selected`.
fn searchable_list(
    ui: &mut Ui,
    id: &str,
    hint: &str,
    search: &mut String,
    items: &[String],
    selected: &mut Option<String>,
) {
    ui.add(
        TextEdit::singleline(search)
            .hint_text(hint)
            .desired_width(f32::INFINITY),
    );
    let needle = search.trim().to_lowercase();
    let matches: Vec<&String> = items
        .iter()
        .filter(|item| needle.is_empty() || item.to_lowercase().contains(&needle))
        .collect();
    egui::ScrollArea::vertical()
        .id_salt(id)
        .max_height(180.0)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            for item in matches.iter().take(MAX_RESULTS) {
                let is_selected = selected.as_deref() == Some(item.as_str());
                // `minecraft:` is implied; show only the path for vanilla ids.
                let label = item.strip_prefix("minecraft:").unwrap_or(item);
                if ui
                    .selectable_label(is_selected, RichText::new(label).size(12.5))
                    .clicked()
                {
                    *selected = Some((*item).clone());
                }
            }
            if matches.len() > MAX_RESULTS {
                ui.label(dim(&format!(
                    "… {} more — refine the search",
                    matches.len() - MAX_RESULTS
                )));
            }
            if matches.is_empty() {
                ui.label(dim("Nothing found"));
            }
        });
}

// --- multi-select lists ----------------------------------------------------

/// A list with removable chips for the chosen entries, a search box and a
/// scrollable result list where a click toggles an entry. `chosen` are the
/// current picks; `toggle`/`clear` edit them. `extra` is a dim status line.
#[allow(clippy::too_many_arguments)]
pub(crate) fn multi_select(
    ui: &mut Ui,
    id: &str,
    hint: &str,
    extra: &str,
    search: &Mutex<String>,
    items: &[String],
    chosen: &BTreeSet<String>,
    toggle: &dyn Fn(&str),
    clear: &dyn Fn(),
) {
    ui.horizontal(|ui| {
        ui.label(dim(&format!("{} chosen{extra}", chosen.len())));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if !chosen.is_empty() && button(ui, "Clear") {
                clear();
            }
        });
    });

    if !chosen.is_empty() {
        ui.horizontal_wrapped(|ui| {
            for entry in chosen {
                let label = entry.strip_prefix("minecraft:").unwrap_or(entry);
                if ui
                    .add(
                        Button::new(RichText::new(format!("{label} ×")).size(12.0))
                            .fill(theme::ELEVATED),
                    )
                    .on_hover_text("Remove")
                    .clicked()
                {
                    toggle(entry);
                }
            }
        });
    }

    let mut search = search.lock().unwrap_or_else(|p| p.into_inner());
    ui.add(
        TextEdit::singleline(&mut *search)
            .hint_text(hint)
            .desired_width(f32::INFINITY),
    );
    let needle = search.trim().to_lowercase();
    let matches: Vec<&String> = items
        .iter()
        .filter(|item| needle.is_empty() || item.to_lowercase().contains(&needle))
        .collect();
    egui::ScrollArea::vertical()
        .id_salt(id)
        .max_height(200.0)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            for item in matches.iter().take(MAX_RESULTS) {
                let label = item.strip_prefix("minecraft:").unwrap_or(item);
                if ui
                    .selectable_label(chosen.contains(*item), RichText::new(label).size(12.5))
                    .clicked()
                {
                    toggle(item);
                }
            }
            if matches.len() > MAX_RESULTS {
                ui.label(dim(&format!(
                    "… {} more — refine the search",
                    matches.len() - MAX_RESULTS
                )));
            }
            if matches.is_empty() {
                ui.label(dim("Nothing found"));
            }
        });
}

// --- Block ESP --------------------------------------------------------------

static BLOCK_ESP_SEARCH: Mutex<String> = Mutex::new(String::new());

fn draw_block_esp(ctx: &Context, progress: f32) {
    tool_window(ctx, progress, "Block ESP", 240.0, |ui| {
        let found = block_esp::with_found(<[_]>::len);
        multi_select(
            ui,
            "block_esp_blocks",
            "Search blocks… (ore, spawner, chest)",
            &format!(" · {found} found"),
            &BLOCK_ESP_SEARCH,
            registry::block_ids(),
            &block_esp::selected(),
            &block_esp::toggle_selected,
            &block_esp::clear_selected,
        );
    });
}

// --- aura targets -----------------------------------------------------------

static KILLAURA_SEARCH: Mutex<String> = Mutex::new(String::new());
static MOBAURA_SEARCH: Mutex<String> = Mutex::new(String::new());
static MOBAURA_IGNORE_SEARCH: Mutex<String> = Mutex::new(String::new());

/// The aura's current target filter, read from its choice setting.
fn aura_filter(id: ModuleId, target: AuraTarget) -> Filter {
    let index = crate::state::client()
        .modules
        .get(id)
        .and_then(|handle| {
            handle.lock().ok().and_then(|module| {
                match module.get_module_data().get_setting(target.filter_setting()) {
                    Some(ModuleSetting::Choice { value, .. }) => Some(*value),
                    _ => None,
                }
            })
        })
        .unwrap_or(0);
    filter_from_choice(target, index)
}

fn list_note(filter: Filter, what: &str) -> String {
    if filter == Filter::OnlyListed {
        format!("Attacks only the {what} chosen here.")
    } else {
        format!("Attacks everything except the {what} chosen here.")
    }
}

fn draw_killaura_targets(ctx: &Context, progress: f32, filter: Filter) {
    tool_window(ctx, progress, "KillAura targets", 120.0, |ui| {
        ui.label(dim(&list_note(filter, "players")));
        // Online players, plus listed names that are offline right now.
        let chosen = targets::list(TargetList::Players);
        let mut names = refresh_players().0;
        for name in &chosen {
            if !names.contains(name) {
                names.push(name.clone());
            }
        }
        names.sort_by_key(|name| name.to_lowercase());
        multi_select(
            ui,
            "killaura_players",
            "Search players…",
            "",
            &KILLAURA_SEARCH,
            &names,
            &chosen,
            &|name| targets::toggle(TargetList::Players, name),
            &|| targets::clear(TargetList::Players),
        );
    });
}

/// Neutral / commonly spared mobs, one click away in the "Never attack" tab.
const MOBAURA_QUICK: [&str; 8] = [
    "minecraft:zombified_piglin",
    "minecraft:enderman",
    "minecraft:piglin",
    "minecraft:wolf",
    "minecraft:iron_golem",
    "minecraft:villager",
    "minecraft:bee",
    "minecraft:llama",
];

fn draw_mobaura_targets(ctx: &Context, progress: f32, filter: Filter) {
    tool_window(ctx, progress, "MobAura targets", 160.0, |ui| {
        // "Never attack" always applies; the target list only with a list filter.
        let mut ignore_tab = !filter.uses_list()
            || crate::config::ui_value("mobaura.tab").as_deref() != Some("targets");
        if filter.uses_list() {
            ui.horizontal(|ui| {
                for (is_ignore, label, key) in [(true, "Never attack", "ignore"), (false, "Targets", "targets")] {
                    if ui.selectable_label(ignore_tab == is_ignore, label).clicked() && ignore_tab != is_ignore {
                        ignore_tab = is_ignore;
                        crate::config::set_ui_value("mobaura.tab", key);
                    }
                }
            });
            ui.separator();
        }

        if !ignore_tab {
            ui.label(dim(&list_note(filter, "mobs")));
            multi_select(
                ui,
                "mobaura_mobs",
                "Search mobs… (zombie, creeper)",
                "",
                &MOBAURA_SEARCH,
                registry::entity_type_ids(),
                &targets::list(TargetList::Mobs),
                &|id| targets::toggle(TargetList::Mobs, id),
                &|| targets::clear(TargetList::Mobs),
            );
            return;
        }

        ui.label(dim("Never attacked, whatever the Mobs filter."));
        let ignored = targets::list(TargetList::MobIgnore);
        ui.horizontal_wrapped(|ui| {
            for id in MOBAURA_QUICK {
                if !ignored.contains(id) {
                    let label = id.strip_prefix("minecraft:").unwrap_or(id);
                    if ui
                        .add(Button::new(RichText::new(format!("+ {label}")).size(12.0)))
                        .clicked()
                    {
                        targets::toggle(TargetList::MobIgnore, id);
                    }
                }
            }
        });
        multi_select(
            ui,
            "mobaura_ignore",
            "Search mobs… (enderman, piglin)",
            "",
            &MOBAURA_IGNORE_SEARCH,
            registry::entity_type_ids(),
            &ignored,
            &|id| targets::toggle(TargetList::MobIgnore, id),
            &|| targets::clear(TargetList::MobIgnore),
        );
    });
}

// --- Macros -----------------------------------------------------------------

static MACRO_NAME: Mutex<String> = Mutex::new(String::new());
/// The macro waiting for its playback key, if any.
static MACRO_BINDING: Mutex<Option<usize>> = Mutex::new(None);

fn draw_macros(ctx: &Context, progress: f32) {
    tool_window(ctx, progress, "Macros", 260.0, |ui| {
        ui.horizontal(|ui| {
            ui.label(dim(&macros::status()));
            if !macros::idle() && button(ui, "Stop") {
                macros::stop();
            }
        });
        ui.separator();

        let mut name = MACRO_NAME.lock().unwrap_or_else(|p| p.into_inner());
        ui.horizontal(|ui| {
            ui.add(
                TextEdit::singleline(&mut *name)
                    .hint_text("Macro name")
                    .desired_width(WINDOW_W - 80.0),
            );
            if button(ui, "Record") {
                macros::arm_record(&name);
                name.clear();
            }
        });
        ui.label(dim("Recording starts when you close the menu and stops when you open it (Right Shift)."));
        ui.separator();

        let list = macros::list();
        if list.is_empty() {
            ui.label(dim("No macros yet."));
        }

        // A pending "Bind" takes the next key / mouse button; Esc unbinds.
        let mut binding = MACRO_BINDING.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(index) = *binding {
            let pressed = LAST_KEY_PRESSED.swap(-1, Ordering::Relaxed);
            if pressed != -1 {
                let bind = if pressed == KeyboardKey::KeyEscape as i32 { -1 } else { pressed };
                macros::edit(|all| {
                    if let Some(entry) = all.get_mut(index) {
                        entry.bind = bind;
                    }
                });
                *binding = None;
            }
        }

        egui::ScrollArea::vertical()
            .id_salt("macros_list")
            .max_height(260.0)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for (index, entry) in list.iter().enumerate() {
                    ui.horizontal(|ui| {
                        if ui
                            .add(Button::new(RichText::new("×").size(12.0)).fill(theme::ELEVATED))
                            .on_hover_text("Delete")
                            .clicked()
                        {
                            macros::edit(|all| {
                                if index < all.len() {
                                    all.remove(index);
                                }
                            });
                            *binding = None;
                        }
                        ui.label(RichText::new(&entry.name).size(13.0).color(theme::TEXT));
                        ui.label(dim(&format!(
                            "{:.1}s · {} events",
                            entry.duration_ms() as f32 / 1000.0,
                            entry.events.len()
                        )));
                    });
                    ui.horizontal(|ui| {
                        if button(ui, "▶ Play") {
                            macros::arm_play(index);
                        }
                        let mut repeat = entry.repeat;
                        if ui.checkbox(&mut repeat, "Loop").changed() {
                            macros::edit(|all| {
                                if let Some(entry) = all.get_mut(index) {
                                    entry.repeat = repeat;
                                }
                            });
                        }
                        let caption = if *binding == Some(index) {
                            "Press a key…".to_string()
                        } else if entry.bind < 0 {
                            "Bind".to_string()
                        } else {
                            format!("Key: {}", KeyboardKey::from(entry.bind))
                        };
                        if button(ui, &caption) {
                            LAST_KEY_PRESSED.store(-1, Ordering::Relaxed);
                            *binding = Some(index);
                        }
                    });
                    if entry.repeat {
                        ui.horizontal(|ui| {
                            let (mut loops, mut delay) = (entry.loops, entry.loop_delay_ms);
                            let runs = ui.add(
                                egui::DragValue::new(&mut loops)
                                    .range(0..=100_000)
                                    .custom_formatter(|n, _| {
                                        if n == 0.0 { "forever".into() } else { format!("{n} runs") }
                                    }),
                            );
                            let pause = ui.add(
                                egui::DragValue::new(&mut delay)
                                    .range(0..=3_600_000)
                                    .speed(10.0)
                                    .prefix("pause ")
                                    .suffix(" ms"),
                            );
                            runs.on_hover_text("How many times to play it; 0 = until stopped");
                            pause.on_hover_text("Wait between runs");
                            if loops != entry.loops || delay != entry.loop_delay_ms {
                                macros::edit(|all| {
                                    if let Some(entry) = all.get_mut(index) {
                                        entry.loops = loops;
                                        entry.loop_delay_ms = delay;
                                    }
                                });
                            }
                        });
                    }
                    ui.add_space(4.0);
                }
            });
        ui.label(dim("Playback keys work while the Macros module is on. Esc stops a playback."));
    });
}

// --- Friends ----------------------------------------------------------------

static FRIENDS_SEARCH: Mutex<String> = Mutex::new(String::new());

fn draw_friends(ctx: &Context, progress: f32) {
    tool_window(ctx, progress, "Friends", 200.0, |ui| {
        ui.label(dim("Never attacked by KillAura, Aimbot, TriggerBot. Middle-click a player to add / remove."));
        let chosen = friends::list();
        let mut names = refresh_players().0;
        for name in &chosen {
            if !names.iter().any(|n| n.eq_ignore_ascii_case(name)) {
                names.push(name.clone());
            }
        }
        names.sort_by_key(|name| name.to_lowercase());
        multi_select(
            ui,
            "friends_players",
            "Search players…",
            "",
            &FRIENDS_SEARCH,
            &names,
            &chosen,
            &|name| {
                friends::toggle(name);
            },
            &friends::clear,
        );
    });
}

// --- AutoCommand ------------------------------------------------------------

static AUTO_COMMAND_SERVER_INPUT: Mutex<String> = Mutex::new(String::new());
static AUTO_COMMAND_ALL_INPUT: Mutex<String> = Mutex::new(String::new());

fn draw_auto_command(ctx: &Context, progress: f32) {
    tool_window(ctx, progress, "AutoCommand", 300.0, |ui| {
        ui.label(dim("Typed after joining. \"/…\" is a command, anything else goes to chat. Saved in plain text."));
        match session::server_address() {
            Some(address) => command_list(ui, &address, &format!("On {address}"), &AUTO_COMMAND_SERVER_INPUT),
            None => {
                ui.label(dim("Join a server to add commands just for it."));
            }
        }
        ui.separator();
        command_list(ui, session::ALL_SERVERS, "On every server", &AUTO_COMMAND_ALL_INPUT);
    });
}

/// The saved lines under `key`, each removable, plus a box to add one.
fn command_list(ui: &mut Ui, key: &str, title: &str, input: &Mutex<String>) {
    ui.label(RichText::new(title).size(12.5).color(theme::TEXT));
    for (index, line) in session::commands(key).iter().enumerate() {
        ui.horizontal(|ui| {
            if ui
                .add(Button::new(RichText::new("×").size(11.0)).fill(theme::ELEVATED))
                .on_hover_text("Remove")
                .clicked()
            {
                session::remove_command(key, index);
            }
            ui.label(RichText::new(line).size(12.5).monospace());
        });
    }
    let mut text = input.lock().unwrap_or_else(|p| p.into_inner());
    ui.horizontal(|ui| {
        let field = ui.add(
            TextEdit::singleline(&mut *text)
                .hint_text("/login password")
                .desired_width(WINDOW_W - 70.0),
        );
        let submitted = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if (button(ui, "Add") || submitted) && !text.trim().is_empty() {
            session::add_command(key, &text);
            text.clear();
        }
    });
}

// --- Baritone ---------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum BaritoneTab {
    Blocks,
    Entities,
    Players,
    Position,
}

struct BaritoneUi {
    tab: BaritoneTab,
    search: String,
    block: Option<String>,
    entity: Option<String>,
    player: Option<String>,
    quantity: String,
    coords: [String; 3],
    command: String,
    last: Option<String>,
}

static BARITONE_UI: Mutex<BaritoneUi> = Mutex::new(BaritoneUi {
    tab: BaritoneTab::Blocks,
    search: String::new(),
    block: None,
    entity: None,
    player: None,
    quantity: String::new(),
    coords: [String::new(), String::new(), String::new()],
    command: String::new(),
    last: None,
});

/// Runs a Baritone command and records / reports the outcome.
fn run(ui_state: &mut BaritoneUi, command: String) {
    match baritone::execute(&command) {
        Ok(()) => ui_state.last = Some(command),
        Err(error) => Notification::send(NotificationType::Warning, "Baritone", &error.to_string()),
    }
}

fn draw_baritone(ctx: &Context, progress: f32) {
    let Ok(mut s) = BARITONE_UI.lock() else {
        return;
    };
    tool_window(ctx, progress, "Baritone", 58.0, |ui| {
        if !baritone::is_installed() {
            ui.label(RichText::new("Baritone is not installed.").color(theme::WARN));
            ui.label(dim(
                "Put the Baritone mod in the mods folder and restart the game.",
            ));
            return;
        }
        if !crate::state::minecraft().in_world() {
            ui.label(dim("Join a world to control Baritone."));
            return;
        }

        const TABS: [(BaritoneTab, &str); 4] = [
            (BaritoneTab::Blocks, "Blocks"),
            (BaritoneTab::Entities, "Entities"),
            (BaritoneTab::Players, "Players"),
            (BaritoneTab::Position, "Position"),
        ];
        // The chosen tab is remembered in the config.
        if let Some(saved) = crate::config::ui_value("baritone.tab") {
            if let Some((tab, _)) = TABS.iter().find(|(_, label)| *label == saved) {
                s.tab = *tab;
            }
        }
        ui.horizontal(|ui| {
            for (tab, label) in TABS {
                if ui.selectable_label(s.tab == tab, label).clicked() && s.tab != tab {
                    s.tab = tab;
                    s.search.clear();
                    crate::config::set_ui_value("baritone.tab", label);
                }
            }
        });
        ui.separator();

        match s.tab {
            BaritoneTab::Blocks => baritone_blocks(ui, &mut s),
            BaritoneTab::Entities => baritone_entities(ui, &mut s),
            BaritoneTab::Players => baritone_players(ui, &mut s),
            BaritoneTab::Position => baritone_position(ui, &mut s),
        }

        ui.separator();
        ui.horizontal(|ui| {
            let stop = Button::new(RichText::new("Stop").size(12.5).color(theme::DANGER))
                .fill(theme::ELEVATED);
            if ui.add(stop).clicked() {
                run(&mut s, "stop".into());
            }
            if button(ui, "Pause") {
                run(&mut s, "pause".into());
            }
            if button(ui, "Resume") {
                run(&mut s, "resume".into());
            }
            if button(ui, "Farm") {
                run(&mut s, "farm".into());
            }
            if button(ui, "Explore") {
                run(&mut s, "explore".into());
            }
        });
        ui.horizontal(|ui| {
            ui.label(RichText::new("#").color(theme::ACCENT));
            let edit = ui.add(
                TextEdit::singleline(&mut s.command)
                    .hint_text("any Baritone command, e.g. tunnel")
                    .desired_width(WINDOW_W - 70.0),
            );
            let submit = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if (button(ui, "Run") || submit) && !s.command.trim().is_empty() {
                let command = s.command.trim().to_string();
                s.command.clear();
                run(&mut s, command);
            }
        });
        if let Some(last) = &s.last {
            ui.label(dim(&format!("Last: #{last}")));
        }
    });
}

fn baritone_blocks(ui: &mut Ui, s: &mut BaritoneUi) {
    let BaritoneUi { search, block, .. } = s;
    searchable_list(
        ui,
        "baritone_blocks",
        "Search blocks…",
        search,
        registry::block_ids(),
        block,
    );
    let Some(block) = s.block.clone() else {
        ui.label(dim("Pick a block to mine or walk to."));
        return;
    };
    ui.horizontal(|ui| {
        ui.label(dim("Amount"));
        ui.add(
            TextEdit::singleline(&mut s.quantity)
                .hint_text("∞")
                .desired_width(48.0),
        );
        let quantity = s.quantity.trim().parse::<u32>().ok().filter(|q| *q > 0);
        if button(ui, "Mine") {
            let command = match quantity {
                Some(q) => format!("mine {q} {block}"),
                None => format!("mine {block}"),
            };
            run(s, command);
        }
        if button(ui, "Go to nearest") {
            run(s, format!("goto {block}"));
        }
    });
}

fn baritone_entities(ui: &mut Ui, s: &mut BaritoneUi) {
    let BaritoneUi { search, entity, .. } = s;
    searchable_list(
        ui,
        "baritone_entities",
        "Search entity types…",
        search,
        registry::entity_type_ids(),
        entity,
    );
    let Some(entity) = s.entity.clone() else {
        ui.label(dim("Pick an entity type to follow."));
        return;
    };
    ui.horizontal(|ui| {
        if button(ui, "Follow nearest") {
            run(s, format!("follow entity {entity}"));
        }
        if button(ui, "Follow all entities") {
            run(s, "follow entities".into());
        }
    });
}

fn baritone_players(ui: &mut Ui, s: &mut BaritoneUi) {
    let players = cached_watchable();
    let BaritoneUi { search, player, .. } = s;
    searchable_list(
        ui,
        "baritone_players",
        "Search players in range…",
        search,
        &players,
        player,
    );
    let Some(name) = s.player.clone() else {
        ui.label(dim("Only players within tracking range are listed."));
        return;
    };
    ui.horizontal(|ui| {
        if button(ui, "Follow") {
            run(s, format!("follow player {name}"));
        }
        if button(ui, "Go to") {
            match player_position(&name) {
                Some([x, y, z]) => run(s, format!("goto {x} {y} {z}")),
                None => {
                    Notification::send(NotificationType::Warning, "Baritone", "Player out of range")
                }
            }
        }
        if button(ui, "Spectate") {
            spectate::watch(&name);
        }
    });
}

fn baritone_position(ui: &mut Ui, s: &mut BaritoneUi) {
    ui.horizontal(|ui| {
        for (axis, value) in ["X", "Y", "Z"].iter().zip(s.coords.iter_mut()) {
            ui.label(dim(axis));
            ui.add(TextEdit::singleline(value).desired_width(62.0));
        }
    });
    ui.horizontal(|ui| {
        if button(ui, "Here") {
            if let Ok(Some(player)) = crate::state::minecraft().player() {
                if let Ok(pos) = player.get_position() {
                    s.coords = [
                        (pos.x().floor() as i64).to_string(),
                        (pos.y().floor() as i64).to_string(),
                        (pos.z().floor() as i64).to_string(),
                    ];
                }
            }
        }
        let parsed: Option<Vec<i64>> = s.coords.iter().map(|c| c.trim().parse().ok()).collect();
        let xz: Option<(i64, i64)> = match (s.coords[0].trim().parse(), s.coords[2].trim().parse())
        {
            (Ok(x), Ok(z)) => Some((x, z)),
            _ => None,
        };
        if button(ui, "Go to") {
            match (&parsed, xz) {
                (Some(c), _) => run(s, format!("goto {} {} {}", c[0], c[1], c[2])),
                // Y left empty: walk to the column.
                (None, Some((x, z))) => run(s, format!("goto {x} {z}")),
                _ => Notification::send(
                    NotificationType::Warning,
                    "Baritone",
                    "Enter X and Z (Y optional)",
                ),
            }
        }
        if button(ui, "Set goal") {
            if let Some(c) = &parsed {
                run(s, format!("goal {} {} {}", c[0], c[1], c[2]));
            }
        }
        if button(ui, "Path") {
            run(s, "path".into());
        }
    });
}

fn player_position(name: &str) -> Option<[i64; 3]> {
    let world = crate::state::minecraft().world().ok()??;
    let player = world.players().ok()?.into_iter().find(|p| {
        p.get_name()
            .and_then(|n| n.get_string())
            .is_ok_and(|n| n == name)
    })?;
    let pos = player.get_position().ok()?;
    Some([
        pos.x().floor() as i64,
        pos.y().floor() as i64,
        pos.z().floor() as i64,
    ])
}

// --- Spectate ---------------------------------------------------------------

/// Player lists refreshed at most twice a second — they walk JNI lists.
struct PlayerCache {
    at: Option<Instant>,
    online: Vec<String>,
    watchable: Vec<String>,
}

static PLAYER_CACHE: Mutex<PlayerCache> = Mutex::new(PlayerCache {
    at: None,
    online: Vec::new(),
    watchable: Vec::new(),
});

fn refresh_players() -> (Vec<String>, Vec<String>) {
    let Ok(mut cache) = PLAYER_CACHE.lock() else {
        return (Vec::new(), Vec::new());
    };
    if cache
        .at
        .is_none_or(|at| at.elapsed() > Duration::from_millis(500))
    {
        cache.online = crate::state::minecraft()
            .online_player_names()
            .unwrap_or_default();
        cache.watchable = spectate::watchable_players();
        cache.at = Some(Instant::now());
    }
    (cache.online.clone(), cache.watchable.clone())
}

fn cached_watchable() -> Vec<String> {
    refresh_players().1
}

struct SpectateUi {
    search: String,
}

static SPECTATE_UI: Mutex<SpectateUi> = Mutex::new(SpectateUi {
    search: String::new(),
});

fn draw_spectate(ctx: &Context, progress: f32) {
    let Ok(mut s) = SPECTATE_UI.lock() else {
        return;
    };
    tool_window(ctx, progress, "Spectate", 400.0, |ui| {
        if !free_camera::is_installed() {
            ui.label(RichText::new("Free Camera API is not installed.").color(theme::WARN));
            ui.label(dim(
                "Spectate moves the camera through the freecameraapi mod.",
            ));
            return;
        }
        if !crate::state::minecraft().in_world() {
            ui.label(dim("Join a world to spectate."));
            return;
        }

        let target = spectate::target();
        let capturing = spectate::capturing();
        ui.horizontal(|ui| {
            ui.label(dim("Mode"));
            let mode = spectate::mode();
            for (option, label) in [(Mode::Orbit, "Auto"), (Mode::Free, "3D")] {
                if ui.selectable_label(mode == option, label).clicked() {
                    spectate::set_mode(option);
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if capturing {
                    let stop = Button::new(RichText::new("Stop").size(12.5).color(theme::DANGER))
                        .fill(theme::ELEVATED);
                    if ui.add(stop).clicked() {
                        spectate::stop();
                    }
                }
            });
        });
        if let (true, Some(name)) = (capturing, &target) {
            ui.label(RichText::new(format!("Watching {name}")).color(theme::ACCENT));
        }

        ui.add(
            TextEdit::singleline(&mut s.search)
                .hint_text("Search players…")
                .desired_width(f32::INFINITY),
        );
        let (online, watchable) = refresh_players();
        let needle = s.search.trim().to_lowercase();
        let me = crate::state::minecraft()
            .player()
            .ok()
            .flatten()
            .and_then(|p| p.get_name().ok()?.get_string().ok());

        egui::ScrollArea::vertical()
            .id_salt("spectate_players")
            .max_height(220.0)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                // Tab-list players, plus any tracked player missing from it.
                let mut names = online.clone();
                for name in &watchable {
                    if !names.contains(name) {
                        names.push(name.clone());
                    }
                }
                let mut shown = 0;
                for name in names
                    .iter()
                    .filter(|n| Some(n.as_str()) != me.as_deref())
                    .filter(|n| needle.is_empty() || n.to_lowercase().contains(&needle))
                {
                    shown += 1;
                    let in_range = watchable.contains(name);
                    let watching = capturing && target.as_deref() == Some(name.as_str());
                    ui.horizontal(|ui| {
                        let color = if watching {
                            theme::ACCENT
                        } else if in_range {
                            theme::TEXT
                        } else {
                            theme::TEXT_MUTED
                        };
                        ui.label(RichText::new(name).size(13.0).color(color));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if in_range {
                                if !watching && button(ui, "Watch") {
                                    spectate::watch(name);
                                }
                            } else {
                                ui.label(dim("out of range"));
                            }
                        });
                    });
                }
                if shown == 0 {
                    ui.label(dim("No other players"));
                }
            });

        ui.label(
            RichText::new(
                "Auto: W/S switch player · mouse rotate · wheel zoom\n\
                 3D: WASD / Space / Shift fly · wheel speed\n\
                 J switches Auto / 3D · Esc stops",
            )
            .size(11.0)
            .color(Color32::from_gray(130)),
        );
    });
}

// --- Optimization ---------------------------------------------------------------

static PARTICLE_SEARCH: Mutex<String> = Mutex::new(String::new());
static CULLER_SEARCH: Mutex<String> = Mutex::new(String::new());

fn draw_particles(ctx: &Context, progress: f32) {
    use crate::module::optimization::lists::{self, List};
    use crate::module::optimization::particles;
    tool_window(ctx, progress, "Particles", 160.0, |ui| {
        ui.label(dim(&format!("Alive now: {}", particles::count())));
        ui.label(dim(
            "Particle types to hide (needs \"Hide my list\"). A hidden type is never created. \
             If a mod crashes after hiding one of its types, unhide it.",
        ));
        let chosen = lists::get(List::Particles);
        multi_select(
            ui,
            "optimization_particles",
            "Search particle types…",
            "",
            &PARTICLE_SEARCH,
            registry::particle_type_ids(),
            &chosen,
            &|id| lists::toggle(List::Particles, id),
            &|| lists::clear(List::Particles),
        );
    });
}

fn draw_entity_culler(ctx: &Context, progress: f32) {
    use crate::module::optimization::lists::{self, List};
    tool_window(ctx, progress, "Entity Culler", 260.0, |ui| {
        ui.label(dim(
            "Living types are not drawn (still there). Other types are removed \
             from your world copy until the server sends them again.",
        ));
        let chosen = lists::get(List::Entities);
        multi_select(
            ui,
            "optimization_entities",
            "Search entity types…",
            "",
            &CULLER_SEARCH,
            registry::entity_type_ids(),
            &chosen,
            &|id| lists::toggle(List::Entities, id),
            &|| lists::clear(List::Entities),
        );
    });
}

static ITEM_SEARCH: Mutex<String> = Mutex::new(String::new());

fn draw_item_esp(ctx: &Context, progress: f32) {
    use crate::module::optimization::lists::{self, List};
    tool_window(ctx, progress, "Item ESP", 200.0, |ui| {
        ui.label(dim(
            "Items to highlight when \"Only listed items\" is on. Otherwise every item on the ground is shown.",
        ));
        let chosen = lists::get(List::Items);
        multi_select(
            ui,
            "item_esp_items",
            "Search items…",
            "",
            &ITEM_SEARCH,
            registry::item_ids(),
            &chosen,
            &|id| lists::toggle(List::Items, id),
            &|| lists::clear(List::Items),
        );
    });
}
