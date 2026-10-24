use crate::mapping::{MappedObject, MinecraftClassType};
use crate::state::mapping;
use jni::objects::GlobalRef;
use jni::sys::jlong;

#[derive(Debug, MappedObject)]
#[mapped(class = Window)]
pub struct Window {
    jni_ref: GlobalRef,
}

impl Window {
    pub fn new(minecraft: &GlobalRef) -> anyhow::Result<Window> {
        mapping().in_frame(|| {
            let window_obj = mapping()
                .call_method(
                    MinecraftClassType::Minecraft,
                    minecraft.as_obj(),
                    "getWindow",
                    &[],
                )?
                .l()?;
            Ok(Window {
                jni_ref: mapping().new_global_ref(window_obj)?,
            })
        })
    }

    /// The native GLFW window handle. `Window.getWindow()` was renamed to
    /// `handle()` in 1.21.9 — the rename registry maps it back on older builds.
    pub fn get_window(&self) -> anyhow::Result<jlong> {
        Ok(self.call_method("handle", &[])?.j()?)
    }

    /// The GUI scale factor (`Window.getGuiScale()`) — how many framebuffer
    /// pixels one vanilla HUD pixel spans.
    pub fn gui_scale(&self) -> anyhow::Result<f64> {
        Ok(self.call_method("getGuiScale", &[])?.d()?)
    }
}
