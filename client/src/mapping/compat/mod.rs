//! Bridges to other client mods running in the same game, reached through the
//! game class loader. Each is optional: `is_installed` probes for the mod, and
//! callers degrade gracefully when it is absent.
//!
//! Third-party classes are not in the Mojmap mapping, so — like the packet
//! layer — these bridges use explicit JNI descriptors.

pub mod baritone;
pub mod free_camera;

use crate::state::mapping;

/// Whether the class `jni_name` can be loaded by the game class loader. The
/// lookup result is cached by the mapping, so this is cheap to poll.
fn class_present(jni_name: &str) -> bool {
    mapping()
        .in_frame(|| {
            let mut env = mapping().get_env()?;
            Ok(mapping().resolve_class(&mut env, jni_name).is_ok())
        })
        .unwrap_or(false)
}
