//! RAM Cleaner — runs Java's garbage collector once the heap is fuller than
//! a threshold, at most once per interval. It frees memory the game no
//! longer uses; it cannot shrink what the game really needs. A collection
//! freezes the game briefly (more with a bigger heap), so keep the interval
//! long — it is meant for long sessions that creep toward the heap limit.

use super::java::jstatic;
use super::{optimization_module, slider, toggle, Snapshot};
use crate::graphic::notification::{Notification, NotificationType};
use crate::mapping::raw;
use crate::state::mapping;
use std::sync::Mutex;
use std::time::{Duration, Instant};

optimization_module!(
    RamCleanerModule,
    RamCleaner,
    "Free unused memory when the heap gets full",
    vec![
        slider("Clean at heap %", 85.0, 50.0, 95.0),
        slider("Min interval (s)", 120.0, 10.0, 900.0),
        toggle("Notify", true),
    ]
);

const CHECK_EVERY: Duration = Duration::from_secs(1);

struct State {
    last_check: Option<Instant>,
    last_clean: Option<Instant>,
}

static STATE: Mutex<State> = Mutex::new(State {
    last_check: None,
    last_clean: None,
});

/// Pure: whether to collect now.
pub fn should_clean(used: u64, max: u64, threshold_percent: f32, since_last: Option<Duration>, interval: Duration) -> bool {
    if max == 0 {
        return false;
    }
    let percent = used as f64 * 100.0 / max as f64;
    percent >= threshold_percent as f64 && since_last.is_none_or(|elapsed| elapsed >= interval)
}

/// `(used, max)` heap bytes.
pub fn heap() -> anyhow::Result<(u64, u64)> {
    mapping().in_frame(|| {
        let mut env = mapping().get_env()?;
        let runtime = jstatic(&mut env, "java/lang/Runtime", "getRuntime", "()Ljava/lang/Runtime;", &[])?;
        let max = raw::call(&mut env, &runtime, "maxMemory", "()J", &[])?.j()?;
        let total = raw::call(&mut env, &runtime, "totalMemory", "()J", &[])?.j()?;
        let free = raw::call(&mut env, &runtime, "freeMemory", "()J", &[])?.j()?;
        Ok(((total - free).max(0) as u64, max.max(0) as u64))
    })
}

pub fn frame(snapshot: Option<Snapshot>) {
    let Some(s) = snapshot else {
        return;
    };
    let mut state = STATE.lock().unwrap_or_else(|p| p.into_inner());
    if state.last_check.is_some_and(|last| last.elapsed() < CHECK_EVERY) {
        return;
    }
    state.last_check = Some(Instant::now());
    let Ok((used, max)) = heap() else {
        return;
    };
    let interval = Duration::from_secs_f32(s.slider("Min interval (s)").max(10.0));
    if !should_clean(used, max, s.slider("Clean at heap %"), state.last_clean.map(|t| t.elapsed()), interval) {
        return;
    }
    state.last_clean = Some(Instant::now());
    let result = mapping().in_frame(|| {
        let mut env = mapping().get_env()?;
        env.call_static_method("java/lang/System", "gc", "()V", &[]).inspect_err(|_| {
            let _ = env.exception_clear();
        })?;
        Ok(())
    });
    if let Err(error) = result {
        log::warn!("RAM Cleaner: {error}");
        return;
    }
    if s.toggle("Notify") {
        let after = heap().map(|(used, _)| used).unwrap_or(used);
        let mb = |bytes: u64| bytes / (1024 * 1024);
        Notification::send(
            NotificationType::Info,
            "RAM Cleaner",
            &format!("Freed {} MB ({} → {} MB of {})", mb(used.saturating_sub(after)), mb(used), mb(after), mb(max)),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_only_past_the_threshold_and_interval() {
        let minute = Duration::from_secs(60);
        assert!(should_clean(90, 100, 85.0, None, minute));
        assert!(!should_clean(80, 100, 85.0, None, minute));
        assert!(!should_clean(90, 100, 85.0, Some(Duration::from_secs(10)), minute));
        assert!(should_clean(90, 100, 85.0, Some(Duration::from_secs(61)), minute));
        assert!(!should_clean(90, 0, 85.0, None, minute));
    }
}
