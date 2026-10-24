//! Macros — record what you do with keyboard and mouse, then play it back.
//!
//! * **Record** — arm it in the Macros window; recording starts when the menu
//!   closes and stops when it is opened again (Right Shift). Every key, mouse
//!   button, mouse movement, wheel step and typed character that reaches the
//!   game is stored — in the world, in inventories / chests and in chat.
//! * **Play** — from the window (starts when the menu closes) or with the
//!   macro's own key while the module is on. Optionally looped: a number of
//!   times or forever, with a pause between runs. Esc, the macro's key or
//!   opening the menu stops it. While playing, your real input is kept from
//!   the game so the two do not mix.
//!
//! Precision:
//!
//! * **Timing is tick-locked.** In a world each event records the game tick
//!   it happened in (counted from the start) and the microseconds since that
//!   tick began. Playback fires it in the same tick, at the same offset — so a
//!   key held for 7 ticks is held for exactly 7 ticks again, and lag or a
//!   different frame rate does not shift movement. Outside a world (menus)
//!   it falls back to microsecond wall-clock timing.
//! * **The mouse is exact.** With the cursor captured (in the world) a
//!   movement is replayed as an offset from where the game itself has the
//!   cursor; with it free (inventory, chest, chat, any screen) as an absolute
//!   position, relative to the window's centre — screens are centred, so
//!   slots line up as long as the GUI scale is the same. The jump the game
//!   makes when it re-captures the cursor is never recorded.
//!
//! Playback feeds the recorded events to the game's own input callbacks (see
//! `graphic::input::replay`) from the frame hook. Keys and buttons still held
//! when it ends are released, and the game's key bindings re-synced.
//!
//! Saved in `macros.json` beside the client config. Macros recorded by older
//! builds (no tick or position data) still play, with the old timing.

use crate::config;
use crate::graphic::notification::{Notification, NotificationType};
use crate::mapping::raw;
use crate::mapping::MappedObject;
use crate::module::{KeyboardKey, Module, ModuleCategory, ModuleData, ModuleId};
use crate::state::{client, mapping, minecraft};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const FILE: &str = "macros.json";
const GLFW_RELEASE: i32 = 0;
const GLFW_PRESS: i32 = 1;
const GLFW_KEY_ESCAPE: i32 = 256;

/// One recorded input event.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Event {
    Key { key: i32, scancode: i32, action: i32, mods: i32 },
    Button { button: i32, action: i32, mods: i32 },
    /// Cursor movement: the offset from the previous position (used while
    /// the cursor is captured) and the position relative to the window
    /// centre (used while it is free, in screens).
    Cursor {
        dx: f64,
        dy: f64,
        #[serde(default)]
        cx: Option<f64>,
        #[serde(default)]
        cy: Option<f64>,
    },
    Scroll { dx: f64, dy: f64 },
    /// A typed character (chat, signs, …).
    Char { codepoint: u32 },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Timed {
    /// Milliseconds since the recording started.
    pub ms: u64,
    /// Microseconds since the recording started (0 in old recordings).
    #[serde(default)]
    pub us: u64,
    /// Game ticks since the recording started; `None` outside a world.
    #[serde(default)]
    pub tick: Option<u64>,
    /// Microseconds into that tick.
    #[serde(default)]
    pub tick_us: u64,
    pub event: Event,
}

impl Timed {
    /// Wall-clock time since the start, in microseconds.
    pub fn time_us(&self) -> u64 {
        if self.us > 0 {
            self.us
        } else {
            self.ms * 1000
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Macro {
    pub name: String,
    /// Playback key (a GLFW key or mouse-button bind code), -1 = none.
    #[serde(default = "no_bind")]
    pub bind: i32,
    /// Loop the macro.
    #[serde(default)]
    pub repeat: bool,
    /// How many runs when looping; 0 = until stopped.
    #[serde(default)]
    pub loops: u32,
    /// Pause between runs, in milliseconds.
    #[serde(default)]
    pub loop_delay_ms: u64,
    pub events: Vec<Timed>,
}

fn no_bind() -> i32 {
    -1
}

impl Macro {
    pub fn duration_ms(&self) -> u64 {
        self.events.last().map_or(0, |timed| timed.time_us() / 1000)
    }
}

/// Pure: whether a recorded event is due. With both the event's tick and
/// the playback's tick count (in a world), by tick then offset into the
/// tick; otherwise by wall-clock time.
pub fn is_due(event: &Timed, ticks_played: Option<u64>, us_into_tick: u64, us_elapsed: u64) -> bool {
    match (event.tick, ticks_played) {
        (Some(tick), Some(played)) => played > tick || (played == tick && us_into_tick >= event.tick_us),
        _ => us_elapsed >= event.time_us(),
    }
}

/// Pure: whether another run follows run number `finished` (0-based).
pub fn loops_again(recording: &Macro, finished: u32) -> bool {
    recording.repeat && (recording.loops == 0 || finished + 1 < recording.loops)
}

/// Pure: where a recorded cursor event puts the cursor. `captured` — the
/// cursor is captured now (in the world); `current` — where the game has it;
/// `size` — the window size in screen coordinates.
pub fn cursor_target(event: &Event, captured: bool, current: (f64, f64), size: (f64, f64)) -> Option<(f64, f64)> {
    let Event::Cursor { dx, dy, cx, cy } = *event else {
        return None;
    };
    match (captured, cx, cy) {
        (false, Some(cx), Some(cy)) => Some((size.0 / 2.0 + cx, size.1 / 2.0 + cy)),
        _ => Some((current.0 + dx, current.1 + dy)),
    }
}

// --- storage --------------------------------------------------------------------

static MACROS: Mutex<Option<Vec<Macro>>> = Mutex::new(None);

fn with_macros<T>(f: impl FnOnce(&mut Vec<Macro>) -> T) -> T {
    let mut guard = MACROS.lock().unwrap_or_else(|p| p.into_inner());
    f(guard.get_or_insert_with(|| {
        std::fs::read_to_string(config::base_dir().join(FILE))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }))
}

fn save(macros: &[Macro]) {
    let json = serde_json::to_string(macros).unwrap_or_else(|_| "[]".into());
    let path = config::base_dir().join(FILE);
    if let Err(error) = std::fs::write(&path, json) {
        log::warn!("Macros: could not write {}: {error}", path.display());
    }
}

/// Every saved macro.
pub fn list() -> Vec<Macro> {
    with_macros(|macros| macros.clone())
}

/// Edits the saved macros and writes them out.
pub fn edit(f: impl FnOnce(&mut Vec<Macro>)) {
    with_macros(|macros| {
        f(macros);
        save(macros);
    });
}

// --- what the game shows ----------------------------------------------------------

/// The game's input situation, read once per frame while macros are busy and
/// used by the input callbacks (which must not make JNI calls per event).
#[derive(Debug, Clone, Copy, Default)]
struct View {
    in_world: bool,
    /// The cursor is captured (world) rather than free (a screen).
    captured: bool,
    /// Window size, in the same screen coordinates as cursor positions.
    width: f64,
    height: f64,
}

static VIEW: Mutex<View> = Mutex::new(View {
    in_world: false,
    captured: true,
    width: 0.0,
    height: 0.0,
});

fn view() -> View {
    *VIEW.lock().unwrap_or_else(|p| p.into_inner())
}

fn refresh_view() {
    let read = mapping().in_frame(|| {
        let mut env = mapping().get_env()?;
        let mc = minecraft().jni_ref().as_obj();
        let mouse = raw::field(&mut env, mc, "mouseHandler", "Lnet/minecraft/client/MouseHandler;")?.l()?;
        let captured = raw::call(&mut env, &mouse, "isMouseGrabbed", "()Z", &[])?.z()?;
        let window = raw::call(&mut env, mc, "getWindow", "()Lcom/mojang/blaze3d/platform/Window;", &[])?.l()?;
        let width = raw::call(&mut env, &window, "getScreenWidth", "()I", &[])?.i()?;
        let height = raw::call(&mut env, &window, "getScreenHeight", "()I", &[])?.i()?;
        Ok((captured, width as f64, height as f64))
    });
    let in_world = minecraft().in_world();
    let mut view = VIEW.lock().unwrap_or_else(|p| p.into_inner());
    view.in_world = in_world;
    match read {
        Ok((captured, width, height)) => {
            view.captured = captured;
            view.width = width;
            view.height = height;
        }
        Err(error) => log::debug!("Macros: view: {error}"),
    }
}

/// Where the game's mouse handler has the cursor.
fn game_cursor() -> anyhow::Result<(f64, f64)> {
    mapping().in_frame(|| {
        let mut env = mapping().get_env()?;
        let mouse = raw::field(
            &mut env,
            minecraft().jni_ref().as_obj(),
            "mouseHandler",
            "Lnet/minecraft/client/MouseHandler;",
        )?
        .l()?;
        Ok((
            raw::field(&mut env, &mouse, "xpos", "D")?.d()?,
            raw::field(&mut env, &mouse, "ypos", "D")?.d()?,
        ))
    })
}

// --- state ------------------------------------------------------------------------

enum State {
    Idle,
    /// Recording starts when the menu closes.
    ArmedRecord { name: String },
    Recording {
        name: String,
        start: Instant,
        start_tick: u64,
        events: Vec<Timed>,
        last_cursor: Option<(f64, f64)>,
        /// Whether the cursor was captured at the last cursor event.
        last_captured: bool,
    },
    /// Playback starts when the menu closes.
    ArmedPlay { index: usize },
    Playing {
        /// The macro being played, copied at start.
        recording: Macro,
        start: Instant,
        start_tick: u64,
        next: usize,
        /// Which run this is (0-based).
        run: u32,
        /// Between runs: the next one starts then.
        paused_until: Option<Instant>,
        /// Keys / buttons pressed by the playback and not yet released.
        held_keys: HashSet<(i32, i32)>,
        held_buttons: HashSet<i32>,
    },
}

static STATE: Mutex<State> = Mutex::new(State::Idle);

fn state() -> std::sync::MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|p| p.into_inner())
}

fn ticks_now() -> u64 {
    crate::graphic::hook::game_clock().0
}

/// A line for the window: what the macro system is doing.
pub fn status() -> String {
    match &*state() {
        State::Idle => "Idle".into(),
        State::ArmedRecord { name } => format!("Close the menu to start recording \"{name}\""),
        State::Recording { name, start, events, .. } => format!(
            "Recording \"{name}\" — {:.1}s, {} events. Open the menu to stop.",
            start.elapsed().as_secs_f32(),
            events.len()
        ),
        State::ArmedPlay { .. } => "Close the menu to play".into(),
        State::Playing { recording, start, run, paused_until, .. } => {
            let runs = match (recording.repeat, recording.loops) {
                (false, _) => String::new(),
                (true, 0) => format!(" · run {}, until stopped", run + 1),
                (true, loops) => format!(" · run {} of {loops}", run + 1),
            };
            if paused_until.is_some() {
                format!("Pause between runs{runs}. Esc stops.")
            } else {
                format!("Playing — {:.1}s{runs}. Esc stops.", start.elapsed().as_secs_f32())
            }
        }
    }
}

/// Whether nothing is armed, recording or playing.
pub fn idle() -> bool {
    matches!(*state(), State::Idle)
}

/// Arms a recording named `name`.
pub fn arm_record(name: &str) {
    *state() = State::ArmedRecord { name: name.trim().to_string() };
}

/// Arms playback of macro `index`.
pub fn arm_play(index: usize) {
    *state() = State::ArmedPlay { index };
}

/// Stops whatever is armed / recording / playing (a recording is saved).
pub fn stop() {
    let previous = std::mem::replace(&mut *state(), State::Idle);
    finish(previous);
}

fn finish(previous: State) {
    match previous {
        State::Recording { name, events, .. } => {
            if events.is_empty() {
                Notification::send(NotificationType::Warning, "Macros", "Nothing was recorded");
                return;
            }
            let name = if name.is_empty() { format!("Macro {}", list().len() + 1) } else { name };
            Notification::send(NotificationType::Info, "Macros", &format!("Saved \"{name}\""));
            edit(|macros| {
                macros.push(Macro {
                    name,
                    bind: -1,
                    repeat: false,
                    loops: 0,
                    loop_delay_ms: 0,
                    events,
                })
            });
        }
        State::Playing { held_keys, held_buttons, .. } => release(&held_keys, &held_buttons),
        _ => {}
    }
}

/// Releases what the playback left pressed and re-syncs the game's key state.
fn release(keys: &HashSet<(i32, i32)>, buttons: &HashSet<i32>) {
    for &(key, scancode) in keys {
        crate::graphic::input::replay(Event::Key { key, scancode, action: GLFW_RELEASE, mods: 0 });
    }
    for &button in buttons {
        crate::graphic::input::replay(Event::Button { button, action: GLFW_RELEASE, mods: 0 });
    }
    if let Err(error) = minecraft().resync_key_mappings() {
        log::debug!("Macros: re-sync failed: {error}");
    }
    crate::graphic::input::recenter_cursor();
}

/// The menu opened (`true`) or closed: arms turn into runs, runs stop.
pub fn gui_toggled(open: bool) {
    let mut guard = state();
    let current = std::mem::replace(&mut *guard, State::Idle);
    *guard = match (current, open) {
        (State::ArmedRecord { name }, false) => State::Recording {
            name,
            start: Instant::now(),
            start_tick: ticks_now(),
            events: Vec::new(),
            last_cursor: None,
            last_captured: true,
        },
        (State::ArmedPlay { index }, false) => playing(index).unwrap_or(State::Idle),
        (running @ (State::Recording { .. } | State::Playing { .. }), true) => {
            drop(guard);
            finish(running);
            return;
        }
        (other, _) => other,
    };
}

fn playing(index: usize) -> Option<State> {
    let recording = list().get(index).cloned()?;
    Some(State::Playing {
        recording,
        start: Instant::now(),
        start_tick: ticks_now(),
        next: 0,
        run: 0,
        paused_until: None,
        held_keys: HashSet::new(),
        held_buttons: HashSet::new(),
    })
}

/// Whether a playback runs — real input is then kept from the game.
pub fn playing_now() -> bool {
    matches!(*state(), State::Playing { .. })
}

/// Whether the macros module is switched on (playback keys work only then).
fn module_on() -> bool {
    client()
        .modules
        .get(ModuleId::Macros)
        .and_then(|handle| handle.lock().ok().map(|m| m.get_module_data().enabled))
        .unwrap_or(false)
}

/// A real key press in the world, before the game sees it. Returns `true`
/// when it was used — to stop or start a playback — and must be swallowed.
pub fn on_key_press(key: i32) -> bool {
    let playing_bind = match &*state() {
        State::Playing { recording, .. } => Some(recording.bind),
        _ => None,
    };
    if let Some(bind) = playing_bind {
        if key == GLFW_KEY_ESCAPE || key == bind {
            stop();
        }
        return true; // real input is withheld while playing
    }
    if !idle() || !module_on() || key < 0 {
        return false;
    }
    // Not while typing in chat or with any other game screen open.
    if !minecraft().current_screen_is_null() || !minecraft().in_world() {
        return false;
    }
    let Some(index) = list().iter().position(|m| m.bind == key) else {
        return false;
    };
    match playing(index) {
        Some(run) => {
            *state() = run;
            true
        }
        None => false,
    }
}

/// The time stamp of an event recorded now.
fn stamp(start: Instant, start_tick: u64, view: View, event: Event) -> Timed {
    let us = start.elapsed().as_micros() as u64;
    let (ticks, since_tick) = crate::graphic::hook::game_clock();
    Timed {
        ms: us / 1000,
        us: us.max(1),
        tick: view.in_world.then(|| ticks.saturating_sub(start_tick)),
        tick_us: since_tick.as_micros() as u64,
        event,
    }
}

/// Records a real input event that is about to reach the game.
pub fn record(event: Event) {
    let view = view();
    if let State::Recording { start, start_tick, events, .. } = &mut *state() {
        events.push(stamp(*start, *start_tick, view, event));
    }
}

/// Records a real cursor position: its offset from the previous one and its
/// place relative to the window centre.
pub fn record_cursor(x: f64, y: f64) {
    let view = view();
    if let State::Recording { start, start_tick, events, last_cursor, last_captured, .. } = &mut *state() {
        // The game warps the cursor when it captures / frees it; that jump
        // is not a movement.
        if *last_captured != view.captured {
            *last_cursor = None;
            *last_captured = view.captured;
        }
        let (dx, dy) = match *last_cursor {
            Some((last_x, last_y)) => (x - last_x, y - last_y),
            None => (0.0, 0.0),
        };
        *last_cursor = Some((x, y));
        // A captured cursor only matters by how far it moved; a free one by
        // where it is (the first position in a screen counts too).
        if view.captured && dx == 0.0 && dy == 0.0 {
            return;
        }
        let event = Event::Cursor {
            dx,
            dy,
            cx: (view.width > 0.0).then(|| x - view.width / 2.0),
            cy: (view.height > 0.0).then(|| y - view.height / 2.0),
        };
        events.push(stamp(*start, *start_tick, view, event));
    }
}

/// What the frame hook hands to the game this frame.
enum Output {
    Input(Event),
    Cursor(f64, f64),
}

/// Plays the events that are due. Called every frame from the frame hook.
pub fn frame() {
    if matches!(*state(), State::Idle) {
        return;
    }
    refresh_view();
    let view = view();

    let mut out = Vec::new();
    let mut ended = false;
    {
        let mut guard = state();
        let State::Playing { recording, start, start_tick, next, run, paused_until, held_keys, held_buttons } =
            &mut *guard
        else {
            return;
        };
        if let Some(until) = *paused_until {
            if Instant::now() < until {
                return;
            }
            *paused_until = None;
            *start = Instant::now();
            *start_tick = ticks_now();
            *next = 0;
        }
        let (ticks, since_tick) = crate::graphic::hook::game_clock();
        let played = view.in_world.then(|| ticks.saturating_sub(*start_tick));
        let elapsed = start.elapsed().as_micros() as u64;
        let since_tick = since_tick.as_micros() as u64;

        // Cursor positions are computed from where the game has the cursor,
        // read once and then followed through this frame's movements.
        let mut cursor: Option<(f64, f64)> = None;
        while let Some(timed) = recording.events.get(*next) {
            if !is_due(timed, played, since_tick, elapsed) {
                break;
            }
            match timed.event {
                Event::Key { key, scancode, action, .. } => {
                    if action == GLFW_RELEASE {
                        held_keys.remove(&(key, scancode));
                    } else if action == GLFW_PRESS {
                        held_keys.insert((key, scancode));
                    }
                    out.push(Output::Input(timed.event));
                }
                Event::Button { button, action, .. } => {
                    if action == GLFW_RELEASE {
                        held_buttons.remove(&button);
                    } else {
                        held_buttons.insert(button);
                    }
                    out.push(Output::Input(timed.event));
                }
                Event::Cursor { .. } => {
                    let current = match cursor {
                        Some(position) => position,
                        None => game_cursor().unwrap_or((view.width / 2.0, view.height / 2.0)),
                    };
                    if let Some(target) =
                        cursor_target(&timed.event, view.captured, current, (view.width, view.height))
                    {
                        cursor = Some(target);
                        out.push(Output::Cursor(target.0, target.1));
                    }
                }
                _ => out.push(Output::Input(timed.event)),
            }
            *next += 1;
        }
        if *next >= recording.events.len() {
            if loops_again(recording, *run) {
                *run += 1;
                *paused_until = Some(Instant::now() + Duration::from_millis(recording.loop_delay_ms));
            } else {
                ended = true;
            }
        }
    }

    // Only the last cursor position of the frame moves the visible pointer.
    let last_cursor = out.iter().rposition(|o| matches!(o, Output::Cursor(..)));
    for (index, output) in out.into_iter().enumerate() {
        match output {
            Output::Input(event) => crate::graphic::input::replay(event),
            Output::Cursor(x, y) => {
                let move_pointer = !view.captured && Some(index) == last_cursor;
                crate::graphic::input::replay_cursor(x, y, move_pointer);
            }
        }
    }
    if ended {
        stop();
    }
}

// --- the module ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct MacrosModule {
    pub module: ModuleData,
}

impl MacrosModule {
    pub fn new() -> Self {
        Self {
            module: ModuleData {
                id: ModuleId::Macros,
                description: "Record and play back keyboard / mouse input".to_string(),
                category: ModuleCategory::Misc,
                key_bind: KeyboardKey::KeyNone,
                enabled: false,
                settings: vec![],
            },
        }
    }
}

impl Module for MacrosModule {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn timed(us: u64, tick: Option<u64>, tick_us: u64) -> Timed {
        Timed {
            ms: us / 1000,
            us,
            tick,
            tick_us,
            event: Event::Char { codepoint: 'a' as u32 },
        }
    }

    #[test]
    fn in_a_world_events_wait_for_their_tick() {
        let event = timed(5_000_000, Some(7), 20_000);
        // Wall-clock time is ignored when ticks are known: lag cannot shift it.
        assert!(!is_due(&event, Some(6), 49_000, 9_000_000));
        assert!(!is_due(&event, Some(7), 10_000, 0));
        assert!(is_due(&event, Some(7), 20_000, 0));
        assert!(is_due(&event, Some(8), 0, 0));
    }

    #[test]
    fn outside_a_world_wall_clock_time_is_used() {
        let event = timed(1_500, None, 0);
        assert!(!is_due(&event, Some(100), 0, 1_000));
        assert!(is_due(&event, Some(0), 0, 1_500));
        // Recorded in a world, played at the menu.
        let event = timed(2_000, Some(3), 0);
        assert!(is_due(&event, None, 0, 2_000));
    }

    #[test]
    fn old_recordings_keep_millisecond_timing() {
        let old = Timed { ms: 12, us: 0, tick: None, tick_us: 0, event: Event::Char { codepoint: 0 } };
        assert_eq!(old.time_us(), 12_000);
        let json = r#"{"ms":5,"event":{"Cursor":{"dx":1.0,"dy":2.0}}}"#;
        let parsed: Timed = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.time_us(), 5_000);
        assert_eq!(parsed.event, Event::Cursor { dx: 1.0, dy: 2.0, cx: None, cy: None });
    }

    #[test]
    fn loops_count_runs() {
        let mut recording = Macro {
            name: String::new(),
            bind: -1,
            repeat: false,
            loops: 3,
            loop_delay_ms: 0,
            events: vec![],
        };
        assert!(!loops_again(&recording, 0));
        recording.repeat = true;
        assert!(loops_again(&recording, 0));
        assert!(loops_again(&recording, 1));
        assert!(!loops_again(&recording, 2)); // three runs done
        recording.loops = 0;
        assert!(loops_again(&recording, 1_000_000));
    }

    #[test]
    fn the_cursor_is_absolute_in_screens_and_relative_in_the_world() {
        let event = Event::Cursor { dx: 3.0, dy: -2.0, cx: Some(-100.0), cy: Some(40.0) };
        // In a screen of another size: still the same offset from the centre.
        assert_eq!(cursor_target(&event, false, (0.0, 0.0), (1000.0, 600.0)), Some((400.0, 340.0)));
        // In the world: from where the game has the cursor.
        assert_eq!(cursor_target(&event, true, (500.0, 300.0), (1000.0, 600.0)), Some((503.0, 298.0)));
        // Old recordings have no position: relative everywhere.
        let old = Event::Cursor { dx: 1.0, dy: 1.0, cx: None, cy: None };
        assert_eq!(cursor_target(&old, false, (10.0, 10.0), (100.0, 100.0)), Some((11.0, 11.0)));
        assert_eq!(cursor_target(&Event::Char { codepoint: 0 }, true, (0.0, 0.0), (1.0, 1.0)), None);
    }
}
