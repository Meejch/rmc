//! Look of the launcher: dark blue surfaces, cards with thin borders,
//! blue accent. Colours, text sizes and small widgets shared by the views.

use eframe::egui::{
    self, Color32, CornerRadius, FontId, Margin, Response, Sense, Stroke, TextStyle, Ui, vec2,
};

use super::icons::{self, Icon};

pub const BG: Color32 = Color32::from_rgb(14, 20, 31);
pub const CARD: Color32 = Color32::from_rgb(21, 29, 43);
pub const CARD_HI: Color32 = Color32::from_rgb(27, 38, 57);
pub const INPUT: Color32 = Color32::from_rgb(12, 18, 29);
pub const BORDER: Color32 = Color32::from_rgb(40, 52, 72);
pub const BORDER_HI: Color32 = Color32::from_rgb(58, 74, 100);
pub const ACCENT: Color32 = Color32::from_rgb(38, 120, 232);
pub const ACCENT_HI: Color32 = Color32::from_rgb(62, 142, 248);
pub const TEXT: Color32 = Color32::from_rgb(228, 234, 242);
pub const WEAK: Color32 = Color32::from_rgb(140, 153, 171);
pub const GREEN: Color32 = Color32::from_rgb(60, 196, 96);
pub const YELLOW: Color32 = Color32::from_rgb(228, 182, 56);
pub const GREY: Color32 = Color32::from_rgb(110, 120, 135);
pub const RED: Color32 = Color32::from_rgb(232, 98, 76);

pub fn apply(ctx: &egui::Context) {
    let mut style = (*ctx.global_style()).clone();
    use egui::FontFamily::Proportional;
    style.text_styles = [
        (TextStyle::Heading, FontId::new(20.0, Proportional)),
        (TextStyle::Body, FontId::new(14.5, Proportional)),
        (TextStyle::Button, FontId::new(14.5, Proportional)),
        (TextStyle::Small, FontId::new(12.0, Proportional)),
        (
            TextStyle::Monospace,
            FontId::new(13.0, egui::FontFamily::Monospace),
        ),
    ]
    .into();
    style.spacing.item_spacing = vec2(8.0, 8.0);
    style.spacing.button_padding = vec2(12.0, 6.0);
    style.spacing.interact_size.y = 30.0;
    let v = &mut style.visuals;
    *v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = CARD;
    v.window_stroke = Stroke::new(1.0, BORDER_HI);
    v.extreme_bg_color = INPUT;
    v.faint_bg_color = CARD_HI;
    v.override_text_color = Some(TEXT);
    v.selection.bg_fill = ACCENT;
    v.selection.stroke = Stroke::new(1.0, TEXT);
    v.hyperlink_color = ACCENT_HI;
    v.window_corner_radius = CornerRadius::same(10);
    v.menu_corner_radius = CornerRadius::same(8);
    let r = CornerRadius::same(7);
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
    v.widgets.noninteractive.corner_radius = r;
    v.widgets.inactive.weak_bg_fill = CARD_HI;
    v.widgets.inactive.bg_fill = CARD_HI;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, BORDER);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT);
    v.widgets.inactive.corner_radius = r;
    v.widgets.hovered.weak_bg_fill = Color32::from_rgb(36, 50, 74);
    v.widgets.hovered.bg_fill = Color32::from_rgb(36, 50, 74);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, BORDER_HI);
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, TEXT);
    v.widgets.hovered.corner_radius = r;
    v.widgets.active.weak_bg_fill = Color32::from_rgb(44, 62, 92);
    v.widgets.active.bg_fill = Color32::from_rgb(44, 62, 92);
    v.widgets.active.bg_stroke = Stroke::new(1.0, ACCENT);
    v.widgets.active.corner_radius = r;
    v.widgets.open.weak_bg_fill = CARD_HI;
    v.widgets.open.corner_radius = r;
    ctx.set_global_style(style);
}

/// A card: panel colour, thin border, rounded.
pub fn card() -> egui::Frame {
    egui::Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::same(16))
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Primary,
    Secondary,
    /// Only the icon, square.
    Icon,
    /// Icon button that turns blue on hover (connect in the favorites list).
    IconAccent,
}

/// Button with an optional icon; `min_w` 0 = as wide as its content.
pub fn button(ui: &mut Ui, icon: Option<Icon>, text: &str, kind: Kind, min_w: f32) -> Response {
    button_enabled(ui, icon, text, kind, min_w, true)
}

pub fn button_enabled(
    ui: &mut Ui,
    icon: Option<Icon>,
    text: &str,
    kind: Kind,
    min_w: f32,
    enabled: bool,
) -> Response {
    let font = TextStyle::Button.resolve(ui.style());
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_string(), font, Color32::WHITE);
    let h = 34.0;
    let icon_w = if icon.is_some() { 18.0 } else { 0.0 };
    let gap = if icon.is_some() && !text.is_empty() {
        8.0
    } else {
        0.0
    };
    let content = icon_w + gap + galley.size().x;
    let w = match kind {
        Kind::Icon | Kind::IconAccent => h,
        _ => (content + 28.0).max(min_w),
    };
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, resp) = ui.allocate_exact_size(vec2(w, h), sense);
    if ui.is_rect_visible(rect) {
        let hovered = enabled && resp.hovered();
        let down = enabled && resp.is_pointer_button_down_on();
        let (fill, stroke) = match kind {
            Kind::Primary => (
                if down {
                    ACCENT
                } else if hovered {
                    ACCENT_HI
                } else {
                    ACCENT
                },
                Stroke::NONE,
            ),
            Kind::IconAccent if hovered => (ACCENT, Stroke::NONE),
            _ => (
                if hovered {
                    Color32::from_rgb(36, 50, 74)
                } else {
                    CARD_HI
                },
                Stroke::new(1.0, if hovered { BORDER_HI } else { BORDER }),
            ),
        };
        let fill = if enabled {
            fill
        } else {
            fill.gamma_multiply(0.5)
        };
        let p = ui.painter();
        p.rect(
            rect,
            CornerRadius::same(7),
            fill,
            stroke,
            egui::StrokeKind::Inside,
        );
        let fg = if enabled { TEXT } else { WEAK };
        let mut x = rect.center().x - content / 2.0;
        if let Some(i) = icon {
            let r = egui::Rect::from_center_size(
                egui::pos2(x + 9.0, rect.center().y),
                vec2(18.0, 18.0),
            );
            icons::paint(p, r, i, fg);
            x += icon_w + gap;
        }
        if !text.is_empty() {
            let pos = egui::pos2(x, rect.center().y - galley.size().y / 2.0);
            p.galley_with_override_text_color(pos, galley, fg);
        }
    }
    if enabled {
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        resp
    }
}

/// Text field with an icon in front, framed like an input. `password`: show/hide toggle.
pub fn field(
    ui: &mut Ui,
    icon: Icon,
    text: &mut String,
    hint: &str,
    width: f32,
    enabled: bool,
    password: Option<&mut bool>,
) -> Response {
    let id = ui.next_auto_id();
    let focused = ui
        .memory(|m| m.focused())
        .is_some_and(|f| f == id.with("edit"));
    let border = if focused { ACCENT } else { BORDER };
    let mut out = None;
    egui::Frame::new()
        .fill(if enabled {
            INPUT
        } else {
            INPUT.gamma_multiply(0.6)
        })
        .stroke(Stroke::new(if focused { 1.5 } else { 1.0 }, border))
        .corner_radius(CornerRadius::same(7))
        .inner_margin(Margin::symmetric(10, 4))
        .show(ui, |ui| {
            ui.set_width(width - 22.0);
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(vec2(18.0, 26.0), Sense::hover());
                icons::paint(ui.painter(), r, icon, if enabled { WEAK } else { GREY });
                let eye_w = if password.is_some() { 28.0 } else { 0.0 };
                let hidden = password.as_ref().is_some_and(|v| !**v);
                let te = egui::TextEdit::singleline(text)
                    .id(id.with("edit"))
                    .frame(egui::Frame::NONE)
                    .hint_text(hint)
                    .password(hidden)
                    .align(egui::Align2::LEFT_CENTER)
                    .desired_width(ui.available_width() - eye_w);
                out = Some(ui.add_enabled(enabled, te));
                if let Some(show) = password {
                    let (r, resp) = ui.allocate_exact_size(vec2(22.0, 26.0), Sense::click());
                    icons::paint(
                        ui.painter(),
                        r,
                        if *show { Icon::EyeOff } else { Icon::Eye },
                        WEAK,
                    );
                    if resp
                        .on_hover_text(if *show {
                            "Passwort verbergen"
                        } else {
                            "Passwort zeigen"
                        })
                        .clicked()
                    {
                        *show = !*show;
                    }
                }
            });
        });
    out.expect("added")
}

/// Status light (filled circle) or ring.
pub fn dot(ui: &mut Ui, color: Option<Color32>, size: f32) -> Response {
    let (r, resp) = ui.allocate_exact_size(vec2(size + 4.0, size + 4.0), Sense::hover());
    match color {
        Some(c) => {
            ui.painter().circle_filled(r.center(), size / 2.0, c);
        }
        None => {
            ui.painter()
                .circle_stroke(r.center(), size / 2.0 - 0.5, Stroke::new(1.5, GREY));
        }
    }
    resp
}

/// Tab header line: returns the clicked tab. Selected tab underlined in accent.
pub fn tabs(ui: &mut Ui, items: &[(Icon, &str)], selected: usize) -> Option<usize> {
    let mut clicked = None;
    let top = ui.cursor().top();
    ui.horizontal(|ui| {
        for (i, (icon, label)) in items.iter().enumerate() {
            let font = TextStyle::Body.resolve(ui.style());
            let g = ui.painter().layout_no_wrap(label.to_string(), font, TEXT);
            let w = g.size().x + 18.0 + 8.0 + 24.0;
            let (rect, resp) = ui.allocate_exact_size(vec2(w, 36.0), Sense::click());
            let sel = i == selected;
            let fg = if sel || resp.hovered() { TEXT } else { WEAK };
            let p = ui.painter();
            let ir = egui::Rect::from_center_size(
                egui::pos2(rect.left() + 21.0, rect.center().y),
                vec2(18.0, 18.0),
            );
            icons::paint(p, ir, *icon, if sel { ACCENT_HI } else { fg });
            p.galley_with_override_text_color(
                egui::pos2(rect.left() + 38.0, rect.center().y - g.size().y / 2.0),
                g,
                fg,
            );
            if sel {
                p.rect_filled(
                    egui::Rect::from_min_max(
                        egui::pos2(rect.left(), rect.bottom() - 3.0),
                        rect.right_bottom(),
                    ),
                    CornerRadius::same(1),
                    ACCENT,
                );
            }
            if resp
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .clicked()
            {
                clicked = Some(i);
            }
        }
    });
    let _ = top;
    let r = ui.max_rect();
    let y = ui.cursor().top() - ui.spacing().item_spacing.y + 0.5;
    ui.painter()
        .hline(r.left()..=r.right(), y, Stroke::new(1.0, BORDER));
    clicked
}

/// Weak, small text.
pub fn weak(text: impl Into<String>) -> egui::RichText {
    egui::RichText::new(text.into()).color(WEAK)
}
