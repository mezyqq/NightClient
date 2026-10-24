use crate::cleanup_client;
use crate::graphic::input::{GUI_OPEN, MOUSE_STATE, PENDING_EVENTS};
use egui::Context;
use egui_glow::Painter;
use lazy_static::lazy_static;
use std::sync::atomic::Ordering;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

pub struct EguiState {
    pub ctx: Context,
    pub painter: Painter,
    pub last_left_down: bool,
    pub last_right_down: bool,
    pub last_mouse_pos: egui::Pos2,
}

lazy_static! {
    pub static ref EGUI_STATE: Mutex<Option<EguiState>> = Mutex::new(None);
}

/// Monotonic seconds since the overlay first rendered.
///
/// Fed to egui as `RawInput::time` so its clock tracks wall time instead of a
/// fixed predicted delta — the cure for animations stuttering when Minecraft
/// paces frames irregularly (most visibly while the game is paused).
fn elapsed_seconds() -> f64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs_f64()
}

pub fn gather_egui_inputs(
    state: &mut EguiState,
    screen_width: f32,
    screen_height: f32,
    scale_factor: f32,
) -> egui::RawInput {
    let mut raw_input = egui::RawInput {
        time: Some(elapsed_seconds()),
        ..Default::default()
    };

    let viewport_info = egui::ViewportInfo {
        native_pixels_per_point: Some(scale_factor),
        ..Default::default()
    };

    raw_input
        .viewports
        .insert(egui::ViewportId::ROOT, viewport_info);

    let logical_width = screen_width / scale_factor;
    let logical_height = screen_height / scale_factor;

    raw_input.screen_rect = Some(egui::Rect::from_min_max(
        egui::pos2(0.0, 0.0),
        egui::pos2(logical_width, logical_height),
    ));

    if !GUI_OPEN.load(Ordering::Relaxed) {
        // Drop any keystrokes captured during the brief window between the GUI
        // closing and the next frame so they cannot leak into the next session.
        if let Ok(mut queue) = PENDING_EVENTS.lock() {
            queue.clear();
        }
        return raw_input;
    }

    // Drain keyboard / text events pushed by the GLFW char + key callbacks.
    if let Ok(mut queue) = PENDING_EVENTS.lock() {
        raw_input.events.append(&mut queue);
    }

    let mut mouse = MOUSE_STATE.lock().unwrap();
    let current_pos = egui::pos2(
        (mouse.x as f32) / scale_factor,
        (mouse.y as f32) / scale_factor,
    );

    if current_pos != state.last_mouse_pos {
        raw_input
            .events
            .push(egui::Event::PointerMoved(current_pos));
        state.last_mouse_pos = current_pos;
    }

    if mouse.left_down != state.last_left_down {
        raw_input.events.push(egui::Event::PointerButton {
            pos: current_pos,
            button: egui::PointerButton::Primary,
            pressed: mouse.left_down,
            modifiers: Default::default(),
        });
        state.last_left_down = mouse.left_down;
    }

    if mouse.right_down != state.last_right_down {
        raw_input.events.push(egui::Event::PointerButton {
            pos: current_pos,
            button: egui::PointerButton::Secondary,
            pressed: mouse.right_down,
            modifiers: Default::default(),
        });
        state.last_right_down = mouse.right_down;
    }

    // Drain the accumulated scroll-wheel delta into an egui event.
    let (scroll_x, scroll_y) = (mouse.scroll_x, mouse.scroll_y);
    mouse.scroll_x = 0.0;
    mouse.scroll_y = 0.0;
    if scroll_x != 0.0 || scroll_y != 0.0 {
        raw_input.events.push(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Line,
            delta: egui::vec2(scroll_x, scroll_y),
            modifiers: Default::default(),
        });
    }

    raw_input
}

pub unsafe fn render_egui_ui() {
    let mut viewport = [0; 4];
    crate::gl::GetIntegerv(crate::gl::VIEWPORT, viewport.as_mut_ptr());
    let screen_width = viewport[2] as f32;
    let screen_height = viewport[3] as f32;

    let scale_factor = (screen_height / 720.0).max(1.0).floor();

    let mut state_guard = EGUI_STATE.lock().unwrap();

    if state_guard.is_none() {
        let gl = glow::Context::from_loader_function(|s| {
            crate::graphic::platform::gl_proc_address(s) as *const _
        });
        let gl = std::sync::Arc::new(gl);
        let ctx = egui::Context::default();

        let fonts = egui::FontDefinitions::default();
        ctx.set_fonts(fonts);

        // Install the overlay theme once — egui stores the style in an Arc,
        // so there is no reason to rebuild it every frame.
        crate::graphic::theme::apply(&ctx);

        unsafe {
            crate::gl::PixelStorei(crate::gl::UNPACK_ALIGNMENT, 1);
            crate::gl::PixelStorei(crate::gl::UNPACK_ROW_LENGTH, 0);
            crate::gl::PixelStorei(crate::gl::UNPACK_SKIP_PIXELS, 0);
            crate::gl::PixelStorei(crate::gl::UNPACK_SKIP_ROWS, 0);
        }

        let painter = egui_glow::Painter::new(gl, "", None, false).unwrap();

        *state_guard = Some(EguiState {
            ctx,
            painter,
            last_left_down: false,
            last_right_down: false,
            last_mouse_pos: egui::pos2(0.0, 0.0),
        });
    }

    let state = state_guard.as_mut().unwrap();
    let raw_input = gather_egui_inputs(state, screen_width, screen_height, scale_factor);

    let ctx = state.ctx.clone();

    let full_output = ctx.run(raw_input, |ctx| {
        crate::graphic::gui::render_all(ctx);
    });

    let clipped_primitives = state
        .ctx
        .tessellate(full_output.shapes, full_output.pixels_per_point);

    // Reset pixel unpack state that is often corrupted by Minecraft
    unsafe {
        let mut last_texture = 0;
        crate::gl::GetIntegerv(crate::gl::TEXTURE_BINDING_2D, &mut last_texture);
        let mut last_active_texture = 0;
        crate::gl::GetIntegerv(crate::gl::ACTIVE_TEXTURE, &mut last_active_texture);
        let mut last_array_buffer = 0;
        crate::gl::GetIntegerv(crate::gl::ARRAY_BUFFER_BINDING, &mut last_array_buffer);
        let mut last_element_array_buffer = 0;
        crate::gl::GetIntegerv(
            crate::gl::ELEMENT_ARRAY_BUFFER_BINDING,
            &mut last_element_array_buffer,
        );
        let mut last_vertex_array = 0;
        crate::gl::GetIntegerv(crate::gl::VERTEX_ARRAY_BINDING, &mut last_vertex_array);
        let mut last_program = 0;
        crate::gl::GetIntegerv(crate::gl::CURRENT_PROGRAM, &mut last_program);

        crate::gl::BindBuffer(crate::gl::PIXEL_UNPACK_BUFFER, 0);

        crate::gl::ActiveTexture(crate::gl::TEXTURE0);

        // Modern Minecraft (Blaze3D, 26.x+) binds GL sampler objects to its
        // texture units. A sampler object *overrides* the texture's own
        // parameters, so a leftover Minecraft sampler — configured for its
        // mipmapped atlas textures — makes egui's mipmap-less font atlas
        // texture-incomplete, and an incomplete texture samples as opaque
        // black. That black multiplies into every egui fragment (panels and
        // text alike). Unbind it from unit 0 — the only unit egui samples —
        // and restore it afterwards so Minecraft's own rendering is untouched.
        let mut last_sampler = 0;
        crate::gl::GetIntegerv(crate::gl::SAMPLER_BINDING, &mut last_sampler);
        crate::gl::BindSampler(0, 0);

        crate::gl::PixelStorei(crate::gl::UNPACK_ALIGNMENT, 1);
        crate::gl::PixelStorei(crate::gl::UNPACK_ROW_LENGTH, 0);
        crate::gl::PixelStorei(crate::gl::UNPACK_SKIP_PIXELS, 0);
        crate::gl::PixelStorei(crate::gl::UNPACK_SKIP_ROWS, 0);

        // Older Minecraft (≤ 1.21.x) caches GL capabilities in GlStateManager
        // and skips glEnable when it believes a flag is already on — so a
        // DEPTH_TEST / CULL_FACE left disabled here is never turned back on,
        // and the world renders without depth (caves showing through terrain).
        let fixed_state = FixedState::capture();

        crate::gl::Disable(crate::gl::CULL_FACE);
        crate::gl::Disable(crate::gl::DEPTH_TEST);
        crate::gl::Enable(crate::gl::BLEND);

        state.painter.paint_and_update_textures(
            [screen_width as u32, screen_height as u32],
            full_output.pixels_per_point,
            &clipped_primitives,
            &full_output.textures_delta,
        );

        fixed_state.restore();

        // Restore critical state to prevent Minecraft rendering corruption
        // (and prevent Minecraft from corrupting our EGUI texture/vao on the next frame)
        crate::gl::BindTexture(crate::gl::TEXTURE_2D, last_texture as u32);
        crate::gl::BindBuffer(crate::gl::ARRAY_BUFFER, last_array_buffer as u32);
        crate::gl::BindBuffer(
            crate::gl::ELEMENT_ARRAY_BUFFER,
            last_element_array_buffer as u32,
        );
        crate::gl::BindVertexArray(last_vertex_array as u32);
        crate::gl::UseProgram(last_program as u32);
        crate::gl::BindSampler(0, last_sampler as u32);
        crate::gl::ActiveTexture(last_active_texture as u32);
    }
}

/// The fixed-function GL state egui's painter changes — capabilities, blend
/// setup, colour mask, viewport and scissor box — captured before painting
/// and put back afterwards, so the host renderer finds the state it left.
struct FixedState {
    caps: [(u32, bool); 5],
    blend_src_rgb: i32,
    blend_dst_rgb: i32,
    blend_src_alpha: i32,
    blend_dst_alpha: i32,
    blend_eq_rgb: i32,
    blend_eq_alpha: i32,
    color_mask: [u8; 4],
    viewport: [i32; 4],
    scissor_box: [i32; 4],
}

impl FixedState {
    const CAPS: [u32; 5] = [
        crate::gl::DEPTH_TEST,
        crate::gl::CULL_FACE,
        crate::gl::BLEND,
        crate::gl::SCISSOR_TEST,
        crate::gl::FRAMEBUFFER_SRGB,
    ];

    unsafe fn capture() -> Self {
        let int = |name| {
            let mut value = 0;
            crate::gl::GetIntegerv(name, &mut value);
            value
        };
        let mut color_mask = [0u8; 4];
        crate::gl::GetBooleanv(crate::gl::COLOR_WRITEMASK, color_mask.as_mut_ptr());
        let mut viewport = [0; 4];
        crate::gl::GetIntegerv(crate::gl::VIEWPORT, viewport.as_mut_ptr());
        let mut scissor_box = [0; 4];
        crate::gl::GetIntegerv(crate::gl::SCISSOR_BOX, scissor_box.as_mut_ptr());

        FixedState {
            caps: Self::CAPS.map(|cap| (cap, crate::gl::IsEnabled(cap) == crate::gl::TRUE)),
            blend_src_rgb: int(crate::gl::BLEND_SRC_RGB),
            blend_dst_rgb: int(crate::gl::BLEND_DST_RGB),
            blend_src_alpha: int(crate::gl::BLEND_SRC_ALPHA),
            blend_dst_alpha: int(crate::gl::BLEND_DST_ALPHA),
            blend_eq_rgb: int(crate::gl::BLEND_EQUATION_RGB),
            blend_eq_alpha: int(crate::gl::BLEND_EQUATION_ALPHA),
            color_mask,
            viewport,
            scissor_box,
        }
    }

    unsafe fn restore(&self) {
        for (cap, enabled) in self.caps {
            if enabled {
                crate::gl::Enable(cap);
            } else {
                crate::gl::Disable(cap);
            }
        }
        crate::gl::BlendFuncSeparate(
            self.blend_src_rgb as u32,
            self.blend_dst_rgb as u32,
            self.blend_src_alpha as u32,
            self.blend_dst_alpha as u32,
        );
        crate::gl::BlendEquationSeparate(self.blend_eq_rgb as u32, self.blend_eq_alpha as u32);
        let [r, g, b, a] = self.color_mask;
        crate::gl::ColorMask(r, g, b, a);
        let [x, y, w, h] = self.viewport;
        crate::gl::Viewport(x, y, w, h);
        let [x, y, w, h] = self.scissor_box;
        crate::gl::Scissor(x, y, w, h);
    }
}

/// Set once a Panic has been requested, so a held or repeated key cannot
/// start a second unload.
static PANIC_REQUESTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Runs [`call_panic`] once, on its own thread — it removes the very hooks
/// (input callback, frame hook) a request may come from.
pub fn request_panic() {
    if !PANIC_REQUESTED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        log::info!("Panic requested — unloading");
        std::thread::spawn(call_panic);
    }
}

pub fn call_panic() {
    // Persist the user's setup before tearing it down: Panic does not go
    // through the GUI's close handler (which is what normally saves), and the
    // modules are about to be force-disabled for the unload — so save now,
    // while they still hold the state the user actually chose.
    crate::config::save();

    // A module is only running — and so only needs stopping — while active.
    let world_active = crate::state::client().modules.is_active();
    for handle in crate::state::client().modules.handles() {
        let Ok(mut module) = handle.lock() else {
            continue;
        };
        if module.get_module_data().enabled {
            module.get_module_data_mut().set_enabled(false);
            if world_active {
                if let Err(e) = module.on_stop() {
                    log::error!(
                        "Failed to stop module {} on panic: {}",
                        module.get_module_data().name(),
                        e
                    );
                }
            }
        }
    }
    cleanup_client();
}
