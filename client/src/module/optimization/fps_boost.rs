//! FPS Boost — the video settings as a profile: switch it on and the game
//! runs with these values, switch it off and your own come back. Every entry
//! can be left at "Keep" (sliders: the "keep" value) to leave that option
//! alone. Values are clamped to the ranges the game accepts.

use super::options::{want, OptVal};
use super::{choice, optimization_module, slider, Snapshot};

optimization_module!(
    FpsBoostModule,
    FpsBoost,
    "Video settings as a switchable profile, restored when off",
    vec![
        slider("Render distance (0 = keep)", 0.0, 0.0, 32.0),
        slider("Simulation distance (0 = keep)", 0.0, 0.0, 32.0),
        choice("Graphics", 1, &["Keep", "Fast", "Fancy"]),
        choice("Clouds", 1, &["Keep", "Off", "Fast", "Fancy"]),
        choice("Particle level", 2, &["Keep", "All", "Decreased", "Minimal"]),
        choice("Entity shadows", 1, &["Keep", "Off", "On"]),
        choice("Smooth lighting", 0, &["Keep", "Off", "On"]),
        slider("Entity distance % (0 = keep)", 0.0, 0.0, 500.0),
        slider("Biome blend (-1 = keep)", -1.0, -1.0, 7.0),
        slider("Max FPS (0 = keep, 260 = unlimited)", 0.0, 0.0, 260.0),
        choice("VSync", 0, &["Keep", "Off", "On"]),
        choice("Chunk updates", 0, &["Keep", "Threaded", "Semi-blocking", "Fully blocking"]),
    ]
);

/// A "Keep / Off / On" choice as a boolean.
fn keep_off_on(index: usize) -> Option<bool> {
    match index {
        1 => Some(false),
        2 => Some(true),
        _ => None,
    }
}

/// The option values a snapshot asks for, as `(key, value)` pairs.
pub fn wants(s: &Snapshot) -> Vec<(&'static str, OptVal)> {
    profile(
        s.slider("Render distance (0 = keep)"),
        s.slider("Simulation distance (0 = keep)"),
        [
            s.choice("Graphics"),
            s.choice("Clouds"),
            s.choice("Particle level"),
            s.choice("Entity shadows"),
            s.choice("Smooth lighting"),
            s.choice("VSync"),
            s.choice("Chunk updates"),
        ],
        s.slider("Entity distance % (0 = keep)"),
        s.slider("Biome blend (-1 = keep)"),
        s.slider("Max FPS (0 = keep, 260 = unlimited)"),
    )
}

/// Pure: the settings → option values. `choices` are, in order: graphics,
/// clouds, particles, shadows, smooth lighting, vsync, chunk updates.
pub fn profile(
    render: f32,
    simulation: f32,
    choices: [usize; 7],
    entity_percent: f32,
    biome_blend: f32,
    max_fps: f32,
) -> Vec<(&'static str, OptVal)> {
    let mut out = Vec::new();
    let enum_value = |names: &[&str], index: usize| -> Option<OptVal> {
        index.checked_sub(1).and_then(|i| names.get(i)).map(|name| OptVal::Enum(name.to_string()))
    };

    let render = render.round() as i32;
    if render > 0 {
        out.push(("renderDistance", OptVal::Int(render.clamp(2, 32))));
    }
    let simulation = simulation.round() as i32;
    if simulation > 0 {
        out.push(("simulationDistance", OptVal::Int(simulation.clamp(5, 32))));
    }
    let [graphics, clouds, particles, shadows, smooth, vsync, chunks] = choices;
    if let Some(value) = enum_value(&["FAST", "FANCY"], graphics) {
        out.push(("graphicsMode", value));
    }
    if let Some(value) = enum_value(&["OFF", "FAST", "FANCY"], clouds) {
        out.push(("cloudStatus", value));
    }
    if let Some(value) = enum_value(&["ALL", "DECREASED", "MINIMAL"], particles) {
        out.push(("particles", value));
    }
    if let Some(value) = keep_off_on(shadows) {
        out.push(("entityShadows", OptVal::Bool(value)));
    }
    if let Some(value) = keep_off_on(smooth) {
        out.push(("ambientOcclusion", OptVal::Bool(value)));
    }
    if let Some(value) = keep_off_on(vsync) {
        out.push(("enableVsync", OptVal::Bool(value)));
    }
    if let Some(value) = enum_value(&["NONE", "PLAYER_AFFECTED", "NEARBY"], chunks) {
        out.push(("prioritizeChunkUpdates", value));
    }
    if entity_percent.round() > 0.0 {
        // The game stores it in quarter steps between 0.5 and 5.0.
        let scale = ((entity_percent / 100.0).clamp(0.5, 5.0) * 4.0).round() / 4.0;
        out.push(("entityDistanceScaling", OptVal::Double(scale as f64)));
    }
    let blend = biome_blend.round() as i32;
    if blend >= 0 {
        out.push(("biomeBlendRadius", OptVal::Int(blend.min(7))));
    }
    let fps = max_fps.round() as i32;
    if fps > 0 {
        out.push(("framerateLimit", OptVal::Int(fps.clamp(10, 260))));
    }
    out
}

pub fn frame(snapshot: Option<Snapshot>) {
    if let Some(snapshot) = snapshot {
        for (key, value) in wants(&snapshot) {
            want(key, value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keep_everywhere_forces_nothing() {
        assert!(profile(0.0, 0.0, [0; 7], 0.0, -1.0, 0.0).is_empty());
    }

    #[test]
    fn values_are_clamped_to_what_the_game_accepts() {
        let out = profile(1.0, 3.0, [0; 7], 333.0, 9.0, 5.0);
        assert!(out.contains(&("renderDistance", OptVal::Int(2))));
        assert!(out.contains(&("simulationDistance", OptVal::Int(5))));
        assert!(out.contains(&("entityDistanceScaling", OptVal::Double(3.25))));
        assert!(out.contains(&("biomeBlendRadius", OptVal::Int(7))));
        assert!(out.contains(&("framerateLimit", OptVal::Int(10))));
    }

    #[test]
    fn choices_map_to_enum_names() {
        let out = profile(0.0, 0.0, [1, 1, 3, 1, 2, 1, 2], 0.0, -1.0, 0.0);
        assert!(out.contains(&("graphicsMode", OptVal::Enum("FAST".into()))));
        assert!(out.contains(&("cloudStatus", OptVal::Enum("OFF".into()))));
        assert!(out.contains(&("particles", OptVal::Enum("MINIMAL".into()))));
        assert!(out.contains(&("entityShadows", OptVal::Bool(false))));
        assert!(out.contains(&("ambientOcclusion", OptVal::Bool(true))));
        assert!(out.contains(&("enableVsync", OptVal::Bool(false))));
        assert!(out.contains(&("prioritizeChunkUpdates", OptVal::Enum("PLAYER_AFFECTED".into()))));
    }
}
