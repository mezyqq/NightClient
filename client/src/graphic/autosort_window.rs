//! The AutoSort tool window: bound chests and their rules, the inventory
//! layout, and the order for other chests. Every edit is saved at once.

use crate::graphic::notification::{Notification, NotificationType};
use crate::graphic::theme;
use crate::graphic::windows::{button, dim, tool_window, MAX_RESULTS, WINDOW_W};
use crate::mapping::registry;
use crate::module::world::autosort::{self, ChestRule, Entry, PRESETS};
use egui::{Button, Context, DragValue, RichText, TextEdit, Ui};
use std::sync::Mutex;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Chests,
    Inventory,
    Other,
}

struct UiState {
    tab: Tab,
    selected: Option<usize>,
    search: String,
    pattern: String,
}

static UI: Mutex<UiState> = Mutex::new(UiState {
    tab: Tab::Chests,
    selected: None,
    search: String::new(),
    pattern: String::new(),
});

pub fn draw(ctx: &Context, progress: f32) {
    tool_window(ctx, progress, "AutoSort", 40.0, |ui| {
        let mut state = UI.lock().unwrap_or_else(|p| p.into_inner());
        ui.horizontal(|ui| {
            if button(ui, "Sort now") {
                autosort::sort_now();
            }
            ui.label(dim(&autosort::status()));
        });
        const TABS: [(Tab, &str); 3] = [(Tab::Chests, "Chests"), (Tab::Inventory, "Inventory"), (Tab::Other, "Other chests")];
        // The chosen tab and chest are remembered in the config.
        if let Some(saved) = crate::config::ui_value("autosort.tab") {
            if let Some((tab, _)) = TABS.iter().find(|(_, label)| *label == saved) {
                state.tab = *tab;
            }
        }
        if let Some(saved) = crate::config::ui_value("autosort.chest") {
            state.selected = saved.parse().ok();
        }
        ui.horizontal(|ui| {
            for (tab, label) in TABS {
                if ui.selectable_value(&mut state.tab, tab, RichText::new(label).size(12.5)).clicked() {
                    crate::config::set_ui_value("autosort.tab", label);
                }
            }
        });
        ui.separator();
        match state.tab {
            Tab::Chests => chests_tab(ui, &mut state),
            Tab::Inventory => inventory_tab(ui, &mut state),
            Tab::Other => other_tab(ui, &mut state),
        }
        let chest = state.selected.map(|i| i.to_string()).unwrap_or_default();
        crate::config::set_ui_value("autosort.chest", &chest);
    });
}

// --- chests ------------------------------------------------------------------

fn chests_tab(ui: &mut Ui, state: &mut UiState) {
    let rules = autosort::rules();
    if button(ui, "+ Bind the chest I'm looking at") {
        match autosort::looked_at_block() {
            Some((dimension, pos)) => {
                let mut index = 0;
                autosort::edit_rules(|rules| {
                    rules.chests.push(ChestRule {
                        name: format!("Chest {}", rules.chests.len() + 1),
                        dimension,
                        pos,
                        entries: Vec::new(),
                        deposit: true,
                        take: false,
                        arrange: true,
                    });
                    index = rules.chests.len() - 1;
                });
                state.selected = Some(index);
            }
            None => Notification::send(NotificationType::Warning, "AutoSort", "Look at a chest first"),
        }
    }

    if rules.chests.is_empty() {
        ui.label(dim("No chests yet. Look at one and bind it."));
        return;
    }
    egui::ScrollArea::vertical()
        .id_salt("autosort_chests")
        .max_height(110.0)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            for (index, chest) in rules.chests.iter().enumerate() {
                ui.horizontal(|ui| {
                    if small(ui, "×").on_hover_text("Unbind").clicked() {
                        autosort::edit_rules(|rules| {
                            rules.chests.remove(index);
                        });
                        state.selected = None;
                    }
                    let [x, y, z] = chest.pos;
                    let dimension = chest.dimension.strip_prefix("minecraft:").unwrap_or(&chest.dimension);
                    let label = format!("{} — {x} {y} {z} ({dimension})", chest.name);
                    if ui
                        .selectable_label(state.selected == Some(index), RichText::new(label).size(12.5))
                        .clicked()
                    {
                        state.selected = Some(index);
                    }
                });
            }
        });

    let Some(index) = state.selected.filter(|&i| i < rules.chests.len()) else {
        ui.label(dim("Pick a chest to edit its rules."));
        return;
    };
    ui.separator();
    let mut chest = rules.chests[index].clone();
    ui.horizontal(|ui| {
        ui.label(dim("Name"));
        ui.add(TextEdit::singleline(&mut chest.name).desired_width(WINDOW_W - 60.0));
    });
    ui.horizontal(|ui| {
        ui.checkbox(&mut chest.deposit, "Deposit").on_hover_text("Move matching items from the inventory into this chest");
        ui.checkbox(&mut chest.take, "Take").on_hover_text("Refill the inventory up to each entry's take amount");
        ui.checkbox(&mut chest.arrange, "Arrange").on_hover_text("Merge stacks and lay them out in entry order");
    });
    if button(ui, "Rebind to the chest I'm looking at") {
        if let Some((dimension, pos)) = autosort::looked_at_block() {
            chest.dimension = dimension;
            chest.pos = pos;
        }
    }
    ui.label(dim("Entries, in order. keep = leave in inventory, take = refill up to."));
    entries_editor(ui, "chest", &mut chest.entries, true, state);
    autosort::edit_rules(|rules| {
        if let Some(slot) = rules.chests.get_mut(index) {
            *slot = chest;
        }
    });
}

// --- inventory ------------------------------------------------------------------

fn inventory_tab(ui: &mut Ui, state: &mut UiState) {
    let mut inventory = autosort::rules().inventory;
    ui.checkbox(&mut inventory.on_open, "Sort when the inventory opens");
    ui.label(RichText::new("Hotbar").size(12.5).color(theme::TEXT));
    ui.label(dim("A pattern per slot (e.g. *_sword, *_pickaxe). Empty = free slot."));
    egui::Grid::new("autosort_hotbar").num_columns(2).show(ui, |ui| {
        for (slot, pattern) in inventory.hotbar.iter_mut().enumerate() {
            ui.label(dim(&format!("Slot {}", slot + 1)));
            ui.add(TextEdit::singleline(pattern).hint_text("free").desired_width(WINDOW_W - 80.0));
            ui.end_row();
        }
    });
    ui.separator();
    ui.label(RichText::new("Main inventory order").size(12.5).color(theme::TEXT));
    entries_editor(ui, "inventory", &mut inventory.order, false, state);
    autosort::edit_rules(|rules| rules.inventory = inventory);
}

// --- other chests ----------------------------------------------------------------------

fn other_tab(ui: &mut Ui, state: &mut UiState) {
    let mut other = autosort::rules().other;
    ui.checkbox(&mut other.arrange, "Arrange chests that have no rule");
    ui.label(dim("Their order (unlisted items go after, alphabetically)."));
    entries_editor(ui, "other", &mut other.order, false, state);
    autosort::edit_rules(|rules| rules.other = other);
}

// --- shared entry editor --------------------------------------------------------------------

fn small(ui: &mut Ui, text: &str) -> egui::Response {
    ui.add(Button::new(RichText::new(text).size(11.0)).fill(theme::ELEVATED))
}

/// An ordered entry list — move up / down, remove, amounts — plus the ways to
/// add: presets, an item search, or a typed pattern.
fn entries_editor(ui: &mut Ui, salt: &str, entries: &mut Vec<Entry>, amounts: bool, state: &mut UiState) {
    let mut action: Option<(usize, i32)> = None; // (index, -1 up / +1 down / 0 remove)
    egui::ScrollArea::vertical()
        .id_salt(format!("autosort_entries_{salt}"))
        .max_height(170.0)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            if entries.is_empty() {
                ui.label(dim("Empty — add items below."));
            }
            let count = entries.len();
            for (index, entry) in entries.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(dim(&format!("{}.", index + 1)));
                    if index > 0 && small(ui, "▲").clicked() {
                        action = Some((index, -1));
                    }
                    if index + 1 < count && small(ui, "▼").clicked() {
                        action = Some((index, 1));
                    }
                    if small(ui, "×").clicked() {
                        action = Some((index, 0));
                    }
                    let label = entry.pattern.strip_prefix("minecraft:").unwrap_or(&entry.pattern);
                    ui.label(RichText::new(label).size(12.5).monospace());
                    if amounts {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.add(DragValue::new(&mut entry.take).range(0..=2304).prefix("take "));
                            ui.add(DragValue::new(&mut entry.keep).range(0..=2304).prefix("keep "));
                        });
                    }
                });
            }
        });
    if let Some((index, direction)) = action {
        match direction {
            0 => {
                entries.remove(index);
            }
            -1 => entries.swap(index, index - 1),
            _ => entries.swap(index, index + 1),
        }
    }

    let add = |pattern: &str, entries: &mut Vec<Entry>| {
        let entry = Entry::new(pattern);
        if !entry.pattern.is_empty() && !entries.iter().any(|e| e.pattern == entry.pattern) {
            entries.push(entry);
        }
    };

    ui.label(dim("Presets"));
    ui.horizontal_wrapped(|ui| {
        for (name, patterns) in PRESETS {
            if small(ui, &format!("+ {name}")).on_hover_text(patterns.join(", ")).clicked() {
                for pattern in *patterns {
                    add(pattern, entries);
                }
            }
        }
    });

    ui.horizontal(|ui| {
        let field = ui.add(
            TextEdit::singleline(&mut state.pattern)
                .hint_text("pattern, e.g. *_ore or oak_*")
                .desired_width(WINDOW_W - 70.0),
        );
        let entered = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if (button(ui, "Add") || entered) && !state.pattern.trim().is_empty() {
            add(&state.pattern.clone(), entries);
            state.pattern.clear();
        }
    });

    ui.add(
        TextEdit::singleline(&mut state.search)
            .hint_text("Search items…")
            .desired_width(f32::INFINITY),
    );
    let needle = state.search.trim().to_lowercase();
    if needle.is_empty() {
        return;
    }
    let items = registry::item_ids();
    let matches: Vec<&String> = items.iter().filter(|id| id.contains(&needle)).collect();
    egui::ScrollArea::vertical()
        .id_salt(format!("autosort_search_{salt}"))
        .max_height(140.0)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            for id in matches.iter().take(MAX_RESULTS) {
                let label = id.strip_prefix("minecraft:").unwrap_or(id);
                let listed = entries.iter().any(|e| e.pattern == **id);
                if ui.selectable_label(listed, RichText::new(label).size(12.5)).clicked() && !listed {
                    add(id, entries);
                }
            }
            if matches.is_empty() {
                ui.label(dim("Nothing found"));
            }
        });
}
