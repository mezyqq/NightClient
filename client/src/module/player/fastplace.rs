//! FastPlace — removes the 4-tick delay between two uses of the right mouse
//! button, so blocks are placed (and items used) every tick while it is held.

use crate::mapping::raw;
use crate::mapping::MappedObject;
use crate::module::{KeyboardKey, Module, ModuleCategory, ModuleData, ModuleId};
use crate::state::{mapping, minecraft};
use jni::objects::JValue;

#[derive(Debug)]
pub struct FastPlaceModule {
    pub module: ModuleData,
}

impl FastPlaceModule {
    pub fn new() -> Self {
        Self {
            module: ModuleData {
                id: ModuleId::FastPlace,
                description: "Places blocks without the use delay".to_string(),
                category: ModuleCategory::Player,
                key_bind: KeyboardKey::KeyNone,
                enabled: false,
                settings: vec![],
            },
        }
    }
}

impl Module for FastPlaceModule {
    fn on_start(&self) -> anyhow::Result<()> {
        Ok(())
    }

    fn on_stop(&self) -> anyhow::Result<()> {
        Ok(())
    }

    fn on_tick(&self) -> anyhow::Result<()> {
        mapping().in_frame(|| {
            let mut env = mapping().get_env()?;
            let mc = minecraft().jni_ref().as_obj();
            if raw::field(&mut env, mc, "rightClickDelay", "I")?.i()? > 0 {
                raw::set_field(&mut env, mc, "rightClickDelay", "I", JValue::Int(0))?;
            }
            Ok(())
        })
    }

    fn get_module_data(&self) -> &ModuleData {
        &self.module
    }

    fn get_module_data_mut(&mut self) -> &mut ModuleData {
        &mut self.module
    }
}
