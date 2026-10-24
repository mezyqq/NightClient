//! Always-on HUD: the brand watermark and the active-module array list.
//!
//! Everything is drawn straight onto a background [`egui::Painter`] with
//! absolute screen coordinates — no layout passes, no per-widget `Area`s.

use crate::graphic::anim::{self, Easing};
use crate::graphic::theme;
use egui::{
    Align2, Color32, Context, FontId, Id, LayerId, Order, Painter, Rect, Rounding, Stroke, Vec2,
};

/// Screen-edge padding shared by every HUD element.
const MARGIN: f32 = 10.0;

/// Draws the full HUD onto the background layer.
pub fn draw(ctx: &Context) {
    let painter = ctx.layer_painter(LayerId::new(Order::Background, Id::new("hud_layer")));
    draw_watermark(ctx, &painter);
    draw_arraylist(ctx, &painter);
    draw_spectate_bar(ctx, &painter);
}

/// Spectate's bar just above the hotbar: `◀  name  ▶`, the mode, and the
/// controls. Shown only while a player is being watched.
fn draw_spectate_bar(ctx: &Context, painter: &Painter) {
    use crate::module::player::spectate::{self, Mode};
    let Some(info) = spectate::hud_info() else {
        return;
    };

    let screen = ctx.screen_rect();
    // The vanilla hotbar plus the health / food rows reach ~40 HUD pixels up
    // from the bottom; one HUD pixel spans `gui_scale` framebuffer pixels.
    let hud_px = info.gui_scale / ctx.pixels_per_point();
    let center = egui::pos2(screen.center().x, screen.bottom() - 52.0 * hud_px);

    let name_font = FontId::proportional(16.0);
    let name_color = if info.in_range {
        theme::TEXT
    } else {
        theme::TEXT_MUTED
    };
    let name_w = ctx.fonts(|f| {
        f.layout_no_wrap(info.name.clone(), name_font.clone(), name_color)
            .size()
            .x
    });

    let chip = Rect::from_center_size(center, Vec2::new(name_w + 70.0, 28.0));
    painter.rect_filled(
        chip,
        Rounding::same(theme::RADIUS),
        Color32::from_black_alpha(165),
    );
    painter.rect_stroke(
        chip,
        Rounding::same(theme::RADIUS),
        Stroke::new(1.0_f32, theme::BORDER),
    );
    painter.text(
        center,
        Align2::CENTER_CENTER,
        &info.name,
        name_font,
        name_color,
    );

    // Arrows either side of the name — W / S step through the players.
    let arrow = |tip_x: f32, dir: f32| {
        let s = 6.0;
        let points = vec![
            egui::pos2(tip_x, center.y),
            egui::pos2(tip_x - dir * s * 1.3, center.y - s),
            egui::pos2(tip_x - dir * s * 1.3, center.y + s),
        ];
        painter.add(egui::Shape::convex_polygon(
            points,
            theme::ACCENT,
            Stroke::NONE,
        ));
    };
    arrow(chip.left() + 10.0, -1.0);
    arrow(chip.right() - 10.0, 1.0);

    let hint = match info.mode {
        Mode::Orbit => "Auto · W/S player · mouse rotate · wheel zoom · J 3D · Esc exit",
        Mode::Free => "3D · WASD Space Shift fly · wheel speed · J Auto · Esc exit",
    };
    let hint = if info.in_range {
        hint.to_string()
    } else {
        format!("out of range · {hint}")
    };
    painter.text(
        egui::pos2(center.x, chip.bottom() + 4.0),
        Align2::CENTER_TOP,
        hint,
        FontId::proportional(11.5),
        theme::TEXT_DIM,
    );
}

/// Top-left brand badge: `Dark` + accented `Client` inside a rounded chip.
fn draw_watermark(ctx: &Context, painter: &Painter) {
    let font = FontId::proportional(17.0);
    let pad = Vec2::new(11.0, 6.0);

    // Measure both halves so the chip hugs the text exactly.
    let (dark_w, text_h) = ctx.fonts(|f| {
        let g = f.layout_no_wrap("Dark".to_owned(), font.clone(), theme::TEXT);
        (g.size().x, g.size().y)
    });
    let client_w = ctx.fonts(|f| {
        f.layout_no_wrap("Client".to_owned(), font.clone(), theme::ACCENT)
            .size()
            .x
    });

    let size = Vec2::new(dark_w + client_w + pad.x * 2.0, text_h + pad.y * 2.0);
    let rect = Rect::from_min_size(egui::pos2(MARGIN, MARGIN), size);

    painter.rect_filled(
        rect,
        Rounding::same(theme::RADIUS),
        Color32::from_black_alpha(165),
    );
    painter.rect_stroke(
        rect,
        Rounding::same(theme::RADIUS),
        Stroke::new(1.0_f32, theme::BORDER),
    );

    // Accent edge on the left side of the chip.
    let edge = Rect::from_min_size(rect.min, Vec2::new(3.0, rect.height()));
    painter.rect_filled(
        edge,
        Rounding {
            nw: theme::RADIUS,
            sw: theme::RADIUS,
            ne: 0.0,
            se: 0.0,
        },
        theme::ACCENT,
    );

    let anchor = egui::pos2(rect.min.x + pad.x, rect.center().y);
    let after = painter.text(
        anchor,
        Align2::LEFT_CENTER,
        "Dark",
        font.clone(),
        theme::TEXT,
    );
    painter.text(
        egui::pos2(after.max.x, anchor.y),
        Align2::LEFT_CENTER,
        "Client",
        font,
        theme::ACCENT,
    );
}

/// Top-right list of enabled modules. Each row slides in/out smoothly when
/// its module is toggled, so nothing ever pops in abruptly.
fn draw_arraylist(ctx: &Context, painter: &Painter) {
    const ROW_H: f32 = 19.0;
    const PAD_X: f32 = 8.0;
    let font = FontId::proportional(14.0);

    // One lock per module: snapshot just the name and enabled flag.
    let snapshot: Vec<(String, bool)> = crate::state::client()
        .modules
        .handles()
        .iter()
        .filter_map(|m| {
            let module = m.lock().ok()?;
            let data = module.get_module_data();
            Some((data.name().to_string(), data.enabled))
        })
        .collect();

    // Resolve a smooth presence factor for every module. Disabled modules
    // keep a slot while their factor decays toward zero.
    let mut rows: Vec<(String, f32, f32)> = Vec::new(); // (name, factor, text width)
    for (name, enabled) in snapshot {
        let factor = anim::toggle(
            ctx,
            Id::new("arraylist").with(&name),
            enabled,
            0.22,
            Easing::Out,
        );
        if factor <= 0.001 {
            continue;
        }
        let width = ctx.fonts(|f| {
            f.layout_no_wrap(name.clone(), font.clone(), theme::TEXT)
                .size()
                .x
        });
        rows.push((name, factor, width));
    }
    if rows.is_empty() {
        return;
    }

    // Longest entry on top — the classic staircase silhouette.
    rows.sort_by(|a, b| b.2.total_cmp(&a.2));

    let screen_w = ctx.screen_rect().width();
    let mut y = MARGIN;

    for (name, factor, text_w) in &rows {
        // `factor` is already eased by `anim::toggle`.
        let eased = *factor;
        let row_w = text_w + PAD_X * 2.0;
        // Slide the row in from beyond the right screen edge.
        let x = screen_w - MARGIN - row_w * eased;
        let rect = Rect::from_min_size(egui::pos2(x, y), Vec2::new(row_w, ROW_H));

        painter.rect_filled(
            rect,
            Rounding::ZERO,
            theme::with_alpha(Color32::from_black_alpha(175), eased),
        );

        // Accent tab welded to the right screen edge.
        let tab = Rect::from_min_size(
            rect.right_top() - Vec2::new(2.0, 0.0),
            Vec2::new(2.0, ROW_H),
        );
        painter.rect_filled(tab, Rounding::ZERO, theme::with_alpha(theme::ACCENT, eased));

        painter.text(
            egui::pos2(rect.min.x + PAD_X, rect.center().y),
            Align2::LEFT_CENTER,
            name,
            font.clone(),
            theme::with_alpha(theme::TEXT, eased),
        );

        y += ROW_H + 2.0;
    }
}
