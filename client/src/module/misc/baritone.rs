//! Baritone — a control panel for the Baritone pathfinding mod, when it is
//! installed. The module itself does nothing in-game: enabling it (or
//! expanding its row) shows the Baritone window in the ClickGUI, which sends
//! commands through [`crate::mapping::compat::baritone`]. Baritone keeps
//! running its current task whatever this module's state.

use crate::module::{KeyboardKey, Module, ModuleCategory, ModuleData, ModuleId};

#[derive(Debug)]
pub struct BaritoneModule {
    pub module: ModuleData,
}

impl BaritoneModule {
    pub fn new() -> Self {
        Self {
            module: ModuleData {
                id: ModuleId::Baritone,
                description: "Control panel for the Baritone mod: goto, mine, follow".to_string(),
                category: ModuleCategory::Misc,
                key_bind: KeyboardKey::KeyNone,
                enabled: false,
                settings: vec![],
            },
        }
    }
}

impl Module for BaritoneModule {
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
