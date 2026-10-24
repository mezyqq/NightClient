//! Background FPS — while the game window is not focused (alt-tabbed,
//! another window on top), cap the frame rate and turn the volume down;
//! both come back the moment you return. Saves battery and keeps the iGPU
//! free for what you are doing instead.

use super::options::{want, OptVal};
use super::{optimization_module, slider, Snapshot};
use crate::mapping::raw;
use crate::mapping::MappedObject;
use crate::state::{mapping, minecraft};

optimization_module!(
    BackgroundFpsModule,
    BackgroundFps,
    "Lower FPS and volume while the game is unfocused",
    vec![
        slider("Unfocused FPS", 10.0, 10.0, 60.0),
        slider("Unfocused volume % (100 = keep)", 30.0, 0.0, 100.0),
    ]
);

pub fn frame(snapshot: Option<Snapshot>) {
    let Some(s) = snapshot else {
        return;
    };
    let active = mapping().in_frame(|| {
        let mut env = mapping().get_env()?;
        Ok(raw::call(&mut env, minecraft().jni_ref().as_obj(), "isWindowActive", "()Z", &[])?.z()?)
    });
    if active.unwrap_or(true) {
        return;
    }
    want("framerateLimit", OptVal::Int((s.slider("Unfocused FPS").round() as i32).clamp(10, 260)));
    let volume = s.slider("Unfocused volume % (100 = keep)").round();
    if volume < 100.0 {
        want("volume:MASTER", OptVal::Double((volume.max(0.0) / 100.0) as f64));
    }
}
