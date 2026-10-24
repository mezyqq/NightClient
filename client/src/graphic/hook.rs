//! Frame hook.
//!
//! Intercepts the host's buffer-swap call so the overlay renders and the
//! client ticks exactly once per frame. Per-platform entry-point resolution
//! lives in [`crate::graphic::platform`].

use crate::graphic::platform;
use crate::mapping::entity::{EntityRef, LivingEntityRef};
use crate::{gl, state, RUNNING};
use ilhook::x64::{CallbackOption, HookFlags, HookPoint, HookType, Hooker, Registers};
use log::info;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

static LAST_TICK: AtomicI32 = AtomicI32::new(0);
/// Game ticks seen since injection (in a world with a living player).
static GAME_TICKS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// When the frame hook first saw the latest tick.
static TICK_SEEN_AT: Mutex<Option<Instant>> = Mutex::new(None);

/// Game ticks seen so far, and how long ago the latest one was seen — the
/// clock macros are timed on.
pub fn game_clock() -> (u64, std::time::Duration) {
    let seen = TICK_SEEN_AT.lock().unwrap_or_else(|p| p.into_inner());
    (
        GAME_TICKS.load(Ordering::Relaxed),
        seen.map(|at| at.elapsed()).unwrap_or_default(),
    )
}
static GL_LOADED: AtomicBool = AtomicBool::new(false);

/// Wrapper that lets an `ilhook` handle cross thread boundaries. Dropping the
/// inner `HookPoint` restores the patched bytes, i.e. removes the hook.
pub struct HookHandle(HookPoint);

unsafe impl Send for HookHandle {}
unsafe impl Sync for HookHandle {}

/// The single active buffer-swap hook. Re-injection replaces it: the old
/// `HookHandle` drops and unhooks itself. `None` means nothing is installed.
static GLOBAL_HOOK: OnceLock<Mutex<Option<HookHandle>>> = OnceLock::new();

fn global_hook() -> &'static Mutex<Option<HookHandle>> {
    GLOBAL_HOOK.get_or_init(|| Mutex::new(None))
}

// --- Per-frame logic -------------------------------------------------------

/// `ilhook` trampoline for the host's buffer-swap function.
unsafe extern "win64" fn swap_buffers_hook(_registers: *mut Registers, _user_data: usize) {
    on_frame();
}

/// Set while [`on_frame`] runs; only the render thread swaps buffers.
static IN_FRAME: AtomicBool = AtomicBool::new(false);

/// Marks [`on_frame`] as running until dropped.
struct FrameGuard;

impl FrameGuard {
    /// `None` if a frame pass is already running (a nested swap).
    fn enter() -> Option<FrameGuard> {
        (!IN_FRAME.swap(true, Ordering::Acquire)).then_some(FrameGuard)
    }
}

impl Drop for FrameGuard {
    fn drop(&mut self) {
        IN_FRAME.store(false, Ordering::Release);
    }
}

/// Runs once per graphical frame, from the buffer-swap hook.
unsafe fn on_frame() {
    // If `cleanup_client()` has run, `RUNNING` is false — bail immediately:
    // no tick, no draw, the hooks are about to be torn down.
    if !RUNNING.load(Ordering::SeqCst) {
        return;
    }
    // A game call made from inside this hook can render frames of its own
    // (`ConnectScreen.startConnecting` → `Minecraft.disconnect` runs a nested
    // render loop) and so re-enter it. The outer pass holds module and
    // session locks, so a nested pass would deadlock on them — skip it.
    let Some(_reentry) = FrameGuard::enter() else {
        return;
    };

    // Resolve the GL function pointers once, on the live render context.
    if !GL_LOADED.load(Ordering::Relaxed) {
        gl::load_with(platform::gl_proc_address);
        GL_LOADED.store(true, Ordering::Relaxed);
    }

    let started = Instant::now();
    check_tick();
    let ticked = Instant::now();
    // Death screen, disconnect screen and joins — handled even while the
    // registry is inactive.
    crate::module::player::session::frame();
    // Apply a config profile switched from darkC, on this (the game) thread.
    crate::config::process_pending();
    // Ease combat-module rotations toward their target every frame — this is
    // what makes aiming smooth rather than the visible 20 Hz tick steps.
    crate::module::combat::rotation::update();
    // Place the spectate camera for the next frame.
    crate::module::player::spectate::update();
    crate::module::render::freelook::update();
    crate::module::render::ambience::frame();
    crate::module::misc::macros::frame();
    crate::module::optimization::frame();
    crate::module::render::spin::frame();
    crate::module::world::farmhelper::frame();
    let updated = Instant::now();
    render_overlay();
    FRAME_COST.lock().unwrap_or_else(|p| p.into_inner()).record(started, ticked, updated);
}

/// Wall-time DarkClient spends inside each frame, logged every
/// [`FrameCost::REPORT_EVERY`] — the way to tell whether the client, or
/// something else, is costing frame rate.
struct FrameCost {
    since: Option<Instant>,
    frames: u32,
    tick: Duration,
    update: Duration,
    overlay: Duration,
    worst: Duration,
}

impl FrameCost {
    const REPORT_EVERY: Duration = Duration::from_secs(10);

    fn record(&mut self, started: Instant, ticked: Instant, updated: Instant) {
        let now = Instant::now();
        let since = *self.since.get_or_insert(started);
        self.frames += 1;
        self.tick += ticked - started;
        self.update += updated - ticked;
        self.overlay += now - updated;
        self.worst = self.worst.max(now - started);

        if now - since >= Self::REPORT_EVERY {
            let per_frame = |total: Duration| total.as_secs_f64() * 1000.0 / self.frames as f64;
            info!(
                "frame cost: {:.0} fps, client {:.2} ms/frame (tick {:.2}, update {:.2}, overlay {:.2}), worst {:.1} ms",
                self.frames as f64 / (now - since).as_secs_f64(),
                per_frame(self.tick + self.update + self.overlay),
                per_frame(self.tick),
                per_frame(self.update),
                per_frame(self.overlay),
                self.worst.as_secs_f64() * 1000.0,
            );
            *self = FrameCost::EMPTY;
        }
    }

    const EMPTY: FrameCost = FrameCost {
        since: None,
        frames: 0,
        tick: Duration::ZERO,
        update: Duration::ZERO,
        overlay: Duration::ZERO,
        worst: Duration::ZERO,
    };
}

static FRAME_COST: Mutex<FrameCost> = Mutex::new(FrameCost::EMPTY);

/// Renders the egui overlay for the current frame.
unsafe fn render_overlay() {
    // Install the input hooks lazily — they need the live GLFW window. The
    // overlay renders whether or not a world is loaded, so the GUI works
    // even when the client is injected from the main menu.
    crate::graphic::input::init();

    crate::graphic::ui_engine::render_egui_ui();
}

/// Detects a new game tick by watching the player's tick counter, and ticks
/// every enabled module when one is observed.
fn check_tick() {
    // Attach this render thread to the JVM so it can make JNI calls.
    if state::env().is_err() {
        return;
    }

    // Modules run only in a world with a *living* player. Notably they stand
    // down while the player is dead on the respawn screen — a cheat still
    // acting then is an instant ban on most servers — and reactivate once the
    // player respawns. `set_active` starts/stops them on each transition.
    let mc = state::minecraft();
    let in_world = matches!(mc.world(), Ok(Some(_)));
    let player = match mc.player() {
        Ok(Some(player)) => Some(player),
        Ok(None) | Err(_) => None,
    };
    let alive = player
        .as_ref()
        .is_some_and(|player| player.is_alive().unwrap_or(false));
    state::client().modules.set_active(in_world && alive);
    if !in_world || !alive {
        return;
    }
    let Some(player) = player else {
        return;
    };

    let tick_count = match player.get_tick_count() {
        Ok(count) => count,
        Err(_) => return,
    };

    // Any change counts as a new tick — not just an increase: a respawn, a
    // dimension change or a rejoin creates a fresh player whose counter
    // restarts at 0, and waiting for it to pass the old value would freeze
    // every module (and the packet handler upkeep) until a re-inject.
    let previous = LAST_TICK.swap(tick_count, Ordering::Relaxed);
    if previous != tick_count {
        // Below 20 FPS several ticks run per frame; a reset counter (respawn,
        // dimension change) counts as one.
        let advanced = if tick_count > previous { (tick_count - previous) as u64 } else { 1 };
        GAME_TICKS.fetch_add(advanced, Ordering::Relaxed);
        *TICK_SEEN_AT.lock().unwrap_or_else(|p| p.into_inner()) = Some(Instant::now());
        // Keep our Netty handler on the live connection's pipeline.
        crate::net::ensure_installed();
        state::client().modules.tick();
    }
}

// --- Hook installation -----------------------------------------------------

/// Stores a freshly built hook. Idempotent across re-injection: the previous
/// `HookHandle` drops here, restoring its patched bytes.
fn store_hook(hook: HookPoint, label: &str) {
    let mut guard = global_hook().lock().unwrap();
    if guard.is_some() {
        info!("Replacing the existing buffer-swap hook.");
    }
    *guard = Some(HookHandle(hook));
    info!(">>> HOOK ACTIVE ON: {} <<<", label);
}

/// Builds an `ilhook` `Hooker` for `target_addr` routed to [`swap_buffers_hook`].
fn hooker_for(target_addr: usize) -> Hooker {
    Hooker::new(
        target_addr,
        HookType::JmpBack(swap_buffers_hook),
        CallbackOption::None,
        0,
        HookFlags::empty(),
    )
}

/// Installs the buffer-swap hook that drives the overlay, trying every
/// platform-provided target until one hooks successfully.
pub fn install_hooks() -> anyhow::Result<()> {
    for target in platform::frame_hook_targets() {
        // SAFETY: `target.address` is a function address resolved by the
        // platform layer; `ilhook` patches it in place.
        let result = unsafe { hooker_for(target.address).hook() };
        match result {
            Ok(hook) => {
                store_hook(hook, &target.label);
                return Ok(());
            }
            Err(e) => info!("hook install failed on {}: {e:?}", target.label),
        }
    }
    Err(anyhow::anyhow!(
        "no buffer-swap target could be hooked on this platform"
    ))
}

/// Removes the active buffer-swap hook, restoring the original bytes.
pub fn uninstall_hooks() {
    let mut guard = global_hook().lock().unwrap();
    if guard.take().is_some() {
        info!("Buffer-swap hook removed; memory restored.");
    } else {
        info!("No active hook to remove.");
    }
}
