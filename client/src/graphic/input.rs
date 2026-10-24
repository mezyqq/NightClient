//! Input interception.
//!
//! Swaps the host's GLFW mouse/cursor/key callbacks for our own so the overlay
//! can read input and, while the GUI is open, swallow events before they reach
//! Minecraft. The logic is platform-agnostic; only locating the GLFW shared
//! library differs between Linux and Windows (see [`open_glfw_library`]).

use crate::module::player::spectate;
use crate::module::misc::macros::{self, Event as MacroEvent};
use crate::module::render::freelook;
use crate::state::{client, minecraft};
use libloading::Library;
use log::info;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

// --- egui keyboard events --------------------------------------------------

/// Egui keyboard / text events accumulated by the GLFW callbacks while the
/// overlay is open. Drained — and cleared — by `gather_egui_inputs` each
/// frame, so widgets like `TextEdit` and `DragValue` (used by the color
/// picker) receive characters and editing keys.
pub static PENDING_EVENTS: Mutex<Vec<egui::Event>> = Mutex::new(Vec::new());

fn push_event(event: egui::Event) {
    if let Ok(mut queue) = PENDING_EVENTS.lock() {
        queue.push(event);
    }
}

fn translate_mods(mods: i32) -> egui::Modifiers {
    let ctrl = mods & 0x2 != 0;
    egui::Modifiers {
        alt: mods & 0x4 != 0,
        ctrl,
        shift: mods & 0x1 != 0,
        mac_cmd: false,
        // On non-mac, egui treats `command` as `ctrl`.
        command: ctrl,
    }
}

/// Maps a GLFW key code to its [`egui::Key`] counterpart, or `None` for keys
/// egui does not represent.
fn translate_glfw_key(key: i32) -> Option<egui::Key> {
    use egui::Key::*;
    Some(match key {
        32 => Space,
        39 => Quote,
        44 => Comma,
        45 => Minus,
        46 => Period,
        47 => Slash,
        48 => Num0,
        49 => Num1,
        50 => Num2,
        51 => Num3,
        52 => Num4,
        53 => Num5,
        54 => Num6,
        55 => Num7,
        56 => Num8,
        57 => Num9,
        59 => Semicolon,
        61 => Equals,
        65 => A,
        66 => B,
        67 => C,
        68 => D,
        69 => E,
        70 => F,
        71 => G,
        72 => H,
        73 => I,
        74 => J,
        75 => K,
        76 => L,
        77 => M,
        78 => N,
        79 => O,
        80 => P,
        81 => Q,
        82 => R,
        83 => S,
        84 => T,
        85 => U,
        86 => V,
        87 => W,
        88 => X,
        89 => Y,
        90 => Z,
        91 => OpenBracket,
        92 => Backslash,
        93 => CloseBracket,
        96 => Backtick,
        256 => Escape,
        257 => Enter,
        258 => Tab,
        259 => Backspace,
        260 => Insert,
        261 => Delete,
        262 => ArrowRight,
        263 => ArrowLeft,
        264 => ArrowDown,
        265 => ArrowUp,
        266 => PageUp,
        267 => PageDown,
        268 => Home,
        269 => End,
        // Numpad digits (GLFW_KEY_KP_0..9).
        320 => Num0,
        321 => Num1,
        322 => Num2,
        323 => Num3,
        324 => Num4,
        325 => Num5,
        326 => Num6,
        327 => Num7,
        328 => Num8,
        329 => Num9,
        330 => Period, // KP_DECIMAL
        331 => Slash,  // KP_DIVIDE
        333 => Minus,  // KP_SUBTRACT
        334 => Plus,   // KP_ADD
        335 => Enter,  // KP_ENTER
        336 => Equals, // KP_EQUAL
        290..=301 => match key - 289 {
            1 => F1,
            2 => F2,
            3 => F3,
            4 => F4,
            5 => F5,
            6 => F6,
            7 => F7,
            8 => F8,
            9 => F9,
            10 => F10,
            11 => F11,
            12 => F12,
            _ => return None,
        },
        _ => return None,
    })
}

// --- Public input state ----------------------------------------------------

/// Whether the overlay GUI is open. While open, input is consumed instead of
/// being forwarded to Minecraft.
pub static GUI_OPEN: AtomicBool = AtomicBool::new(false);

/// Last key pressed, or `-1` once consumed. Used by the keybind picker.
pub static LAST_KEY_PRESSED: AtomicI32 = AtomicI32::new(-1);

/// Snapshot of the mouse, updated from the GLFW callbacks and read by the UI.
pub static MOUSE_STATE: Mutex<MouseState> = Mutex::new(MouseState::NEW);

#[derive(Default, Clone, Copy)]
pub struct MouseState {
    pub x: f64,
    pub y: f64,
    pub left_down: bool,
    pub right_down: bool,
    pub left_clicked: bool,
    pub right_clicked: bool,
    /// Scroll-wheel delta accumulated since the last frame; drained — and
    /// reset — by the UI engine when it builds egui's input.
    pub scroll_x: f32,
    pub scroll_y: f32,
}

impl MouseState {
    const NEW: MouseState = MouseState {
        x: 0.0,
        y: 0.0,
        left_down: false,
        right_down: false,
        left_clicked: false,
        right_clicked: false,
        scroll_x: 0.0,
        scroll_y: 0.0,
    };
}

// --- GLFW constants --------------------------------------------------------

const GLFW_RELEASE: i32 = 0;
const GLFW_PRESS: i32 = 1;
const GLFW_REPEAT: i32 = 2;
const GLFW_MOUSE_BUTTON_LEFT: i32 = 0;
const GLFW_MOUSE_BUTTON_RIGHT: i32 = 1;
const GLFW_MOUSE_BUTTON_MIDDLE: i32 = 2;
const GLFW_KEY_RIGHT_SHIFT: i32 = 344;
const GLFW_KEY_ESCAPE: i32 = 256;
const GLFW_KEY_END: i32 = 269;
const GLFW_MOD_CONTROL: i32 = 0x2;

const GLFW_CURSOR: i32 = 0x0003_3001;
const GLFW_CURSOR_NORMAL: i32 = 0x0003_4001;
const GLFW_CURSOR_DISABLED: i32 = 0x0003_4003;

// --- GLFW function-pointer types -------------------------------------------

type MouseButtonFun = extern "C" fn(*mut c_void, i32, i32, i32);
type CursorPosFun = extern "C" fn(*mut c_void, f64, f64);
type KeyFun = extern "C" fn(*mut c_void, i32, i32, i32, i32);
type ScrollFun = extern "C" fn(*mut c_void, f64, f64);
type CharFun = extern "C" fn(*mut c_void, u32);

type GetCurrentContext = extern "C" fn() -> *mut c_void;
type SetMouseButtonCallback = extern "C" fn(*mut c_void, MouseButtonFun) -> *mut c_void;
type SetCursorPosCallback = extern "C" fn(*mut c_void, CursorPosFun) -> *mut c_void;
type SetKeyCallback = extern "C" fn(*mut c_void, KeyFun) -> *mut c_void;
type SetScrollCallback = extern "C" fn(*mut c_void, ScrollFun) -> *mut c_void;
type SetCharCallback = extern "C" fn(*mut c_void, CharFun) -> *mut c_void;
type SetInputMode = extern "C" fn(*mut c_void, i32, i32);
type SetCursorPos = extern "C" fn(*mut c_void, f64, f64);

// --- Installed-hook state --------------------------------------------------

/// Everything captured when the GLFW callbacks were swapped: the library (kept
/// alive so the resolved symbols stay valid), the window, the host's original
/// callbacks, and the two functions needed to toggle cursor capture.
struct GlfwHooks {
    library: Library,
    window: *mut c_void,
    original_mouse_button: *mut c_void,
    original_cursor_pos: *mut c_void,
    original_key: *mut c_void,
    original_scroll: *mut c_void,
    original_char: *mut c_void,
    set_input_mode: SetInputMode,
    set_cursor_pos: SetCursorPos,
}

// The raw pointers are only ever touched on the render/GLFW thread; the struct
// is published through `OnceLock` purely for read-only access.
unsafe impl Send for GlfwHooks {}
unsafe impl Sync for GlfwHooks {}

static HOOKS: OnceLock<GlfwHooks> = OnceLock::new();

/// Last real cursor position, stored as `f64` bits. While the GUI is open the
/// cursor reported to Minecraft is frozen here, so the camera does not spin.
static CURSOR_LOCK_X: AtomicU64 = AtomicU64::new(0);
static CURSOR_LOCK_Y: AtomicU64 = AtomicU64::new(0);

fn cursor_lock() -> (f64, f64) {
    (
        f64::from_bits(CURSOR_LOCK_X.load(Ordering::Relaxed)),
        f64::from_bits(CURSOR_LOCK_Y.load(Ordering::Relaxed)),
    )
}

fn set_cursor_lock(x: f64, y: f64) {
    CURSOR_LOCK_X.store(x.to_bits(), Ordering::Relaxed);
    CURSOR_LOCK_Y.store(y.to_bits(), Ordering::Relaxed);
}

// --- Callbacks -------------------------------------------------------------

extern "C" fn on_mouse_button(window: *mut c_void, button: i32, action: i32, mods: i32) {
    if let Ok(mut state) = MOUSE_STATE.lock() {
        match (button, action) {
            (GLFW_MOUSE_BUTTON_LEFT, GLFW_PRESS) => {
                state.left_down = true;
                state.left_clicked = true;
            }
            (GLFW_MOUSE_BUTTON_LEFT, GLFW_RELEASE) => state.left_down = false,
            (GLFW_MOUSE_BUTTON_RIGHT, GLFW_PRESS) => {
                state.right_down = true;
                state.right_clicked = true;
            }
            (GLFW_MOUSE_BUTTON_RIGHT, GLFW_RELEASE) => state.right_down = false,
            _ => {}
        }
    }

    // Middle / side buttons are bindable: report them to the bind capture
    // while the GUI is open, toggle bound modules / play macros otherwise.
    if action == GLFW_PRESS && button >= GLFW_MOUSE_BUTTON_MIDDLE {
        let code = crate::module::MOUSE_BUTTON_BASE + button;
        if GUI_OPEN.load(Ordering::Relaxed) {
            LAST_KEY_PRESSED.store(code, Ordering::Relaxed);
        } else if macros::on_key_press(code) {
            return;
        } else {
            handle_module_keybind(code);
            if button == GLFW_MOUSE_BUTTON_MIDDLE {
                crate::module::misc::friends::middle_click();
            }
        }
    }

    // A macro is playing: real clicks are kept from the game.
    if !GUI_OPEN.load(Ordering::Relaxed) && macros::playing_now() {
        return;
    }

    // A right click on a sign covering a chest opens the chest instead; the
    // press then never reaches the game (its release does, harmlessly).
    if button == GLFW_MOUSE_BUTTON_RIGHT
        && action == GLFW_PRESS
        && !GUI_OPEN.load(Ordering::Relaxed)
        && !spectate::capturing()
        && crate::module::world::throughsigns::intercept_use()
    {
        return;
    }

    // While the GUI is open, swallow the event instead of forwarding it. While
    // spectating, too: the game keeps the button state it had (a held attack
    // keeps mining).
    if GUI_OPEN.load(Ordering::Relaxed) || spectate::capturing() {
        return;
    }
    macros::record(MacroEvent::Button { button, action, mods });
    if let Some(hooks) = HOOKS.get() {
        if !hooks.original_mouse_button.is_null() {
            let original: MouseButtonFun =
                unsafe { std::mem::transmute(hooks.original_mouse_button) };
            original(window, button, action, mods);
        }
    }
}

extern "C" fn on_cursor_pos(window: *mut c_void, x: f64, y: f64) {
    if let Ok(mut state) = MOUSE_STATE.lock() {
        state.x = x;
        state.y = y;
    }

    // While the GUI is open, feed Minecraft the frozen position so dragging the
    // overlay does not move the camera; otherwise track the real cursor.
    // While spectating the mouse steers the spectate camera instead, and the
    // game is likewise fed the frozen position so the body does not turn.
    let (send_x, send_y) = if GUI_OPEN.load(Ordering::Relaxed) {
        cursor_lock()
    } else if macros::playing_now() {
        // The playback moves the cursor; the real one is held back.
        cursor_lock()
    } else if spectate::capturing() {
        spectate::on_cursor(x, y);
        cursor_lock()
    } else if freelook::capturing() {
        freelook::on_cursor(x, y);
        cursor_lock()
    } else {
        macros::record_cursor(x, y);
        set_cursor_lock(x, y);
        (x, y)
    };

    if let Some(hooks) = HOOKS.get() {
        if !hooks.original_cursor_pos.is_null() {
            let original: CursorPosFun = unsafe { std::mem::transmute(hooks.original_cursor_pos) };
            original(window, send_x, send_y);
        }
    }
}

extern "C" fn on_key(window: *mut c_void, key: i32, scancode: i32, action: i32, mods: i32) {
    // Ctrl+End: Panic, from anywhere — world, Minecraft screens, the menu.
    // The key never reaches the game or a keybind capture.
    if key == GLFW_KEY_END && mods & GLFW_MOD_CONTROL != 0 {
        if action == GLFW_PRESS {
            crate::graphic::ui_engine::request_panic();
        }
        return;
    }

    if action == GLFW_PRESS {
        LAST_KEY_PRESSED.store(key, Ordering::Relaxed);
    }

    let gui_was_open = GUI_OPEN.load(Ordering::Relaxed);

    // Right Shift toggles the GUI; ESC also closes it while it is open.
    if action == GLFW_PRESS
        && (key == GLFW_KEY_RIGHT_SHIFT || (key == GLFW_KEY_ESCAPE && gui_was_open))
    {
        toggle_gui();
    }

    // While the GUI is open, forward the keystroke to egui so text widgets
    // (DragValue editing, TextEdit, the color picker's hex / numeric fields)
    // see Backspace, arrows, Enter, digits, etc.
    if gui_was_open {
        if let Some(egui_key) = translate_glfw_key(key) {
            let pressed = action != GLFW_RELEASE;
            push_event(egui::Event::Key {
                key: egui_key,
                physical_key: None,
                pressed,
                repeat: action == GLFW_REPEAT,
                modifiers: translate_mods(mods),
            });
        }
    }

    // Swallow the event — instead of forwarding it — whenever the GUI is open,
    // or was open until this very keystroke closed it (so ESC closing the GUI
    // does not also reach Minecraft and open its pause menu).
    if gui_was_open || GUI_OPEN.load(Ordering::Relaxed) {
        return;
    }

    // Macro keys start / stop playback; while one plays, real keys are
    // kept from the game.
    if action == GLFW_PRESS && macros::on_key_press(key) {
        return;
    }
    if macros::playing_now() {
        return;
    }

    if action == GLFW_PRESS {
        handle_module_keybind(key);
    }

    // While spectating, keys drive the spectate camera and never reach the
    // game, whose key bindings stay as they were — the body keeps running,
    // mining, … The keybind above still works, so Spectate's own bind stops it.
    if spectate::capturing() {
        spectate::on_key(key, action);
        return;
    }

    macros::record(MacroEvent::Key { key, scancode, action, mods });
    if let Some(hooks) = HOOKS.get() {
        if !hooks.original_key.is_null() {
            let original: KeyFun = unsafe { std::mem::transmute(hooks.original_key) };
            original(window, key, scancode, action, mods);
        }
    }
}

/// Text-input callback: receives Unicode codepoints already mapped through the
/// active keyboard layout, so this — not `on_key` — is what supplies actual
/// typed characters to egui text widgets.
extern "C" fn on_char(window: *mut c_void, codepoint: u32) {
    let gui_was_open = GUI_OPEN.load(Ordering::Relaxed);

    if gui_was_open {
        if let Some(ch) = char::from_u32(codepoint) {
            push_event(egui::Event::Text(ch.to_string()));
        }
    }

    if gui_was_open || GUI_OPEN.load(Ordering::Relaxed) || spectate::capturing() || macros::playing_now() {
        return;
    }
    macros::record(MacroEvent::Char { codepoint });

    if let Some(hooks) = HOOKS.get() {
        if !hooks.original_char.is_null() {
            let original: CharFun = unsafe { std::mem::transmute(hooks.original_char) };
            original(window, codepoint);
        }
    }
}

extern "C" fn on_scroll(window: *mut c_void, x_offset: f64, y_offset: f64) {
    // While the GUI is open, feed the wheel to egui and swallow it — otherwise
    // it would also reach Minecraft and change the selected hotbar slot.
    if GUI_OPEN.load(Ordering::Relaxed) {
        if let Ok(mut state) = MOUSE_STATE.lock() {
            state.scroll_x += x_offset as f32;
            state.scroll_y += y_offset as f32;
        }
        return;
    }
    if macros::playing_now() {
        return;
    }
    // While spectating, the wheel zooms the camera / sets the fly speed.
    if spectate::capturing() {
        spectate::on_scroll(y_offset);
        return;
    }

    macros::record(MacroEvent::Scroll { dx: x_offset, dy: y_offset });
    if let Some(hooks) = HOOKS.get() {
        if !hooks.original_scroll.is_null() {
            let original: ScrollFun = unsafe { std::mem::transmute(hooks.original_scroll) };
            original(window, x_offset, y_offset);
        }
    }
}

/// Toggles the overlay GUI and the matching cursor-capture mode.
fn toggle_gui() {
    // `fetch_xor` returns the previous value; the new state is its negation.
    let open = !GUI_OPEN.fetch_xor(true, Ordering::Relaxed);
    if open {
        crate::graphic::menu::clear_search();
    }

    // Macro recording / playback is armed by closing the menu and ended by
    // opening it.
    macros::gui_toggled(open);

    // Persist anything the user changed in the menu before it closes.
    if !open {
        crate::config::save();
    }
    // The cursor is re-centred on close; Spectate must not read that as a
    // mouse movement.
    spectate::reset_cursor();
    freelook::reset_cursor();

    let Some(hooks) = HOOKS.get() else {
        return;
    };
    if hooks.window.is_null() {
        return;
    }

    if open {
        // GUI open: release the cursor.
        (hooks.set_input_mode)(hooks.window, GLFW_CURSOR, GLFW_CURSOR_NORMAL);
    } else if minecraft_screen_open() {
        // GUI closed onto an open Minecraft screen (chat, inventory, menu, …):
        // leave the cursor free, since that screen still needs it.
        (hooks.set_input_mode)(hooks.window, GLFW_CURSOR, GLFW_CURSOR_NORMAL);
    } else {
        // GUI closed back into the world: restore the cursor where Minecraft
        // last had it, then re-capture it — avoids a camera jump on the next
        // mouse move.
        let (lock_x, lock_y) = cursor_lock();
        (hooks.set_cursor_pos)(hooks.window, lock_x, lock_y);
        (hooks.set_input_mode)(hooks.window, GLFW_CURSOR, GLFW_CURSOR_DISABLED);
    }
}

/// Feeds one recorded event to the game's own input callbacks, as if it had
/// just come from GLFW — the macro playback path. A cursor event moves the
/// remembered cursor position by its offset.
pub fn replay(event: MacroEvent) {
    let Some(hooks) = HOOKS.get() else {
        return;
    };
    let window = hooks.window;
    if window.is_null() {
        return;
    }
    // SAFETY: each `original_*` pointer is the callback GLFW had installed
    // for that event type, with exactly the transmuted signature.
    unsafe {
        match event {
            MacroEvent::Key { key, scancode, action, mods } if !hooks.original_key.is_null() => {
                let original: KeyFun = std::mem::transmute(hooks.original_key);
                original(window, key, scancode, action, mods);
            }
            MacroEvent::Button { button, action, mods } if !hooks.original_mouse_button.is_null() => {
                let original: MouseButtonFun = std::mem::transmute(hooks.original_mouse_button);
                original(window, button, action, mods);
            }
            MacroEvent::Cursor { dx, dy, .. } => {
                let (x, y) = cursor_lock();
                replay_cursor(x + dx, y + dy, false);
            }
            MacroEvent::Scroll { dx, dy } if !hooks.original_scroll.is_null() => {
                let original: ScrollFun = std::mem::transmute(hooks.original_scroll);
                original(window, dx, dy);
            }
            MacroEvent::Char { codepoint } if !hooks.original_char.is_null() => {
                let original: CharFun = std::mem::transmute(hooks.original_char);
                original(window, codepoint);
            }
            _ => {}
        }
    }
}

/// Feeds the game a cursor position, as if the mouse had moved there — the
/// macro playback path. `move_pointer` also moves the visible pointer (in
/// screens, where it is shown; ignored where the system forbids it).
pub fn replay_cursor(x: f64, y: f64, move_pointer: bool) {
    let Some(hooks) = HOOKS.get() else {
        return;
    };
    if hooks.window.is_null() || hooks.original_cursor_pos.is_null() {
        return;
    }
    set_cursor_lock(x, y);
    if move_pointer && !GUI_OPEN.load(Ordering::Relaxed) {
        (hooks.set_cursor_pos)(hooks.window, x, y);
    }
    // SAFETY: the callback GLFW had installed for cursor movement.
    unsafe {
        let original: CursorPosFun = std::mem::transmute(hooks.original_cursor_pos);
        original(hooks.window, x, y);
    }
}

/// Moves the (captured) cursor back to where the game last saw it, so the
/// game's next mouse delta is a real movement rather than the distance the
/// cursor travelled while input was withheld from it (by Spectate).
pub fn recenter_cursor() {
    let Some(hooks) = HOOKS.get() else {
        return;
    };
    if hooks.window.is_null() || GUI_OPEN.load(Ordering::Relaxed) {
        return;
    }
    let (lock_x, lock_y) = cursor_lock();
    (hooks.set_cursor_pos)(hooks.window, lock_x, lock_y);
}

/// Toggles any module whose keybind matches `key`, when in-world.
fn handle_module_keybind(key: i32) {
    let minecraft = minecraft();
    if !minecraft.current_screen_is_null() || !minecraft.in_world() {
        return;
    }

    let mut changed = false;
    for handle in client().modules.handles() {
        let Ok(mut module) = handle.lock() else {
            continue;
        };
        if module.get_module_data().key_bind as i32 != key {
            continue;
        }

        let enabled = !module.get_module_data().enabled;
        info!(
            "{} {}",
            module.get_module_data().name(),
            if enabled { "enabled" } else { "disabled" }
        );
        if enabled {
            let _ = module.on_start();
        } else {
            let _ = module.on_stop();
        }
        module.get_module_data_mut().set_enabled(enabled);
        changed = true;
    }

    // Persist the new enabled state.
    if changed {
        crate::config::save();
    }
}

// --- Lifecycle -------------------------------------------------------------

/// Installs the GLFW input hooks once the window exists. Cheap to call every
/// frame: it returns immediately after the first success and only retries
/// while the window is not yet available.
pub fn init() {
    if HOOKS.get().is_some() {
        return;
    }
    if let Some(hooks) = install_glfw_hooks() {
        let _ = HOOKS.set(hooks);
    }
}

/// Resolves the GLFW symbols, swaps in our callbacks, and captures the state
/// needed to restore them. Returns `None` until the window is ready.
fn install_glfw_hooks() -> Option<GlfwHooks> {
    let library = crate::graphic::platform::open_glfw_library()?;

    unsafe {
        let get_context = *library
            .get::<GetCurrentContext>(b"glfwGetCurrentContext")
            .ok()?;
        let set_mouse_button = *library
            .get::<SetMouseButtonCallback>(b"glfwSetMouseButtonCallback")
            .ok()?;
        let set_cursor_pos_cb = *library
            .get::<SetCursorPosCallback>(b"glfwSetCursorPosCallback")
            .ok()?;
        let set_key = *library.get::<SetKeyCallback>(b"glfwSetKeyCallback").ok()?;
        let set_scroll = *library
            .get::<SetScrollCallback>(b"glfwSetScrollCallback")
            .ok()?;
        let set_char = *library
            .get::<SetCharCallback>(b"glfwSetCharCallback")
            .ok()?;
        let set_input_mode = *library.get::<SetInputMode>(b"glfwSetInputMode").ok()?;
        let set_cursor_pos = *library.get::<SetCursorPos>(b"glfwSetCursorPos").ok()?;

        let window = get_context();
        if window.is_null() {
            return None;
        }

        info!("GLFW window acquired; installing input callbacks.");

        let original_mouse_button = set_mouse_button(window, on_mouse_button);
        let original_cursor_pos = set_cursor_pos_cb(window, on_cursor_pos);
        let original_key = set_key(window, on_key);
        let original_scroll = set_scroll(window, on_scroll);
        let original_char = set_char(window, on_char);

        // If the GUI was toggled on before hooks existed, release the cursor.
        if GUI_OPEN.load(Ordering::Relaxed) {
            set_input_mode(window, GLFW_CURSOR, GLFW_CURSOR_NORMAL);
        }

        Some(GlfwHooks {
            library,
            window,
            original_mouse_button,
            original_cursor_pos,
            original_key,
            original_scroll,
            original_char,
            set_input_mode,
            set_cursor_pos,
        })
    }
}

/// Restores the host's original GLFW callbacks and cursor capture. Safe to
/// call even if the hooks were never installed.
pub fn cleanup() {
    let Some(hooks) = HOOKS.get() else {
        return;
    };
    if hooks.window.is_null() {
        return;
    }

    type RestoreCallback = extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void;
    let restorations: [(&[u8], *mut c_void); 5] = [
        (b"glfwSetMouseButtonCallback", hooks.original_mouse_button),
        (b"glfwSetCursorPosCallback", hooks.original_cursor_pos),
        (b"glfwSetKeyCallback", hooks.original_key),
        (b"glfwSetScrollCallback", hooks.original_scroll),
        (b"glfwSetCharCallback", hooks.original_char),
    ];
    unsafe {
        for (name, original) in restorations {
            if let Ok(restore) = hooks.library.get::<RestoreCallback>(name) {
                restore(hooks.window, original);
            }
        }
    }

    // Restore the cursor to the mode Minecraft itself wants right now: visible
    // while one of its screens (chat, inventory, menu, …) is open, captured
    // when in-world. Forcing it captured would hide the cursor after a Panic
    // triggered from inside a menu.
    let cursor_mode = if minecraft_screen_open() {
        GLFW_CURSOR_NORMAL
    } else {
        GLFW_CURSOR_DISABLED
    };
    (hooks.set_input_mode)(hooks.window, GLFW_CURSOR, cursor_mode);
    info!("GLFW input callbacks restored.");
}

/// Whether Minecraft has one of its own screens open — meaning the cursor
/// should be visible. Best-effort: a JNI failure resolves to "screen open",
/// so the safer, recoverable outcome (a visible cursor) wins.
fn minecraft_screen_open() -> bool {
    !minecraft().current_screen_is_null()
}
