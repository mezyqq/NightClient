//! Render-category modules.
//!
//! These modules are pure configuration holders: the actual drawing is done
//! every frame by [`crate::graphic::esp`], which reads their enabled state and
//! settings. Keeping the JNI/render work in one place is what lets the ESP run
//! without costing frame rate.

pub mod ambience;
pub mod block_esp;
pub mod crosshair;
pub mod hit_effects;
pub mod item_esp;
pub mod target_hud;
pub mod spin;
pub mod chest_esp;
pub mod freelook;
pub mod fullbright;
pub mod mob_esp;
pub mod nametags;
pub mod player_esp;
pub mod tracers;
pub mod trajectories;
